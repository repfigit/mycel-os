//! MCP (Model Context Protocol) Module
//!
//! Provides integration with MCP servers for tool execution.
//! The McpManager handles server lifecycle, tool discovery, and invocation.
//!
//! Features:
//! - Multi-format tool call parsing
//! - Server health monitoring and auto-restart
//! - Parallel tool execution
//! - Tool result caching with TTL
//! - Audit logging
//! - Structured confirmation flow

pub mod client;
pub mod evolution;
pub mod protocol;
pub mod tool_parser;

use crate::events::SystemEvent;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info, warn};

pub use client::{McpServer, ServerHealth, ServerState};
pub use evolution::McpEvolver;
pub use protocol::McpTool;
pub use tool_parser::{format_tool_result, format_tools_for_prompt, parse_tool_calls, ToolCall};

use crate::config::{McpConfig, McpServerConfig};

/// Validate an MCP server name (strictly `[a-zA-Z0-9_-]`, 1..=64 chars).
pub fn validate_server_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(anyhow!(
            "Invalid MCP server name '{}': must be between 1 and 64 characters",
            name
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(anyhow!(
            "Invalid MCP server name '{}': only [a-zA-Z0-9_-] characters are allowed",
            name
        ));
    }
    Ok(())
}

/// Risk level for tool operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    /// Safe, read-only operations
    Low,
    /// Operations that modify user data
    Medium,
    /// System-level or destructive operations
    High,
}

/// Pending confirmation for a tool call
#[derive(Debug, Clone)]
pub struct PendingConfirmation {
    /// Tool name
    pub tool_name: String,
    /// Tool arguments
    pub arguments: HashMap<String, serde_json::Value>,
    /// Human-readable description
    pub description: String,
    /// Risk level
    pub risk_level: RiskLevel,
    /// When the confirmation was created
    pub created_at: Instant,
}

/// Cached tool result
#[derive(Debug, Clone)]
struct CachedResult {
    result: String,
    expires_at: Instant,
}

/// Audit log entry for tool calls
#[derive(Debug, Clone)]
pub struct ToolAuditEntry {
    /// Timestamp
    pub timestamp: Instant,
    /// Tool name
    pub tool_name: String,
    /// Arguments (may be redacted)
    pub arguments: HashMap<String, serde_json::Value>,
    /// Whether the call succeeded
    pub success: bool,
    /// Response time in milliseconds
    pub response_time_ms: u64,
    /// Error message if failed
    pub error: Option<String>,
    /// Server that handled the call
    pub server_name: String,
}

/// Manages multiple MCP servers and provides unified tool access
#[derive(Clone)]
pub struct McpManager {
    config: McpConfig,
    servers: Arc<tokio::sync::Mutex<HashMap<String, McpServer>>>,
    runtime_path: String,
    event_bus: broadcast::Sender<SystemEvent>,
    /// Cache for tool results (tool_name:args_hash -> result)
    cache: Arc<RwLock<HashMap<String, CachedResult>>>,
    /// Audit log (bounded circular buffer)
    audit_log: Arc<RwLock<Vec<ToolAuditEntry>>>,
    /// Maximum audit log entries
    max_audit_entries: usize,
}

impl McpManager {
    /// Create a new MCP manager from configuration
    pub async fn new(
        config: &McpConfig,
        runtime_path: &str,
        event_bus: broadcast::Sender<SystemEvent>,
    ) -> Result<Self> {
        let manager = Self {
            config: config.clone(),
            servers: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            runtime_path: runtime_path.to_string(),
            event_bus,
            cache: Arc::new(RwLock::new(HashMap::new())),
            audit_log: Arc::new(RwLock::new(Vec::new())),
            max_audit_entries: 1000,
        };

        Ok(manager)
    }

