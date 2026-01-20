//! Policy Layer - Action evaluation and confirmation
//!
//! Provides a policy layer to evaluate actions before execution,
//! preventing accidental destructive operations and requiring
//! explicit confirmation for high-risk actions.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::context::Context;
use crate::intent::{ActionType, Intent};

/// Result of policy evaluation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActionPolicy {
    /// Action is allowed to proceed
    Allow,
    /// Action is denied
    Deny { reason: String },
    /// Action requires explicit user confirmation
    RequiresConfirmation {
        message: String,
        risk_level: RiskLevel,
    },
}

/// Risk level for actions requiring confirmation
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

/// Policy evaluator for actions
#[derive(Clone)]
pub struct PolicyEvaluator {
    config: PolicyConfig,
}

/// Policy configuration
#[derive(Clone, Debug)]
pub struct PolicyConfig {
    /// Require confirmation for destructive file operations
    pub confirm_destructive_file_ops: bool,
    /// Require confirmation for system modifications
    pub confirm_system_modifications: bool,
    /// Require confirmation for network operations
    pub confirm_network_ops: bool,
    /// Allow code execution
    pub allow_code_execution: bool,
    /// Maximum file size for operations (bytes)
    pub max_file_size_bytes: u64,
    /// Blocked file patterns (glob patterns)
    pub blocked_file_patterns: Vec<String>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            confirm_destructive_file_ops: true,
            confirm_system_modifications: true,
            confirm_network_ops: false,
            allow_code_execution: true,
            max_file_size_bytes: 100 * 1024 * 1024, // 100MB
            blocked_file_patterns: vec![
                "/etc/passwd".to_string(),
                "/etc/shadow".to_string(),
                "~/.ssh/*".to_string(),
                "~/.gnupg/*".to_string(),
                "/root/*".to_string(),
            ],
        }
    }
}

impl PolicyEvaluator {
    pub fn new(config: PolicyConfig) -> Self {
        Self { config }
    }

    pub fn with_defaults() -> Self {
        Self::new(PolicyConfig::default())
    }

    /// Evaluate an intent before execution
    pub fn evaluate(&self, intent: &Intent, context: &Context) -> ActionPolicy {
        debug!(action = %intent.action, "Evaluating policy for action");

        // Check action type
        match intent.action_type {
            ActionType::GenerateCode => self.evaluate_code_execution(intent, context),
            ActionType::SystemAction => self.evaluate_system_action(intent, context),
            ActionType::SimpleResponse | ActionType::GenerateUi => ActionPolicy::Allow,
            ActionType::CloudEscalate => ActionPolicy::Allow,
        }
    }

    fn evaluate_code_execution(&self, intent: &Intent, _context: &Context) -> ActionPolicy {
        if !self.config.allow_code_execution {
            return ActionPolicy::Deny {
                reason: "Code execution is disabled by policy".to_string(),
            };
        }

        // Check for dangerous code patterns
        let action_lower = intent.action.to_lowercase();

        // Critical patterns - always require confirmation
        let critical_patterns = [
            "rm -rf",
            "delete all",
            "drop database",
            "format disk",
            "sudo rm",
            "dd if=",
        ];

        for pattern in critical_patterns {
            if action_lower.contains(pattern) {
                warn!(pattern = pattern, "Critical action pattern detected");
                return ActionPolicy::RequiresConfirmation {
                    message: format!(
                        "This action contains a potentially dangerous pattern: '{}'. Are you sure you want to proceed?",
                        pattern
                    ),
                    risk_level: RiskLevel::Critical,
                };
            }
        }

        // High-risk patterns
        let high_risk_patterns = [
            "delete",
            "remove",
            "uninstall",
            "modify system",
            "change config",
        ];

        for pattern in high_risk_patterns {
            if action_lower.contains(pattern) {
                return ActionPolicy::RequiresConfirmation {
                    message: format!(
                        "This action will {}: Please confirm you want to proceed.",
                        pattern
                    ),
                    risk_level: RiskLevel::High,
                };
            }
        }

        ActionPolicy::Allow
    }

