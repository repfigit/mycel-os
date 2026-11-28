//! Execution recovery - deterministic failure classification and verified repair
//!
//! When generated code fails, the recovery layer decides what happens next.
//! The decision itself is pure Rust, never delegated to an LLM: the model may
//! *propose* a fix, but whether a fix is even attempted, and whether it is
//! accepted, is enforced here by hard safety preconditions.
//!
//! The shape is deliberately conservative:
//!
//! - Auto-repair is opt-in (`RecoveryConfig::auto_repair`, default `false`).
//! - Only a plain non-zero exit inside a fully isolated bubblewrap sandbox
//!   (no network, no writable paths, untruncated output, attempts remaining)
//!   is even eligible for a repair attempt.
//! - Everything else - timeouts, signals, a missing sandbox, truncated output,
//!   host mode, network or filesystem side effects - aborts for human
//!   inspection rather than guessing.
//! - A proposed candidate is checked statically, gated by the policy evaluator
//!   and the sandbox invariants, then executed in a *fresh* sandbox and
//!   optionally re-run to confirm it is deterministic before it is accepted.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::SandboxMode;
use crate::executor::{CodeExecutor, ExecutionReport};
use crate::policy::{ActionPolicy, PolicyEvaluator};

/// Recovery policy. Secure by default: recovery may classify a failure, but it
/// will not attempt a repair unless `auto_repair` is explicitly enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecoveryConfig {
    /// Enable the recovery layer's failure classification.
    pub enabled: bool,
    /// Allow an opt-in attempt to repair eligible failures (default: off).
    pub auto_repair: bool,
    /// Maximum number of repair attempts before aborting for inspection.
    pub max_repair_attempts: u32,
    /// Largest candidate program accepted, in bytes.
    pub max_candidate_bytes: usize,
    /// Re-run an accepted candidate and require identical stdout.
    pub require_repeat_check: bool,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_repair: false,
            max_repair_attempts: 1,
            max_candidate_bytes: 32_768,
            require_repeat_check: false,
        }
    }
}

/// The determined cause of a failed execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureClass {
    MissingCommand,
    Timeout,
    SandboxUnavailable,
    Signaled,
    ExitNonZero,
    Unknown,
}

impl FailureClass {
    /// Stable lowercase name used in events and diagnostics.
    pub fn as_str(&self) -> &'static str {
        match self {
            FailureClass::MissingCommand => "missing_command",
            FailureClass::Timeout => "timeout",
            FailureClass::SandboxUnavailable => "sandbox_unavailable",
            FailureClass::Signaled => "signaled",
            FailureClass::ExitNonZero => "exit_non_zero",
            FailureClass::Unknown => "unknown",
        }
    }
}

/// What the recovery layer decided to do about a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryDecision {
    /// Surface the failure to the user; no repair is appropriate.
    ReturnFailure { class: FailureClass, reason: String },
    /// An isolated, deterministic failure may be repaired (opt-in only).
    ProposeRepair { class: FailureClass, reason: String },
    /// Stop and surface the failure for human inspection; never auto-repair.
    AbortForInspection { class: FailureClass, reason: String },
}

impl RecoveryDecision {
    /// Stable lowercase name used in events and diagnostics.
    pub fn kind_str(&self) -> &'static str {
        match self {
            RecoveryDecision::ReturnFailure { .. } => "return_failure",
            RecoveryDecision::ProposeRepair { .. } => "propose_repair",
            RecoveryDecision::AbortForInspection { .. } => "abort_for_inspection",
        }
    }

    /// The human-readable reason attached to this decision.
    pub fn reason(&self) -> &str {
        match self {
            RecoveryDecision::ReturnFailure { reason, .. }
            | RecoveryDecision::ProposeRepair { reason, .. }
            | RecoveryDecision::AbortForInspection { reason, .. } => reason,
        }
    }
}