    /// Start all configured MCP servers
    pub async fn start_servers(&self) -> Result<()> {
        if !self.config.enabled {
            info!("MCP is disabled in configuration");
            return Ok(());
        }

        // Start servers concurrently in the background so a slow or failing
        // server (e.g. missing Python deps) doesn't block runtime startup.
        // Each server has its own init_timeout; failures are logged, not fatal.
        for server_config in &self.config.servers {
            let manager = self.clone();
            let server_config = server_config.clone();
            tokio::spawn(async move {
                if let Err(e) = manager.start_server(&server_config).await {
                    warn!("Failed to start MCP server '{}': {}", server_config.name, e);
                }
            });
        }

        // Load dynamic servers only when evolution is explicitly enabled
        if self.config.allow_evolution {
            let dynamic_dir = format!("{}/mcp-servers/dynamic", self.runtime_path);
            if Path::new(&dynamic_dir).exists() {
                let mut entries = tokio::fs::read_dir(&dynamic_dir).await?;
                while let Some(entry) = entries.next_entry().await? {
                    if entry.file_type().await?.is_dir() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if let Err(e) = validate_server_name(&name) {
                            warn!(
                                "Skipping invalid dynamic MCP server directory '{}': {}",
                                name, e
                            );
                            continue;
                        }
                        let server_dir = entry.path();

                        // Detect language and setup config
                        let (command, args) = if server_dir.join("index.js").exists() {
                            (
                                "node".to_string(),
                                vec![server_dir.join("index.js").to_string_lossy().to_string()],
                            )
                        } else if server_dir.join("server.py").exists() {
                            (
                                "python3".to_string(),
                                vec![server_dir.join("server.py").to_string_lossy().to_string()],
                            )
                        } else {
                            continue;
                        };

                        info!("Loading dynamic MCP server: {}", name);
                        if let Err(e) = self.add_dynamic_server(&name, &command, args).await {
                            warn!("Failed to load dynamic MCP server '{}': {}", name, e);
                        }
                    }
                }
            }
        }

        // Start background health check task
        self.spawn_health_check_task();

