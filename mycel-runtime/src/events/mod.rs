use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SystemEvent {
    /// Fired when the AI creates a new tool locally
    CapabilityCreated {
        name: String,
        language: String,
        source_code: String,
    },
    /// Fired when an MCP tool is called
    ToolCalled {
        tool_name: String,
        server_name: String,
        success: bool,
        response_time_ms: u64,
    },
    /// Fired when an MCP server is restarted after failure
    McpServerRestarted { name: String },
    /// Fired when generated code fails to run cleanly
    ExecutionFailed {
        language: String,
        failure_class: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
        sandbox_mode: String,
    },
    /// Fired when the recovery layer decides what to do about a failure
    RecoveryDecided {
        decision: String,
        reason: String,
        verified: bool,
        attempts: u32,
    },
    /// Fired when an action or code execution requires explicit user confirmation
    ConfirmationRequired {
        id: String,
        session_id: String,
        command: String,
        reason: String,
    },
    /// Fired when an AI provider's availability changes
    ProviderHealthChanged {
        provider: String,
        available: bool,
        detail: String,
    },
    /// Fired when mesh sync connectivity or state changes
    SyncStateChanged {
        peers_connected: usize,
        state: String,
    },
}

impl SystemEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            SystemEvent::CapabilityCreated { .. } => "CapabilityCreated",
            SystemEvent::ToolCalled { .. } => "ToolCalled",
            SystemEvent::McpServerRestarted { .. } => "McpServerRestarted",
            SystemEvent::ExecutionFailed { .. } => "ExecutionFailed",
            SystemEvent::RecoveryDecided { .. } => "RecoveryDecided",
            SystemEvent::ConfirmationRequired { .. } => "ConfirmationRequired",
            SystemEvent::ProviderHealthChanged { .. } => "ProviderHealthChanged",
            SystemEvent::SyncStateChanged { .. } => "SyncStateChanged",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_event_kind_matches_variant_names() {
        assert_eq!(
            SystemEvent::CapabilityCreated {
                name: "cap".to_string(),
                language: "python".to_string(),
                source_code: "print(1)".to_string(),
            }
            .kind(),
            "CapabilityCreated"
        );
        assert_eq!(
            SystemEvent::ToolCalled {
                tool_name: "read".to_string(),
                server_name: "fs".to_string(),
                success: true,
                response_time_ms: 12,
            }
            .kind(),
            "ToolCalled"
        );
        assert_eq!(
            SystemEvent::McpServerRestarted {
                name: "fs".to_string(),
            }
            .kind(),
            "McpServerRestarted"
        );
        assert_eq!(
            SystemEvent::ExecutionFailed {
                language: "bash".to_string(),
                failure_class: "non_zero_exit".to_string(),
                exit_code: Some(1),
                signal: None,
                timed_out: false,
                sandbox_mode: "namespace".to_string(),
            }
            .kind(),
            "ExecutionFailed"
        );
        assert_eq!(
            SystemEvent::RecoveryDecided {
                decision: "retry".to_string(),
                reason: "transient".to_string(),
                verified: true,
                attempts: 1,
            }
            .kind(),
            "RecoveryDecided"
        );
        assert_eq!(
            SystemEvent::ConfirmationRequired {
                id: "conf-1".to_string(),
                session_id: "sess-1".to_string(),
                command: "rm -rf /tmp/x".to_string(),
                reason: "destructive".to_string(),
            }
            .kind(),
            "ConfirmationRequired"
        );
        assert_eq!(
            SystemEvent::ProviderHealthChanged {
                provider: "ollama".to_string(),
                available: false,
                detail: "connection refused".to_string(),
            }
            .kind(),
            "ProviderHealthChanged"
        );
        assert_eq!(
            SystemEvent::SyncStateChanged {
                peers_connected: 2,
                state: "connected".to_string(),
            }
            .kind(),
            "SyncStateChanged"
        );
    }

    #[test]
    fn new_variants_json_roundtrip() {
        let events = vec![
            SystemEvent::ConfirmationRequired {
                id: "id-123".to_string(),
                session_id: "sess-abc".to_string(),
                command: "sudo reboot".to_string(),
                reason: "system reboot".to_string(),
            },
            SystemEvent::ProviderHealthChanged {
                provider: "claude".to_string(),
                available: true,
                detail: "ok".to_string(),
            },
            SystemEvent::SyncStateChanged {
                peers_connected: 3,
                state: "syncing".to_string(),
            },
        ];

        for event in events {
            let expected_kind = event.kind();
            let json = serde_json::to_string(&event).unwrap();
            let decoded: SystemEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded.kind(), expected_kind);
            assert_eq!(serde_json::to_string(&decoded).unwrap(), json);
        }
    }
}
