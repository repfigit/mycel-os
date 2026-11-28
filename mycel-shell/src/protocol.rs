use serde::{Deserialize, Serialize};

/// LLM provider selection for chat requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmProvider {
    /// Automatically choose based on config (default).
    #[default]
    Auto,
    /// Force local LLM (Ollama).
    Local,
    /// Force cloud LLM (Claude / OpenRouter).
    Cloud,
}

/// Requests sent from `mycel-shell` to `mycel-runtime`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcRequest {
    /// Authenticate with token (required before other requests except Ping).
    Authenticate { token: String },
    /// Send a chat message.
    Chat {
        message: String,
        #[serde(default)]
        provider: LlmProvider,
    },
    /// Set the session ID for this connection.
    SetSession { id: String },
    /// Get system status.
    Status,
    /// Confirm or deny a pending policy-gated action.
    ConfirmAction { id: String, approved: bool },
    /// Subscribe to pushed system events (switches connection to push-only mode).
    Subscribe {
        #[serde(default)]
        events: Option<Vec<String>>,
    },
    /// Get mesh sync status.
    SyncStatus,
    /// Ping for health check (allowed without auth).
    Ping,
}

/// Peer connection status in mesh sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerStatus {
    Connected,
    Disconnected,
    Pairing,
}

/// Information about a mesh sync peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
    pub status: PeerStatus,
    pub addresses: Vec<String>,
}

/// System events pushed by `mycel-runtime` in `Subscribe` mode.
///
/// Uses serde's default external enum tagging to match `mycel-runtime/src/events/mod.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemEvent {
    /// Fired when the AI creates a new tool locally.
    CapabilityCreated {
        name: String,
        language: String,
        source_code: String,
    },
    /// Fired when an MCP tool is called.
    ToolCalled {
        tool_name: String,
        server_name: String,
        success: bool,
        response_time_ms: u64,
    },
    /// Fired when an MCP server is restarted after failure.
    McpServerRestarted { name: String },
    /// Fired when generated code fails to run cleanly.
    ExecutionFailed {
        language: String,
        failure_class: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
        sandbox_mode: String,
    },
    /// Fired when the recovery layer decides what to do about a failure.
    RecoveryDecided {
        decision: String,
        reason: String,
        verified: bool,
        attempts: u32,
    },
    /// Fired when an action or code execution requires explicit user confirmation.
    ConfirmationRequired {
        id: String,
        session_id: String,
        command: String,
        reason: String,
    },
    /// Fired when an AI provider's availability changes.
    ProviderHealthChanged {
        provider: String,
        available: bool,
        detail: String,
    },
    /// Fired when mesh sync connectivity or state changes.
    SyncStateChanged {
        peers_connected: usize,
        state: String,
    },
}

impl SystemEvent {
    /// Returns the variant name matching `mycel-runtime`'s event filter strings.
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

/// Responses sent from `mycel-runtime` to `mycel-shell`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcResponse {
    /// Terminal chat response.
    Chat {
        response: String,
        #[serde(default)]
        surface: Option<serde_json::Value>,
    },
    /// Streaming chat delta chunk.
    ChatChunk { delta: String },
    /// Code execution result.
    CodeResult {
        code: String,
        output: String,
        success: bool,
    },
    /// System status.
    Status {
        version: String,
        uptime: u64,
        sessions: usize,
        llm_model: String,
        #[serde(default)]
        cloud_spend_usd: f64,
    },
    /// Pushed system event (in `Subscribe` mode).
    Event { event: SystemEvent },
    /// Structured confirmation prompt for policy-gated actions.
    ConfirmationRequired {
        id: String,
        action: String,
        code: String,
        reason: String,
    },
    /// Generic OK response.
    Ok { message: String },
    /// Mesh sync status.
    SyncStatus {
        enabled: bool,
        device_id: String,
        transport: String,
        interface: Option<String>,
        listen_addr: Option<String>,
        trusted_peers: usize,
        connected_peers: usize,
        event_log_len: usize,
        peers: Vec<PeerInfo>,
    },
    /// Error response.
    Error { message: String },
    /// Pong response to ping.
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_provider_wire_format() {
        assert_eq!(
            serde_json::to_string(&LlmProvider::Auto).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::to_string(&LlmProvider::Local).unwrap(),
            "\"local\""
        );
        assert_eq!(
            serde_json::to_string(&LlmProvider::Cloud).unwrap(),
            "\"cloud\""
        );

        assert_eq!(
            serde_json::from_str::<LlmProvider>("\"auto\"").unwrap(),
            LlmProvider::Auto
        );
        assert_eq!(
            serde_json::from_str::<LlmProvider>("\"local\"").unwrap(),
            LlmProvider::Local
        );
        assert_eq!(
            serde_json::from_str::<LlmProvider>("\"cloud\"").unwrap(),
            LlmProvider::Cloud
        );
    }

