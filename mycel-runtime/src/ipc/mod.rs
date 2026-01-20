//! IPC - Inter-process communication for Mycel Runtime
//!
//! Allows the UI compositor and other components to communicate
//! with the runtime daemon.
//!
//! Security features:
//! - Socket permissions set to 0600 (owner only)
//! - Token-based authentication required
//! - Rate limiting per connection (100 req/min)
//! - Message size limit (1MB)

#![allow(dead_code)]

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::MycelRuntime;

/// Maximum message size in bytes (1MB)
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// Rate limit: requests per minute
const RATE_LIMIT_REQUESTS: u32 = 100;

/// Rate limit window duration
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// Rate limiter for a connection
struct RateLimiter {
    requests: Vec<Instant>,
    max_requests: u32,
    window: Duration,
}

impl RateLimiter {
    fn new(max_requests: u32, window: Duration) -> Self {
        Self {
            requests: Vec::new(),
            max_requests,
            window,
        }
    }

    fn check(&mut self) -> bool {
        let now = Instant::now();
        // Remove old requests outside the window
        self.requests
            .retain(|t| now.duration_since(*t) < self.window);

        if self.requests.len() >= self.max_requests as usize {
            false
        } else {
            self.requests.push(now);
            true
        }
    }
}

/// IPC Server for Mycel Runtime
pub struct IpcServer {
    listener: UnixListener,
    runtime: Arc<MycelRuntime>,
    auth_token: String,
}

impl IpcServer {
    pub async fn new(runtime: &MycelRuntime) -> Result<Self> {
        let socket_path = &runtime.config.ipc_socket_path;

        // Remove existing socket if present
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path)?;

        // Set socket permissions to 0600 (owner read/write only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(socket_path, permissions)?;
            info!("IPC socket permissions set to 0600");
        }

        // Generate authentication token
        let auth_token = uuid::Uuid::new_v4().to_string();
        info!("IPC server listening on {}", socket_path);
        info!("IPC auth token: {}", auth_token);

        Ok(Self {
            listener,
            runtime: Arc::new(runtime.clone()),
            auth_token,
        })
    }

    /// Get the authentication token (for clients)
    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    pub async fn run(&self) -> Result<()> {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    let runtime = Arc::clone(&self.runtime);
                    let auth_token = self.auth_token.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, runtime, auth_token).await {
                            error!("Connection handler error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("Accept error: {}", e);
                }
            }
        }
    }
}