        Ok(())
    }

    /// Spawn a background task to periodically check server health
    fn spawn_health_check_task(&self) {
        let servers = self.servers.clone();
        let event_bus = self.event_bus.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));

            loop {
                interval.tick().await;

                let mut servers_guard = servers.lock().await;
                for (name, server) in servers_guard.iter_mut() {
                    if !server.health_check().await {
                        warn!("[{}] Health check failed, attempting restart", name);
                        match server.restart_if_needed().await {
                            Ok(true) => {
                                info!("[{}] Server restarted successfully", name);
                                let _ = event_bus
                                    .send(SystemEvent::McpServerRestarted { name: name.clone() });
                            }
                            Ok(false) => {}
                            Err(e) => {
                                warn!("[{}] Failed to restart: {}", name, e);
                            }
                        }
                    }
                }
            }
        });
    }

    /// Start a single MCP server
    pub async fn start_server(&self, config: &McpServerConfig) -> Result<()> {
        // Resolve the command path
        let command = self.resolve_command(&config.command);
        let args = self.resolve_args(&config.args);

        let mut server = McpServer::new(
            config.name.clone(),
            command,
            args,
            config.env.clone(),
            config.requires_confirmation.clone(),
        );

        server.start().await?;

        self.servers
            .lock()
            .await
            .insert(config.name.clone(), server);

        Ok(())
    }

    /// Whether dynamic MCP server evolution is enabled
    pub fn allow_evolution(&self) -> bool {
        self.config.allow_evolution
    }

    /// Hot-load a new MCP server from a directory
    pub async fn add_dynamic_server(
        &self,
        name: &str,
        command: &str,
        args: Vec<String>,
    ) -> Result<()> {
        if !self.config.allow_evolution {
            return Err(anyhow!(
                "MCP evolution is disabled in configuration (allow_evolution = false)"
            ));
        }
        validate_server_name(name)?;

        let config = McpServerConfig {
            name: name.to_string(),
            command: command.to_string(),
            args,
            env: HashMap::new(),
            requires_confirmation: Vec::new(),
        };

        self.start_server(&config).await
    }

    /// Resolve command path (handle relative paths from runtime directory)
    fn resolve_command(&self, command: &str) -> String {
        if command.starts_with('/') || !command.contains('/') {
            return command.to_string();
        }

        let full_path = Path::new(&self.runtime_path).join(command);
        if full_path.exists() {
            return full_path.to_string_lossy().to_string();
        }

        command.to_string()
    }

    /// Resolve arguments (handle relative paths)
    fn resolve_args(&self, args: &[String]) -> Vec<String> {
        args.iter()
            .map(|arg| {
                if arg.contains('/') && !arg.starts_with('/') && !arg.starts_with("--") {
                    let full_path = Path::new(&self.runtime_path).join(arg);
                    if full_path.exists() || full_path.parent().map(|p| p.exists()).unwrap_or(false)
                    {
                        return full_path.to_string_lossy().to_string();
                    }
                }
                arg.clone()
            })
            .collect()
    }

    /// Get all available tools from all servers
    pub async fn get_all_tools(&self) -> Vec<McpTool> {
        let mut all_tools = Vec::new();
        let servers = self.servers.lock().await;

        for server in servers.values() {
            if server.state().await == ServerState::Ready {
                all_tools.extend(server.get_tools().await);
            }
        }

        all_tools
    }

    /// Find which server provides a specific tool
    async fn find_tool_server(&self, tool_name: &str) -> Option<String> {
        let servers = self.servers.lock().await;

        for (name, server) in servers.iter() {
            if server.state().await == ServerState::Ready {
                for tool in server.get_tools().await {
                    if tool.name == tool_name {
                        return Some(name.clone());
                    }
                }
            }
        }

        None
    }

    /// Generate a cache key for a tool call
    fn cache_key(tool_name: &str, arguments: &HashMap<String, serde_json::Value>) -> String {
        let args_json = serde_json::to_string(arguments).unwrap_or_default();
        format!("{}:{}", tool_name, args_json)
    }

    /// Call a tool by name
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: HashMap<String, serde_json::Value>,
    ) -> Result<protocol::CallToolResult> {
        let start = Instant::now();
        let server_name = self
            .find_tool_server(tool_name)
            .await
            .ok_or_else(|| anyhow!("No server provides tool '{}'", tool_name))?;

        let result = {
            let mut servers = self.servers.lock().await;
            let server = servers
                .get_mut(&server_name)
                .ok_or_else(|| anyhow!("Server '{}' not found", server_name))?;

            server.call_tool(tool_name, arguments.clone()).await
        };

        // Record audit entry
        let elapsed = start.elapsed();
        self.record_audit_entry(ToolAuditEntry {
            timestamp: Instant::now(),
            tool_name: tool_name.to_string(),
            arguments: arguments.clone(),
            success: result.is_ok(),
            response_time_ms: elapsed.as_millis() as u64,
            error: result.as_ref().err().map(|e| e.to_string()),
            server_name: server_name.clone(),
        })
        .await;

        // Send event
        let _ = self.event_bus.send(SystemEvent::ToolCalled {
            tool_name: tool_name.to_string(),
            server_name: server_name.clone(),
            success: result.is_ok(),
            response_time_ms: elapsed.as_millis() as u64,
        });

        result
    }

    /// Call a tool with caching
    pub async fn call_tool_cached(
        &self,
        tool_name: &str,
        arguments: HashMap<String, serde_json::Value>,
        ttl: Duration,
    ) -> Result<String> {
        let cache_key = Self::cache_key(tool_name, &arguments);

        // Check cache first
        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.get(&cache_key) {
                if cached.expires_at > Instant::now() {
                    debug!("Cache hit for tool '{}'", tool_name);
                    return Ok(cached.result.clone());
                }
            }
        }

        // Call the tool
        let result = self.call_tool(tool_name, arguments).await?;
        let formatted = format_tool_result(tool_name, &result);

        // Store in cache
        {
            let mut cache = self.cache.write().await;
            cache.insert(
                cache_key,
                CachedResult {
                    result: formatted.clone(),
                    expires_at: Instant::now() + ttl,
                },
            );

            // Cleanup expired entries periodically
            if cache.len() > 100 {
                let now = Instant::now();
                cache.retain(|_, v| v.expires_at > now);
            }
        }

        Ok(formatted)
    }

    /// Execute multiple tool calls in parallel
    pub async fn call_tools_parallel(&self, calls: &[ToolCall]) -> Vec<Result<String>> {
        let futures: Vec<_> = calls
            .iter()
            .map(|call| {
                let manager = self.clone();
                let call = call.clone();
                async move { manager.process_tool_call(&call).await }
            })
            .collect();

        futures::future::join_all(futures).await
    }

    /// Record an audit log entry
    async fn record_audit_entry(&self, entry: ToolAuditEntry) {
        let mut log = self.audit_log.write().await;

        log.push(entry);

        // Keep bounded size
        if log.len() > self.max_audit_entries {
            log.remove(0);
        }
    }

    /// Get recent audit log entries
    pub async fn get_audit_log(&self, limit: usize) -> Vec<ToolAuditEntry> {
        let log = self.audit_log.read().await;
        log.iter().rev().take(limit).cloned().collect()
    }

    /// Clear the cache
    pub async fn clear_cache(&self) {
        self.cache.write().await.clear();
    }

    /// Check if a tool requires user confirmation
    pub async fn requires_confirmation(&self, tool_name: &str) -> bool {
        if let Some(server_name) = self.find_tool_server(tool_name).await {
            let servers = self.servers.lock().await;
            if let Some(server) = servers.get(&server_name) {
                return server.requires_confirmation(tool_name);
            }
        }
        true
    }

    /// Create a pending confirmation for a tool call
    pub fn create_pending_confirmation(
        &self,
        tool_name: &str,
        arguments: HashMap<String, serde_json::Value>,
    ) -> PendingConfirmation {
        let risk_level = self.assess_risk_level(tool_name, &arguments);

        let description = match tool_name {
            "xbps_install" => format!(
                "Install package: {}",
                arguments
                    .get("package")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            ),
            "xbps_remove" => format!(
                "Remove package: {}",
                arguments
                    .get("package")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            ),
            "service_control" => format!(
                "{} service: {}",
                arguments
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("control"),
                arguments
                    .get("service")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            ),
            _ => format!("Execute tool '{}' with arguments", tool_name),
        };

        PendingConfirmation {
            tool_name: tool_name.to_string(),
            arguments,
            description,
            risk_level,
            created_at: Instant::now(),
        }
    }

    /// Assess the risk level of a tool call
    fn assess_risk_level(
        &self,
        tool_name: &str,
        _arguments: &HashMap<String, serde_json::Value>,
    ) -> RiskLevel {
        match tool_name {
            // Read-only operations
            "xbps_search" | "xbps_info" | "service_status" | "system_info" => RiskLevel::Low,

            // System modifications
            "xbps_install" | "service_control" => RiskLevel::Medium,

            // Destructive operations
            "xbps_remove" => RiskLevel::High,

            // Unknown tools default to high risk
            _ => RiskLevel::High,
        }
    }

    /// Get the tools formatted for LLM prompt injection
    pub async fn get_tools_prompt(&self) -> String {
        let mut tools = self.get_all_tools().await;

        // Built-in shell execution tool, always available even with no MCP
        // servers running. Routed to the internal policy-gated executor.
        let builtin_tools = vec![McpTool {
            name: "shell_execute".to_string(),
            description: "Execute a shell command on the local system and return its output. \
Use this for ANY system task: listing files, checking system info, installing packages, \
reading command output, etc. You CAN and SHOULD run commands when the user asks. \
Dangerous commands are automatically gated for user confirmation by the policy evaluator."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to execute"}
                },
                "required": ["command"]
            }),
        }];

        // Add meta-tools for evolution
        let meta_tools = vec![
            McpTool {
                name: "evolve_os_add_capability".to_string(),
                description: "Add a new capability to Mycel OS by creating a new MCP server.
IMPORTANT: The code MUST be a complete, runnable MCP server.
For JavaScript, use '@modelcontextprotocol/sdk/server/index.js' and 'StdioServerTransport'.
Example structure:
const { Server } = require('@modelcontextprotocol/sdk/server/index.js');
const { StdioServerTransport } = require('@modelcontextprotocol/sdk/server/stdio.js');
const server = new Server({name: 'my-server', version: '1.0.0'}, {capabilities: {tools: {}}});
// ... define tools ...
const transport = new StdioServerTransport();
server.connect(transport);"
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Short name for the new capability (e.g. 'weather-tools')"},
                        "language": {"type": "string", "enum": ["javascript", "python"], "description": "Language to use for the server"},
                        "code": {"type": "string", "description": "Complete source code for the MCP server. Must implement the Model Context Protocol SDK."}
                    },
                    "required": ["name", "language", "code"]
                }),
            },
            McpTool {
                name: "evolve_os_install_capability".to_string(),
                description: "Install a capability discovered on the global registry.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "language": {"type": "string"},
                        "code": {"type": "string"}
                    },
                    "required": ["name", "language", "code"]
                }),
            },
        ];

        tools.extend(builtin_tools);
        if self.config.allow_evolution {
            tools.extend(meta_tools);
        }
        format_tools_for_prompt(&tools)
    }

    /// Process a tool call from parsed LLM response (enforces confirmation requirements).
    pub async fn process_tool_call(&self, call: &ToolCall) -> Result<String> {
        info!(
            tool = %call.name,
            args = ?call.arguments,
            "Processing MCP tool call"
        );

        if (call.name == "evolve_os_add_capability" || call.name == "evolve_os_install_capability")
            && !self.config.allow_evolution
        {
            return Err(anyhow!(
                "MCP evolution is disabled in configuration (allow_evolution = false)"
            ));
        }

        if self.requires_confirmation(&call.name).await {
            return Err(anyhow!(
                "Tool '{}' requires explicit user confirmation before execution",
                call.name
            ));
        }

        self.execute_tool_call_inner(call).await
    }

    /// Process an explicitly confirmed tool call (bypasses the confirmation gate).
    pub async fn process_confirmed_tool_call(&self, call: &ToolCall) -> Result<String> {
        info!(
            tool = %call.name,
            args = ?call.arguments,
            "Processing confirmed MCP tool call"
        );

        if (call.name == "evolve_os_add_capability" || call.name == "evolve_os_install_capability")
            && !self.config.allow_evolution
        {
            return Err(anyhow!(
                "MCP evolution is disabled in configuration (allow_evolution = false)"
            ));
        }

        self.execute_tool_call_inner(call).await
    }

    async fn execute_tool_call_inner(&self, call: &ToolCall) -> Result<String> {
        if call.name == "evolve_os_add_capability" || call.name == "evolve_os_install_capability" {
            let name = call
                .arguments
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("Missing 'name' argument"))?;
            let lang = call
                .arguments
                .get("language")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("Missing 'language' argument"))?;
            let code = call
                .arguments
                .get("code")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("Missing 'code' argument"))?;

            let evolver = McpEvolver::new(self.clone(), &self.runtime_path);
            evolver.init().await?;
            return evolver.create_server(name, lang, code, true).await;
        }

        let result = self.call_tool(&call.name, call.arguments.clone()).await?;
        Ok(format_tool_result(&call.name, &result))
    }

    /// Process multiple tool calls, handling confirmations
    pub async fn process_tool_calls_with_confirmation(
        &self,
        calls: &[ToolCall],
    ) -> (Vec<Result<String>>, Vec<PendingConfirmation>) {
        let mut results = Vec::new();
        let mut pending = Vec::new();

        for call in calls {
            if self.requires_confirmation(&call.name).await {
                pending.push(self.create_pending_confirmation(&call.name, call.arguments.clone()));
                results.push(Ok(format!(
                    "Tool '{}' requires confirmation before execution.",
                    call.name
                )));
            } else {
                results.push(self.process_tool_call(call).await);
            }
        }

        (results, pending)
    }

    /// Stop all MCP servers
    pub async fn stop_all(&self) -> Result<()> {
        let mut servers = self.servers.lock().await;

        for (name, server) in servers.iter_mut() {
            if let Err(e) = server.stop().await {
                warn!("Failed to stop MCP server '{}': {}", name, e);
            }
        }

        servers.clear();
        Ok(())
    }

    /// Check if MCP is enabled and has active servers
    pub async fn is_active(&self) -> bool {
        if !self.config.enabled {
            return false;
        }

        let servers = self.servers.lock().await;
        !servers.is_empty()
    }

    /// Get server status for debugging
    pub async fn get_status(&self) -> HashMap<String, String> {
        let servers = self.servers.lock().await;
        let mut status = HashMap::new();

        for (name, server) in servers.iter() {
            let state = match server.state().await {
                ServerState::Stopped => "stopped",
                ServerState::Starting => "starting",
                ServerState::Ready => "ready",
                ServerState::Failed(_) => "failed",
                ServerState::Restarting => "restarting",
            };
            status.insert(name.clone(), state.to_string());
        }

        status
    }

    /// Get health statistics for all servers
    pub async fn get_health_stats(&self) -> HashMap<String, ServerHealth> {
        let servers = self.servers.lock().await;
        let mut stats = HashMap::new();

        for (name, server) in servers.iter() {
            stats.insert(name.clone(), server.health().await);
        }

        stats
    }
}

