//! Auto-router for local-vs-cloud LLM inference.
//!
//! A "basic" router: an interpretable, debuggable decision list — no learned
//! model. Evaluated in order, first match wins:
//!
//!   1. Privacy   — sensitive content NEVER goes to cloud (hard stop)
//!   2. Size      — prompts too big for the local context window go to cloud
//!   3. Budget    — over the cloud budget? stay local
//!   4. Task type — known-hard tasks go straight to cloud
//!   5. Default   — local, with escalation if the answer looks inadequate
//!
//! The goal is to never pay the "try local, wait 8s, discover it can't" cost
//! for queries we could have routed correctly up front — while keeping private
//! data on-device by default.

use serde::{Deserialize, Serialize};

/// Where a query should be routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Route {
    /// Must stay on-device. Cloud is not an option (privacy).
    LocalOnly,
    /// Prefer local, but escalate to cloud if the answer looks inadequate.
    LocalWithEscalation,
    /// Go straight to cloud (too big, too hard, or local unavailable).
    Cloud,
}

impl Route {
    /// Whether this route permits cloud inference at all.
    /// Used by callers deciding if escalation is even an option.
    #[allow(dead_code)]
    pub fn allows_cloud(self) -> bool {
        !matches!(self, Route::LocalOnly)
    }
}

/// Configuration for the router.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    /// Max estimated tokens to keep a prompt on the local model.
    /// Local CPU context is ~4096; leave headroom for the response.
    pub local_ctx_safe_tokens: usize,
    /// Daily cloud budget in USD. 0.0 = unlimited. Over budget => local.
    pub cloud_budget_daily_usd: f64,
    /// Latency preference: true = favor fast local answers.
    pub prefer_fast: bool,
    /// Enable escalation to cloud on low-confidence local answers.
    pub escalate_on_low_confidence: bool,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            local_ctx_safe_tokens: 3000,
            cloud_budget_daily_usd: 0.0, // unlimited unless set
            prefer_fast: false,
            escalate_on_low_confidence: true,
        }
    }
}

/// The auto-router. Cheap to construct; all state is borrowed.
pub struct AutoRouter {
    config: RouterConfig,
    local_available: bool,
    cloud_available: bool,
}

impl AutoRouter {
    pub fn new(config: RouterConfig, local_available: bool, cloud_available: bool) -> Self {
        Self {
            config,
            local_available,
            cloud_available,
        }
    }

    /// Decide where to route a query. `cloud_spent_today_usd` is the caller's
    /// running total for budget enforcement.
    pub fn route(&self, query: &str, context: &str, cloud_spent_today_usd: f64) -> Route {
        // 0. Availability short-circuits.
        if !self.local_available {
            return if self.cloud_available {
                Route::Cloud
            } else {
                // Nothing available; caller will surface the error.
                Route::LocalOnly
            };
        }
        if !self.cloud_available {
            return Route::LocalOnly;
        }

        // 1. Privacy: sensitive content never leaves the device.
        if is_sensitive(query) || is_sensitive(context) {
            return Route::LocalOnly;
        }

        // 2. Size: too big for the local context window.
        let est = estimate_tokens(query) + estimate_tokens(context);
        if est > self.config.local_ctx_safe_tokens {
            return Route::Cloud;
        }

        // 3. Budget: over the daily cloud cap.
        if self.config.cloud_budget_daily_usd > 0.0
            && cloud_spent_today_usd >= self.config.cloud_budget_daily_usd
        {
            return Route::LocalOnly;
        }

        // 4. Task type: known-hard for small local models.
        if is_known_hard_task(query) && !self.config.prefer_fast {
            return Route::Cloud;
        }

        // 5. Default: local, escalate if inadequate.
        if self.config.escalate_on_low_confidence {
            Route::LocalWithEscalation
        } else {
            Route::LocalOnly
        }
    }
}

/// Rough token estimate: ~4 chars per token for English text.
/// Good enough for routing decisions; not for billing.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Detect content that must not leave the device.
///
/// Broader than PII: also credentials, private file paths, and raw file
/// contents. Conservative — false positives just mean "stays local," which
/// is the safe direction.
pub fn is_sensitive(text: &str) -> bool {
    let lower = text.to_lowercase();

    // Credentials & secrets
    const SECRET_MARKERS: &[&str] = &[
        "password",
        "passwd",
        "api_key",
        "apikey",
        "secret",
        "private_key",
        "BEGIN RSA PRIVATE KEY",
        "BEGIN OPENSSH PRIVATE KEY",
        "BEGIN PGP PRIVATE KEY",
        "bearer ",
        "sk-ant-",
        "sk-or-",
        "sk-",   // generic API key prefix
        "ghp_",  // GitHub PAT
        "gho_",  // GitHub OAuth
        "xoxb-", // Slack bot token
        "AKIA",  // AWS access key id
        "-----BEGIN",
    ];
    if SECRET_MARKERS
        .iter()
        .any(|m| lower.contains(&m.to_lowercase()))
    {
        return true;
    }

    // PII patterns (cheap substring/regex-free checks; the collective module
    // has a fuller regex scanner for the sharing path).
    if contains_email_like(text) || contains_ssn_like(text) || contains_card_like(text) {
        return true;
    }

    // Home-directory paths often embed a username + private layout.
    if text.contains("/home/") || text.contains("C:\\Users\\") {
        return true;
    }

    false
}