/// Classify an execution report into a [`FailureClass`].
pub fn classify_failure(report: &ExecutionReport) -> FailureClass {
    if report.sandbox_unavailable {
        return FailureClass::SandboxUnavailable;
    }
    if report.timed_out {
        return FailureClass::Timeout;
    }
    if report.signal.is_some() {
        return FailureClass::Signaled;
    }
    if report.exit_code == Some(127) || report.stderr.to_lowercase().contains("command not found") {
        return FailureClass::MissingCommand;
    }
    if report.exit_code.is_some_and(|c| c != 0) {
        return FailureClass::ExitNonZero;
    }
    FailureClass::Unknown
}

/// Decide how to respond to a failed execution. Deterministic pure Rust; never
/// consults a model. `attempt` is the number of repair attempts already made.
pub fn decide_recovery(
    report: &ExecutionReport,
    config: &RecoveryConfig,
    attempt: u32,
) -> RecoveryDecision {
    let class = classify_failure(report);
    match class {
        // Timeouts, signals, an absent sandbox and unclassified failures always
        // stop for inspection, regardless of configuration.
        FailureClass::Timeout => RecoveryDecision::AbortForInspection {
            class,
            reason: "timeout requires human inspection".to_string(),
        },
        FailureClass::SandboxUnavailable => RecoveryDecision::AbortForInspection {
            class,
            reason: "sandbox unavailable: refusing to run code on the host".to_string(),
        },
        FailureClass::Signaled => RecoveryDecision::AbortForInspection {
            class,
            reason: "process was signaled; state is unknown".to_string(),
        },
        FailureClass::Unknown => RecoveryDecision::AbortForInspection {
            class,
            reason: "unclassified failure requires inspection".to_string(),
        },
        FailureClass::MissingCommand => RecoveryDecision::ReturnFailure {
            class,
            reason: "a missing command is fixed by installation, not repair".to_string(),
        },
        FailureClass::ExitNonZero => {
            if !config.enabled || !config.auto_repair {
                return RecoveryDecision::ReturnFailure {
                    class,
                    reason: "auto-repair is disabled".to_string(),
                };
            }
            if attempt >= config.max_repair_attempts {
                return RecoveryDecision::AbortForInspection {
                    class,
                    reason: "repair attempts exhausted".to_string(),
                };
            }
            if report.sandbox_mode != SandboxMode::Bwrap {
                return RecoveryDecision::AbortForInspection {
                    class,
                    reason: "host-mode runs are never auto-repaired".to_string(),
                };
            }
            if report.allow_network {
                return RecoveryDecision::AbortForInspection {
                    class,
                    reason: "network access may have external side effects".to_string(),
                };
            }
            if report.has_writable_paths {
                return RecoveryDecision::AbortForInspection {
                    class,
                    reason: "writable paths may have filesystem side effects".to_string(),
                };
            }
            if report.stdout_truncated || report.stderr_truncated {
                return RecoveryDecision::AbortForInspection {
                    class,
                    reason: "truncated output: observability is incomplete".to_string(),
                };
            }
            RecoveryDecision::ProposeRepair {
                class,
                reason: "deterministic exit failure in an isolated sandbox".to_string(),
            }
        }
    }
}

/// Extract a candidate repair program from a model response.
///
/// Rejects responses that leak reasoning tags (`<think>`, `</think>`, or
/// `<thinking`, case-insensitive), then returns the contents of a fenced code
/// block if present, otherwise the text after a `REPAIR_CODE:` header, trimmed.
/// Only skips the opening fence line when it contains a bare language tag (not
/// executable code). Returns `None` when nothing usable remains.
pub fn extract_repair_candidate(raw: &str) -> Option<String> {
    let lower = raw.to_lowercase();
    if lower.contains("<think>") || lower.contains("</think>") || lower.contains("<thinking") {
        return None;
    }

    if let Some(start) = raw.find("```") {
        let after = &raw[start + 3..];
        let body = if let Some(nl) = after.find('\n') {
            let tag = after[..nl].trim();
            let is_lang_tag = tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.' | '#'));
            if is_lang_tag {
                &after[nl + 1..]
            } else {
                after
            }
        } else {
            after
        };
        if let Some(end) = body.find("```") {
            let code = body[..end].trim();
            if !code.is_empty() {
                return Some(code.to_string());
            }
        }
    }

    if let Some(idx) = raw.find("REPAIR_CODE:") {
        let code = raw[idx + "REPAIR_CODE:".len()..].trim();
        if !code.is_empty() {
            return Some(code.to_string());
        }
    }

    None
}