async fn handle_connection(
    stream: UnixStream,
    runtime: Arc<MycelRuntime>,
    expected_token: String,
) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let writer = Arc::new(Mutex::new(writer));

    let mut session_id = uuid::Uuid::new_v4().to_string();
    let mut authenticated = false;
    let mut rate_limiter = RateLimiter::new(RATE_LIMIT_REQUESTS, RATE_LIMIT_WINDOW);

    debug!("New IPC connection, session: {}", session_id);

    let mut line = String::new();
    loop {
        line.clear();

        // Read with size limit check
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(n) => {
                // Check message size limit
                if n > MAX_MESSAGE_SIZE {
                    warn!("Message exceeds size limit ({} bytes)", n);
                    let error_response = IpcResponse::Error {
                        message: format!(
                            "Message too large: {} bytes (max: {} bytes)",
                            n, MAX_MESSAGE_SIZE
                        ),
                    };
                    let response_json = serde_json::to_string(&error_response)? + "\n";
                    let mut w = writer.lock().await;
                    w.write_all(response_json.as_bytes()).await?;
                    w.flush().await?;
                    continue;
                }

                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                // Check rate limit
                if !rate_limiter.check() {
                    warn!("Rate limit exceeded for session {}", session_id);
                    let error_response = IpcResponse::Error {
                        message: format!(
                            "Rate limit exceeded: max {} requests per minute",
                            RATE_LIMIT_REQUESTS
                        ),
                    };
                    let response_json = serde_json::to_string(&error_response)? + "\n";
                    let mut w = writer.lock().await;
                    w.write_all(response_json.as_bytes()).await?;
                    w.flush().await?;
                    continue;
                }

                match serde_json::from_str::<IpcRequest>(trimmed) {
                    Ok(request) => {
                        // Check if authentication is required
                        if !authenticated {
                            match &request {
                                IpcRequest::Authenticate { token } => {
                                    if token == &expected_token {
                                        authenticated = true;
                                        let response = IpcResponse::Ok {
                                            message: "Authenticated successfully".to_string(),
                                        };
                                        let response_json =
                                            serde_json::to_string(&response)? + "\n";
                                        let mut w = writer.lock().await;
                                        w.write_all(response_json.as_bytes()).await?;
                                        w.flush().await?;
                                        info!("Client authenticated for session {}", session_id);
                                    } else {
                                        warn!("Invalid auth token for session {}", session_id);
                                        let error_response = IpcResponse::Error {
                                            message: "Invalid authentication token".to_string(),
                                        };
                                        let response_json =
                                            serde_json::to_string(&error_response)? + "\n";
                                        let mut w = writer.lock().await;
                                        w.write_all(response_json.as_bytes()).await?;
                                        w.flush().await?;
                                    }
                                    continue;
                                }
                                IpcRequest::Ping => {
                                    // Allow Ping without auth for health checks
                                    let response_json =
                                        serde_json::to_string(&IpcResponse::Pong)? + "\n";
                                    let mut w = writer.lock().await;
                                    w.write_all(response_json.as_bytes()).await?;
                                    w.flush().await?;
                                    continue;
                                }
                                _ => {
                                    let error_response = IpcResponse::Error {
                                        message: "Authentication required. Send Authenticate request first.".to_string(),
                                    };
                                    let response_json =
                                        serde_json::to_string(&error_response)? + "\n";
                                    let mut w = writer.lock().await;
                                    w.write_all(response_json.as_bytes()).await?;
                                    w.flush().await?;
                                    continue;
                                }
                            }
                        }

                        let response = process_request(&request, &runtime, &mut session_id).await;
                        let response_json = serde_json::to_string(&response)? + "\n";

                        let mut w = writer.lock().await;
                        w.write_all(response_json.as_bytes()).await?;
                        w.flush().await?;
                    }
                    Err(e) => {
                        let error_response = IpcResponse::Error {
                            message: format!("Invalid request: {}", e),
                        };
                        let response_json = serde_json::to_string(&error_response)? + "\n";

                        let mut w = writer.lock().await;
                        w.write_all(response_json.as_bytes()).await?;
                        w.flush().await?;
                    }
                }
            }
            Err(e) => {
                error!("Read error: {}", e);
                break;
            }
        }
    }

    debug!("IPC connection closed, session: {}", session_id);
    Ok(())
}

async fn process_request(
    request: &IpcRequest,
    runtime: &MycelRuntime,
    session_id: &mut String,
) -> IpcResponse {
    match request {
        IpcRequest::Authenticate { .. } => {
            // Already authenticated, ignore
            IpcResponse::Ok {
                message: "Already authenticated".to_string(),
            }
        }
        IpcRequest::Chat { message } => match runtime.process_input(message, session_id).await {
            Ok(response) => match response {
                crate::RuntimeResponse::Text(text) => IpcResponse::Chat {
                    response: text,
                    surface: None,
                },
                crate::RuntimeResponse::CodeResult { code, output } => IpcResponse::CodeResult {
                    code,
                    output,
                    success: true,
                },
                crate::RuntimeResponse::UiSurface(surface) => IpcResponse::Chat {
                    response: format!("Created surface: {}", surface.title),
                    surface: Some(surface),
                },
                crate::RuntimeResponse::Error(err) => IpcResponse::Error { message: err },
            },
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        },
        IpcRequest::SetSession { id } => {
            *session_id = id.clone();
            IpcResponse::Ok {
                message: format!("Session set to {}", id),
            }
        }
        IpcRequest::GetContext => match runtime.context_manager.get_context(session_id).await {
            Ok(ctx) => IpcResponse::Context {
                working_directory: ctx.working_directory,
                recent_files: ctx.recent_files,
            },
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        },
        IpcRequest::Ping => IpcResponse::Pong,
    }
}

/// Requests that can be sent to the runtime
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcRequest {
    /// Authenticate with token (required before other requests)
    Authenticate { token: String },
    /// Send a chat message
    Chat { message: String },
    /// Set the session ID
    SetSession { id: String },
    /// Get current context
    GetContext,
    /// Ping for health check (allowed without auth)
    Ping,
}

/// Responses from the runtime
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcResponse {
    /// Chat response
    Chat {
        response: String,
        surface: Option<crate::ui::Surface>,
    },
    /// Code execution result
    CodeResult {
        code: String,
        output: String,
        success: bool,
    },
    /// Context information
    Context {
        working_directory: String,
        recent_files: Vec<String>,
    },
    /// Generic OK response
    Ok { message: String },
    /// Error response
    Error { message: String },
    /// Pong response to ping
    Pong,
}

