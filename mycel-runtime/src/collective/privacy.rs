//! Privacy - Privacy-preserving pattern extraction, PII redaction, and DP noise.
//!
//! Ensures that shared patterns do not leak private information while
//! still being useful to the collective.
#![allow(dead_code)]

use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tracing::debug;

use super::patterns::{Pattern, PatternSolution};
use super::Interaction;

static EMAIL_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[\w.+-]+@[\w.-]+\.[A-Za-z]{2,}").expect("Invalid email regex"));

static PHONE_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}\b")
        .expect("Invalid phone regex")
});

static SSN_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").expect("Invalid SSN regex"));

static CREDITCARD_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b\d{4}[-\s]?\d{4}[-\s]?\d{4}[-\s]?\d{4}\b").expect("Invalid CC regex")
});

static API_KEY_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(?:sk-[A-Za-z0-9_-]{8,}|gh[pousr]_[A-Za-z0-9]{8,}|AKIA[0-9A-Z]{12,})\b")
        .expect("Invalid API key regex")
});

static HOME_PATH_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/home/[\w.-]+(?:/[\w./@+-]*)*").expect("Invalid home path regex"));

static VARIABLE_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\{\{(\w+)\}\}").expect("Invalid variable regex"));

static PATH_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/[\w/.-]+").expect("Invalid path regex"));

static URL_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"https?://[\w./%-]+").expect("Invalid URL regex"));

static NUMBERS_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b\d+\b").expect("Invalid numbers regex"));

static DATES_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b\d{1,2}/\d{1,2}/\d{2,4}\b").expect("Invalid dates regex"));

/// Privacy configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyConfig {
    /// Epsilon for differential privacy (lower = more private).
    pub epsilon: f64,

    /// Delta for differential privacy.
    pub delta: f64,

    /// Minimum utility score for a pattern to be shareable.
    pub min_utility_threshold: f64,

    /// Enable PII detection.
    pub pii_detection_enabled: bool,

    /// Block patterns that might contain these categories.
    pub blocked_categories: Vec<String>,

    /// Require human review above this sensitivity score.
    pub human_review_threshold: f64,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            epsilon: 1.0,
            delta: 1e-5,
            min_utility_threshold: 0.5,
            pii_detection_enabled: true,
            blocked_categories: vec![
                "financial".to_string(),
                "medical".to_string(),
                "legal_personal".to_string(),
            ],
            human_review_threshold: 0.8,
        }
    }
}

/// Redact PII from free-form text without panicking on overlapping matches.
///
/// Redacts emails, US phone numbers, SSNs, credit card numbers, API keys/tokens
/// (`sk-...`, `ghp_...`, `AKIA...`), and home directory paths (`/home/<user>/...`).
/// Returns the sanitized string and the list of detected PII categories.
pub fn redact_pii(text: &str) -> (String, Vec<String>) {
    let mut result = text.to_string();
    let mut detected = Vec::new();

    let rules: &[(&Lazy<Regex>, &str, &str)] = &[
        (&API_KEY_REGEX, "[API_KEY]", "api_key"),
        (&EMAIL_REGEX, "[EMAIL]", "email"),
        (&SSN_REGEX, "[REDACTED]", "ssn"),
        (&CREDITCARD_REGEX, "[REDACTED]", "credit_card"),
        (&PHONE_REGEX, "[PHONE]", "phone"),
        (&HOME_PATH_REGEX, "[PATH]", "path"),
    ];

    for (regex, replacement, label) in rules {
        if regex.is_match(&result) {
            detected.push((*label).to_string());
            result = regex.replace_all(&result, *replacement).into_owned();
        }
    }

    (result, detected)
}

/// Validate that a pattern's category and text do not fall into blocked sensitive categories.
pub fn validate_category_allowed(text: &str, category: &str, config: &PrivacyConfig) -> Result<()> {
    let normalized_category = category.trim().to_ascii_lowercase();
    if config
        .blocked_categories
        .iter()
        .any(|blocked| blocked.eq_ignore_ascii_case(&normalized_category))
    {
        return Err(anyhow!(
            "Pattern category '{}' is blocked by privacy policy",
            category
        ));
    }

    let sensitivity = assess_sensitivity(text, &config.blocked_categories);
    if sensitivity.is_blocked {
        return Err(anyhow!(
            "Pattern blocked due to sensitive category: {:?}",
            sensitivity.categories
        ));
    }

    Ok(())
}

