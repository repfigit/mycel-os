//! MCP Client - STDIO communication with MCP servers
//!
//! Manages subprocess lifecycle and JSON-RPC communication over stdin/stdout.
//! Includes health monitoring, auto-restart, and configurable timeouts.

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tracing::{debug, error, info, warn};

use super::protocol::*;

/// MCP Server connection state
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    /// Server not started
    Stopped,
    /// Server starting up
    Starting,
    /// Server initialized and ready
    Ready,
    /// Server failed to start or crashed
    Failed(String),
    /// Server is restarting after a failure
    Restarting,
}

/// Server health statistics
#[derive(Debug, Clone, Default)]
pub struct ServerHealth {
    /// Number of successful requests
    pub requests_success: u64,
    /// Number of failed requests
    pub requests_failed: u64,
    /// Number of times the server has been restarted
    pub restart_count: u64,
    /// Last successful request time
    pub last_success: Option<Instant>,
    /// Last error message
    pub last_error: Option<String>,
    /// Average response time in milliseconds
    pub avg_response_ms: f64,
}

/// Configuration for an MCP server
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Timeout for individual tool calls (default: 30s)
    pub tool_timeout: Duration,
    /// Timeout for initialization (default: 60s)
    pub init_timeout: Duration,
    /// Maximum number of auto-restart attempts (default: 3)
    pub max_restart_attempts: usize,
    /// Delay between restart attempts (default: 1s)
    pub restart_delay: Duration,
    /// Enable automatic health checks (default: true)
    #[allow(dead_code)]
    pub health_check_enabled: bool,
    /// Interval between health checks (default: 60s)
    #[allow(dead_code)]
    pub health_check_interval: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            tool_timeout: Duration::from_secs(30),
            init_timeout: Duration::from_secs(60),
            max_restart_attempts: 3,
            restart_delay: Duration::from_secs(1),
            health_check_enabled: true,
            health_check_interval: Duration::from_secs(60),
        }
    }
}

/// MCP Server instance
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub requires_confirmation: Vec<String>,
    pub config: ServerConfig,
    state: Arc<RwLock<ServerState>>,
    process: Arc<Mutex<Option<Child>>>,
    request_tx: Arc<Mutex<Option<mpsc::Sender<(JsonRpcRequest, oneshot::Sender<Result<JsonRpcResponse>>)>>>>,
    next_id: AtomicU64,
    tools: Arc<RwLock<Vec<McpTool>>>,
    server_info: Arc<RwLock<Option<ServerInfo>>>,
    health: Arc<RwLock<ServerHealth>>,
    restart_attempts: AtomicUsize,
}

impl McpServer {
    /// Create a new MCP server instance (not yet started)
    pub fn new(
        name: String,
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        requires_confirmation: Vec<String>,
    ) -> Self {
        Self::with_config(name, command, args, env, requires_confirmation, ServerConfig::default())
    }

    /// Create a new MCP server instance with custom configuration
    pub fn with_config(
        name: String,
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        requires_confirmation: Vec<String>,
        config: ServerConfig,
    ) -> Self {
        Self {
            name,
            command,
            args,
            env,
            requires_confirmation,
            config,
            state: Arc::new(RwLock::new(ServerState::Stopped)),
            process: Arc::new(Mutex::new(None)),
            request_tx: Arc::new(Mutex::new(None)),
            next_id: AtomicU64::new(1),
            tools: Arc::new(RwLock::new(Vec::new())),
            server_info: Arc::new(RwLock::new(None)),
            health: Arc::new(RwLock::new(ServerHealth::default())),
            restart_attempts: AtomicUsize::new(0),
        }
    }

    /// Start the MCP server process
    pub async fn start(&mut self) -> Result<()> {
        {
            let state = self.state.read().await;
            if *state == ServerState::Ready {
                return Ok(());
            }
        }

        *self.state.write().await = ServerState::Starting;
        info!("Starting MCP server: {} ({} {})", self.name, self.command, self.args.join(" "));

        // Spawn the subprocess
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // Add environment variables
        for (key, value) in &self.env {
            cmd.env(key, value);
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                let err_msg = format!("Failed to start MCP server '{}': {}", self.name, e);
                *self.state.write().await = ServerState::Failed(err_msg.clone());
                self.health.write().await.last_error = Some(err_msg.clone());
                return Err(anyhow!(err_msg));
            }
        };

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("Failed to get stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("Failed to get stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("Failed to get stderr"))?;

