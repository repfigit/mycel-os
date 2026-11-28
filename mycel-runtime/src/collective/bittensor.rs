//! Bittensor Integration
//!
//! Evaluates pattern quality against a configured Bittensor HTTP evaluator
//! endpoint, with a deterministic offline local scoring fallback.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::debug;

const DEFAULT_EVAL_TIMEOUT_SECS: u64 = 10;

/// Result of evaluating a pattern's quality.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationResult {
    pub score: f64,
    pub confidence: f64,
    pub evaluator_count: usize,
}

/// Private gradients structure used by federated privacy helpers.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivateGradients {
    pub model_id: String,
    pub hash: String,
    pub compressed: Vec<u8>,
    pub sample_count: usize,
    pub epsilon: f64,
}

/// Bittensor pattern evaluator client.
#[derive(Clone, Debug)]
pub struct BittensorClient {
    api_url: Option<String>,
    network: String,
    subnet: u16,
    http_client: reqwest::Client,
}

impl BittensorClient {
    /// Create a new Bittensor evaluator client.
    pub fn new(api_url: Option<String>, network: String, subnet: u16) -> Result<Self> {
        let normalized_url = api_url
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty());

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_EVAL_TIMEOUT_SECS))
            .build()
            .context("Failed to build Bittensor HTTP client")?;

        Ok(Self {
            api_url: normalized_url,
            network,
            subnet,
            http_client,
        })
    }

    /// Evaluate a pattern record using the configured HTTP `/evaluate` endpoint
    /// if reachable, falling back deterministically to `score_pattern_locally`.
    pub async fn evaluate_pattern_record(
        &self,
        name: &str,
        description: &str,
        category: &str,
    ) -> EvaluationResult {
        if let Some(ref base_url) = self.api_url {
            let endpoint = format!("{}/evaluate", base_url);
            let payload = serde_json::json!({
                "name": name,
                "description": description,
                "category": category,
                "network": self.network,
                "subnet": self.subnet,
            });

            match self.http_client.post(&endpoint).json(&payload).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(parsed) = resp.json::<EvaluationResult>().await {
                        return EvaluationResult {
                            score: sanitize_unit_float(parsed.score, 0.5),
                            confidence: sanitize_unit_float(parsed.confidence, 0.5),
                            evaluator_count: parsed.evaluator_count,
                        };
                    }
                    debug!("Bittensor /evaluate returned unparseable JSON; using local fallback");
                }
                Ok(resp) => {
                    debug!(
                        "Bittensor /evaluate returned HTTP {}; using local fallback",
                        resp.status()
                    );
                }
                Err(err) => {
                    debug!(
                        "Bittensor /evaluate unreachable ({}); using local fallback",
                        err
                    );
                }
            }
        }

        score_pattern_locally(name, description, category)
    }
}

/// Deterministically compute a local quality score in `[0.0, 1.0]` with zero panics.
pub fn score_pattern_locally(name: &str, description: &str, category: &str) -> EvaluationResult {
    let name_trimmed = name.trim();
    let desc_trimmed = description.trim();
    let cat_trimmed = category.trim();

    if name_trimmed.is_empty() && desc_trimmed.is_empty() {
        return EvaluationResult {
            score: 0.05,
            confidence: 0.5,
            evaluator_count: 1,
        };
    }

    let name_words = name_trimmed.split_whitespace().count();
    let desc_words = desc_trimmed.split_whitespace().count();
    let desc_chars = desc_trimmed.chars().count();

    let name_component = (name_words.min(6) as f64 / 6.0) * 0.25;
    let desc_word_component = (desc_words.min(20) as f64 / 20.0) * 0.45;
    let desc_len_component = (desc_chars.min(160) as f64 / 160.0) * 0.20;
    let cat_component = if cat_trimmed.is_empty() { 0.0 } else { 0.10 };

    let score =
        (name_component + desc_word_component + desc_len_component + cat_component).clamp(0.0, 1.0);

    EvaluationResult {
        score,
        confidence: 0.6,
        evaluator_count: 1,
    }
}

fn sanitize_unit_float(val: f64, fallback: f64) -> f64 {
    if val.is_finite() {
        val.clamp(0.0, 1.0)
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn local_scoring_is_deterministic_and_bounded() {
        let a = score_pattern_locally(
            "Rust Error Handling",
            "Use anyhow::Result and ? propagation instead of unwrap in production code",
            "coding",
        );
        let b = score_pattern_locally(
            "Rust Error Handling",
            "Use anyhow::Result and ? propagation instead of unwrap in production code",
            "coding",
        );
        assert_eq!(a, b);
        assert!((0.0..=1.0).contains(&a.score));
        assert!(a.score > 0.5);

        let empty = score_pattern_locally("", "", "");
        assert!((0.0..=1.0).contains(&empty.score));
        assert!(empty.score < a.score);
    }

    #[tokio::test]
    async fn bittensor_client_uses_http_endpoint_and_falls_back_offline() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock bittensor");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let body = r#"{"score":0.91,"confidence":0.88,"evaluator_count":7}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });

        let client =
            BittensorClient::new(Some(format!("http://{}", addr)), "finney".to_string(), 32)
                .expect("client");
        let eval = client
            .evaluate_pattern_record("test", "a helpful pattern", "coding")
            .await;
        assert!((eval.score - 0.91).abs() < 1e-6);
        assert!((eval.confidence - 0.88).abs() < 1e-6);
        assert_eq!(eval.evaluator_count, 7);

        let offline = BittensorClient::new(None, "finney".to_string(), 0).expect("offline");
        let local_eval = offline
            .evaluate_pattern_record("test", "a helpful pattern", "coding")
            .await;
        assert_eq!(
            local_eval,
            score_pattern_locally("test", "a helpful pattern", "coding")
        );
    }
}
