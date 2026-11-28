//! Cloud spend tracking for the auto-router's budget enforcement.
//!
//! Records token usage per UTC day, estimates cost from a per-model pricing
//! table, and persists to disk so budgets survive restarts. Shared via
//! `Arc<SpendTracker>`; the router reads `spent_today_usd()` before deciding
//! whether cloud is allowed.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Per-model pricing in USD per 1M tokens.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

impl ModelPricing {
    fn cost_usd(&self, prompt_tokens: u64, completion_tokens: u64) -> f64 {
        (prompt_tokens as f64 / 1_000_000.0) * self.input_per_million
            + (completion_tokens as f64 / 1_000_000.0) * self.output_per_million
    }
}

/// Look up pricing for a cloud model id (across OpenRouter, Venice, Anthropic,
/// OpenAI, Google, DeepSeek, xAI, MiniMax, Kimi, Bedrock, Llama, etc.). Falls
/// back to a conservative mid-tier estimate for unknown models so budgets still bind.
pub fn pricing_for(model: &str) -> ModelPricing {
    // Prices are approximate list prices (USD / 1M tokens).
    // Update periodically; the fallback keeps unknown models bounded.
    let m = model.to_ascii_lowercase();
    let (input, output) = match m.as_str() {
        // Anthropic Claude
        s if s.contains("claude-opus-4") || s.contains("claude-opus") => (15.0, 75.0),
        s if s.contains("claude-3.5-sonnet")
            || s.contains("claude-3-5-sonnet")
            || s.contains("claude-sonnet-4") =>
        {
            (3.0, 15.0)
        }
        s if s.contains("claude-3.5-haiku")
            || s.contains("claude-3-5-haiku")
            || s.contains("claude-haiku") =>
        {
            (0.8, 4.0)
        }
        // OpenAI
        s if s.contains("gpt-4o-mini") => (0.15, 0.60),
        s if s.contains("gpt-4o") => (2.50, 10.0),
        s if s.contains("gpt-4.1-nano") => (0.10, 0.40),
        s if s.contains("gpt-4.1-mini") => (0.40, 1.60),
        s if s.contains("gpt-4.1") => (2.00, 8.00),
        s if s.contains("gpt-4-turbo") => (10.0, 30.0),
        s if s.contains("o3-mini") || s.contains("o4-mini") => (1.10, 4.40),
        s if s.contains("gpt-5") => (1.25, 10.0),
        s if s.contains("gpt-oss-120b") => (0.15, 0.60),
        // Google Gemini
        s if s.contains("gemini-2.5-pro") || s.contains("gemini-1.5-pro") => (1.25, 10.0),
        s if s.contains("gemini-2.5-flash") || s.contains("gemini-2.0-flash") => (0.15, 0.60),
        s if s.contains("gemini") => (0.30, 1.20),
        // xAI Grok
        s if s.contains("grok-4") || s.contains("grok-3-mini") => (0.30, 0.50),
        s if s.contains("grok-3") || s.contains("grok") => (3.00, 15.0),
        // DeepSeek
        s if s.contains("deepseek-r1") || s.contains("deepseek-reasoner") => (0.55, 2.19),
        s if s.contains("deepseek") => (0.14, 0.28),
        // MiniMax & Kimi / Moonshot
        s if s.contains("minimax") => (0.30, 1.20),
        s if s.contains("kimi") || s.contains("moonshot") => (0.60, 2.50),
        // Amazon Bedrock Nova
        s if s.contains("nova-pro") => (0.80, 3.20),
        s if s.contains("nova-lite") => (0.06, 0.24),
        s if s.contains("nova-micro") => (0.035, 0.14),
        // Venice / Llama / Qwen / Mistral
        s if s.contains("venice-uncensored") => (0.20, 0.90),
        s if s.contains("llama-3.1-405b") || s.contains("llama-4") => (2.70, 2.70),
        s if s.contains("llama-3.3-70b") || s.contains("llama-3.1-70b") => (0.35, 0.40),
        s if s.contains("llama") => (0.20, 0.40),
        s if s.contains("qwen") => (0.30, 0.60),
        s if s.contains("mistral-large") => (2.0, 6.0),
        // Conservative default: assume a mid-tier paid model.
        _ => (1.0, 4.0),
    };
    ModelPricing {
        input_per_million: input,
        output_per_million: output,
    }
}

/// A single day's accumulated spend.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DaySpend {
    /// Total estimated cost in USD.
    cost_usd: f64,
    /// Token totals (for observability / debugging).
    prompt_tokens: u64,
    completion_tokens: u64,
    /// Number of cloud calls.
    calls: u64,
}

/// On-disk record. Keyed by ISO date (YYYY-MM-DD, UTC).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SpendRecord {
    days: HashMap<String, DaySpend>,
}

/// Thread-safe, persistent cloud spend tracker.
#[derive(Clone)]
pub struct SpendTracker {
    state: Arc<RwLock<SpendRecord>>,
    /// `None` = in-memory only (tests); persistence is skipped.
    path: Option<PathBuf>,
}