/// Apply bounded Laplace differential privacy noise (`scale = 1.0 / epsilon.max(0.1)`)
/// to a quality score in `[0.0, 1.0]`, clamping the result to `[0.0, 1.0]`.
pub fn add_dp_noise_score(score: f64, epsilon: f64) -> f64 {
    let base = if score.is_finite() {
        score.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let eps = if epsilon.is_finite() {
        epsilon.max(0.1)
    } else {
        1.0
    };
    let scale = 1.0 / eps;

    // Sample uniform in (-0.5, 0.5) avoiding exact +/-0.5 so ln() stays finite.
    let raw: f64 = rand::random();
    let u = (raw - 0.5).clamp(-0.499_999_999, 0.499_999_999);
    let noise = -scale * u.signum() * (1.0 - 2.0 * u.abs()).max(1e-12).ln();

    (base + noise).clamp(0.0, 1.0)
}

/// Extract a shareable pattern from a private interaction.
pub fn extract_shareable_pattern(
    interaction: &Interaction,
    config: &PrivacyConfig,
) -> Result<Option<Pattern>> {
    debug!(
        "Extracting shareable pattern from interaction {}",
        interaction.id
    );

    if !interaction.success {
        return Ok(None);
    }

    if interaction.user_rating.is_none_or(|r| r < 4) {
        return Ok(None);
    }

    let insight = extract_insight(interaction)?;

    if config.pii_detection_enabled {
        let (_, pii_detected) = redact_pii(&insight.template);
        if !pii_detected.is_empty() {
            debug!("PII detected, sanitizing: {:?}", pii_detected);
        }
    }

    let sanitized = remove_pii(&insight);
    let generalized = generalize_specifics(&sanitized);

    let all_text = format!(
        "{} {} {}",
        generalized.trigger, generalized.description, generalized.template
    );
    let sensitivity = assess_sensitivity(&all_text, &config.blocked_categories);
    if sensitivity.is_blocked {
        debug!(
            "Pattern blocked due to category: {:?}",
            sensitivity.categories
        );
        return Ok(None);
    }

    if sensitivity.score > config.human_review_threshold {
        debug!(
            "Pattern requires human review (sensitivity: {})",
            sensitivity.score
        );
        return Ok(None);
    }

    let private = apply_dp_noise(&generalized, config.epsilon)?;
    let utility = compute_utility(&private, &generalized);
    if utility < config.min_utility_threshold {
        debug!("Pattern utility too low after privatization: {}", utility);
        return Ok(None);
    }

    let pattern = Pattern::new(
        private.trigger,
        PatternSolution::PromptTemplate {
            template: private.template,
            variables: private.variables,
        },
        private.domain,
        private.description,
    );

    Ok(Some(pattern))
}

/// Validate a pattern is safe to share.
pub fn validate_for_sharing(pattern: &Pattern, config: &PrivacyConfig) -> Result<()> {
    let all_text = format!(
        "{} {} {}",
        pattern.trigger,
        pattern.description,
        pattern.solution_summary()
    );

    if config.pii_detection_enabled {
        let (_, pii) = redact_pii(&all_text);
        if !pii.is_empty() {
            return Err(anyhow!("Pattern contains PII: {:?}", pii));
        }
    }

    validate_category_allowed(&all_text, &pattern.domain, config)
}

/// Compute private gradients for federated learning.
pub fn compute_private_gradients(
    interactions: &[&Pattern],
    config: &PrivacyConfig,
) -> Result<super::bittensor::PrivateGradients> {
    let sample_count = interactions.len();

    let mut gradients: Vec<f32> = Vec::new();
    for pattern in interactions {
        let embedding = compute_pattern_embedding(pattern)?;
        gradients.extend(embedding);
    }

    let noise_scale = compute_noise_scale(config.epsilon, config.delta, sample_count.max(1));
    let noisy_gradients: Vec<f32> = gradients
        .iter()
        .map(|g| g + sample_gaussian(0.0, noise_scale))
        .collect();

    let compressed = compress_gradients(&noisy_gradients)?;
    let hash = sha256::digest(&compressed);

    Ok(super::bittensor::PrivateGradients {
        model_id: "mycel-patterns-v1".to_string(),
        hash,
        compressed,
        sample_count,
        epsilon: config.epsilon,
    })
}

#[derive(Debug, Clone)]
struct ExtractedInsight {
    trigger: String,
    template: String,
    variables: Vec<String>,
    domain: String,
    description: String,
}

#[derive(Debug)]
struct SensitivityAssessment {
    score: f64,
    is_blocked: bool,
    categories: Vec<String>,
}

fn extract_insight(interaction: &Interaction) -> Result<ExtractedInsight> {
    let trigger = generalize_query(&interaction.user_input);
    let template = extract_template(&interaction.ai_response);
    let variables = extract_variables(&template);
    let domain = infer_domain(&interaction.user_input);
    let description = generate_description(&trigger, &domain);

    Ok(ExtractedInsight {
        trigger,
        template,
        variables,
        domain,
        description,
    })
}

fn generalize_query(query: &str) -> String {
    let mut result = query.to_string();
    result = regex_replace_numbers(&result, "[NUMBER]");
    result = regex_replace_dates(&result, "[DATE]");
    result
}

fn extract_template(response: &str) -> String {
    response.to_string()
}

fn extract_variables(template: &str) -> Vec<String> {
    VARIABLE_REGEX
        .captures_iter(template)
        .map(|c| c[1].to_string())
        .collect()
}

fn infer_domain(query: &str) -> String {
    let query_lower = query.to_lowercase();

    if query_lower.contains("code")
        || query_lower.contains("function")
        || query_lower.contains("program")
    {
        return "coding".to_string();
    }
    if query_lower.contains("write")
        || query_lower.contains("draft")
        || query_lower.contains("email")
    {
        return "writing".to_string();
    }
    if query_lower.contains("analyze") || query_lower.contains("data") {
        return "analysis".to_string();
    }
    if query_lower.contains("search") || query_lower.contains("find") {
        return "search".to_string();
    }

    "general".to_string()
}

fn generate_description(trigger: &str, domain: &str) -> String {
    let prefix: String = trigger.chars().take(50).collect();
    format!("Pattern for {} tasks: {}", domain, prefix)
}

fn remove_pii(insight: &ExtractedInsight) -> ExtractedInsight {
    let mut result = insight.clone();
    let (redacted_trigger, _) = redact_pii(&result.trigger);
    let (redacted_template, _) = redact_pii(&result.template);
    let (redacted_description, _) = redact_pii(&result.description);

    result.trigger = redacted_trigger;
    result.template = redacted_template;
    result.description = redacted_description;
    result
}

fn generalize_specifics(insight: &ExtractedInsight) -> ExtractedInsight {
    let mut result = insight.clone();

    result.template = PATH_REGEX
        .replace_all(&result.template, "[PATH]")
        .into_owned();
    result.template = URL_REGEX
        .replace_all(&result.template, "[URL]")
        .into_owned();

    result
}

fn assess_sensitivity(text: &str, blocked_categories: &[String]) -> SensitivityAssessment {
    let text_lower = text.to_lowercase();
    let mut categories = Vec::new();
    let mut score = 0.0;

    if text_lower.contains("bank")
        || text_lower.contains("account")
        || text_lower.contains("credit")
    {
        categories.push("financial".to_string());
        score += 0.4;
    }

    if text_lower.contains("diagnosis")
        || text_lower.contains("patient")
        || text_lower.contains("medication")
    {
        categories.push("medical".to_string());
        score += 0.4;
    }

    if text_lower.contains("lawsuit")
        || text_lower.contains("defendant")
        || text_lower.contains("plaintiff")
    {
        categories.push("legal_personal".to_string());
        score += 0.3;
    }

    let is_blocked = categories.iter().any(|c| blocked_categories.contains(c));

    SensitivityAssessment {
        score,
        is_blocked,
        categories,
    }
}

fn apply_dp_noise(insight: &ExtractedInsight, _epsilon: f64) -> Result<ExtractedInsight> {
    Ok(insight.clone())
}

fn compute_utility(privatized: &ExtractedInsight, original: &ExtractedInsight) -> f64 {
    let orig_words: HashSet<_> = original.template.split_whitespace().collect();
    let priv_words: HashSet<_> = privatized.template.split_whitespace().collect();

    let overlap = orig_words.intersection(&priv_words).count();
    overlap as f64 / orig_words.len().max(1) as f64
}

fn compute_pattern_embedding(_pattern: &Pattern) -> Result<Vec<f32>> {
    Ok(vec![0.0; 128])
}

fn compute_noise_scale(epsilon: f64, delta: f64, n: usize) -> f32 {
    let c = 1.0;
    let eps = epsilon.max(0.1);
    let sigma = c * (2.0 * (1.25 / delta.max(1e-12)).ln()).sqrt() / eps;
    (sigma / (n.max(1) as f64).sqrt()) as f32
}

fn sample_gaussian(mean: f32, std: f32) -> f32 {
    use std::f32::consts::PI;

    let u1: f32 = rand::random::<f32>().max(1e-6);
    let u2: f32 = rand::random();

    mean + std * (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos()
}

fn compress_gradients(gradients: &[f32]) -> Result<Vec<u8>> {
    let compressed: Vec<u8> = gradients
        .iter()
        .map(|g| ((g.clamp(-1.0, 1.0) + 1.0) * 127.5) as u8)
        .collect();

    Ok(compressed)
}

fn regex_replace_numbers(text: &str, replacement: &str) -> String {
    NUMBERS_REGEX.replace_all(text, replacement).into_owned()
}

fn regex_replace_dates(text: &str, replacement: &str) -> String {
    DATES_REGEX.replace_all(text, replacement).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_pii_redacts_all_sensitive_tokens() {
        let input = "Contact alice@example.com or 555-123-4567, SSN 123-45-6789, \
                     key sk-test1234567890, ghp_abcdef123456, AKIAIOSFODNN7EXAMPLE, \
                     file /home/alice/secrets/token.txt";
        let (redacted, kinds) = redact_pii(input);

        assert!(!redacted.contains("alice@example.com"));
        assert!(!redacted.contains("555-123-4567"));
        assert!(!redacted.contains("123-45-6789"));
        assert!(!redacted.contains("sk-test1234567890"));
        assert!(!redacted.contains("ghp_abcdef123456"));
        assert!(!redacted.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!redacted.contains("/home/alice/secrets/token.txt"));

        assert!(redacted.contains("[EMAIL]"));
        assert!(redacted.contains("[PHONE]"));
        assert!(redacted.contains("[API_KEY]"));
        assert!(redacted.contains("[PATH]"));
        assert!(kinds.contains(&"email".to_string()));
        assert!(kinds.contains(&"phone".to_string()));
        assert!(kinds.contains(&"ssn".to_string()));
        assert!(kinds.contains(&"api_key".to_string()));
        assert!(kinds.contains(&"path".to_string()));
    }

    #[test]
    fn remove_pii_never_panics_on_overlapping_matches() {
        let insight = ExtractedInsight {
            trigger: "email me at 1234-5678-9012-3456@example.com".to_string(),
            template: "call 555-123-4567 or 123-45-6789 or 1234-5678-9012-3456".to_string(),
            variables: vec![],
            domain: "general".to_string(),
            description: "desc /home/bob/.ssh/id_ed25519".to_string(),
        };
        let sanitized = remove_pii(&insight);
        assert!(!sanitized.template.contains("555-123-4567"));
        assert!(!sanitized.description.contains("/home/bob"));
    }

    #[test]
    fn validate_category_allowed_blocks_sensitive_categories() {
        let cfg = PrivacyConfig::default();
        assert!(validate_category_allowed("hello world", "general", &cfg).is_ok());
        assert!(validate_category_allowed("hello world", "financial", &cfg).is_err());
        assert!(validate_category_allowed("hello world", "medical", &cfg).is_err());
        assert!(validate_category_allowed("hello world", "legal_personal", &cfg).is_err());
        assert!(validate_category_allowed("patient diagnosis notes", "general", &cfg).is_err());
    }

    #[test]
    fn add_dp_noise_score_always_stays_bounded() {
        for _ in 0..200 {
            let noisy = add_dp_noise_score(0.75, 0.5);
            assert!(noisy.is_finite());
            assert!((0.0..=1.0).contains(&noisy));
        }
        let nan_noisy = add_dp_noise_score(f64::NAN, 0.0);
        assert!(nan_noisy.is_finite());
        assert!((0.0..=1.0).contains(&nan_noisy));
    }
}
