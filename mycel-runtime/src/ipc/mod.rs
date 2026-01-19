//! IPC - Inter-process communication for Clay Runtime
//!
//! Allows the UI compositor and other components to communicate
//! with the runtime daemon.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::MycelRuntime;

/// IPC Server for Clay Runtime
pub struct IpcServer {
    listener: UnixListener,
    runtime: Arc<MycelRuntime>,
}

impl IpcServer {
    pub async fn new(runtime: &MycelRuntime) -> Result<Self> {
        let socket_path = &runtime.config.ipc_socket_path;

        // Remove existing socket if present
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path)?;
        info!("IPC server listening on {}", socket_path);

        Ok(Self {
            listener,
            runtime: Arc::new(runtime.clone()),
        })
    }

    pub async fn run(&self) -> Result<()> {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    let runtime = Arc::clone(&self.runtime);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, runtime).await {
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

async fn handle_connection(stream: UnixStream, runtime: Arc<MycelRuntime>) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let writer = Arc::new(Mutex::new(writer));

    let mut session_id = uuid::Uuid::new_v4().to_string();
    debug!("New IPC connection, session: {}", session_id);

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                match serde_json::from_str::<IpcRequest>(trimmed) {
                    Ok(request) => {
                        let response =
                            process_request(&request, &runtime, &mut session_id).await;
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
        IpcRequest::Chat { message } => {
            match runtime.process_input(message, session_id).await {
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
            }
        }
        IpcRequest::SetSession { id } => {
            *session_id = id.clone();
            IpcResponse::Ok {
                message: format!("Session set to {}", id),
            }
        }
        IpcRequest::GetContext => {
            match runtime.context_manager.get_context(session_id).await {
                Ok(ctx) => IpcResponse::Context {
                    working_directory: ctx.working_directory,
                    recent_files: ctx.recent_files,
                },
                Err(e) => IpcResponse::Error {
                    message: e.to_string(),
                },
            }
        }
        IpcRequest::Ping => IpcResponse::Pong,
    }
}

/// Requests that can be sent to the runtime
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcRequest {
    /// Send a chat message
    Chat { message: String },
    /// Set the session ID
    SetSession { id: String },
    /// Get current context
    GetContext,
    /// Ping for health check
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