    fn evaluate_system_action(&self, intent: &Intent, _context: &Context) -> ActionPolicy {
        let action_lower = intent.action.to_lowercase();

        // Check for blocked file patterns
        for blocked in &self.config.blocked_file_patterns {
            let pattern_lower = blocked.to_lowercase();
            // Simple check - in production would use glob matching
            if action_lower.contains(&pattern_lower.replace("*", "")) {
                return ActionPolicy::Deny {
                    reason: format!("Access to '{}' is blocked by security policy", blocked),
                };
            }
        }

        // System modification checks
        if self.config.confirm_system_modifications {
            let system_patterns = [
                "install",
                "uninstall",
                "update system",
                "change setting",
                "modify config",
                "create user",
                "delete user",
                "chmod",
                "chown",
            ];

            for pattern in system_patterns {
                if action_lower.contains(pattern) {
                    return ActionPolicy::RequiresConfirmation {
                        message: format!(
                            "This will modify system settings ({}). Continue?",
                            pattern
                        ),
                        risk_level: RiskLevel::Medium,
                    };
                }
            }
        }

        // Destructive file operations
        if self.config.confirm_destructive_file_ops {
            let destructive_patterns = ["delete", "remove", "overwrite", "truncate", "rm "];

            for pattern in destructive_patterns {
                if action_lower.contains(pattern) {
                    return ActionPolicy::RequiresConfirmation {
                        message: "This will delete or modify files. Continue?".to_string(),
                        risk_level: RiskLevel::Medium,
                    };
                }
            }
        }

        ActionPolicy::Allow
    }

    /// Check if a specific file path is allowed
    pub fn is_path_allowed(&self, path: &str) -> bool {
        for blocked in &self.config.blocked_file_patterns {
            // Simple substring match - would use glob in production
            let normalized_blocked = blocked.replace(
                "~",
                &dirs::home_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
            );

            if path.starts_with(&normalized_blocked.replace("*", "")) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_intent(action: &str, action_type: ActionType) -> Intent {
        Intent {
            action: action.to_string(),
            action_type,
            confidence: 0.9,
            parameters: serde_json::Value::Null,
            requires_cloud: false,
        }
    }

    fn test_context() -> Context {
        Context {
            session_id: "test".to_string(),
            working_directory: "/tmp".to_string(),
            recent_files: vec![],
            conversation_history: vec![],
            timestamp: chrono::Utc::now(),
            user_name: None,
            user_preferences: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn test_simple_response_allowed() {
        let evaluator = PolicyEvaluator::with_defaults();
        let intent = test_intent("hello", ActionType::SimpleResponse);
        let context = test_context();

        match evaluator.evaluate(&intent, &context) {
            ActionPolicy::Allow => {}
            _ => panic!("Expected Allow"),
        }
    }

    #[test]
    fn test_rm_rf_requires_confirmation() {
        let evaluator = PolicyEvaluator::with_defaults();
        let intent = test_intent("rm -rf /tmp/test", ActionType::GenerateCode);
        let context = test_context();

        match evaluator.evaluate(&intent, &context) {
            ActionPolicy::RequiresConfirmation { risk_level, .. } => {
                assert_eq!(risk_level, RiskLevel::Critical);
            }
            _ => panic!("Expected RequiresConfirmation"),
        }
    }

    #[test]
    fn test_delete_requires_confirmation() {
        let evaluator = PolicyEvaluator::with_defaults();
        let intent = test_intent("delete the file", ActionType::SystemAction);
        let context = test_context();

        match evaluator.evaluate(&intent, &context) {
            ActionPolicy::RequiresConfirmation { .. } => {}
            _ => panic!("Expected RequiresConfirmation"),
        }
    }

    #[test]
    fn test_blocked_paths() {
        let evaluator = PolicyEvaluator::with_defaults();
        assert!(!evaluator.is_path_allowed("/etc/passwd"));
        assert!(!evaluator.is_path_allowed("/etc/shadow"));
        assert!(evaluator.is_path_allowed("/tmp/test.txt"));
    }

    #[test]
    fn test_code_execution_disabled() {
        let config = PolicyConfig {
            allow_code_execution: false,
            ..Default::default()
        };
        let evaluator = PolicyEvaluator::new(config);
        let intent = test_intent("run some code", ActionType::GenerateCode);
        let context = test_context();

        match evaluator.evaluate(&intent, &context) {
            ActionPolicy::Deny { .. } => {}
            _ => panic!("Expected Deny"),
        }
    }
}
