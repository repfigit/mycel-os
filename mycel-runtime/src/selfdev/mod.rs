//! Self-development: the OS files GitHub issues about its own limitations.
//!
//! When the runtime hits something it can't do — no matching tool, repeated
//! low-confidence answers, a command it couldn't run — it records a structured
//! "limitation." Deduped, PII-sanitized, rate-capped, and (if enabled) filed
//! to the GitHub repo via the `gh` CLI so developers and agents can fix them.
//!
//! Safety model (this feature is a trust problem more than a tech problem):
//!   - OPT-IN, off by default (`[selfdev] enabled = false`)
//!   - Local dedup: a limitation signature is filed at most once per window
//!   - Rate cap: max N issues per week per instance
//!   - Sanitize: the router's `is_sensitive` scanner gates every field; any
//!     sensitive content redacts the whole issue to a generic signature
//!   - Rigid template: issue bodies are built from fixed fields, never raw
//!     free-form LLM output pasted into the repo
//!   - No token in the runtime: shells out to the user's `gh` CLI, which is
//!     already authenticated and scoped. Read-only on the repo (issues:write).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::ai::router::is_sensitive;

/// Configuration for the self-development loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfDevConfig {
    /// Master switch. OFF by default — this is a deliberate user choice.
    #[serde(default)]
    pub enabled: bool,
    /// GitHub repo in "owner/name" form.
    #[serde(default = "default_repo")]
    pub repo: String,
    /// Max issues to file per rolling 7-day window.
    #[serde(default = "default_max_per_week")]
    pub max_issues_per_week: u32,
    /// Don't re-file the same limitation signature within this many days.
    #[serde(default = "default_dedup_days")]
    pub dedup_window_days: u32,
    /// Actually file to GitHub (vs. just logging locally). Requires `gh`.
    #[serde(default)]
    pub file_to_github: bool,
}

fn default_repo() -> String {
    "repfigit/mycel-os".to_string()
}
fn default_max_per_week() -> u32 {
    5
}
fn default_dedup_days() -> u32 {
    7
}

impl Default for SelfDevConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            repo: default_repo(),
            max_issues_per_week: default_max_per_week(),
            dedup_window_days: default_dedup_days(),
            file_to_github: false,
        }
    }
}

/// The kind of limitation detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LimitationKind {
    /// No MCP tool matched the user's request.
    MissingTool,
    /// A command the user needs isn't installed.
    MissingCommand,
    /// The local model repeatedly produced inadequate answers.
    LowConfidence,
    /// The executor failed to run generated code.
    ExecutionFailure,
    /// Catch-all.
    Other,
}

impl LimitationKind {
    fn label(&self) -> &'static str {
        match self {
            LimitationKind::MissingTool => "capability-gap",
            LimitationKind::MissingCommand => "missing-dependency",
            LimitationKind::LowConfidence => "model-quality",
            LimitationKind::ExecutionFailure => "execution-failure",
            LimitationKind::Other => "auto-filed",
        }
    }
}

/// A detected limitation, before sanitization.
#[derive(Debug, Clone)]
pub struct Limitation {
    pub kind: LimitationKind,
    /// Short description of what the user wanted (will be PII-scanned).
    pub summary: String,
    /// What the system tried / why it couldn't comply.
    pub detail: String,
    /// Optional suggested fix (system-generated, not raw LLM text).
    pub suggestion: Option<String>,
}

impl Limitation {
    /// A stable signature for dedup: kind + normalized summary.
    fn signature(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.kind.hash(&mut h);
        // Normalize: lowercase, collapse whitespace, first 80 chars.
        let norm: String = self
            .summary
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect();
        norm.hash(&mut h);
        format!("{:x}", h.finish())
    }
}

/// Persisted state: filed signatures and weekly count.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SelfDevState {
    /// signature -> epoch seconds when last filed.
    filed: HashMap<String, i64>,
    /// epoch seconds of issues filed this window (for rate cap).
    recent: Vec<i64>,
}

/// The self-development service.
#[derive(Clone)]
pub struct SelfDevService {
    config: SelfDevConfig,
    state: Arc<RwLock<SelfDevState>>,
    path: PathBuf,
}

impl SelfDevService {
    pub async fn new(config: SelfDevConfig, state_path: PathBuf) -> Self {
        let state = match tokio::fs::read_to_string(&state_path).await {
            Ok(c) => serde_json::from_str(&c).unwrap_or_default(),
            Err(_) => SelfDevState::default(),
        };
        Self {
            config,
            state: Arc::new(RwLock::new(state)),
            path: state_path,
        }
    }

