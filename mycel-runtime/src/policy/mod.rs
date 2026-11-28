//! Policy Layer - Action evaluation and confirmation
//!
//! Provides a policy layer to evaluate actions before execution,
//! preventing accidental destructive operations and requiring
//! explicit confirmation for high-risk actions.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::context::Context;
use crate::intent::{ActionType, Intent};

/// Default TTL for pending confirmations (5 minutes).
pub const DEFAULT_CONFIRMATION_TTL: Duration = Duration::from_secs(300);
/// Default global cap on pending confirmations.
pub const DEFAULT_MAX_PENDING_CONFIRMATIONS: usize = 64;
/// Default per-session cap on pending confirmations.
pub const DEFAULT_MAX_PENDING_PER_SESSION: usize = 8;

/// A pending confirmation awaiting user approval.
#[derive(Debug, Clone)]
pub struct PendingConfirmation {
    pub id: String,
    pub session_id: String,
    pub action: String,
    pub code: String,
    pub reason: String,
    pub created_at: Instant,
}

/// Errors returned when looking up or consuming a pending confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmationLookupError {
    NotFound,
    Expired,
    WrongSession,
}

#[derive(Debug, Default)]
struct ConfirmationStoreInner {
    entries: VecDeque<PendingConfirmation>,
}

/// Bounded, session-scoped, TTL-enforcing store for pending confirmations.
#[derive(Clone)]
pub struct ConfirmationStore {
    inner: Arc<Mutex<ConfirmationStoreInner>>,
    max_total: usize,
    max_per_session: usize,
    ttl: Duration,
}

impl Default for ConfirmationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfirmationStore {
    /// Create a store with default limits and TTL.
    pub fn new() -> Self {
        Self::with_limits(
            DEFAULT_MAX_PENDING_CONFIRMATIONS,
            DEFAULT_MAX_PENDING_PER_SESSION,
            DEFAULT_CONFIRMATION_TTL,
        )
    }