    #[test]
    fn ipc_request_roundtrip_all_variants() {
        let requests = vec![
            IpcRequest::Authenticate {
                token: "tok-123".to_string(),
            },
            IpcRequest::Chat {
                message: "hello".to_string(),
                provider: LlmProvider::Local,
            },
            IpcRequest::SetSession {
                id: "sess-1".to_string(),
            },
            IpcRequest::Status,
            IpcRequest::ConfirmAction {
                id: "act-42".to_string(),
                approved: true,
            },
            IpcRequest::Subscribe {
                events: Some(vec!["ExecutionFailed".to_string()]),
            },
            IpcRequest::Subscribe { events: None },
            IpcRequest::SyncStatus,
            IpcRequest::Ping,
        ];

        for req in requests {
            let json = serde_json::to_string(&req).unwrap();
            let decoded: IpcRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, req);
        }

        // Verify exact tag format and default provider deserialization.
        let chat_no_provider: IpcRequest =
            serde_json::from_str(r#"{"type":"Chat","message":"hi"}"#).unwrap();
        assert_eq!(
            chat_no_provider,
            IpcRequest::Chat {
                message: "hi".to_string(),
                provider: LlmProvider::Auto,
            }
        );

        let sub_no_events: IpcRequest = serde_json::from_str(r#"{"type":"Subscribe"}"#).unwrap();
        assert_eq!(sub_no_events, IpcRequest::Subscribe { events: None });
    }

    #[test]
    fn ipc_response_roundtrip_all_variants() {
        let responses = vec![
            IpcResponse::Chat {
                response: "done".to_string(),
                surface: Some(serde_json::json!({"kind": "card"})),
            },
            IpcResponse::ChatChunk {
                delta: "hel".to_string(),
            },
            IpcResponse::CodeResult {
                code: "echo 1".to_string(),
                output: "1\n".to_string(),
                success: true,
            },
            IpcResponse::Status {
                version: "0.1.0".to_string(),
                uptime: 42,
                sessions: 2,
                llm_model: "qwen2.5".to_string(),
                cloud_spend_usd: 0.12,
            },
            IpcResponse::Event {
                event: SystemEvent::McpServerRestarted {
                    name: "fs".to_string(),
                },
            },
            IpcResponse::ConfirmationRequired {
                id: "conf-1".to_string(),
                action: "exec".to_string(),
                code: "rm -rf /tmp/test".to_string(),
                reason: "destructive".to_string(),
            },
            IpcResponse::Ok {
                message: "Authenticated successfully".to_string(),
            },
            IpcResponse::SyncStatus {
                enabled: true,
                device_id: "dev-1".to_string(),
                transport: "wireguard".to_string(),
                interface: Some("wg0".to_string()),
                listen_addr: Some("0.0.0.0:51820".to_string()),
                trusted_peers: 2,
                connected_peers: 1,
                event_log_len: 15,
                peers: vec![
                    PeerInfo {
                        id: "peer-1".to_string(),
                        name: "laptop".to_string(),
                        status: PeerStatus::Connected,
                        addresses: vec!["10.0.0.2:51820".to_string()],
                    },
                    PeerInfo {
                        id: "peer-2".to_string(),
                        name: "phone".to_string(),
                        status: PeerStatus::Disconnected,
                        addresses: vec![],
                    },
                    PeerInfo {
                        id: "peer-3".to_string(),
                        name: "tablet".to_string(),
                        status: PeerStatus::Pairing,
                        addresses: vec![],
                    },
                ],
            },
            IpcResponse::Error {
                message: "boom".to_string(),
            },
            IpcResponse::Pong,
        ];

        for resp in responses {
            let json = serde_json::to_string(&resp).unwrap();
            let decoded: IpcResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, resp);
        }

        // Verify optional/default fields on Chat and Status.
        let chat_minimal: IpcResponse =
            serde_json::from_str(r#"{"type":"Chat","response":"ok","surface":null}"#).unwrap();
        assert_eq!(
            chat_minimal,
            IpcResponse::Chat {
                response: "ok".to_string(),
                surface: None,
            }
        );

        let status_minimal: IpcResponse = serde_json::from_str(
            r#"{"type":"Status","version":"0.1.0","uptime":1,"sessions":0,"llm_model":"local"}"#,
        )
        .unwrap();
        assert_eq!(
            status_minimal,
            IpcResponse::Status {
                version: "0.1.0".to_string(),
                uptime: 1,
                sessions: 0,
                llm_model: "local".to_string(),
                cloud_spend_usd: 0.0,
            }
        );
    }

    #[test]
    fn system_event_roundtrip_and_kind_all_variants() {
        let events = vec![
            (
                SystemEvent::CapabilityCreated {
                    name: "cap".to_string(),
                    language: "python".to_string(),
                    source_code: "print(1)".to_string(),
                },
                "CapabilityCreated",
            ),
            (
                SystemEvent::ToolCalled {
                    tool_name: "read_file".to_string(),
                    server_name: "fs".to_string(),
                    success: true,
                    response_time_ms: 14,
                },
                "ToolCalled",
            ),
            (
                SystemEvent::McpServerRestarted {
                    name: "fs".to_string(),
                },
                "McpServerRestarted",
            ),
            (
                SystemEvent::ExecutionFailed {
                    language: "bash".to_string(),
                    failure_class: "non_zero_exit".to_string(),
                    exit_code: Some(2),
                    signal: None,
                    timed_out: false,
                    sandbox_mode: "namespace".to_string(),
                },
                "ExecutionFailed",
            ),
            (
                SystemEvent::RecoveryDecided {
                    decision: "retry".to_string(),
                    reason: "transient".to_string(),
                    verified: true,
                    attempts: 1,
                },
                "RecoveryDecided",
            ),
            (
                SystemEvent::ConfirmationRequired {
                    id: "conf-1".to_string(),
                    session_id: "sess-1".to_string(),
                    command: "rm -rf /tmp/x".to_string(),
                    reason: "destructive".to_string(),
                },
                "ConfirmationRequired",
            ),
            (
                SystemEvent::ProviderHealthChanged {
                    provider: "ollama".to_string(),
                    available: false,
                    detail: "connection refused".to_string(),
                },
                "ProviderHealthChanged",
            ),
            (
                SystemEvent::SyncStateChanged {
                    peers_connected: 2,
                    state: "connected".to_string(),
                },
                "SyncStateChanged",
            ),
        ];

        for (event, expected_kind) in events {
            assert_eq!(event.kind(), expected_kind);
            let json = serde_json::to_string(&event).unwrap();
            // External tagging puts the variant name as the outer JSON key.
            assert!(
                json.starts_with(&format!("{{\"{}\":", expected_kind)),
                "unexpected external tagging JSON: {}",
                json
            );
            let decoded: SystemEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, event);
            assert_eq!(decoded.kind(), expected_kind);
        }
    }
}