impl SpendTracker {
    /// Load (or create) the tracker at `path` (a JSON file).
    pub async fn load(path: PathBuf) -> Self {
        let record = match tokio::fs::read_to_string(&path).await {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                warn!("Corrupt spend record at {:?}, starting fresh: {}", path, e);
                SpendRecord::default()
            }),
            Err(_) => SpendRecord::default(),
        };
        Self {
            state: Arc::new(RwLock::new(record)),
            path: Some(path),
        }
    }

    /// An in-memory tracker (no persistence) — used in tests.
    #[cfg(test)]
    fn in_memory() -> Self {
        Self {
            state: Arc::new(RwLock::new(SpendRecord::default())),
            path: None,
        }
    }

    fn today_key() -> String {
        chrono::Utc::now().format("%Y-%m-%d").to_string()
    }

    /// Record a completed cloud call and persist.
    pub async fn record(
        &self,
        model: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) -> Result<()> {
        let cost = pricing_for(model).cost_usd(prompt_tokens, completion_tokens);
        let key = Self::today_key();
        {
            let mut state = self.state.write().await;
            let day = state.days.entry(key).or_default();
            day.cost_usd += cost;
            day.prompt_tokens += prompt_tokens;
            day.completion_tokens += completion_tokens;
            day.calls += 1;
        }
        debug!(
            "Cloud call: {} ({}+{} tokens) = ${:.6}",
            model, prompt_tokens, completion_tokens, cost
        );
        self.persist().await
    }

    /// Estimated USD spent today (UTC).
    pub async fn spent_today_usd(&self) -> f64 {
        let state = self.state.read().await;
        state
            .days
            .get(&Self::today_key())
            .map(|d| d.cost_usd)
            .unwrap_or(0.0)
    }

    /// Human-readable summary of today's usage (for status/observability).
    pub async fn today_summary(&self) -> String {
        let state = self.state.read().await;
        match state.days.get(&Self::today_key()) {
            Some(d) => format!(
                "${:.4} ({} calls, {} prompt + {} completion tokens)",
                d.cost_usd, d.calls, d.prompt_tokens, d.completion_tokens
            ),
            None => "$0.0000 (0 calls)".to_string(),
        }
    }

    async fn persist(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(()); // in-memory only
        };
        let state = self.state.read().await;
        let json = serde_json::to_string_pretty(&*state)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        // Write to temp then rename for atomicity.
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, json).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_record_and_total() {
        let t = SpendTracker::in_memory();
        t.record("anthropic/claude-3.5-sonnet", 1_000_000, 100_000)
            .await
            .unwrap();
        // 1M input @ $3 + 0.1M output @ $15 = $3 + $1.5 = $4.5
        let spent = t.spent_today_usd().await;
        assert!((spent - 4.5).abs() < 0.001, "spent was {}", spent);
    }

    #[tokio::test]
    async fn test_unknown_model_uses_fallback() {
        let t = SpendTracker::in_memory();
        t.record("some/new-model", 1_000_000, 1_000_000)
            .await
            .unwrap();
        // fallback: $1 + $4 = $5
        let spent = t.spent_today_usd().await;
        assert!((spent - 5.0).abs() < 0.001, "spent was {}", spent);
    }

    #[tokio::test]
    async fn test_summary() {
        let t = SpendTracker::in_memory();
        assert_eq!(t.today_summary().await, "$0.0000 (0 calls)");
        t.record("gpt-4o-mini", 1_000_000, 1_000_000).await.unwrap();
        let s = t.today_summary().await;
        assert!(s.contains("1 calls"), "summary: {}", s);
    }

    #[test]
    fn test_pricing_table() {
        let p = pricing_for("anthropic/claude-3.5-sonnet");
        assert_eq!(p.input_per_million, 3.0);
        let cheap = pricing_for("gpt-4o-mini");
        assert!(cheap.input_per_million < 1.0);
    }

    #[test]
    fn test_pricing_new_model_families() {
        let gemini = pricing_for("gemini-2.5-flash");
        assert_eq!(gemini.input_per_million, 0.15);
        let gemini_pro = pricing_for("gemini-2.5-pro");
        assert_eq!(gemini_pro.input_per_million, 1.25);

        let grok = pricing_for("grok-3");
        assert_eq!(grok.input_per_million, 3.0);

        let deepseek_r1 = pricing_for("deepseek-r1");
        assert_eq!(deepseek_r1.input_per_million, 0.55);

        let minimax = pricing_for("MiniMax-M2.5");
        assert_eq!(minimax.input_per_million, 0.30);

        let kimi = pricing_for("kimi-k2.5");
        assert_eq!(kimi.input_per_million, 0.60);

        let nova = pricing_for("amazon.nova-pro-v1:0");
        assert_eq!(nova.input_per_million, 0.80);

        let venice = pricing_for("venice-uncensored");
        assert_eq!(venice.input_per_million, 0.20);

        let llama33 = pricing_for("llama-3.3-70b");
        assert_eq!(llama33.input_per_million, 0.35);
    }
}