    /// Create a store with explicit capacity limits and TTL.
    pub fn with_limits(max_total: usize, max_per_session: usize, ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ConfirmationStoreInner::default())),
            max_total: max_total.max(1),
            max_per_session: max_per_session.max(1),
            ttl,
        }
    }

    /// Insert a new pending confirmation, pruning expired entries and enforcing
    /// per-session and global FIFO eviction caps.
    pub async fn insert(
        &self,
        session_id: &str,
        action: &str,
        code: &str,
        reason: &str,
    ) -> PendingConfirmation {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();

        // Prune expired entries first.
        inner
            .entries
            .retain(|entry| now.duration_since(entry.created_at) < self.ttl);

        // Enforce per-session cap, evicting the oldest entry in this session.
        let mut session_count = inner
            .entries
            .iter()
            .filter(|entry| entry.session_id == session_id)
            .count();
        while session_count >= self.max_per_session {
            if let Some(pos) = inner
                .entries
                .iter()
                .position(|entry| entry.session_id == session_id)
            {
                inner.entries.remove(pos);
                session_count -= 1;
            } else {
                break;
            }
        }

        // Enforce global cap, evicting the oldest entry overall.
        while inner.entries.len() >= self.max_total {
            inner.entries.pop_front();
        }

        let confirmation = PendingConfirmation {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            action: action.to_string(),
            code: code.to_string(),
            reason: reason.to_string(),
            created_at: now,
        };
        inner.entries.push_back(confirmation.clone());
        confirmation
    }

    /// Look up and consume a pending confirmation for `session_id`.
    ///
    /// Checks session ownership before TTL or removal so another session cannot
    /// probe or cancel a confirmation it does not own.
    pub async fn take_for_session(
        &self,
        id: &str,
        session_id: &str,
    ) -> Result<PendingConfirmation, ConfirmationLookupError> {
        let mut inner = self.inner.lock().await;
        let Some(pos) = inner.entries.iter().position(|entry| entry.id == id) else {
            return Err(ConfirmationLookupError::NotFound);
        };

        if inner.entries[pos].session_id != session_id {
            return Err(ConfirmationLookupError::WrongSession);
        }

        if inner.entries[pos].created_at.elapsed() >= self.ttl {
            inner.entries.remove(pos);
            return Err(ConfirmationLookupError::Expired);
        }

        inner
            .entries
            .remove(pos)
            .ok_or(ConfirmationLookupError::NotFound)
    }

    /// Remove all pending confirmations in `session_id` matching `code`.
    pub async fn clear_matching_code(&self, session_id: &str, code: &str) -> usize {
        let mut inner = self.inner.lock().await;
        let before = inner.entries.len();
        inner
            .entries
            .retain(|entry| !(entry.session_id == session_id && entry.code == code));
        before - inner.entries.len()
    }

    /// Remove all pending confirmations belonging to `session_id`.
    pub async fn clear_session(&self, session_id: &str) -> usize {
        let mut inner = self.inner.lock().await;
        let before = inner.entries.len();
        inner.entries.retain(|entry| entry.session_id != session_id);
        before - inner.entries.len()
    }

    /// Return the number of currently stored pending confirmations.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.entries.len()
    }

    /// Return whether the store currently has no pending confirmations.
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.entries.is_empty()
    }
}

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

    /// Evaluate generated code for safety
    pub fn evaluate_code(&self, code: &str) -> ActionPolicy {
        if !self.config.allow_code_execution {
            return ActionPolicy::Deny {
                reason: "Code execution is disabled by policy".to_string(),
            };
        }

        let code_lower = code.to_lowercase();

        // Critical patterns - always require confirmation
        let critical_patterns = [
            "rm -rf",
            "sudo ",
            "mkfs",
            "dd if=",
            "> /dev/",
            "drop database",
            "format disk",
            ":(){ :|:& };:", // Fork bomb
        ];

        for pattern in critical_patterns {
            if code_lower.contains(pattern) {
                warn!(pattern = pattern, "Critical code pattern detected");
                return ActionPolicy::RequiresConfirmation {
                    message: format!(
                        "Dangerous command detected: '{}'. This could cause permanent data loss. Proceed?",
                        pattern.trim()
                    ),
                    risk_level: RiskLevel::Critical,
                };
            }
        }

        // High-risk patterns
        let high_risk_patterns = [
            "chmod -r 777",
            "chown -r",
            "mv /etc/",
            "cp /etc/",
            "mv /boot/",
            "rm ",
            "apt-get remove",
            "apt remove",
            "uninstall",
        ];

        for pattern in high_risk_patterns {
            if code_lower.contains(pattern) {
                return ActionPolicy::RequiresConfirmation {
                    message: format!("Potentially risky command: '{}'. Proceed?", pattern.trim()),
                    risk_level: RiskLevel::High,
                };
            }
        }

        ActionPolicy::Allow
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
            pending_command: None,
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

    #[test]
    fn test_evaluate_dangerous_code() {
        let evaluator = PolicyEvaluator::with_defaults();

        // Test critical pattern
        match evaluator.evaluate_code("rm -rf /") {
            ActionPolicy::RequiresConfirmation { risk_level, .. } => {
                assert_eq!(risk_level, RiskLevel::Critical);
            }
            _ => panic!("Expected RequiresConfirmation for rm -rf"),
        }

        // Test high risk pattern
        match evaluator.evaluate_code("chmod -R 777 /home/user") {
            ActionPolicy::RequiresConfirmation { risk_level, .. } => {
                assert_eq!(risk_level, RiskLevel::High);
            }
            _ => panic!("Expected RequiresConfirmation for chmod"),
        }

        // Test safe code
        match evaluator.evaluate_code("ls -la") {
            ActionPolicy::Allow => {}
            _ => panic!("Expected Allow for ls -la"),
        }
    }

    #[tokio::test]
    async fn confirmation_store_insert_and_take_happy_path() {
        let store = ConfirmationStore::new();
        assert!(store.is_empty().await);

        let inserted = store
            .insert("sess-1", "delete", "rm -rf /tmp/x", "dangerous")
            .await;
        assert!(!inserted.id.is_empty());
        assert_eq!(inserted.session_id, "sess-1");
        assert_eq!(inserted.action, "delete");
        assert_eq!(inserted.code, "rm -rf /tmp/x");
        assert_eq!(inserted.reason, "dangerous");
        assert_eq!(store.len().await, 1);
        assert!(!store.is_empty().await);

        let taken = store
            .take_for_session(&inserted.id, "sess-1")
            .await
            .unwrap();
        assert_eq!(taken.id, inserted.id);
        assert_eq!(taken.code, "rm -rf /tmp/x");
        assert!(store.is_empty().await);

        let second_take = store.take_for_session(&inserted.id, "sess-1").await;
        assert_eq!(second_take.unwrap_err(), ConfirmationLookupError::NotFound);
    }

    #[tokio::test]
    async fn confirmation_store_wrong_session_preserves_entry() {
        let store = ConfirmationStore::new();
        let inserted = store
            .insert("owner-sess", "delete", "rm -rf /tmp/x", "dangerous")
            .await;

        let wrong = store.take_for_session(&inserted.id, "other-sess").await;
        assert_eq!(wrong.unwrap_err(), ConfirmationLookupError::WrongSession);
        assert_eq!(store.len().await, 1);

        let taken = store
            .take_for_session(&inserted.id, "owner-sess")
            .await
            .unwrap();
        assert_eq!(taken.id, inserted.id);
        assert!(store.is_empty().await);
    }

    #[tokio::test]
    async fn confirmation_store_ttl_expiry() {
        let store = ConfirmationStore::with_limits(10, 5, Duration::from_millis(20));
        let inserted = store
            .insert("sess-1", "delete", "rm -rf /tmp/x", "dangerous")
            .await;

        tokio::time::sleep(Duration::from_millis(35)).await;

        let expired = store.take_for_session(&inserted.id, "sess-1").await;
        assert_eq!(expired.unwrap_err(), ConfirmationLookupError::Expired);
        assert!(store.is_empty().await);
    }

    #[tokio::test]
    async fn confirmation_store_per_session_cap_evicts_oldest_in_session() {
        let store = ConfirmationStore::with_limits(10, 2, Duration::from_secs(60));
        let s1_first = store.insert("sess-1", "a1", "code-1", "r1").await;
        let s2_first = store.insert("sess-2", "b1", "code-b1", "rb1").await;
        let s1_second = store.insert("sess-1", "a2", "code-2", "r2").await;
        let s1_third = store.insert("sess-1", "a3", "code-3", "r3").await;

        // sess-1 cap is 2, so s1_first was evicted while s2_first remains.
        assert_eq!(store.len().await, 3);
        assert_eq!(
            store
                .take_for_session(&s1_first.id, "sess-1")
                .await
                .unwrap_err(),
            ConfirmationLookupError::NotFound
        );
        assert!(store.take_for_session(&s2_first.id, "sess-2").await.is_ok());
        assert!(store
            .take_for_session(&s1_second.id, "sess-1")
            .await
            .is_ok());
        assert!(store.take_for_session(&s1_third.id, "sess-1").await.is_ok());
    }

    #[tokio::test]
    async fn confirmation_store_global_cap_evicts_oldest_globally() {
        let store = ConfirmationStore::with_limits(3, 3, Duration::from_secs(60));
        let first = store.insert("sess-1", "a1", "c1", "r1").await;
        let second = store.insert("sess-2", "a2", "c2", "r2").await;
        let third = store.insert("sess-3", "a3", "c3", "r3").await;
        let fourth = store.insert("sess-4", "a4", "c4", "r4").await;

        assert_eq!(store.len().await, 3);
        assert_eq!(
            store
                .take_for_session(&first.id, "sess-1")
                .await
                .unwrap_err(),
            ConfirmationLookupError::NotFound
        );
        assert!(store.take_for_session(&second.id, "sess-2").await.is_ok());
        assert!(store.take_for_session(&third.id, "sess-3").await.is_ok());
        assert!(store.take_for_session(&fourth.id, "sess-4").await.is_ok());
    }

    #[tokio::test]
    async fn confirmation_store_clear_matching_code_and_clear_session() {
        let store = ConfirmationStore::new();
        let s1_a = store.insert("sess-1", "act", "rm -rf /tmp/a", "r").await;
        let s1_a_dup = store.insert("sess-1", "act", "rm -rf /tmp/a", "r").await;
        let s1_b = store.insert("sess-1", "act", "rm -rf /tmp/b", "r").await;
        let s2_a = store.insert("sess-2", "act", "rm -rf /tmp/a", "r").await;

        let cleared_code = store.clear_matching_code("sess-1", "rm -rf /tmp/a").await;
        assert_eq!(cleared_code, 2);
        assert_eq!(store.len().await, 2);
        assert_eq!(
            store
                .take_for_session(&s1_a.id, "sess-1")
                .await
                .unwrap_err(),
            ConfirmationLookupError::NotFound
        );
        assert_eq!(
            store
                .take_for_session(&s1_a_dup.id, "sess-1")
                .await
                .unwrap_err(),
            ConfirmationLookupError::NotFound
        );

        let cleared_sess = store.clear_session("sess-1").await;
        assert_eq!(cleared_sess, 1);
        assert_eq!(store.len().await, 1);
        assert_eq!(
            store
                .take_for_session(&s1_b.id, "sess-1")
                .await
                .unwrap_err(),
            ConfirmationLookupError::NotFound
        );
        assert!(store.take_for_session(&s2_a.id, "sess-2").await.is_ok());
    }
}