/// Why a repair candidate was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairRejectReason {
    /// The candidate was empty or only whitespace.
    EmptyCandidate,
    /// The candidate exceeded `RecoveryConfig::max_candidate_bytes`.
    TooLarge,
    /// The candidate contained a NUL byte.
    ContainsNullByte,
    /// The candidate was identical to the original code.
    UnchangedCode,
    /// The executor is not a network-isolated bubblewrap sandbox.
    UnsafeSandboxState,
    /// The policy evaluator denied the candidate.
    PolicyDenied(String),
    /// The policy evaluator requires user confirmation for the candidate.
    PolicyRequiresConfirmation(String),
    /// The candidate ran but did not succeed.
    ExecutionFailed(FailureClass),
    /// The candidate succeeded but a postcondition was not met.
    PostconditionFailed(String),
    /// The candidate produced different output when re-run.
    NonDeterministicRepeat,
}

/// What an accepted repair must satisfy after it runs.
#[derive(Debug, Clone, Default)]
pub struct Postcondition {
    /// Require non-empty combined output.
    pub must_not_be_empty: bool,
    /// Require the combined output to contain this substring.
    pub must_contain: Option<String>,
    /// Re-run the candidate and require identical stdout.
    pub require_repeat_match: bool,
}

/// The outcome of verifying (and possibly running) a repair candidate.
#[derive(Debug, Clone)]
pub struct RepairVerification {
    pub accepted: bool,
    /// The deepest tier reached: 1 = static, 2 = safety/policy, 3 = execution.
    pub tier_reached: u8,
    pub reject_reason: Option<RepairRejectReason>,
    /// True when a rejected candidate was dangerous (policy/state/run hazard).
    pub dangerous_attempt: bool,
    /// The successful run's report, when the candidate was executed.
    pub report: Option<ExecutionReport>,
}

impl RepairVerification {
    fn rejected(tier: u8, reason: RepairRejectReason, dangerous: bool) -> Self {
        Self {
            accepted: false,
            tier_reached: tier,
            reject_reason: Some(reason),
            dangerous_attempt: dangerous,
            report: None,
        }
    }
}