        // Create channels for request/response coordination
        let (request_tx, mut request_rx) = mpsc::channel::<(JsonRpcRequest, oneshot::Sender<Result<JsonRpcResponse>>)>(32);

        // Pending requests map
        let pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<Result<JsonRpcResponse>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Track if stdout reader is alive (for health monitoring)
        let reader_alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let reader_alive_clone = reader_alive.clone();

        // Spawn stdout reader task
        let pending_clone = pending.clone();
        let server_name = self.name.clone();
        let state_clone = self.state.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                debug!("[{}] <- {}", server_name, line);
                match serde_json::from_str::<JsonRpcResponse>(&line) {
                    Ok(response) => {
                        let mut pending = pending_clone.lock().await;
                        if let Some(sender) = pending.remove(&response.id) {
                            let _ = sender.send(Ok(response));
                        }
                    }
                    Err(e) => {
                        warn!("[{}] Failed to parse response: {} - {}", server_name, e, line);
                    }
                }
            }
            debug!("[{}] stdout reader exited", server_name);
            reader_alive_clone.store(false, Ordering::SeqCst);
            // Mark server as failed when reader exits unexpectedly
            let current_state = state_clone.read().await.clone();
            if current_state == ServerState::Ready {
                *state_clone.write().await = ServerState::Failed("Server process exited".to_string());
            }
        });

        // Spawn stderr reader task (log and capture errors)
        let server_name_err = self.name.clone();
        let health_clone = self.health.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                debug!("[{}] stderr: {}", server_name_err, line);
                // Capture last error for health reporting
                if line.to_lowercase().contains("error") || line.to_lowercase().contains("exception") {
                    health_clone.write().await.last_error = Some(line);
                }
            }
        });

        // Spawn writer task
        let pending_clone = pending.clone();
        let server_name_write = self.name.clone();
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some((request, response_sender)) = request_rx.recv().await {
                let id = request.id.clone();
                match serde_json::to_string(&request) {
                    Ok(json) => {
                        debug!("[{}] -> {}", server_name_write, json);
                        // Store pending request
                        pending_clone.lock().await.insert(id.clone(), response_sender);
                        // Write to stdin
                        if let Err(e) = stdin.write_all(format!("{}\n", json).as_bytes()).await {
                            error!("[{}] Write error: {}", server_name_write, e);
                            pending_clone.lock().await.remove(&id);
                        }
                        if let Err(e) = stdin.flush().await {
                            error!("[{}] Flush error: {}", server_name_write, e);
                        }
                    }
                    Err(e) => {
                        let _ = response_sender.send(Err(anyhow!("Failed to serialize request: {}", e)));
                    }
                }
            }
            debug!("[{}] writer task exited", server_name_write);
        });

        *self.request_tx.lock().await = Some(request_tx);
        *self.process.lock().await = Some(child);

        // Initialize the server with timeout
        match tokio::time::timeout(self.config.init_timeout, self.initialize()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let err_msg = format!("Initialize failed: {}", e);
                *self.state.write().await = ServerState::Failed(err_msg.clone());
                self.health.write().await.last_error = Some(err_msg.clone());
                return Err(anyhow!(err_msg));
            }
            Err(_) => {
                let err_msg = "Initialize timed out".to_string();
                *self.state.write().await = ServerState::Failed(err_msg.clone());
                self.health.write().await.last_error = Some(err_msg.clone());
                return Err(anyhow!(err_msg));
            }
        }

        // List available tools
        self.refresh_tools().await?;

        *self.state.write().await = ServerState::Ready;
        self.restart_attempts.store(0, Ordering::SeqCst); // Reset restart count on successful start

        let tool_count = self.tools.read().await.len();
        info!("MCP server '{}' is ready with {} tools", self.name, tool_count);

        Ok(())
    }

    /// Send a JSON-RPC request and wait for response with configured timeout
    async fn send_request(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        self.send_request_with_timeout(request, self.config.tool_timeout).await
    }

    /// Send a JSON-RPC request with custom timeout
    async fn send_request_with_timeout(&self, request: JsonRpcRequest, timeout: Duration) -> Result<JsonRpcResponse> {
        let tx_guard = self.request_tx.lock().await;
        let tx = tx_guard.as_ref()
            .ok_or_else(|| anyhow!("Server not started"))?;

        let (response_tx, response_rx) = oneshot::channel();
        let start = Instant::now();

        tx.send((request, response_tx)).await
            .map_err(|_| anyhow!("Failed to send request - server may have crashed"))?;

        let result = tokio::time::timeout(timeout, response_rx)
            .await
            .map_err(|_| anyhow!("Request timed out after {:?}", timeout))?
            .map_err(|_| anyhow!("Response channel closed - server may have crashed"))?;

        // Update health stats
        let elapsed = start.elapsed();
        let mut health = self.health.write().await;
        match &result {
            Ok(_) => {
                health.requests_success += 1;
                health.last_success = Some(Instant::now());
                // Update moving average
                let total = health.requests_success + health.requests_failed;
                health.avg_response_ms = (health.avg_response_ms * (total - 1) as f64 + elapsed.as_millis() as f64) / total as f64;
            }
            Err(e) => {
                health.requests_failed += 1;
                health.last_error = Some(e.to_string());
            }
        }

        result
    }

    /// Initialize the MCP server
    async fn initialize(&self) -> Result<()> {
        let params = InitializeParams {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo {
                name: "mycel-runtime".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };

        let request = JsonRpcRequest::new(
            self.next_id.fetch_add(1, Ordering::SeqCst),
            "initialize",
            Some(serde_json::to_value(params)?),
        );

        let response = self.send_request_with_timeout(request, self.config.init_timeout).await?;

        if let Some(error) = response.error {
            return Err(anyhow!("Initialize failed: {}", error.message));
        }

        if let Some(result) = response.result {
            let init_result: InitializeResult = serde_json::from_value(result)?;
            *self.server_info.write().await = Some(init_result.server_info);
        }

        // Send initialized notification
        let notification = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(0),
            method: "notifications/initialized".to_string(),
            params: None,
        };

        // For notifications, we don't wait for response
        if let Some(tx) = &*self.request_tx.lock().await {
            let (response_tx, _) = oneshot::channel();
            let _ = tx.send((notification, response_tx)).await;
        }

        Ok(())
    }

    /// Refresh the list of available tools
    pub async fn refresh_tools(&self) -> Result<()> {
        let request = JsonRpcRequest::new(
            self.next_id.fetch_add(1, Ordering::SeqCst),
            "tools/list",
            None,
        );

        let response = self.send_request(request).await?;

        if let Some(error) = response.error {
            return Err(anyhow!("tools/list failed: {}", error.message));
        }

        if let Some(result) = response.result {
            let list_result: ListToolsResult = serde_json::from_value(result)?;
            *self.tools.write().await = list_result.tools;
        }

        Ok(())
    }

    /// Get the list of available tools
    pub async fn get_tools(&self) -> Vec<McpTool> {
        self.tools.read().await.clone()
    }

    /// Call a tool with configured timeout
    pub async fn call_tool(&self, name: &str, arguments: HashMap<String, serde_json::Value>) -> Result<CallToolResult> {
        self.call_tool_with_timeout(name, arguments, self.config.tool_timeout).await
    }

    /// Call a tool with custom timeout
    pub async fn call_tool_with_timeout(
        &self,
        name: &str,
        arguments: HashMap<String, serde_json::Value>,
        timeout: Duration,
    ) -> Result<CallToolResult> {
        let params = CallToolParams {
            name: name.to_string(),
            arguments,
        };

        let request = JsonRpcRequest::new(
            self.next_id.fetch_add(1, Ordering::SeqCst),
            "tools/call",
            Some(serde_json::to_value(params)?),
        );

        let response = self.send_request_with_timeout(request, timeout).await?;

        if let Some(error) = response.error {
            return Err(anyhow!("Tool call failed: {}", error.message));
        }

        let result = response.result
            .ok_or_else(|| anyhow!("Empty result from tool call"))?;

        serde_json::from_value(result).map_err(|e| anyhow!("Failed to parse tool result: {}", e))
    }

    /// Check if a tool requires user confirmation
    pub fn requires_confirmation(&self, tool_name: &str) -> bool {
        self.requires_confirmation.contains(&tool_name.to_string())
    }

    /// Get the current state
    pub async fn state(&self) -> ServerState {
        self.state.read().await.clone()
    }

    /// Get health statistics
    pub async fn health(&self) -> ServerHealth {
        self.health.read().await.clone()
    }

    /// Perform a health check by sending a tools/list request
    pub async fn health_check(&self) -> bool {
        let state = self.state.read().await.clone();
        if state != ServerState::Ready {
            return false;
        }

        // Try to list tools as a health check
        match tokio::time::timeout(Duration::from_secs(5), self.refresh_tools()).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                warn!("[{}] Health check failed: {}", self.name, e);
                false
            }
            Err(_) => {
                warn!("[{}] Health check timed out", self.name);
                false
            }
        }
    }

    /// Restart the server if it's unhealthy
    pub async fn restart_if_needed(&mut self) -> Result<bool> {
        if self.health_check().await {
            return Ok(false); // No restart needed
        }

        let attempts = self.restart_attempts.load(Ordering::SeqCst);
        if attempts >= self.config.max_restart_attempts {
            warn!("[{}] Max restart attempts ({}) reached", self.name, self.config.max_restart_attempts);
            return Err(anyhow!("Max restart attempts reached"));
        }

        info!("[{}] Restarting server (attempt {}/{})", self.name, attempts + 1, self.config.max_restart_attempts);
        *self.state.write().await = ServerState::Restarting;
        self.restart_attempts.fetch_add(1, Ordering::SeqCst);
        self.health.write().await.restart_count += 1;

        // Stop existing process
        self.stop().await?;

        // Wait before restarting
        tokio::time::sleep(self.config.restart_delay).await;

        // Try to start again
        self.start().await?;

        Ok(true)
    }

    /// Stop the server
    pub async fn stop(&mut self) -> Result<()> {
        // Clear request channel first
        *self.request_tx.lock().await = None;

        // Kill process
        if let Some(mut process) = self.process.lock().await.take() {
            let _ = process.kill().await;
        }

        *self.state.write().await = ServerState::Stopped;
        Ok(())
    }

    /// Check if the server process is still running
    #[allow(dead_code)]
    pub async fn is_process_alive(&self) -> bool {
        if let Some(child) = &*self.process.lock().await {
            // Try to get process ID - if it exists, process is likely still running
            child.id().is_some()
        } else {
            false
        }
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        // Process will be killed automatically due to kill_on_drop(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_state() {
        let server = McpServer::new(
            "test".to_string(),
            "echo".to_string(),
            vec![],
            HashMap::new(),
            vec!["dangerous_tool".to_string()],
        );

        assert!(server.requires_confirmation("dangerous_tool"));
        assert!(!server.requires_confirmation("safe_tool"));
    }

    #[test]
    fn test_server_config_defaults() {
        let config = ServerConfig::default();
        assert_eq!(config.tool_timeout, Duration::from_secs(30));
        assert_eq!(config.max_restart_attempts, 3);
        assert!(config.health_check_enabled);
    }

    #[test]
    fn test_custom_config() {
        let config = ServerConfig {
            tool_timeout: Duration::from_secs(60),
            init_timeout: Duration::from_secs(120),
            max_restart_attempts: 5,
            restart_delay: Duration::from_secs(2),
            health_check_enabled: false,
            health_check_interval: Duration::from_secs(30),
        };

        let server = McpServer::with_config(
            "test".to_string(),
            "echo".to_string(),
            vec![],
            HashMap::new(),
            vec![],
            config.clone(),
        );

        assert_eq!(server.config.tool_timeout, Duration::from_secs(60));
        assert_eq!(server.config.max_restart_attempts, 5);
    }
}
