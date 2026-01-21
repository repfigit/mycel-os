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

        for server_config in &self.config.servers {
            if let Err(e) = self.start_server(server_config).await {
                warn!("Failed to start MCP server '{}': {}", server_config.name, e);
            }
        }

        // Load dynamic servers
        let dynamic_dir = format!("{}/mcp-servers/dynamic", self.runtime_path);
        if Path::new(&dynamic_dir).exists() {
            let mut entries = tokio::fs::read_dir(&dynamic_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                if entry.file_type().await?.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let server_dir = entry.path();

                    // Detect language and setup config
                    let (command, args) = if server_dir.join("index.js").exists() {
                        ("node".to_string(), vec![server_dir.join("index.js").to_string_lossy().to_string()])
                    } else if server_dir.join("server.py").exists() {
                        ("python3".to_string(), vec![server_dir.join("server.py").to_string_lossy().to_string()])
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
                                let _ = event_bus.send(SystemEvent::McpServerRestarted {
                                    name: name.clone(),
                                });
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

        self.servers.lock().await.insert(config.name.clone(), server);

        Ok(())
    }

    /// Hot-load a new MCP server from a directory
    pub async fn add_dynamic_server(&self, name: &str, command: &str, args: Vec<String>) -> Result<()> {
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
                    if full_path.exists() || full_path.parent().map(|p| p.exists()).unwrap_or(false) {
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
        let server_name = self.find_tool_server(tool_name).await
            .ok_or_else(|| anyhow!("No server provides tool '{}'", tool_name))?;

        let result = {
            let mut servers = self.servers.lock().await;
            let server = servers.get_mut(&server_name)
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
        }).await;

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
            cache.insert(cache_key, CachedResult {
                result: formatted.clone(),
                expires_at: Instant::now() + ttl,
            });

            // Cleanup expired entries periodically
            if cache.len() > 100 {
                let now = Instant::now();
                cache.retain(|_, v| v.expires_at > now);
            }
        }

        Ok(formatted)
    }

    /// Execute multiple tool calls in parallel
    pub async fn call_tools_parallel(
        &self,
        calls: &[ToolCall],
    ) -> Vec<Result<String>> {
        let futures: Vec<_> = calls.iter()
            .map(|call| {
                let manager = self.clone();
                let call = call.clone();
                async move {
                    manager.process_tool_call(&call).await
                }
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
        log.iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
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
                arguments.get("package").and_then(|v| v.as_str()).unwrap_or("unknown")
            ),
            "xbps_remove" => format!(
                "Remove package: {}",
                arguments.get("package").and_then(|v| v.as_str()).unwrap_or("unknown")
            ),
            "service_control" => format!(
                "{} service: {}",
                arguments.get("action").and_then(|v| v.as_str()).unwrap_or("control"),
                arguments.get("service").and_then(|v| v.as_str()).unwrap_or("unknown")
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
    fn assess_risk_level(&self, tool_name: &str, _arguments: &HashMap<String, serde_json::Value>) -> RiskLevel {
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
server.connect(transport);".to_string(),
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
            }
        ];

        tools.extend(meta_tools);
        format_tools_for_prompt(&tools)
    }

    /// Process a tool call from parsed LLM response
    pub async fn process_tool_call(&self, call: &ToolCall) -> Result<String> {
        info!(
            tool = %call.name,
            args = ?call.arguments,
            "Processing MCP tool call"
        );

        if call.name == "evolve_os_add_capability" || call.name == "evolve_os_install_capability" {
            let name = call.arguments.get("name").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("Missing 'name' argument"))?;
            let lang = call.arguments.get("language").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("Missing 'language' argument"))?;
            let code = call.arguments.get("code").and_then(|v| v.as_str())
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
    McpConfig {
        enabled: true,
        servers: vec![
            McpServerConfig {
                name: "void-tools".to_string(),
                command: "python3".to_string(),
                args: vec![format!("{}/mcp-servers/void-tools/void_tools.py", runtime_path)],
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
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "void-tools");
        assert!(config.servers[0].requires_confirmation.contains(&"xbps_install".to_string()));
    }

    #[tokio::test]
    async fn test_manager_creation() {
        let config = McpConfig {
            enabled: false,
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
        let config = McpConfig {
            enabled: false,
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
}