fn contains_email_like(text: &str) -> bool {
    // '@' with a '.' after it and no spaces around — good enough for routing.
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'@' && i > 0 && i + 3 < bytes.len() {
            let after = &text[i..];
            if after.contains('.') && !after.chars().take(2).any(|c| c.is_whitespace()) {
                return true;
            }
        }
    }
    false
}

fn contains_ssn_like(text: &str) -> bool {
    // ddd-dd-dddd
    let digits: Vec<char> = text.chars().collect();
    for w in text.split_whitespace() {
        let w = w.trim_matches(|c: char| !c.is_ascii_digit() && c != '-');
        let parts: Vec<&str> = w.split('-').collect();
        if parts.len() == 3
            && parts[0].len() == 3
            && parts[1].len() == 2
            && parts[2].len() == 4
            && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
        {
            return true;
        }
    }
    let _ = digits;
    false
}

fn contains_card_like(text: &str) -> bool {
    // 13-19 consecutive digits (possibly space/dash separated).
    let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    // Only flag if there's a long run, not just a phone # or date scattered.
    let mut run = 0;
    for c in text.chars() {
        if c.is_ascii_digit() {
            run += 1;
            if run >= 15 {
                return true;
            }
        } else if c == ' ' || c == '-' {
            continue;
        } else {
            run = 0;
        }
    }
    let _ = digits;
    false
}

/// Detect task types that small local models handle poorly.
fn is_known_hard_task(query: &str) -> bool {
    let lower = query.to_lowercase();
    const HARD_MARKERS: &[&str] = &[
        "analyze this",
        "analyse this",
        "refactor",
        "across multiple files",
        "entire codebase",
        "whole codebase",
        "write a full",
        "write an entire",
        "step by step tutorial",
        "research",
        "compare and contrast",
        "long document",
        "summarize this book",
        "legal implications",
        "security audit",
    ];
    HARD_MARKERS.iter().any(|m| lower.contains(m))
}

/// Heuristic: does a local answer look inadequate, warranting escalation?
///
/// Cheap checks only — no extra LLM call. Catches the common local-model
/// failure modes we observed in testing (empty, refusal, tool-name
/// hallucination, degenerate repetition).
pub fn looks_inadequate(answer: &str) -> bool {
    let trimmed = answer.trim();

    // Empty or near-empty
    if trimmed.len() < 3 {
        return true;
    }

    let lower = trimmed.to_lowercase();

    // Refusals / incapability claims (the model punting)
    const PUNT_MARKERS: &[&str] = &[
        "i cannot",
        "i can't",
        "i am unable",
        "i'm unable",
        "i don't have access",
        "i do not have access",
        "as an ai language model",
        "i'm just a language model",
        "i cannot execute",
        "i cannot run",
    ];
    if PUNT_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }

    // Degenerate repetition (same line repeated many times)
    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() >= 6 {
        let first = lines[0];
        let repeats = lines.iter().filter(|&&l| l == first).count();
        if repeats >= lines.len() / 2 {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router(local: bool, cloud: bool) -> AutoRouter {
        AutoRouter::new(RouterConfig::default(), local, cloud)
    }

    #[test]
    fn test_sensitive_stays_local() {
        let r = router(true, true);
        assert_eq!(r.route("my password is hunter2", "", 0.0), Route::LocalOnly);
        assert_eq!(
            r.route("send this to sk-or-abc123", "", 0.0),
            Route::LocalOnly
        );
        assert_eq!(
            r.route("email me at bob@example.com", "", 0.0),
            Route::LocalOnly
        );
        assert_eq!(
            r.route("cat /home/alice/secret.txt", "", 0.0),
            Route::LocalOnly
        );
    }

    #[test]
    fn test_big_prompt_goes_cloud() {
        let r = router(true, true);
        let big = "x".repeat(20_000); // ~5000 tokens
        assert_eq!(r.route(&big, "", 0.0), Route::Cloud);
    }

    #[test]
    fn test_budget_cap() {
        let r = router(true, true);
        assert_eq!(r.route("hi", "", 10.0), Route::LocalWithEscalation);
        let r_budget = AutoRouter::new(
            RouterConfig {
                cloud_budget_daily_usd: 5.0,
                ..Default::default()
            },
            true,
            true,
        );
        assert_eq!(r_budget.route("hi", "", 6.0), Route::LocalOnly);
    }

    #[test]
    fn test_hard_task_goes_cloud() {
        let r = router(true, true);
        assert_eq!(
            r.route("refactor the entire codebase", "", 0.0),
            Route::Cloud
        );
    }

    #[test]
    fn test_default_local_with_escalation() {
        let r = router(true, true);
        assert_eq!(
            r.route("what time is it", "", 0.0),
            Route::LocalWithEscalation
        );
    }

    #[test]
    fn test_no_cloud_means_local_only() {
        let r = router(true, false);
        assert_eq!(r.route("refactor everything", "", 0.0), Route::LocalOnly);
    }

    #[test]
    fn test_no_local_means_cloud() {
        let r = router(false, true);
        assert_eq!(r.route("hi", "", 0.0), Route::Cloud);
    }

    #[test]
    fn test_looks_inadequate() {
        assert!(looks_inadequate(""));
        assert!(looks_inadequate("  "));
        assert!(looks_inadequate("I cannot execute shell commands."));
        assert!(looks_inadequate("I don't have access to previous messages"));
        assert!(!looks_inadequate("You are running Ubuntu 26.04 LTS."));
        assert!(!looks_inadequate("234"));
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens(&"x".repeat(400)), 100);
    }
}