/// IPC Client for connecting to Clay Runtime
pub struct IpcClient {
    stream: UnixStream,
}

impl IpcClient {
    pub async fn connect(socket_path: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket_path).await?;
        Ok(Self { stream })
    }

    pub async fn send(&mut self, request: &IpcRequest) -> Result<IpcResponse> {
        let request_json = serde_json::to_string(request)? + "\n";
        self.stream.write_all(request_json.as_bytes()).await?;

        let mut reader = BufReader::new(&mut self.stream);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await?;

        Ok(serde_json::from_str(&response_line)?)
    }

    pub async fn chat(&mut self, message: &str) -> Result<IpcResponse> {
        self.send(&IpcRequest::Chat {
            message: message.to_string(),
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Rate limiter tests

    #[test]
    fn test_rate_limiter_allows_within_limit() {
        let mut limiter = RateLimiter::new(10, Duration::from_secs(60));

        // Should allow 10 requests
        for _ in 0..10 {
            assert!(limiter.check(), "Should allow requests within limit");
        }
    }

    #[test]
    fn test_rate_limiter_blocks_over_limit() {
        let mut limiter = RateLimiter::new(5, Duration::from_secs(60));

        // Use up the limit
        for _ in 0..5 {
            assert!(limiter.check());
        }

        // Should block the 6th request
        assert!(!limiter.check(), "Should block requests over limit");
    }

    #[test]
    fn test_rate_limiter_resets_after_window() {
        let mut limiter = RateLimiter::new(2, Duration::from_millis(10));

        // Use up the limit
        assert!(limiter.check());
        assert!(limiter.check());
        assert!(!limiter.check());

        // Wait for window to pass
        std::thread::sleep(Duration::from_millis(15));

        // Should allow again
        assert!(limiter.check(), "Should allow requests after window resets");
    }

    // Request/Response serialization tests

    #[test]
    fn test_authenticate_request_serialization() {
        let request = IpcRequest::Authenticate {
            token: "test-token".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("Authenticate"));
        assert!(json.contains("test-token"));

        let deserialized: IpcRequest = serde_json::from_str(&json).unwrap();
        match deserialized {
            IpcRequest::Authenticate { token } => assert_eq!(token, "test-token"),
            _ => panic!("Expected Authenticate request"),
        }
    }

    #[test]
    fn test_chat_request_serialization() {
        let request = IpcRequest::Chat {
            message: "Hello, world!".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("Chat"));
        assert!(json.contains("Hello, world!"));
    }

    #[test]
    fn test_ping_request_serialization() {
        let request = IpcRequest::Ping;
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("Ping"));

        let deserialized: IpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, IpcRequest::Ping));
    }

    #[test]
    fn test_error_response_serialization() {
        let response = IpcResponse::Error {
            message: "Something went wrong".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("Error"));
        assert!(json.contains("Something went wrong"));
    }

    #[test]
    fn test_ok_response_serialization() {
        let response = IpcResponse::Ok {
            message: "Success".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("Ok"));
        assert!(json.contains("Success"));
    }

    #[test]
    fn test_pong_response_serialization() {
        let response = IpcResponse::Pong;
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("Pong"));
    }

    // Message size validation tests

    #[test]
    fn test_max_message_size_constant() {
        assert_eq!(MAX_MESSAGE_SIZE, 1024 * 1024); // 1MB
    }

    #[test]
    fn test_rate_limit_constants() {
        assert_eq!(RATE_LIMIT_REQUESTS, 100);
        assert_eq!(RATE_LIMIT_WINDOW, Duration::from_secs(60));
    }

    // Request type coverage tests

    #[test]
    fn test_all_request_types_deserialize() {
        let test_cases = [
            r#"{"type":"Authenticate","token":"abc"}"#,
            r#"{"type":"Chat","message":"hello"}"#,
            r#"{"type":"SetSession","id":"sess-1"}"#,
            r#"{"type":"GetContext"}"#,
            r#"{"type":"Ping"}"#,
        ];

        for json in test_cases {
            let result: Result<IpcRequest, _> = serde_json::from_str(json);
            assert!(result.is_ok(), "Failed to deserialize: {}", json);
        }
    }

    #[test]
    fn test_invalid_request_fails() {
        let invalid_json = r#"{"type":"InvalidType"}"#;
        let result: Result<IpcRequest, _> = serde_json::from_str(invalid_json);
        assert!(result.is_err(), "Should fail on invalid request type");
    }
}