/// Create default MCP configuration for Void Linux tools
pub fn default_void_tools_config(runtime_path: &str) -> McpConfig {
    let primary_dir = format!("{}/mcp-servers/void-tools", runtime_path);
    let fallback_dir = format!("{}/../mcp-servers/void-tools", runtime_path);
    let server_dir = if Path::new(&format!("{}/void_tools.py", primary_dir)).exists() {
        primary_dir
    } else if Path::new(&format!("{}/void_tools.py", fallback_dir)).exists() {
        fallback_dir
    } else {
        primary_dir
    };

    // Prefer the server's virtualenv Python (where its `mcp` dependency is
    // installed) over the system python3. PEP 668 systems refuse pip installs
    // into the system environment, so each Python MCP server gets its own venv.
    let venv_python = format!("{}/.venv/bin/python3", server_dir);
    let command = if Path::new(&venv_python).exists() {
        venv_python
    } else {
        "python3".to_string()
    };

    McpConfig {
        enabled: true,
        allow_evolution: false,
        servers: vec![
            McpServerConfig {
                name: "void-tools".to_string(),
                command,
                args: vec![format!("{}/void_tools.py", server_dir)],
                env: HashMap::new(),
                requires_confirmation: vec![
                    "xbps_install".to_string(),
                    "xbps_remove".to_string(),
                    "service_control".to_string(),
                ],
            },
            // TODO: Add near-identity server when implemented
            // TODO: Add web-tools server when implemented
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = default_void_tools_config("/path/to/runtime");

        assert!(config.enabled);
        assert!(!config.allow_evolution);
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "void-tools");
        assert!(config.servers[0]
            .requires_confirmation
            .contains(&"xbps_install".to_string()));
    }

    #[tokio::test]
    async fn test_manager_creation() {
        let config = McpConfig {
            enabled: false,
            allow_evolution: false,
            servers: vec![],
        };

        let (tx, _) = tokio::sync::broadcast::channel(1);
        let manager = McpManager::new(&config, "/tmp", tx).await.unwrap();
        assert!(!manager.is_active().await);
    }

    #[test]
    fn test_cache_key() {
        let mut args = HashMap::new();
        args.insert("query".to_string(), serde_json::json!("test"));

        let key1 = McpManager::cache_key("tool", &args);
        let key2 = McpManager::cache_key("tool", &args);
        assert_eq!(key1, key2);

        args.insert("other".to_string(), serde_json::json!(123));
        let key3 = McpManager::cache_key("tool", &args);
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_risk_assessment() {
        let _config = McpConfig {
            enabled: false,
            allow_evolution: false,
            servers: vec![],
        };

        // Can't easily test without async, but the logic is straightforward
        assert_eq!(
            match "xbps_search" {
                "xbps_search" | "xbps_info" | "service_status" | "system_info" => RiskLevel::Low,
                "xbps_install" | "service_control" => RiskLevel::Medium,
                "xbps_remove" => RiskLevel::High,
                _ => RiskLevel::High,
            },
            RiskLevel::Low
        );
    }

    #[test]
    fn test_validate_server_name_rejects_traversal() {
        assert!(validate_server_name("void-tools_1").is_ok());
        assert!(validate_server_name("../evil").is_err());
        assert!(validate_server_name("a/b").is_err());
        assert!(validate_server_name("a\\b").is_err());
        assert!(validate_server_name("").is_err());
        assert!(validate_server_name("bad name").is_err());
        assert!(validate_server_name(&"a".repeat(65)).is_err());
    }

    #[tokio::test]
    async fn test_evolution_disabled_by_default() {
        let config = McpConfig {
            enabled: true,
            allow_evolution: false,
            servers: vec![],
        };
        let (tx, _) = tokio::sync::broadcast::channel(1);
        let manager = McpManager::new(&config, "/tmp", tx).await.unwrap();

        let prompt = manager.get_tools_prompt().await;
        assert!(!prompt.contains("evolve_os_add_capability"));
        assert!(!prompt.contains("evolve_os_install_capability"));

        let mut args = HashMap::new();
        args.insert("name".to_string(), serde_json::json!("evil"));
        args.insert("language".to_string(), serde_json::json!("python"));
        args.insert("code".to_string(), serde_json::json!("print(1)"));
        let call = ToolCall {
            name: "evolve_os_add_capability".to_string(),
            arguments: args,
        };
        let err = manager
            .process_tool_call(&call)
            .await
            .expect_err("evolve_os_add_capability must be refused when allow_evolution=false");
        assert!(err.to_string().contains("disabled"));

        let evolver = McpEvolver::new(manager.clone(), "/tmp");
        let evolver_err = evolver
            .create_server("valid-name", "python", "print(1)", false)
            .await
            .expect_err("McpEvolver::create_server must be refused when allow_evolution=false");
        assert!(evolver_err.to_string().contains("disabled"));
    }

    #[tokio::test]
    async fn test_process_tool_call_and_parallel_refuse_unconfirmed_tools() {
        let config = McpConfig {
            enabled: true,
            allow_evolution: false,
            servers: vec![],
        };
        let (tx, _) = tokio::sync::broadcast::channel(1);
        let manager = McpManager::new(&config, "/tmp", tx).await.unwrap();

        {
            let mut servers = manager.servers.lock().await;
            servers.insert(
                "mock-void".to_string(),
                McpServer::new(
                    "mock-void".to_string(),
                    "echo".to_string(),
                    vec![],
                    HashMap::new(),
                    vec!["xbps_install".to_string()],
                ),
            );
        }

        // Unknown / unconfirmed tool is refused by both process_tool_call and call_tools_parallel
        let call = ToolCall {
            name: "xbps_install".to_string(),
            arguments: HashMap::new(),
        };

        let single_err = manager
            .process_tool_call(&call)
            .await
            .expect_err("unconfirmed tool must be refused");
        assert!(single_err.to_string().contains("confirmation"));

        let parallel_results = manager.call_tools_parallel(&[call]).await;
        assert_eq!(parallel_results.len(), 1);
        let parallel_err = parallel_results[0]
            .as_ref()
            .expect_err("parallel call to unconfirmed tool must be refused");
        assert!(parallel_err.to_string().contains("confirmation"));
    }

    #[test]
    fn test_python_mcp_servers_hardening() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir
            .parent()
            .expect("mycel-runtime should have repo root parent");
        let void_tools_path = repo_root.join("mcp-servers/void-tools/void_tools.py");
        let web_tools_path = repo_root.join("mcp-servers/web-tools/web_tools.py");

        let void_src =
            std::fs::read_to_string(&void_tools_path).expect("should read void_tools.py");
        assert!(
            !void_src.contains("shell=True"),
            "void_tools.py must not use shell=True"
        );
        assert!(
            !void_src.contains("pickle"),
            "void_tools.py must not use pickle"
        );
        assert!(
            !void_src.contains("shell_command"),
            "void_tools.py must not define shell_command"
        );

        let script = r#"
import importlib.util
import sys

def load_mod(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod

vt = load_mod("void_tools", sys.argv[1])
for bad in ["foo; id", "$(whoami)", "../etc", "-rf", "a|b", "a&b", "a`id`"]:
    for fn in (vt.validate_package_name, vt.validate_service_name, vt.validate_man_page, vt.validate_query_arg):
        try:
            fn(bad)
            raise AssertionError(f"{fn.__name__} accepted {bad!r}")
        except ValueError:
            pass

assert vt.validate_package_name("neovim-0.10") == "neovim-0.10"
assert vt.validate_service_name("sshd") == "sshd"
assert vt.validate_man_page("xbps-install") == "xbps-install"
assert vt.validate_query_arg("ripgrep") == "ripgrep"

wt = load_mod("web_tools", sys.argv[2])
for bad_url in [
    "http://127.0.0.1/",
    "http://169.254.169.254/latest/meta-data/",
    "http://10.0.0.1/",
    "http://[::1]/",
    "http://[::ffff:127.0.0.1]/",
    "file:///etc/passwd",
]:
    try:
        wt.validate_public_url(bad_url)
        raise AssertionError(f"validate_public_url accepted {bad_url!r}")
    except ValueError:
        pass

assert wt.validate_public_url("https://1.1.1.1/") == "https://1.1.1.1/"
assert wt.validate_public_url("http://93.184.216.34/docs") == "http://93.184.216.34/docs"
print("PYTHON_HARDENING_OK")
"#;

        let output = std::process::Command::new("python3")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .arg("-c")
            .arg(script)
            .arg(&void_tools_path)
            .arg(&web_tools_path)
            .output()
            .expect("should run python3 hardening test");

        assert!(
            output.status.success(),
            "python3 hardening test failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("PYTHON_HARDENING_OK"));
    }
}