/// Verify a proposed repair and, if it passes every gate, run it in a fresh
/// sandbox. Three tiers:
///
/// 1. Static checks on the candidate itself.
/// 2. Deterministic sandbox invariants plus the policy evaluator's verdict.
/// 3. A fresh sandbox execution, followed by postcondition and (optional)
///    determinism checks.
///
/// A candidate is only ever executed after tier 2 passes; a policy rejection or
/// an unsafe sandbox state returns without running anything.
pub async fn verify_and_run_repair(
    original_code: &str,
    candidate_code: &str,
    executor: &CodeExecutor,
    policy: &PolicyEvaluator,
    config: &RecoveryConfig,
    postcondition: &Postcondition,
) -> Result<RepairVerification> {
    // Tier 1: static candidate checks.
    if candidate_code.trim().is_empty() {
        return Ok(RepairVerification::rejected(
            1,
            RepairRejectReason::EmptyCandidate,
            false,
        ));
    }
    if candidate_code.len() > config.max_candidate_bytes {
        return Ok(RepairVerification::rejected(
            1,
            RepairRejectReason::TooLarge,
            false,
        ));
    }
    if candidate_code.contains('\0') {
        return Ok(RepairVerification::rejected(
            1,
            RepairRejectReason::ContainsNullByte,
            false,
        ));
    }
    if candidate_code.trim() == original_code.trim() {
        return Ok(RepairVerification::rejected(
            1,
            RepairRejectReason::UnchangedCode,
            false,
        ));
    }

    // Tier 2: deterministic safety and policy gate.
    let sandbox = executor.sandbox_config();
    if sandbox.mode != SandboxMode::Bwrap
        || sandbox.allow_network
        || !sandbox.writable_paths.is_empty()
    {
        return Ok(RepairVerification::rejected(
            2,
            RepairRejectReason::UnsafeSandboxState,
            true,
        ));
    }
    match policy.evaluate_code(candidate_code) {
        ActionPolicy::Allow => {}
        ActionPolicy::Deny { reason } => {
            return Ok(RepairVerification::rejected(
                2,
                RepairRejectReason::PolicyDenied(reason),
                true,
            ));
        }
        ActionPolicy::RequiresConfirmation { message, .. } => {
            return Ok(RepairVerification::rejected(
                2,
                RepairRejectReason::PolicyRequiresConfirmation(message),
                true,
            ));
        }
    }

    // Tier 3: fresh sandbox execution plus postconditions.
    let report = executor.run_detailed(candidate_code).await?;
    if !report.succeeded() {
        let class = classify_failure(&report);
        let dangerous = report.timed_out || report.signal.is_some();
        return Ok(RepairVerification {
            accepted: false,
            tier_reached: 3,
            reject_reason: Some(RepairRejectReason::ExecutionFailed(class)),
            dangerous_attempt: dangerous,
            report: Some(report),
        });
    }

    let combined = report.combined_output();
    if postcondition.must_not_be_empty && combined.trim().is_empty() {
        return Ok(RepairVerification {
            accepted: false,
            tier_reached: 3,
            reject_reason: Some(RepairRejectReason::PostconditionFailed(
                "output is empty".to_string(),
            )),
            dangerous_attempt: false,
            report: Some(report),
        });
    }
    if let Some(needle) = &postcondition.must_contain {
        if !combined.contains(needle) {
            return Ok(RepairVerification {
                accepted: false,
                tier_reached: 3,
                reject_reason: Some(RepairRejectReason::PostconditionFailed(format!(
                    "output does not contain {:?}",
                    needle
                ))),
                dangerous_attempt: false,
                report: Some(report),
            });
        }
    }

    if postcondition.require_repeat_match || config.require_repeat_check {
        let repeat = executor.run_detailed(candidate_code).await?;
        if !repeat.succeeded() || repeat.stdout != report.stdout {
            return Ok(RepairVerification {
                accepted: false,
                tier_reached: 3,
                reject_reason: Some(RepairRejectReason::NonDeterministicRepeat),
                dangerous_attempt: false,
                report: Some(report),
            });
        }
    }

    Ok(RepairVerification {
        accepted: true,
        tier_reached: 3,
        reject_reason: None,
        dangerous_attempt: false,
        report: Some(report),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MycelConfig, SandboxConfig};
    use crate::executor::Language;

    fn failure_report() -> ExecutionReport {
        ExecutionReport {
            language: Language::Shell,
            sandbox_mode: SandboxMode::Bwrap,
            allow_network: false,
            has_writable_paths: false,
            exit_code: Some(1),
            signal: None,
            timed_out: false,
            sandbox_unavailable: false,
            stdout: "out".to_string(),
            stderr: "err".to_string(),
            stdout_truncated: false,
            stderr_truncated: false,
            duration_ms: 3,
        }
    }

    fn report_with(
        sandbox_mode: SandboxMode,
        allow_network: bool,
        has_writable_paths: bool,
        truncated: bool,
    ) -> ExecutionReport {
        ExecutionReport {
            sandbox_mode,
            allow_network,
            has_writable_paths,
            stdout_truncated: truncated,
            stderr_truncated: false,
            ..failure_report()
        }
    }

    #[test]
    fn test_recovery_config_partial_deserialization_keeps_defaults() {
        // A partial `[recovery]` table (as in config.toml) must fall back to
        // the secure defaults for every omitted key.
        let cfg: RecoveryConfig = serde_json::from_str(r#"{"auto_repair": true}"#).unwrap();
        assert!(cfg.enabled);
        assert!(cfg.auto_repair);
        assert_eq!(cfg.max_repair_attempts, 1);
        assert_eq!(cfg.max_candidate_bytes, 32_768);
        assert!(!cfg.require_repeat_check);
    }

    #[test]
    fn test_classify_failure_classes() {
        assert_eq!(
            classify_failure(&ExecutionReport {
                sandbox_unavailable: true,
                ..failure_report()
            }),
            FailureClass::SandboxUnavailable
        );
        assert_eq!(
            classify_failure(&ExecutionReport {
                timed_out: true,
                exit_code: None,
                ..failure_report()
            }),
            FailureClass::Timeout
        );
        assert_eq!(
            classify_failure(&ExecutionReport {
                signal: Some(9),
                exit_code: None,
                ..failure_report()
            }),
            FailureClass::Signaled
        );
        assert_eq!(
            classify_failure(&ExecutionReport {
                exit_code: Some(127),
                ..failure_report()
            }),
            FailureClass::MissingCommand
        );
        assert_eq!(
            classify_failure(&ExecutionReport {
                exit_code: Some(1),
                stderr: "bash: foo: Command Not Found".to_string(),
                ..failure_report()
            }),
            FailureClass::MissingCommand
        );
        assert_eq!(
            classify_failure(&ExecutionReport {
                exit_code: Some(2),
                stderr: String::new(),
                ..failure_report()
            }),
            FailureClass::ExitNonZero
        );
        assert_eq!(
            classify_failure(&ExecutionReport {
                exit_code: None,
                stderr: String::new(),
                ..failure_report()
            }),
            FailureClass::Unknown
        );
    }

    #[test]
    fn test_decisions_for_hard_abort_classes() {
        let config = RecoveryConfig {
            auto_repair: true,
            ..Default::default()
        };
        for report in [
            ExecutionReport {
                sandbox_unavailable: true,
                ..failure_report()
            },
            ExecutionReport {
                timed_out: true,
                exit_code: None,
                ..failure_report()
            },
            ExecutionReport {
                signal: Some(9),
                exit_code: None,
                ..failure_report()
            },
            ExecutionReport {
                exit_code: None,
                ..failure_report()
            },
        ] {
            let decision = decide_recovery(&report, &config, 0);
            assert!(
                matches!(decision, RecoveryDecision::AbortForInspection { .. }),
                "expected abort, got {:?}",
                decision
            );
        }
    }

    #[test]
    fn test_missing_command_returns_failure() {
        let config = RecoveryConfig {
            auto_repair: true,
            ..Default::default()
        };
        let decision = decide_recovery(
            &ExecutionReport {
                exit_code: Some(127),
                ..failure_report()
            },
            &config,
            0,
        );
        assert!(matches!(
            decision,
            RecoveryDecision::ReturnFailure {
                class: FailureClass::MissingCommand,
                ..
            }
        ));
    }

    #[test]
    fn test_decision_matrix_never_proposes_repair_under_hazard() {
        let mut dangerous_retries = 0u32;
        let mut proposed = 0u32;
        for mode in [SandboxMode::Bwrap, SandboxMode::Host] {
            for allow_network in [false, true] {
                for writable in [false, true] {
                    for truncated in [false, true] {
                        for auto_repair in [false, true] {
                            for enabled in [true, false] {
                                for attempt in [0u32, 1, 2] {
                                    let config = RecoveryConfig {
                                        enabled,
                                        auto_repair,
                                        max_repair_attempts: 1,
                                        ..Default::default()
                                    };
                                    let report =
                                        report_with(mode, allow_network, writable, truncated);
                                    let decision = decide_recovery(&report, &config, attempt);
                                    let hazardous = mode != SandboxMode::Bwrap
                                        || allow_network
                                        || writable
                                        || truncated
                                        || attempt >= config.max_repair_attempts
                                        || !auto_repair
                                        || !enabled;
                                    match decision {
                                        RecoveryDecision::ProposeRepair { .. } => {
                                            if hazardous {
                                                dangerous_retries += 1;
                                            } else {
                                                proposed += 1;
                                            }
                                        }
                                        _ => assert!(
                                            hazardous,
                                            "safe configuration did not propose a repair"
                                        ),
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(dangerous_retries, 0, "proposed repair under a hazard");
        assert!(proposed > 0, "no safe configuration proposed a repair");
    }

    #[test]
    fn test_recovery_config_toml_deserialization_matches_config_file() {
        let toml_str = r#"
[recovery]
enabled = true
auto_repair = true
max_repair_attempts = 2
max_candidate_bytes = 16384
require_repeat_check = true
"#;
        let cfg: MycelConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.recovery.enabled);
        assert!(cfg.recovery.auto_repair);
        assert_eq!(cfg.recovery.max_repair_attempts, 2);
        assert_eq!(cfg.recovery.max_candidate_bytes, 16_384);
        assert!(cfg.recovery.require_repeat_check);
    }

    #[test]
    fn test_extract_repair_candidate_rejects_markers() {
        assert!(extract_repair_candidate("<think>plan</think>\n```\necho hi\n```").is_none());
        assert!(extract_repair_candidate("<THINK>leak\n```\necho hi\n```").is_none());
        assert!(extract_repair_candidate("done</think>\n```\necho hi\n```").is_none());
        assert!(extract_repair_candidate("<thinking>\n```\nrm -rf /\n```").is_none());
        // Legitimate SQL/PHP inequality operator `<>` must NOT be rejected.
        assert_eq!(
            extract_repair_candidate("```sql\nSELECT 1 WHERE a <> b;\n```").as_deref(),
            Some("SELECT 1 WHERE a <> b;")
        );
    }

    #[test]
    fn test_extract_repair_candidate_fenced_and_header() {
        assert_eq!(
            extract_repair_candidate("Sure:\n```sh\necho fixed\n```\nDone").as_deref(),
            Some("echo fixed")
        );
        assert_eq!(
            extract_repair_candidate("```\nprintf 'hi'\n```").as_deref(),
            Some("printf 'hi'")
        );
        // Unlabeled fence whose first line is actual code must NOT drop the first line.
        assert_eq!(
            extract_repair_candidate("```echo fixed\nprintf done\n```").as_deref(),
            Some("echo fixed\nprintf done")
        );
        assert_eq!(
            extract_repair_candidate("REPAIR_CODE:\nprintf fixed").as_deref(),
            Some("printf fixed")
        );
        assert!(extract_repair_candidate("REPAIR_CODE:   ").is_none());
        assert!(extract_repair_candidate("```\n\n```").is_none());
        assert!(extract_repair_candidate("no code here").is_none());
    }

    fn bwrap_present() -> bool {
        std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn executor_with(sandbox: SandboxConfig, timeout: u64) -> CodeExecutor {
        let base =
            std::env::temp_dir().join(format!("mycel-recovery-test-{}", uuid::Uuid::new_v4()));
        let config = MycelConfig {
            sandbox,
            code_path: base.join("code").to_string_lossy().to_string(),
            context_path: base.join("data").to_string_lossy().to_string(),
            execution_timeout_secs: timeout,
            ..Default::default()
        };
        CodeExecutor::new(&config).unwrap()
    }

    fn host_executor(timeout: u64) -> CodeExecutor {
        executor_with(
            SandboxConfig {
                mode: SandboxMode::Host,
                ..Default::default()
            },
            timeout,
        )
    }

    /// Sandboxed executor, or None when bwrap isn't installed (skips test).
    fn sandboxed_executor(timeout: u64) -> Option<CodeExecutor> {
        if bwrap_present() {
            Some(executor_with(SandboxConfig::default(), timeout))
        } else {
            eprintln!("bwrap not installed; skipping recovery sandbox test");
            None
        }
    }

    fn policy() -> PolicyEvaluator {
        PolicyEvaluator::with_defaults()
    }

    #[tokio::test]
    async fn test_tier1_static_rejections() {
        // Tier 1 rejects before any execution, so the sandbox mode is irrelevant.
        let exec = host_executor(5);
        let pol = policy();
        let cfg = RecoveryConfig::default();
        let pc = Postcondition::default();

        let too_large = "x".repeat(cfg.max_candidate_bytes + 1);
        let cases = [
            ("echo ok", String::new(), RepairRejectReason::EmptyCandidate),
            (
                "echo ok",
                "   \n ".to_string(),
                RepairRejectReason::EmptyCandidate,
            ),
            ("echo ok", too_large, RepairRejectReason::TooLarge),
            (
                "echo ok",
                "echo ok\0".to_string(),
                RepairRejectReason::ContainsNullByte,
            ),
            (
                "echo same",
                "echo same".to_string(),
                RepairRejectReason::UnchangedCode,
            ),
        ];

        for (original, candidate, expected) in cases {
            let verification = verify_and_run_repair(original, &candidate, &exec, &pol, &cfg, &pc)
                .await
                .unwrap();
            assert!(!verification.accepted);
            assert_eq!(verification.tier_reached, 1);
            assert_eq!(verification.reject_reason, Some(expected));
            assert!(!verification.dangerous_attempt);
            assert!(verification.report.is_none());
        }
    }

    #[tokio::test]
    async fn test_tier2_blocks_host_mode_without_executing() {
        let exec = host_executor(5);
        let pol = policy();
        let cfg = RecoveryConfig::default();
        let pc = Postcondition::default();

        let verification =
            verify_and_run_repair("echo broken\nexit 1", "echo fixed", &exec, &pol, &cfg, &pc)
                .await
                .unwrap();
        assert!(!verification.accepted);
        assert_eq!(verification.tier_reached, 2);
        assert_eq!(
            verification.reject_reason,
            Some(RepairRejectReason::UnsafeSandboxState)
        );
        assert!(verification.dangerous_attempt);
        assert!(
            verification.report.is_none(),
            "host-mode candidate was executed"
        );
    }

    #[tokio::test]
    async fn test_tier2_blocks_dangerous_candidates_without_executing() {
        // Tier 2 rejects before spawning bwrap, so this test works even without bwrap installed.
        let exec = executor_with(SandboxConfig::default(), 10);
        let pol = policy();
        let cfg = RecoveryConfig::default();
        let pc = Postcondition::default();

        for candidate in ["rm -rf /", "sudo apt install foo"] {
            let verification =
                verify_and_run_repair("echo broken\nexit 1", candidate, &exec, &pol, &cfg, &pc)
                    .await
                    .unwrap();
            assert!(
                !verification.accepted,
                "dangerous candidate ran: {candidate}"
            );
            assert_eq!(verification.tier_reached, 2, "{candidate}");
            assert!(matches!(
                verification.reject_reason,
                Some(RepairRejectReason::PolicyRequiresConfirmation(_))
                    | Some(RepairRejectReason::PolicyDenied(_))
            ));
            assert!(verification.dangerous_attempt);
            assert!(
                verification.report.is_none(),
                "dangerous candidate was executed: {candidate}"
            );
        }
    }

    #[tokio::test]
    async fn test_tier3_valid_fix_accepted() {
        let Some(exec) = sandboxed_executor(10) else {
            return;
        };
        let pol = policy();
        let cfg = RecoveryConfig::default();
        let pc = Postcondition::default();

        let verification =
            verify_and_run_repair("echo broken\nexit 1", "echo fixed", &exec, &pol, &cfg, &pc)
                .await
                .unwrap();
        assert!(verification.accepted);
        assert_eq!(verification.tier_reached, 3);
        assert!(verification.reject_reason.is_none());
        assert!(!verification.dangerous_attempt);
        assert_eq!(verification.report.unwrap().stdout.trim(), "fixed");
    }

    #[tokio::test]
    async fn test_tier3_failing_candidate_rejected() {
        let Some(exec) = sandboxed_executor(10) else {
            return;
        };
        let pol = policy();
        let cfg = RecoveryConfig::default();
        let pc = Postcondition::default();

        let verification = verify_and_run_repair(
            "echo broken\nexit 1",
            "echo start\nexit 3",
            &exec,
            &pol,
            &cfg,
            &pc,
        )
        .await
        .unwrap();
        assert!(!verification.accepted);
        assert_eq!(verification.tier_reached, 3);
        assert_eq!(
            verification.reject_reason,
            Some(RepairRejectReason::ExecutionFailed(
                FailureClass::ExitNonZero
            ))
        );
        assert!(!verification.dangerous_attempt);
        assert!(verification.report.is_some());
    }

    #[tokio::test]
    async fn test_tier3_timeout_candidate_rejected() {
        let Some(exec) = sandboxed_executor(1) else {
            return;
        };
        let pol = policy();
        let cfg = RecoveryConfig::default();
        let pc = Postcondition::default();

        let verification = verify_and_run_repair(
            "echo broken\nexit 1",
            "echo start\nsleep 60",
            &exec,
            &pol,
            &cfg,
            &pc,
        )
        .await
        .unwrap();
        assert!(!verification.accepted);
        assert_eq!(verification.tier_reached, 3);
        assert_eq!(
            verification.reject_reason,
            Some(RepairRejectReason::ExecutionFailed(FailureClass::Timeout))
        );
        assert!(verification.dangerous_attempt);
    }

    #[tokio::test]
    async fn test_tier3_postcondition_mismatch_rejected() {
        let Some(exec) = sandboxed_executor(10) else {
            return;
        };
        let pol = policy();
        let cfg = RecoveryConfig::default();

        // Wrong content.
        let pc = Postcondition {
            must_not_be_empty: true,
            must_contain: Some("goodbye".to_string()),
            require_repeat_match: false,
        };
        let verification =
            verify_and_run_repair("echo broken\nexit 1", "echo hello", &exec, &pol, &cfg, &pc)
                .await
                .unwrap();
        assert!(!verification.accepted);
        assert_eq!(verification.tier_reached, 3);
        assert!(matches!(
            verification.reject_reason,
            Some(RepairRejectReason::PostconditionFailed(_))
        ));

        // Empty output / trivial no-ops (e.g., `true`, `exit 0`) must fail `must_not_be_empty`.
        let pc = Postcondition {
            must_not_be_empty: true,
            ..Default::default()
        };
        for noop in ["true", "exit 0"] {
            let verification =
                verify_and_run_repair("echo broken\nexit 1", noop, &exec, &pol, &cfg, &pc)
                    .await
                    .unwrap();
            assert!(!verification.accepted, "no-op {noop:?} should be rejected");
            assert!(matches!(
                verification.reject_reason,
                Some(RepairRejectReason::PostconditionFailed(_))
            ));
        }
    }

    #[tokio::test]
    async fn test_tier3_non_deterministic_output_rejected() {
        let Some(exec) = sandboxed_executor(10) else {
            return;
        };
        let pol = policy();
        let cfg = RecoveryConfig {
            require_repeat_check: true,
            ..Default::default()
        };
        let pc = Postcondition::default();

        let verification = verify_and_run_repair(
            "echo broken\nexit 1",
            "head -c 8 /dev/urandom | od -An -tx1",
            &exec,
            &pol,
            &cfg,
            &pc,
        )
        .await
        .unwrap();
        assert!(!verification.accepted);
        assert_eq!(verification.tier_reached, 3);
        assert_eq!(
            verification.reject_reason,
            Some(RepairRejectReason::NonDeterministicRepeat)
        );
    }

    #[tokio::test]
    async fn test_tier3_repeat_check_accepts_deterministic_output() {
        let Some(exec) = sandboxed_executor(10) else {
            return;
        };
        let pol = policy();
        let cfg = RecoveryConfig {
            require_repeat_check: true,
            ..Default::default()
        };
        let pc = Postcondition::default();

        let verification =
            verify_and_run_repair("echo broken\nexit 1", "echo stable", &exec, &pol, &cfg, &pc)
                .await
                .unwrap();
        assert!(verification.accepted);
        assert_eq!(verification.tier_reached, 3);
    }
}
