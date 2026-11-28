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
}