    #[cfg(test)]
    fn in_memory(config: SelfDevConfig) -> Self {
        Self {
            config,
            state: Arc::new(RwLock::new(SelfDevState::default())),
            path: PathBuf::from("/nonexistent/selfdev.json"),
        }
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// Report a limitation. Dedups, rate-caps, sanitizes, and files if enabled.
    /// Returns true if an issue was (or would be) filed.
    pub async fn report(&self, lim: Limitation) -> Result<bool> {
        if !self.config.enabled {
            return Ok(false);
        }

        let sig = lim.signature();
        let now = Self::now();
        let dedup_window = (self.config.dedup_window_days as i64) * 86_400;
        let week = 7 * 86_400;

        {
            let mut state = self.state.write().await;

            // Dedup: same signature within window?
            if let Some(&last) = state.filed.get(&sig) {
                if now - last < dedup_window {
                    debug!("SelfDev: dedup hit for signature {}", sig);
                    return Ok(false);
                }
            }

            // Rate cap: prune old, then count.
            state.recent.retain(|&t| now - t < week);
            if state.recent.len() >= self.config.max_issues_per_week as usize {
                warn!("SelfDev: weekly issue cap reached, dropping report");
                return Ok(false);
            }

            // Record.
            state.filed.insert(sig.clone(), now);
            state.recent.push(now);
        }
        self.persist().await.ok();

        // Sanitize + build the issue.
        let (title, body) = self.render_issue(&lim);

        if self.config.file_to_github {
            match self.file_issue(&title, &body, lim.kind.label()).await {
                Ok(url) => {
                    info!("SelfDev: filed issue to {}: {}", self.config.repo, url);
                    return Ok(true);
                }
                Err(e) => {
                    warn!("SelfDev: failed to file issue (logged locally): {}", e);
                    self.log_local(&title, &body).await;
                    return Ok(false);
                }
            }
        }

        // Local-only mode: just log it.
        self.log_local(&title, &body).await;
        Ok(true)
    }

    /// Build a rigid, sanitized issue. If any field is sensitive, redact the
    /// whole thing to a generic signature-based report (never leak PII).
    fn render_issue(&self, lim: &Limitation) -> (String, String) {
        let sensitive = is_sensitive(&lim.summary)
            || is_sensitive(&lim.detail)
            || lim.suggestion.as_deref().map(is_sensitive).unwrap_or(false);

        let (summary, detail, suggestion) = if sensitive {
            (
                "[redacted: sensitive content]".to_string(),
                "Details withheld to avoid exposing private data.".to_string(),
                None,
            )
        } else {
            (
                lim.summary.clone(),
                lim.detail.clone(),
                lim.suggestion.clone(),
            )
        };

        let kind_name = format!("{:?}", lim.kind);
        let title = format!("[auto] {}: {}", kind_name, truncate(&summary, 60));

        let mut body = format!(
            "## What the user needed\n{}\n\n## What happened\n{}\n",
            summary, detail
        );
        if let Some(s) = suggestion {
            body.push_str(&format!(
                "\n## Suggested fix (auto-generated, unreviewed)\n{}\n",
                s
            ));
        }
        body.push_str(&format!(
            "\n---\n*Auto-filed by mycel-runtime {} ({}). Labels: auto-filed, needs-triage, {}*\n",
            env!("CARGO_PKG_VERSION"),
            self.config.repo,
            lim.kind.label()
        ));

        (title, body)
    }

    /// File via the `gh` CLI (already authenticated, user-scoped token).
    async fn file_issue(&self, title: &str, body: &str, label: &str) -> Result<String> {
        let output = tokio::process::Command::new("gh")
            .args([
                "issue",
                "create",
                "--repo",
                &self.config.repo,
                "--title",
                title,
                "--body",
                body,
                "--label",
                "auto-filed",
                "--label",
                "needs-triage",
                "--label",
                label,
            ])
            .output()
            .await
            .map_err(|e| anyhow!("failed to spawn gh: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("gh issue create failed: {}", stderr.trim()));
        }
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(url)
    }

    /// Local fallback: append to a log file for later review.
    async fn log_local(&self, title: &str, body: &str) {
        info!("SelfDev (local): {}\n{}", title, body);
    }

    async fn persist(&self) -> Result<()> {
        if self.path.as_os_str() == "/nonexistent/selfdev.json" {
            return Ok(());
        }
        let state = self.state.read().await;
        let json = serde_json::to_string_pretty(&*state)?;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let tmp = self.path.with_extension("tmp");
        tokio::fs::write(&tmp, json).await?;
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_config() -> SelfDevConfig {
        SelfDevConfig {
            enabled: true,
            file_to_github: false, // never hit network in tests
            ..Default::default()
        }
    }

    fn lim(summary: &str) -> Limitation {
        Limitation {
            kind: LimitationKind::MissingTool,
            summary: summary.to_string(),
            detail: "no tool matched".to_string(),
            suggestion: Some("add a tool".to_string()),
        }
    }

    #[tokio::test]
    async fn test_disabled_by_default() {
        let svc = SelfDevService::in_memory(SelfDevConfig::default());
        assert!(!svc.report(lim("connect bluetooth")).await.unwrap());
    }

    #[tokio::test]
    async fn test_dedup() {
        let svc = SelfDevService::in_memory(enabled_config());
        assert!(svc.report(lim("connect bluetooth")).await.unwrap());
        // Same signature again -> deduped
        assert!(!svc.report(lim("connect bluetooth")).await.unwrap());
    }

    #[tokio::test]
    async fn test_rate_cap() {
        let mut cfg = enabled_config();
        cfg.max_issues_per_week = 2;
        let svc = SelfDevService::in_memory(cfg);
        assert!(svc.report(lim("issue one")).await.unwrap());
        assert!(svc.report(lim("issue two")).await.unwrap());
        assert!(!svc.report(lim("issue three")).await.unwrap());
    }

    #[tokio::test]
    async fn test_sensitive_redacted() {
        let svc = SelfDevService::in_memory(enabled_config());
        let l = Limitation {
            kind: LimitationKind::Other,
            summary: "my password is hunter2".to_string(),
            detail: "x".to_string(),
            suggestion: None,
        };
        let (title, body) = svc.render_issue(&l);
        assert!(body.contains("redacted"), "body: {}", body);
        assert!(!body.contains("hunter2"), "leaked PII: {}", body);
        assert!(!title.contains("hunter2"));
    }

    #[tokio::test]
    async fn test_render_structure() {
        let svc = SelfDevService::in_memory(enabled_config());
        let (title, body) = svc.render_issue(&lim("connect bluetooth"));
        assert!(title.starts_with("[auto] MissingTool:"));
        assert!(body.contains("What the user needed"));
        assert!(body.contains("needs-triage"));
    }
}
