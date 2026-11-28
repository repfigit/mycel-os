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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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

/// A line buffer that keeps its allocation across frames so small messages
/// don't allocate, but releases it after an unusually large frame so one big
/// message does not pin memory for the connection's lifetime.
struct LineBuffer {
    buf: String,
}

/// Capacity kept after a large frame. Anything above this is handed back to
/// the allocator.
const LINE_BUFFER_KEEP_BYTES: usize = 64 * 1024;

impl LineBuffer {
    fn new() -> Self {
        Self {
            buf: String::with_capacity(1024),
        }
    }

    fn as_str(&self) -> &str {
        &self.buf
    }

    /// Reset for the next frame, handing capacity beyond the keep threshold
    /// back to the allocator. A large frame keeps its buffer while it is
    /// processed, but the memory is released as soon as the frame is done
    /// instead of for the lifetime of the connection.
    fn reset(&mut self) {
        self.buf.clear();
        if self.buf.capacity() > LINE_BUFFER_KEEP_BYTES {
            self.buf.shrink_to(LINE_BUFFER_KEEP_BYTES);
        }
    }
}

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

/// Compute the auth token file path corresponding to a Unix socket path.
pub fn token_path_for_socket(socket_path: &str) -> PathBuf {
    Path::new(socket_path).with_extension("token")
}

fn write_token_atomically(token_path: &Path, token: &str) -> Result<()> {
    let parent = match token_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let file_name = token_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("mycel");
    let tmp_path = parent.join(format!(".{}.tmp.{}", file_name, uuid::Uuid::new_v4()));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let write_res = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = opts.open(&tmp_path)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp_path, token_path)?;
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }

    Ok(())
}

/// IPC Server for Mycel Runtime
pub struct IpcServer {
    listener: UnixListener,
    runtime: Arc<MycelRuntime>,
    auth_token: String,
    socket_path: PathBuf,
    token_path: PathBuf,
}

impl IpcServer {
    pub async fn new(runtime: &MycelRuntime) -> Result<Self> {
        let socket_path = PathBuf::from(&runtime.config.ipc_socket_path);
        let token_path = token_path_for_socket(&runtime.config.ipc_socket_path);

        if let Some(parent) = socket_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // Remove existing socket if present
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path)?;

        // Set socket permissions to 0600 (owner read/write only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&socket_path, permissions)?;
            info!("IPC socket permissions set to 0600");
        }

        // Generate authentication token (never log the secret token)
        let auth_token = uuid::Uuid::new_v4().to_string();
        if let Err(e) = write_token_atomically(&token_path, &auth_token) {
            let _ = std::fs::remove_file(&socket_path);
            return Err(e);
        }
        info!("IPC auth token written to {}", token_path.display());
        info!("IPC server listening on {}", socket_path.display());

        Ok(Self {
            listener,
            runtime: Arc::new(runtime.clone()),
            auth_token,
            socket_path,
            token_path,
        })
    }

    /// Get the authentication token (for clients)
    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    /// Get the auth token file path
    pub fn token_path(&self) -> &Path {
        &self.token_path
    }

    /// Remove the auth token file and Unix socket, ignoring NotFound errors.
    pub fn cleanup(&self) {
        for path in [&self.token_path, &self.socket_path] {
            if let Err(e) = std::fs::remove_file(path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!("Failed to remove {}: {}", path.display(), e);
                }
            }
        }
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

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.cleanup();
    }
}

enum BoundedRead {
    Eof,
    /// A line was read into the caller's [`LineBuffer`].
    Line,
    TooLarge(usize),
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut LineBuffer,
    max_size: usize,
) -> std::io::Result<BoundedRead> {
    line.reset();
    let n = reader
        .take((max_size + 1) as u64)
        .read_line(&mut line.buf)
        .await?;
    if n == 0 {
        return Ok(BoundedRead::Eof);
    }
    if n > max_size {
        return Ok(BoundedRead::TooLarge(n));
    }
    Ok(BoundedRead::Line)
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
    let mut line_buf = LineBuffer::new();

    debug!("New IPC connection, session: {}", session_id);

    loop {
        // Read with size limit enforced before allocation
        match read_bounded_line(&mut reader, &mut line_buf, MAX_MESSAGE_SIZE).await {
            Ok(BoundedRead::Eof) => break,
            Ok(BoundedRead::TooLarge(n)) => {
                warn!("Message exceeds size limit ({} bytes)", n);
                let error_response = IpcResponse::Error {
                    message: format!("Message too large: exceeds max {} bytes", MAX_MESSAGE_SIZE),
                };
                let response_json = serde_json::to_string(&error_response)? + "\n";
                let mut w = writer.lock().await;
                w.write_all(response_json.as_bytes()).await?;
                w.flush().await?;
                break;
            }
            Ok(BoundedRead::Line) => {
                let trimmed = line_buf.as_str().trim();
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

                        // Process request
                        match &request {
                            IpcRequest::Chat { message, provider } => {
                                match runtime
                                    .process_input_with_provider(message, &session_id, *provider)
                                    .await
                                {
                                    Ok(crate::RuntimeResponse::Text(text)) => {
                                        // Record the interaction for history and sync
                                        let _ = runtime
                                            .record_interaction(&session_id, message, &text)
                                            .await;

                                        let response = IpcResponse::Chat {
                                            response: text,
                                            surface: None,
                                        };
                                        let json = serde_json::to_string(&response)? + "\n";
                                        let mut w = writer.lock().await;
                                        w.write_all(json.as_bytes()).await?;
                                        w.flush().await?;
                                    }
                                    Ok(crate::RuntimeResponse::Stream(mut stream)) => {
                                        use futures_util::StreamExt;
                                        let mut full_response = String::new();

                                        while let Some(chunk_result) = stream.next().await {
                                            if let Ok(chunk) = chunk_result {
                                                full_response.push_str(&chunk);
                                                let chunk_response =
                                                    IpcResponse::ChatChunk { delta: chunk };
                                                if let Ok(json) =
                                                    serde_json::to_string(&chunk_response)
                                                {
                                                    let mut w = writer.lock().await;
                                                    let _ =
                                                        w.write_all((json + "\n").as_bytes()).await;
                                                    let _ = w.flush().await;
                                                }
                                            }
                                        }

                                        // If the model requested shell execution
                                        // (#!exec marker), route through the
                                        // policy-gated executor instead of
                                        // returning the marker as text.
                                        if full_response.starts_with("#!exec\n")
                                            || full_response.starts_with("#!exec ")
                                        {
                                            let code =
                                                full_response.trim_start_matches("#!exec").trim();
                                            match runtime
                                                .execute_code_with_policy(code, &session_id)
                                                .await
                                            {
                                                Ok(crate::RuntimeResponse::Text(text)) => {
                                                    let _ = runtime
                                                        .record_interaction(
                                                            &session_id,
                                                            message,
                                                            &text,
                                                        )
                                                        .await;
                                                    let resp = IpcResponse::Chat {
                                                        response: text,
                                                        surface: None,
                                                    };
                                                    let json = serde_json::to_string(&resp)? + "\n";
                                                    let mut w = writer.lock().await;
                                                    w.write_all(json.as_bytes()).await?;
                                                    w.flush().await?;
                                                }
                                                Ok(crate::RuntimeResponse::Stream(_)) => {
                                                    // Executor doesn't return streams
                                                }
                                                Err(e) => {
                                                    let resp = IpcResponse::Error {
                                                        message: e.to_string(),
                                                    };
                                                    let json = serde_json::to_string(&resp)? + "\n";
                                                    let mut w = writer.lock().await;
                                                    w.write_all(json.as_bytes()).await?;
                                                    w.flush().await?;
                                                }
                                            }
                                            continue;
                                        }

                                        // Record the interaction for history and sync
                                        let _ = runtime
                                            .record_interaction(
                                                &session_id,
                                                message,
                                                &full_response,
                                            )
                                            .await;

                                        // Send final full message
                                        let final_response = IpcResponse::Chat {
                                            response: full_response,
                                            surface: None,
                                        };
                                        let json = serde_json::to_string(&final_response)? + "\n";
                                        let mut w = writer.lock().await;
                                        w.write_all(json.as_bytes()).await?;
                                        w.flush().await?;
                                    }
                                    Err(e) => {
                                        let response = IpcResponse::Error {
                                            message: e.to_string(),
                                        };
                                        let json = serde_json::to_string(&response)? + "\n";
                                        let mut w = writer.lock().await;
                                        w.write_all(json.as_bytes()).await?;
                                        w.flush().await?;
                                    }
                                }
                            }
                            IpcRequest::Subscribe { events } => {
                                // Subscribe BEFORE sending the Ok ack so no post-ack events are lost.
                                let rx = runtime.event_bus.subscribe();
                                let ack = IpcResponse::Ok {
                                    message: "Subscribed to system events".to_string(),
                                };
                                if send_response(&writer, &ack).await.is_err() {
                                    break;
                                }
                                handle_subscribe_loop(
                                    &mut reader,
                                    &writer,
                                    &mut line_buf,
                                    rx,
                                    events.clone(),
                                )
                                .await;
                                break;
                            }
                            _ => {
                                let response =
                                    process_request(&request, &runtime, &mut session_id).await;
                                let json = serde_json::to_string(&response)? + "\n";
                                let mut w = writer.lock().await;
                                w.write_all(json.as_bytes()).await?;
                                w.flush().await?;
                            }
                        }
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

async fn send_response(
    writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    response: &IpcResponse,
) -> Result<()> {
    let json = serde_json::to_string(response)? + "\n";
    let mut w = writer.lock().await;
    w.write_all(json.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

async fn handle_subscribe_loop<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    line_buf: &mut LineBuffer,
    mut rx: tokio::sync::broadcast::Receiver<crate::events::SystemEvent>,
    events: Option<Vec<String>>,
) {
    loop {
        tokio::select! {
            recv_res = rx.recv() => {
                match recv_res {
                    Ok(event) => {
                        let matches = match &events {
                            None => true,
                            Some(filter) => filter.iter().any(|k| k == event.kind()),
                        };
                        if matches {
                            let resp = IpcResponse::Event { event };
                            if send_response(writer, &resp).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!("IPC subscriber lagged by {} events", skipped);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
            read_res = read_bounded_line(reader, line_buf, MAX_MESSAGE_SIZE) => {
                match read_res {
                    Ok(BoundedRead::Eof) => break,
                    Ok(BoundedRead::TooLarge(n)) => {
                        warn!("Message exceeds size limit ({} bytes) in subscribe mode", n);
                        let _ = send_response(
                            writer,
                            &IpcResponse::Error {
                                message: format!("Message too large: exceeds max {} bytes", MAX_MESSAGE_SIZE),
                            },
                        ).await;
                        break;
                    }
                    Ok(BoundedRead::Line) => {
                        if line_buf.as_str().trim().is_empty() {
                            continue;
                        }
                        let err_resp = IpcResponse::Error {
                            message: "Connection is in push-only subscribe mode".to_string(),
                        };
                        if send_response(writer, &err_resp).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

async fn execute_and_report(runtime: &MycelRuntime, code: &str) -> IpcResponse {
    match runtime.executor.run_detailed(code).await {
        Ok(report) => {
            if !report.succeeded() {
                let class = crate::recovery::classify_failure(&report);
                let _ = runtime
                    .event_bus
                    .send(crate::events::SystemEvent::ExecutionFailed {
                        language: report.language.as_str().to_string(),
                        failure_class: class.as_str().to_string(),
                        exit_code: report.exit_code,
                        signal: report.signal,
                        timed_out: report.timed_out,
                        sandbox_mode: report.sandbox_mode.as_str().to_string(),
                    });
            }
            IpcResponse::CodeResult {
                code: code.to_string(),
                output: report.combined_output(),
                success: report.succeeded(),
            }
        }
        Err(e) => IpcResponse::CodeResult {
            code: code.to_string(),
            output: e.to_string(),
            success: false,
        },
    }
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
        IpcRequest::Chat { .. } => {
            // Handled separately in handle_connection for streaming/provider routing
            IpcResponse::Error {
                message: "Internal error: Chat should be handled by streaming handler".to_string(),
            }
        }
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
        IpcRequest::Status => {
            let session_count = runtime.context_manager.session_count().await;
            let cloud_spend = runtime.ai_router.cloud_spend_today().await;
            IpcResponse::Status {
                version: env!("CARGO_PKG_VERSION").to_string(),
                uptime: 0, // TODO: Track uptime
                sessions: session_count,
                llm_model: runtime.config.local_model.clone(),
                cloud_spend_usd: cloud_spend,
            }
        }
        IpcRequest::ExecuteCode { code } => {
            // Gate direct execution through the policy evaluator, same as the
            // chat flow. Denied commands are rejected outright; commands
            // requiring confirmation are stored in ConfirmationStore and
            // returned as structured ConfirmationRequired responses.
            use crate::policy::ActionPolicy;
            match runtime.policy_evaluator.evaluate_code(code) {
                ActionPolicy::Allow => execute_and_report(runtime, code).await,
                ActionPolicy::RequiresConfirmation { message, .. } => {
                    if let Err(e) = runtime.context_manager.get_context(session_id).await {
                        return IpcResponse::Error {
                            message: e.to_string(),
                        };
                    }
                    if let Err(e) = runtime
                        .context_manager
                        .set_pending_command(session_id, Some(code.clone()))
                        .await
                    {
                        return IpcResponse::Error {
                            message: e.to_string(),
                        };
                    }
                    let pending = runtime
                        .confirmations
                        .insert(session_id, "execute_code", code, &message)
                        .await;
                    let _ =
                        runtime
                            .event_bus
                            .send(crate::events::SystemEvent::ConfirmationRequired {
                                id: pending.id.clone(),
                                session_id: session_id.clone(),
                                command: code.clone(),
                                reason: message.clone(),
                            });
                    IpcResponse::ConfirmationRequired {
                        id: pending.id,
                        action: pending.action,
                        code: code.clone(),
                        reason: message,
                    }
                }
                ActionPolicy::Deny { reason } => IpcResponse::CodeResult {
                    code: code.clone(),
                    output: format!("denied by policy: {}", reason),
                    success: false,
                },
            }
        }
        IpcRequest::ConfirmAction { id, approved } => {
            match runtime.confirmations.take_for_session(id, session_id).await {
                Err(crate::policy::ConfirmationLookupError::NotFound)
                | Err(crate::policy::ConfirmationLookupError::WrongSession) => IpcResponse::Error {
                    message: "Confirmation not found".to_string(),
                },
                Err(crate::policy::ConfirmationLookupError::Expired) => IpcResponse::Error {
                    message: "Confirmation expired".to_string(),
                },
                Ok(pending) => {
                    if let Ok(ctx) = runtime.context_manager.get_context(session_id).await {
                        if ctx.pending_command.as_deref() == Some(pending.code.as_str()) {
                            let _ = runtime
                                .context_manager
                                .clear_pending_command(session_id)
                                .await;
                        }
                    }
                    if !approved {
                        IpcResponse::Ok {
                            message: format!("Action {} denied", pending.id),
                        }
                    } else if let Some(payload) = pending.code.strip_prefix(WINE_CONFIRM_PREFIX) {
                        match serde_json::from_str::<PendingWineRun>(payload) {
                            Ok(req) => wine_result_to_ipc(
                                runtime
                                    .wine_service
                                    .run_windows_app(&req.app_name, &req.exe_path, &req.args)
                                    .await,
                            ),
                            Err(e) => IpcResponse::Error {
                                message: format!("Invalid pending Wine request: {}", e),
                            },
                        }
                    } else {
                        execute_and_report(runtime, &pending.code).await
                    }
                }
            }
        }
        IpcRequest::Subscribe { .. } => IpcResponse::Error {
            message: "Internal error: Subscribe should be handled by connection loop".to_string(),
        },
        IpcRequest::SyncStatus => {
            let status = runtime.sync_service.status().await;
            IpcResponse::SyncStatus {
                enabled: status.enabled,
                device_id: status.device_id,
                transport: status.transport,
                interface: status.interface,
                listen_addr: status.listen_addr,
                trusted_peers: status.trusted_peers,
                connected_peers: status.connected_peers,
                event_log_len: status.event_log_len,
                peers: status.peers,
                public_endpoint: status.public_endpoint,
                relay_connected: status.relay_connected,
            }
        }
        IpcRequest::GeneratePairingCode { endpoint } => {
            let info = runtime
                .sync_service
                .generate_pairing_code(endpoint.clone())
                .await;
            IpcResponse::PairingCode {
                device_id: info.device_id,
                code: info.code,
                endpoint: info.endpoint,
            }
        }
        IpcRequest::AddTrustedPeer {
            peer_id,
            name,
            endpoint,
        } => {
            match runtime
                .sync_service
                .add_trusted_peer(peer_id, name.clone(), endpoint.clone())
                .await
            {
                Ok((peer, synced_events)) => IpcResponse::PeerAdded {
                    peer,
                    synced_events,
                },
                Err(e) => IpcResponse::Error {
                    message: e.to_string(),
                },
            }
        }
        IpcRequest::PairPeer { code } => match runtime.sync_service.pair_peer(code).await {
            Ok((peer, synced_events)) => IpcResponse::PeerAdded {
                peer,
                synced_events,
            },
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        },
        IpcRequest::TriggerSync { peer_id } => match peer_id {
            Some(pid) => match runtime.sync_service.sync_with_peer(pid).await {
                Ok(n) => IpcResponse::Ok {
                    message: format!("Synced {} event(s) with {}", n, pid),
                },
                Err(e) => IpcResponse::Error {
                    message: e.to_string(),
                },
            },
            None => {
                let n = runtime.sync_service.sync_all_peers().await;
                IpcResponse::Ok {
                    message: format!("Synced {} event(s) across mesh", n),
                }
            }
        },
        IpcRequest::ListProviders => {
            let active = runtime.ai_router.active_cloud_selection().await;
            let (active_provider, active_model) = match active {
                Some((p, m)) => (Some(p), Some(m)),
                None => (None, None),
            };
            let providers = runtime.ai_router.list_providers();
            IpcResponse::Providers {
                active_provider,
                active_model,
                providers,
            }
        }
        IpcRequest::SetProvider { provider, model } => {
            match runtime
                .ai_router
                .set_cloud_provider(provider, model.as_deref())
                .await
            {
                Ok((pid, mid)) => IpcResponse::Ok {
                    message: format!("Active cloud provider set to {pid}/{mid}"),
                },
                Err(e) => IpcResponse::Error {
                    message: e.to_string(),
                },
            }
        }
        IpcRequest::CollectiveStatus => {
            let status = runtime.collective_service.status().await;
            IpcResponse::CollectiveStatus {
                enabled: status.enabled,
                near_network: status.near_network,
                near_rpc_url: status.near_rpc_url,
                near_account: status.near_account,
                near_connected: status.near_connected,
                ipfs_enabled: status.ipfs_enabled,
                ipfs_api: status.ipfs_api,
                bittensor_enabled: status.bittensor_enabled,
                local_patterns: status.local_patterns,
                patterns_shared: status.patterns_shared,
                share_patterns_enabled: status.share_patterns_enabled,
            }
        }
        IpcRequest::ListPatterns => {
            let patterns = runtime.collective_service.list_patterns().await;
            IpcResponse::Patterns { patterns }
        }
        IpcRequest::SharePattern {
            name,
            description,
            category,
        } => match runtime
            .collective_service
            .share_pattern(name, description, category)
            .await
        {
            Ok(pattern) => IpcResponse::PatternShared { pattern },
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        },
        IpcRequest::WineStatus => {
            let status = runtime.wine_service.status().await;
            IpcResponse::WineStatus {
                enabled: status.enabled,
                available: status.available,
                wine_binary: status.wine_binary,
                resolved_binary: status.resolved_binary,
                wine_version: status.wine_version,
                prefix_root: status.prefix_root,
                sandbox_with_bwrap: status.sandbox_with_bwrap,
                allow_network: status.allow_network,
                prefixes: status.prefixes,
                install_guidance: status.install_guidance,
            }
        }
        IpcRequest::RunWindowsApp {
            app_name,
            exe_path,
            args,
        } => {
            if !runtime.wine_service.is_enabled() {
                return IpcResponse::Error {
                    message: "Wine subsystem is disabled; set [wine] enabled = true in config.toml to run Windows applications.".to_string(),
                };
            }
            if let Err(e) = crate::wine::WineService::sanitize_app_name(app_name) {
                return IpcResponse::Error {
                    message: e.to_string(),
                };
            }
            if let Err(e) = crate::wine::WineService::validate_exe_path(exe_path) {
                return IpcResponse::Error {
                    message: e.to_string(),
                };
            }

            let command_display = if args.is_empty() {
                format!("wine {}", exe_path)
            } else {
                format!("wine {} {}", exe_path, args.join(" "))
            };
            use crate::policy::ActionPolicy;
            match runtime.policy_evaluator.evaluate_code(&command_display) {
                ActionPolicy::Deny { reason } => IpcResponse::Error {
                    message: format!("denied by policy: {}", reason),
                },
                ActionPolicy::RequiresConfirmation { message, .. } => {
                    let payload = PendingWineRun {
                        app_name: app_name.clone(),
                        exe_path: exe_path.clone(),
                        args: args.clone(),
                    };
                    let marker = match serde_json::to_string(&payload) {
                        Ok(json) => format!("{}{}", WINE_CONFIRM_PREFIX, json),
                        Err(e) => {
                            return IpcResponse::Error {
                                message: format!("Failed to encode Wine confirmation: {}", e),
                            };
                        }
                    };
                    let pending = runtime
                        .confirmations
                        .insert(session_id, "run_windows_app", &marker, &message)
                        .await;
                    let _ =
                        runtime
                            .event_bus
                            .send(crate::events::SystemEvent::ConfirmationRequired {
                                id: pending.id.clone(),
                                session_id: session_id.clone(),
                                command: command_display.clone(),
                                reason: message.clone(),
                            });
                    IpcResponse::ConfirmationRequired {
                        id: pending.id,
                        action: pending.action,
                        code: command_display,
                        reason: message,
                    }
                }
                ActionPolicy::Allow => wine_result_to_ipc(
                    runtime
                        .wine_service
                        .run_windows_app(app_name, exe_path, args)
                        .await,
                ),
            }
        }
        IpcRequest::Ping => IpcResponse::Pong,
    }
}

const WINE_CONFIRM_PREFIX: &str = "#!wine\n";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingWineRun {
    app_name: String,
    exe_path: String,
    args: Vec<String>,
}

fn wine_result_to_ipc(res: Result<crate::wine::WineRunResult>) -> IpcResponse {
    match res {
        Ok(r) => IpcResponse::WineResult {
            app_name: r.app_name,
            exe_path: r.exe_path,
            prefix_path: r.prefix_path,
            sandboxed: r.sandboxed,
            exit_code: r.exit_code,
            timed_out: r.timed_out,
            stdout: r.stdout,
            stderr: r.stderr,
            success: r.success,
        },
        Err(e) => IpcResponse::Error {
            message: e.to_string(),
        },
    }
}

fn default_pattern_category() -> String {
    "general".to_string()
}

/// LLM provider selection
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LlmProvider {
    /// Automatically choose based on config (default)
    #[default]
    Auto,
    /// Force local LLM (Ollama)
    Local,
    /// Force cloud LLM (OpenRouter)
    Cloud,
}

/// Requests that can be sent to the runtime
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcRequest {
    /// Authenticate with token (required before other requests)
    Authenticate { token: String },
    /// Send a chat message
    Chat {
        message: String,
        /// Optional: force a specific LLM provider (local, cloud, or auto)
        #[serde(default)]
        provider: LlmProvider,
    },
    /// Set the session ID
    SetSession { id: String },
    /// Get current context
    GetContext,
    /// Get system status
    Status,
    /// Direct code execution
    ExecuteCode { code: String },
    /// Confirm or deny a pending policy-gated action
    ConfirmAction { id: String, approved: bool },
    /// Subscribe to pushed system events (switches connection to push-only mode)
    Subscribe {
        #[serde(default)]
        events: Option<Vec<String>>,
    },
    /// Get mesh sync status
    SyncStatus,
    /// Generate a pairing code for this device
    GeneratePairingCode {
        #[serde(default)]
        endpoint: Option<String>,
    },
    /// Add a trusted peer by ID (and optional name/endpoint)
    AddTrustedPeer {
        peer_id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        endpoint: Option<String>,
    },
    /// Pair a peer using a pairing code or device ID
    PairPeer { code: String },
    /// Trigger an immediate mesh sync with one or all peers
    TriggerSync {
        #[serde(default)]
        peer_id: Option<String>,
    },
    /// List all known cloud AI providers and the active selection
    ListProviders,
    /// Switch the active cloud AI provider (and optional model) at runtime
    SetProvider {
        provider: String,
        #[serde(default)]
        model: Option<String>,
    },
    /// Get collective network status
    CollectiveStatus,
    /// List stored collective patterns
    ListPatterns,
    /// Share a pattern with the collective (PII-redacted)
    SharePattern {
        name: String,
        description: String,
        #[serde(default = "default_pattern_category")]
        category: String,
    },
    /// Get Wine compatibility subsystem status
    WineStatus,
    /// Run a Windows executable inside an isolated Wine prefix
    RunWindowsApp {
        app_name: String,
        exe_path: String,
        #[serde(default)]
        args: Vec<String>,
    },
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
    /// Chat chunk (for streaming)
    ChatChunk { delta: String },
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
    /// System status
    Status {
        version: String,
        uptime: u64,
        sessions: usize,
        llm_model: String,
        #[serde(default)]
        cloud_spend_usd: f64,
    },
    /// Pushed system event (in Subscribe mode)
    Event { event: crate::events::SystemEvent },
    /// Structured confirmation prompt for policy-gated actions
    ConfirmationRequired {
        id: String,
        action: String,
        code: String,
        reason: String,
    },
    /// Generic OK response
    Ok { message: String },
    /// Mesh sync status
    SyncStatus {
        enabled: bool,
        device_id: String,
        transport: String,
        interface: Option<String>,
        listen_addr: Option<String>,
        trusted_peers: usize,
        connected_peers: usize,
        event_log_len: usize,
        peers: Vec<crate::sync::PeerInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        public_endpoint: Option<String>,
        #[serde(default)]
        relay_connected: bool,
    },
    /// Generated pairing code
    PairingCode {
        device_id: String,
        code: String,
        endpoint: Option<String>,
    },
    /// Trusted peer added/paired
    PeerAdded {
        peer: crate::sync::PeerInfo,
        synced_events: usize,
    },
    /// Cloud AI provider catalog and active selection
    Providers {
        active_provider: Option<String>,
        active_model: Option<String>,
        providers: Vec<crate::ai::providers::ProviderSummary>,
    },
    /// Collective network status
    CollectiveStatus {
        enabled: bool,
        near_network: String,
        near_rpc_url: String,
        near_account: String,
        near_connected: bool,
        ipfs_enabled: bool,
        ipfs_api: String,
        bittensor_enabled: bool,
        local_patterns: usize,
        patterns_shared: usize,
        share_patterns_enabled: bool,
    },
    /// Stored collective patterns
    Patterns {
        patterns: Vec<crate::collective::patterns::CollectivePatternRecord>,
    },
    /// Newly shared pattern record
    PatternShared {
        pattern: crate::collective::patterns::CollectivePatternRecord,
    },
    /// Wine compatibility subsystem status
    WineStatus {
        enabled: bool,
        available: bool,
        wine_binary: String,
        resolved_binary: Option<String>,
        wine_version: Option<String>,
        prefix_root: String,
        sandbox_with_bwrap: bool,
        allow_network: bool,
        prefixes: Vec<String>,
        install_guidance: Option<String>,
    },
    /// Result of running a Windows application via Wine
    WineResult {
        app_name: String,
        exe_path: String,
        prefix_path: String,
        sandboxed: bool,
        exit_code: Option<i32>,
        timed_out: bool,
        stdout: String,
        stderr: String,
        success: bool,
    },
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
        self.chat_with_provider(message, LlmProvider::Auto).await
    }

    pub async fn chat_with_provider(
        &mut self,
        message: &str,
        provider: LlmProvider,
    ) -> Result<IpcResponse> {
        self.send(&IpcRequest::Chat {
            message: message.to_string(),
            provider,
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
            provider: LlmProvider::Auto,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("Chat"));
        assert!(json.contains("Hello, world!"));
    }

    #[test]
    fn test_status_request_serialization() {
        let request = IpcRequest::Status;
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("Status"));

        let deserialized: IpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, IpcRequest::Status));
    }

    #[test]
    fn test_exec_request_serialization() {
        let request = IpcRequest::ExecuteCode {
            code: "ls".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("ExecuteCode"));
        assert!(json.contains("ls"));

        let deserialized: IpcRequest = serde_json::from_str(&json).unwrap();
        match deserialized {
            IpcRequest::ExecuteCode { code } => assert_eq!(code, "ls"),
            _ => panic!("Expected ExecuteCode request"),
        }
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
            r#"{"type":"Status"}"#,
            r#"{"type":"ExecuteCode","code":"ls"}"#,
            r#"{"type":"ConfirmAction","id":"conf-1","approved":true}"#,
            r#"{"type":"Subscribe"}"#,
            r#"{"type":"Subscribe","events":["ConfirmationRequired"]}"#,
            r#"{"type":"ListProviders"}"#,
            r#"{"type":"SetProvider","provider":"venice"}"#,
            r#"{"type":"SetProvider","provider":"venice","model":"llama-3.3-70b"}"#,
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

    #[tokio::test]
    async fn test_read_bounded_line_rejects_oversized_without_newline() {
        // Infinite stream of 'a' with no newline: must stop at MAX_MESSAGE_SIZE + 1 without OOM.
        let mut reader = BufReader::new(tokio::io::repeat(b'a'));
        let mut buf = LineBuffer::new();
        match read_bounded_line(&mut reader, &mut buf, MAX_MESSAGE_SIZE)
            .await
            .unwrap()
        {
            BoundedRead::TooLarge(n) => assert_eq!(n, MAX_MESSAGE_SIZE + 1),
            other => panic!(
                "expected TooLarge, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        assert_eq!(buf.as_str().len(), MAX_MESSAGE_SIZE + 1);
    }

    #[tokio::test]
    async fn test_read_bounded_line_accepts_valid_and_rejects_oversized_with_newline() {
        let valid = b"{\"type\":\"Ping\"}\n";
        let mut reader = BufReader::new(&valid[..]);
        let mut buf = LineBuffer::new();
        assert!(matches!(
            read_bounded_line(&mut reader, &mut buf, MAX_MESSAGE_SIZE)
                .await
                .unwrap(),
            BoundedRead::Line
        ));
        assert_eq!(buf.as_str(), "{\"type\":\"Ping\"}\n");
        assert!(matches!(
            read_bounded_line(&mut reader, &mut buf, MAX_MESSAGE_SIZE)
                .await
                .unwrap(),
            BoundedRead::Eof
        ));

        let mut oversized = vec![b'x'; MAX_MESSAGE_SIZE + 100];
        oversized.push(b'\n');
        let mut reader = BufReader::new(&oversized[..]);
        match read_bounded_line(&mut reader, &mut buf, MAX_MESSAGE_SIZE)
            .await
            .unwrap()
        {
            BoundedRead::TooLarge(n) => assert_eq!(n, MAX_MESSAGE_SIZE + 1),
            _ => panic!("expected TooLarge"),
        }
    }

    #[tokio::test]
    async fn test_line_buffer_reuses_and_releases_capacity() {
        let big = LINE_BUFFER_KEEP_BYTES * 4;
        let mut data = vec![b'x'; big];
        data.push(b'\n');
        data.extend_from_slice(b"{\"type\":\"Ping\"}\n");
        let mut reader = BufReader::new(&data[..]);
        let mut buf = LineBuffer::new();

        // A large frame grows the buffer.
        assert!(matches!(
            read_bounded_line(&mut reader, &mut buf, MAX_MESSAGE_SIZE)
                .await
                .unwrap(),
            BoundedRead::Line
        ));
        assert_eq!(buf.as_str().len(), big + 1);
        assert!(buf.buf.capacity() >= big);

        // The next frame reuses the buffer but releases the large capacity.
        assert!(matches!(
            read_bounded_line(&mut reader, &mut buf, MAX_MESSAGE_SIZE)
                .await
                .unwrap(),
            BoundedRead::Line
        ));
        assert_eq!(buf.as_str(), "{\"type\":\"Ping\"}\n");
        assert!(
            buf.buf.capacity() <= LINE_BUFFER_KEEP_BYTES,
            "large capacity retained: {}",
            buf.buf.capacity()
        );
    }

    async fn build_test_runtime(dir: &std::path::Path) -> crate::MycelRuntime {
        std::fs::create_dir_all(dir).unwrap();
        let mut config = crate::config::MycelConfig {
            context_path: dir.to_string_lossy().to_string(),
            ollama_url: "http://127.0.0.1:1".to_string(),
            openrouter_api_key: String::new(),
            ..Default::default()
        };
        config.mcp.enabled = false;
        config.sandbox.mode = crate::config::SandboxMode::Host;

        let context_manager = crate::context::ContextManager::new(&config).await.unwrap();
        let ai_router = crate::ai::AiRouter::cloud_only(&config).await.unwrap();
        let executor = crate::executor::CodeExecutor::new(&config).unwrap();
        let policy_evaluator = crate::policy::PolicyEvaluator::with_defaults();
        let confirmations = crate::policy::ConfirmationStore::new();
        let ui_factory = crate::ui::UiFactory::new(&config).unwrap();
        let (event_bus, _) = tokio::sync::broadcast::channel(16);
        let mcp_manager = crate::mcp::McpManager::new(&config.mcp, ".", event_bus.clone())
            .await
            .unwrap();
        let sync_service = crate::sync::SyncService::new(&config, None, event_bus.clone())
            .await
            .unwrap();
        let selfdev =
            crate::selfdev::SelfDevService::new(config.selfdev.clone(), dir.join("selfdev.json"))
                .await;
        let command_registry =
            crate::commands::CommandRegistry::load(dir.join("commands.json")).await;
        let collective_service = crate::collective::CollectiveService::new(&config, false)
            .await
            .unwrap();
        let wine_service = crate::wine::WineService::new(&config);

        crate::MycelRuntime {
            config,
            context_manager,
            ai_router,
            executor,
            policy_evaluator,
            confirmations,
            ui_factory,
            sync_service,
            mcp_manager,
            selfdev,
            command_registry,
            collective_service,
            wine_service,
            event_bus,
        }
    }

    #[tokio::test]
    async fn test_execute_code_reports_success_and_failure() {
        let dir = std::env::temp_dir().join(format!("mycel-ipc-test-{}", uuid::Uuid::new_v4()));
        let runtime = build_test_runtime(&dir).await;
        let mut session = "ipc-test".to_string();

        let ok = process_request(
            &IpcRequest::ExecuteCode {
                code: "echo ok".to_string(),
            },
            &runtime,
            &mut session,
        )
        .await;
        match ok {
            IpcResponse::CodeResult {
                success, output, ..
            } => {
                assert!(success, "expected success, output: {output}");
                assert_eq!(output.trim(), "ok");
            }
            other => panic!("expected CodeResult, got {other:?}"),
        }

        let fail = process_request(
            &IpcRequest::ExecuteCode {
                code: "exit 1".to_string(),
            },
            &runtime,
            &mut session,
        )
        .await;
        match fail {
            IpcResponse::CodeResult { success, .. } => {
                assert!(!success, "expected failure for exit 1");
            }
            other => panic!("expected CodeResult, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn read_ipc_response(
        reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    ) -> IpcResponse {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("timed out reading IPC response")
            .unwrap();
        assert!(n > 0, "unexpected EOF reading IPC response");
        serde_json::from_str(line.trim()).unwrap()
    }

    async fn send_ipc_request(writer: &mut tokio::net::unix::OwnedWriteHalf, req: &IpcRequest) {
        let json = serde_json::to_string(req).unwrap() + "\n";
        writer.write_all(json.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn spawn_test_server(
        dir: &std::path::Path,
        runtime: crate::MycelRuntime,
    ) -> (Arc<IpcServer>, tokio::task::JoinHandle<()>) {
        let server = Arc::new(IpcServer::new(&runtime).await.unwrap());
        let server_task = Arc::clone(&server);
        let handle = tokio::spawn(async move {
            let _ = server_task.run().await;
        });
        // Give the accept loop a moment to start polling
        tokio::task::yield_now().await;
        assert!(dir.exists());
        (server, handle)
    }

    #[test]
    fn test_token_path_for_socket() {
        assert_eq!(
            token_path_for_socket("/tmp/mycel-dev.sock"),
            PathBuf::from("/tmp/mycel-dev.token")
        );
        assert_eq!(
            token_path_for_socket("/run/mycel/runtime.sock"),
            PathBuf::from("/run/mycel/runtime.token")
        );
    }

    #[tokio::test]
    async fn test_atomic_token_file_permissions_and_cleanup_on_drop() {
        let dir = std::env::temp_dir().join(format!("mycel-ipc-tok-{}", uuid::Uuid::new_v4()));
        let mut runtime = build_test_runtime(&dir).await;
        let sock_path = dir.join("nested").join("runtime.sock");
        runtime.config.ipc_socket_path = sock_path.to_string_lossy().to_string();

        let expected_token_path = dir.join("nested").join("runtime.token");
        let server = IpcServer::new(&runtime).await.unwrap();
        assert_eq!(server.token_path(), expected_token_path.as_path());
        assert!(expected_token_path.exists());
        assert!(sock_path.exists());

        let token_contents = std::fs::read_to_string(&expected_token_path).unwrap();
        assert_eq!(token_contents, server.auth_token());
        assert!(!token_contents.is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&expected_token_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "token file must have 0600 permissions");
            let sock_mode = std::fs::metadata(&sock_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(sock_mode, 0o600, "socket file must have 0600 permissions");
        }

        drop(server);
        assert!(
            !expected_token_path.exists(),
            "token file should be removed on drop"
        );
        assert!(!sock_path.exists(), "socket file should be removed on drop");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_unauthenticated_subscribe_and_confirm_action_rejected() {
        let dir = std::env::temp_dir().join(format!("mycel-ipc-unauth-{}", uuid::Uuid::new_v4()));
        let mut runtime = build_test_runtime(&dir).await;
        let sock_path = dir.join("mycel.sock");
        runtime.config.ipc_socket_path = sock_path.to_string_lossy().to_string();

        let (server, handle) = spawn_test_server(&dir, runtime).await;

        let stream = UnixStream::connect(&sock_path).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);

        send_ipc_request(&mut w, &IpcRequest::Subscribe { events: None }).await;
        match read_ipc_response(&mut reader).await {
            IpcResponse::Error { message } => {
                assert!(
                    message.contains("Authentication required"),
                    "got: {message}"
                );
            }
            other => panic!("expected Error for unauthenticated Subscribe, got {other:?}"),
        }

        send_ipc_request(
            &mut w,
            &IpcRequest::ConfirmAction {
                id: "fake-id".to_string(),
                approved: true,
            },
        )
        .await;
        match read_ipc_response(&mut reader).await {
            IpcResponse::Error { message } => {
                assert!(
                    message.contains("Authentication required"),
                    "got: {message}"
                );
            }
            other => panic!("expected Error for unauthenticated ConfirmAction, got {other:?}"),
        }

        handle.abort();
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_authenticated_subscribe_unfiltered_filtered_lag_and_eof() {
        let dir = std::env::temp_dir().join(format!("mycel-ipc-sub-{}", uuid::Uuid::new_v4()));
        let mut runtime = build_test_runtime(&dir).await;
        let sock_path = dir.join("mycel.sock");
        runtime.config.ipc_socket_path = sock_path.to_string_lossy().to_string();
        let event_bus = runtime.event_bus.clone();

        let (server, handle) = spawn_test_server(&dir, runtime).await;
        let token = std::fs::read_to_string(server.token_path()).unwrap();

        // 1. Unfiltered subscriber + push-only error on unexpected line + lag survival + clean EOF
        let stream1 = UnixStream::connect(&sock_path).await.unwrap();
        let (r1, mut w1) = stream1.into_split();
        let mut reader1 = BufReader::new(r1);

        send_ipc_request(
            &mut w1,
            &IpcRequest::Authenticate {
                token: token.clone(),
            },
        )
        .await;
        assert!(matches!(
            read_ipc_response(&mut reader1).await,
            IpcResponse::Ok { .. }
        ));

        send_ipc_request(&mut w1, &IpcRequest::Subscribe { events: None }).await;
        assert!(matches!(
            read_ipc_response(&mut reader1).await,
            IpcResponse::Ok { .. }
        ));

        // Emit an event and verify it arrives
        let _ = event_bus.send(crate::events::SystemEvent::McpServerRestarted {
            name: "fs".to_string(),
        });
        match read_ipc_response(&mut reader1).await {
            IpcResponse::Event {
                event: crate::events::SystemEvent::McpServerRestarted { name },
            } => assert_eq!(name, "fs"),
            other => panic!("expected Event(McpServerRestarted), got {other:?}"),
        }

        // Sending a request while in push-only subscribe mode returns Error
        send_ipc_request(&mut w1, &IpcRequest::Ping).await;
        match read_ipc_response(&mut reader1).await {
            IpcResponse::Error { message } => {
                assert!(
                    message.contains("push-only subscribe mode"),
                    "got: {message}"
                );
            }
            other => panic!("expected push-only Error, got {other:?}"),
        }

        // Overflow the broadcast channel (capacity 16) to trigger RecvError::Lagged
        for i in 0..32 {
            let _ = event_bus.send(crate::events::SystemEvent::McpServerRestarted {
                name: format!("lag-{i}"),
            });
        }
        // Subscriber must survive lag and still receive subsequent events
        let _ = event_bus.send(crate::events::SystemEvent::ProviderHealthChanged {
            provider: "ollama".to_string(),
            available: true,
            detail: "post-lag".to_string(),
        });
        let mut saw_post_lag = false;
        for _ in 0..33 {
            if let IpcResponse::Event {
                event: crate::events::SystemEvent::ProviderHealthChanged { detail, .. },
            } = read_ipc_response(&mut reader1).await
            {
                if detail == "post-lag" {
                    saw_post_lag = true;
                    break;
                }
            }
        }
        assert!(
            saw_post_lag,
            "subscriber should survive lag and receive post-lag event"
        );

        // Close client cleanly (EOF)
        drop(w1);
        drop(reader1);

        // 2. Filtered subscriber: only ProviderHealthChanged
        let stream2 = UnixStream::connect(&sock_path).await.unwrap();
        let (r2, mut w2) = stream2.into_split();
        let mut reader2 = BufReader::new(r2);

        send_ipc_request(&mut w2, &IpcRequest::Authenticate { token }).await;
        assert!(matches!(
            read_ipc_response(&mut reader2).await,
            IpcResponse::Ok { .. }
        ));

        send_ipc_request(
            &mut w2,
            &IpcRequest::Subscribe {
                events: Some(vec!["ProviderHealthChanged".to_string()]),
            },
        )
        .await;
        assert!(matches!(
            read_ipc_response(&mut reader2).await,
            IpcResponse::Ok { .. }
        ));

        let _ = event_bus.send(crate::events::SystemEvent::McpServerRestarted {
            name: "ignored".to_string(),
        });
        let _ = event_bus.send(crate::events::SystemEvent::ProviderHealthChanged {
            provider: "claude".to_string(),
            available: true,
            detail: "matched".to_string(),
        });

        match read_ipc_response(&mut reader2).await {
            IpcResponse::Event {
                event:
                    crate::events::SystemEvent::ProviderHealthChanged {
                        provider, detail, ..
                    },
            } => {
                assert_eq!(provider, "claude");
                assert_eq!(detail, "matched");
            }
            other => panic!("expected filtered ProviderHealthChanged, got {other:?}"),
        }

        handle.abort();
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_structured_confirmation_end_to_end() {
        let dir = std::env::temp_dir().join(format!("mycel-ipc-conf-{}", uuid::Uuid::new_v4()));
        let mut runtime = build_test_runtime(&dir).await;
        let sock_path = dir.join("mycel.sock");
        runtime.config.ipc_socket_path = sock_path.to_string_lossy().to_string();

        let target_approve = dir.join("target_approve");
        let target_deny = dir.join("target_deny");
        std::fs::create_dir_all(&target_approve).unwrap();
        std::fs::create_dir_all(&target_deny).unwrap();

        let (server, handle) = spawn_test_server(&dir, runtime).await;
        let token = std::fs::read_to_string(server.token_path()).unwrap();

        // Subscriber to observe ConfirmationRequired push event
        let sub_stream = UnixStream::connect(&sock_path).await.unwrap();
        let (sub_r, mut sub_w) = sub_stream.into_split();
        let mut sub_reader = BufReader::new(sub_r);
        send_ipc_request(
            &mut sub_w,
            &IpcRequest::Authenticate {
                token: token.clone(),
            },
        )
        .await;
        assert!(matches!(
            read_ipc_response(&mut sub_reader).await,
            IpcResponse::Ok { .. }
        ));
        send_ipc_request(
            &mut sub_w,
            &IpcRequest::Subscribe {
                events: Some(vec!["ConfirmationRequired".to_string()]),
            },
        )
        .await;
        assert!(matches!(
            read_ipc_response(&mut sub_reader).await,
            IpcResponse::Ok { .. }
        ));

        // Session A client
        let stream_a = UnixStream::connect(&sock_path).await.unwrap();
        let (r_a, mut w_a) = stream_a.into_split();
        let mut reader_a = BufReader::new(r_a);
        send_ipc_request(
            &mut w_a,
            &IpcRequest::Authenticate {
                token: token.clone(),
            },
        )
        .await;
        assert!(matches!(
            read_ipc_response(&mut reader_a).await,
            IpcResponse::Ok { .. }
        ));
        send_ipc_request(
            &mut w_a,
            &IpcRequest::SetSession {
                id: "sess-a".to_string(),
            },
        )
        .await;
        assert!(matches!(
            read_ipc_response(&mut reader_a).await,
            IpcResponse::Ok { .. }
        ));

        // Session B client (wrong session)
        let stream_b = UnixStream::connect(&sock_path).await.unwrap();
        let (r_b, mut w_b) = stream_b.into_split();
        let mut reader_b = BufReader::new(r_b);
        send_ipc_request(
            &mut w_b,
            &IpcRequest::Authenticate {
                token: token.clone(),
            },
        )
        .await;
        assert!(matches!(
            read_ipc_response(&mut reader_b).await,
            IpcResponse::Ok { .. }
        ));
        send_ipc_request(
            &mut w_b,
            &IpcRequest::SetSession {
                id: "sess-b".to_string(),
            },
        )
        .await;
        assert!(matches!(
            read_ipc_response(&mut reader_b).await,
            IpcResponse::Ok { .. }
        ));

        // 1. Session A triggers ExecuteCode needing confirmation
        let cmd_approve = format!("rm -rf {}", target_approve.display());
        send_ipc_request(
            &mut w_a,
            &IpcRequest::ExecuteCode {
                code: cmd_approve.clone(),
            },
        )
        .await;
        let conf_id = match read_ipc_response(&mut reader_a).await {
            IpcResponse::ConfirmationRequired {
                id,
                action,
                code,
                reason,
            } => {
                assert_eq!(action, "execute_code");
                assert_eq!(code, cmd_approve);
                assert!(!reason.is_empty());
                id
            }
            other => panic!("expected ConfirmationRequired, got {other:?}"),
        };
        assert!(
            target_approve.exists(),
            "command must not run before confirmation"
        );

        // Verify pushed SystemEvent::ConfirmationRequired
        match read_ipc_response(&mut sub_reader).await {
            IpcResponse::Event {
                event:
                    crate::events::SystemEvent::ConfirmationRequired {
                        id,
                        session_id,
                        command,
                        ..
                    },
            } => {
                assert_eq!(id, conf_id);
                assert_eq!(session_id, "sess-a");
                assert_eq!(command, cmd_approve);
            }
            other => panic!("expected pushed ConfirmationRequired event, got {other:?}"),
        }

        // 2. Wrong session cannot confirm or steal the entry
        send_ipc_request(
            &mut w_b,
            &IpcRequest::ConfirmAction {
                id: conf_id.clone(),
                approved: true,
            },
        )
        .await;
        match read_ipc_response(&mut reader_b).await {
            IpcResponse::Error { message } => {
                assert!(!message.contains("sess-a"), "must not leak session id");
                assert!(!message.contains(&cmd_approve), "must not leak command");
            }
            other => panic!("expected Error for wrong session, got {other:?}"),
        }
        assert!(target_approve.exists());

        // 3. Session A approves -> executes once
        send_ipc_request(
            &mut w_a,
            &IpcRequest::ConfirmAction {
                id: conf_id.clone(),
                approved: true,
            },
        )
        .await;
        match read_ipc_response(&mut reader_a).await {
            IpcResponse::CodeResult { success, .. } => assert!(success),
            other => panic!("expected CodeResult on approval, got {other:?}"),
        }
        assert!(
            !target_approve.exists(),
            "approved command should have executed"
        );

        // 4. Second submit of same id fails
        send_ipc_request(
            &mut w_a,
            &IpcRequest::ConfirmAction {
                id: conf_id,
                approved: true,
            },
        )
        .await;
        assert!(matches!(
            read_ipc_response(&mut reader_a).await,
            IpcResponse::Error { .. }
        ));

        // 5. Deny flow
        let cmd_deny = format!("rm -rf {}", target_deny.display());
        send_ipc_request(
            &mut w_a,
            &IpcRequest::ExecuteCode {
                code: cmd_deny.clone(),
            },
        )
        .await;
        let deny_id = match read_ipc_response(&mut reader_a).await {
            IpcResponse::ConfirmationRequired { id, .. } => id,
            other => panic!("expected ConfirmationRequired, got {other:?}"),
        };
        send_ipc_request(
            &mut w_a,
            &IpcRequest::ConfirmAction {
                id: deny_id,
                approved: false,
            },
        )
        .await;
        match read_ipc_response(&mut reader_a).await {
            IpcResponse::Ok { message } => assert!(message.contains("denied")),
            other => panic!("expected Ok on denial, got {other:?}"),
        }
        assert!(target_deny.exists(), "denied command must not execute");

        handle.abort();
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_expired_confirmation_and_chat_yes_no_clears_store() {
        let dir = std::env::temp_dir().join(format!("mycel-ipc-exp-{}", uuid::Uuid::new_v4()));
        let mut runtime = build_test_runtime(&dir).await;
        runtime.confirmations =
            crate::policy::ConfirmationStore::with_limits(64, 8, Duration::from_millis(15));

        let mut sess = "sess-exp".to_string();
        let conf_id = match process_request(
            &IpcRequest::ExecuteCode {
                code: "rm -rf /tmp/mycel-nonexistent-exp".to_string(),
            },
            &runtime,
            &mut sess,
        )
        .await
        {
            IpcResponse::ConfirmationRequired { id, .. } => id,
            other => panic!("expected ConfirmationRequired, got {other:?}"),
        };

        tokio::time::sleep(Duration::from_millis(30)).await;
        match process_request(
            &IpcRequest::ConfirmAction {
                id: conf_id,
                approved: true,
            },
            &runtime,
            &mut sess,
        )
        .await
        {
            IpcResponse::Error { message } => {
                assert!(message.contains("expired"), "got: {message}");
            }
            other => panic!("expected expired Error, got {other:?}"),
        }

        // Chat yes/no clears structured pending confirmation
        let mut runtime2 = build_test_runtime(&dir).await;
        runtime2.confirmations = crate::policy::ConfirmationStore::new();
        let target_yes = dir.join("chat_yes_target");
        std::fs::create_dir_all(&target_yes).unwrap();
        let cmd_yes = format!("rm -rf {}", target_yes.display());

        let _ = runtime2
            .execute_code_with_policy(&cmd_yes, "sess-chat-yes")
            .await
            .unwrap();
        assert_eq!(runtime2.confirmations.len().await, 1);

        let _ = runtime2
            .process_input("yes", "sess-chat-yes")
            .await
            .unwrap();
        assert_eq!(
            runtime2.confirmations.len().await,
            0,
            "chat 'yes' must clear ConfirmationStore"
        );
        assert!(!target_yes.exists());

        let _ = runtime2
            .execute_code_with_policy("rm -rf /tmp/mycel-nonexistent-no", "sess-chat-no")
            .await
            .unwrap();
        assert_eq!(runtime2.confirmations.len().await, 1);

        let _ = runtime2.process_input("no", "sess-chat-no").await.unwrap();
        assert_eq!(
            runtime2.confirmations.len().await,
            0,
            "chat 'no' must clear ConfirmationStore"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_mesh_ipc_endpoints_and_auth_gate() {
        let dir = std::env::temp_dir().join(format!("mycel-ipc-mesh-{}", uuid::Uuid::new_v4()));
        let mut runtime = build_test_runtime(&dir).await;
        let sock_path = dir.join("mycel.sock");
        runtime.config.ipc_socket_path = sock_path.to_string_lossy().to_string();

        let (server, handle) = spawn_test_server(&dir, runtime).await;
        let token = std::fs::read_to_string(server.token_path()).unwrap();

        let stream = UnixStream::connect(&sock_path).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);

        // 1. All 5 mesh requests must be rejected before authentication.
        let dummy_key = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::random_from_rng(
                rand::thread_rng(),
            ))
            .as_bytes(),
        );
        let unauth_reqs = vec![
            IpcRequest::SyncStatus,
            IpcRequest::GeneratePairingCode { endpoint: None },
            IpcRequest::AddTrustedPeer {
                peer_id: dummy_key.clone(),
                name: None,
                endpoint: None,
            },
            IpcRequest::PairPeer {
                code: dummy_key.clone(),
            },
            IpcRequest::TriggerSync { peer_id: None },
        ];
        for req in &unauth_reqs {
            send_ipc_request(&mut w, req).await;
            match read_ipc_response(&mut reader).await {
                IpcResponse::Error { message } => {
                    assert!(
                        message.contains("Authentication required"),
                        "unexpected unauth error: {message}"
                    );
                }
                other => panic!("expected Error before auth, got {other:?}"),
            }
        }

        // 2. Authenticate
        send_ipc_request(&mut w, &IpcRequest::Authenticate { token }).await;
        assert!(matches!(
            read_ipc_response(&mut reader).await,
            IpcResponse::Ok { .. }
        ));

        // 3. SyncStatus
        send_ipc_request(&mut w, &IpcRequest::SyncStatus).await;
        let own_id = match read_ipc_response(&mut reader).await {
            IpcResponse::SyncStatus {
                enabled,
                device_id,
                trusted_peers,
                ..
            } => {
                assert!(!enabled);
                assert_eq!(trusted_peers, 0);
                device_id
            }
            other => panic!("expected SyncStatus, got {other:?}"),
        };

        // 4. GeneratePairingCode
        send_ipc_request(
            &mut w,
            &IpcRequest::GeneratePairingCode {
                endpoint: Some("127.0.0.1:51821".to_string()),
            },
        )
        .await;
        match read_ipc_response(&mut reader).await {
            IpcResponse::PairingCode {
                device_id,
                code,
                endpoint,
            } => {
                assert_eq!(device_id, own_id);
                assert!(code.starts_with("mycel-pair-v1:"));
                assert_eq!(endpoint.as_deref(), Some("127.0.0.1:51821"));
            }
            other => panic!("expected PairingCode, got {other:?}"),
        }

        // 5. AddTrustedPeer
        send_ipc_request(
            &mut w,
            &IpcRequest::AddTrustedPeer {
                peer_id: dummy_key.clone(),
                name: Some("laptop".to_string()),
                endpoint: Some("127.0.0.1:51999".to_string()),
            },
        )
        .await;
        match read_ipc_response(&mut reader).await {
            IpcResponse::PeerAdded {
                peer,
                synced_events,
            } => {
                assert_eq!(peer.id, dummy_key);
                assert_eq!(peer.name, "laptop");
                assert_eq!(synced_events, 0);
            }
            other => panic!("expected PeerAdded, got {other:?}"),
        }

        // 6. PairPeer with mycel-pair-v1 code for another peer
        let peer2_key = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::random_from_rng(
                rand::thread_rng(),
            ))
            .as_bytes(),
        );
        let payload = serde_json::json!({
            "device_id": peer2_key,
            "endpoint": "127.0.0.1:52000",
            "name": "desktop"
        });
        let code2 = format!(
            "mycel-pair-v1:{}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                payload.to_string().as_bytes()
            )
        );
        send_ipc_request(&mut w, &IpcRequest::PairPeer { code: code2 }).await;
        match read_ipc_response(&mut reader).await {
            IpcResponse::PeerAdded { peer, .. } => {
                assert_eq!(peer.id, peer2_key);
                assert_eq!(peer.name, "desktop");
            }
            other => panic!("expected PeerAdded from PairPeer, got {other:?}"),
        }

        // 7. TriggerSync (all peers when mesh disabled -> Ok with 0 synced)
        send_ipc_request(&mut w, &IpcRequest::TriggerSync { peer_id: None }).await;
        match read_ipc_response(&mut reader).await {
            IpcResponse::Ok { message } => assert!(message.contains("Synced")),
            other => panic!("expected Ok from TriggerSync, got {other:?}"),
        }

        // TriggerSync for specific peer when mesh disabled -> Error
        send_ipc_request(
            &mut w,
            &IpcRequest::TriggerSync {
                peer_id: Some(dummy_key),
            },
        )
        .await;
        assert!(matches!(
            read_ipc_response(&mut reader).await,
            IpcResponse::Error { .. }
        ));

        handle.abort();
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_provider_ipc_list_set_and_auth_gate() {
        let dir =
            std::env::temp_dir().join(format!("mycel-ipc-providers-{}", uuid::Uuid::new_v4()));
        let mut runtime = build_test_runtime(&dir).await;
        let sock_path = dir.join("mycel.sock");
        runtime.config.ipc_socket_path = sock_path.to_string_lossy().to_string();

        // Add a configured custom provider with a secret API key and rebuild ai_router.
        let secret = "super-secret-custom-token-98765";
        runtime.config.providers.insert(
            "my-custom-llm".to_string(),
            crate::config::CustomProviderConfig {
                api: Some("openai-completions".to_string()),
                base_url: Some("http://127.0.0.1:9999/v1".to_string()),
                api_key: Some(secret.to_string()),
                api_key_env: None,
                default_model: Some("custom-model-1".to_string()),
                requires_auth: Some(true),
                headers: std::collections::HashMap::new(),
            },
        );
        // Add an explicitly unconfigured custom provider so unconfigured rejection is deterministic.
        runtime.config.providers.insert(
            "unconfigured-custom".to_string(),
            crate::config::CustomProviderConfig {
                api: Some("openai-completions".to_string()),
                base_url: Some("http://127.0.0.1:9998/v1".to_string()),
                api_key: None,
                api_key_env: Some("MYCEL_NONEXISTENT_TEST_KEY_12345".to_string()),
                default_model: Some("none".to_string()),
                requires_auth: Some(true),
                headers: std::collections::HashMap::new(),
            },
        );
        runtime.ai_router = crate::ai::AiRouter::cloud_only(&runtime.config)
            .await
            .unwrap();

        let (server, handle) = spawn_test_server(&dir, runtime).await;
        let token = std::fs::read_to_string(server.token_path()).unwrap();

        let stream = UnixStream::connect(&sock_path).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);

        // 1. Unauthenticated ListProviders and SetProvider must be rejected.
        for req in [
            IpcRequest::ListProviders,
            IpcRequest::SetProvider {
                provider: "my-custom-llm".to_string(),
                model: None,
            },
        ] {
            send_ipc_request(&mut w, &req).await;
            match read_ipc_response(&mut reader).await {
                IpcResponse::Error { message } => {
                    assert!(
                        message.contains("Authentication required"),
                        "unexpected unauth error: {message}"
                    );
                }
                other => panic!("expected Error before auth, got {other:?}"),
            }
        }

        // 2. Authenticate
        send_ipc_request(&mut w, &IpcRequest::Authenticate { token }).await;
        assert!(matches!(
            read_ipc_response(&mut reader).await,
            IpcResponse::Ok { .. }
        ));

        // 3. Authenticated ListProviders returns >= 45 providers including venice & openrouter, zero secrets.
        send_ipc_request(&mut w, &IpcRequest::ListProviders).await;
        let resp = read_ipc_response(&mut reader).await;
        let serialized = serde_json::to_string(&resp).unwrap();
        assert!(
            !serialized.contains(secret),
            "ListProviders leaked API key secret!"
        );
        match resp {
            IpcResponse::Providers { providers, .. } => {
                assert!(
                    providers.len() >= 45,
                    "expected >= 45 providers, got {}",
                    providers.len()
                );
                assert!(providers.iter().any(|p| p.id == "venice"));
                assert!(providers.iter().any(|p| p.id == "openrouter"));
                assert!(providers
                    .iter()
                    .any(|p| p.id == "my-custom-llm" && p.configured));
            }
            other => panic!("expected Providers, got {other:?}"),
        }

        // 4. Reject unknown provider cleanly.
        send_ipc_request(
            &mut w,
            &IpcRequest::SetProvider {
                provider: "does-not-exist".to_string(),
                model: None,
            },
        )
        .await;
        match read_ipc_response(&mut reader).await {
            IpcResponse::Error { message } => {
                assert!(
                    message.contains("Unknown provider"),
                    "unexpected unknown provider error: {message}"
                );
            }
            other => panic!("expected Error for unknown provider, got {other:?}"),
        }

        // 5. Reject unconfigured provider cleanly.
        send_ipc_request(
            &mut w,
            &IpcRequest::SetProvider {
                provider: "unconfigured-custom".to_string(),
                model: None,
            },
        )
        .await;
        match read_ipc_response(&mut reader).await {
            IpcResponse::Error { message } => {
                assert!(
                    message.contains("not configured"),
                    "unexpected unconfigured provider error: {message}"
                );
            }
            other => panic!("expected Error for unconfigured provider, got {other:?}"),
        }

        // 6. Succeed for configured custom provider and verify via ListProviders.
        send_ipc_request(
            &mut w,
            &IpcRequest::SetProvider {
                provider: "my-custom-llm".to_string(),
                model: Some("custom-model-2".to_string()),
            },
        )
        .await;
        match read_ipc_response(&mut reader).await {
            IpcResponse::Ok { message } => {
                assert_eq!(
                    message,
                    "Active cloud provider set to my-custom-llm/custom-model-2"
                );
            }
            other => panic!("expected Ok for SetProvider, got {other:?}"),
        }

        send_ipc_request(&mut w, &IpcRequest::ListProviders).await;
        match read_ipc_response(&mut reader).await {
            IpcResponse::Providers {
                active_provider,
                active_model,
                ..
            } => {
                assert_eq!(active_provider.as_deref(), Some("my-custom-llm"));
                assert_eq!(active_model.as_deref(), Some("custom-model-2"));
            }
            other => panic!("expected Providers, got {other:?}"),
        }

        handle.abort();
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_ipc_collective_disabled_and_enabled_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "mycel-ipc-collective-test-{}",
            uuid::Uuid::new_v4()
        ));
        let mut runtime = build_test_runtime(&dir).await;
        let mut session = "collective-test".to_string();

        // 1. Disabled by default: CollectiveStatus reports !enabled, ListPatterns empty, SharePattern refused.
        match process_request(&IpcRequest::CollectiveStatus, &runtime, &mut session).await {
            IpcResponse::CollectiveStatus {
                enabled,
                local_patterns,
                ..
            } => {
                assert!(!enabled);
                assert_eq!(local_patterns, 0);
            }
            other => panic!("expected CollectiveStatus, got {other:?}"),
        }

        match process_request(&IpcRequest::ListPatterns, &runtime, &mut session).await {
            IpcResponse::Patterns { patterns } => assert!(patterns.is_empty()),
            other => panic!("expected empty Patterns, got {other:?}"),
        }

        match process_request(
            &IpcRequest::SharePattern {
                name: "test".to_string(),
                description: "desc".to_string(),
                category: "general".to_string(),
            },
            &runtime,
            &mut session,
        )
        .await
        {
            IpcResponse::Error { message } => {
                assert!(
                    message.to_lowercase().contains("disabled"),
                    "expected disabled error, got: {message}"
                );
            }
            other => panic!("expected Error when collective disabled, got {other:?}"),
        }

        // 2. Enabled with share_patterns = true: PII is redacted and pattern persists across reloads.
        runtime.config.collective.enabled = true;
        runtime.config.collective.share_patterns = true;
        runtime.collective_service =
            crate::collective::CollectiveService::new(&runtime.config, false)
                .await
                .unwrap();

        match process_request(
            &IpcRequest::SharePattern {
                name: "net-diag".to_string(),
                description: "Contact alice@example.com or check /home/alice/.ssh/config for help"
                    .to_string(),
                category: "network".to_string(),
            },
            &runtime,
            &mut session,
        )
        .await
        {
            IpcResponse::PatternShared { pattern } => {
                assert_eq!(pattern.name, "net-diag");
                assert_eq!(pattern.category, "network");
                assert!(!pattern.description.contains("alice@example.com"));
                assert!(!pattern.description.contains("/home/alice"));
                assert!(!pattern.approved_for_execution);
            }
            other => panic!("expected PatternShared, got {other:?}"),
        }

        match process_request(&IpcRequest::ListPatterns, &runtime, &mut session).await {
            IpcResponse::Patterns { patterns } => {
                assert_eq!(patterns.len(), 1);
                assert_eq!(patterns[0].name, "net-diag");
            }
            other => panic!("expected Patterns with 1 entry, got {other:?}"),
        }

        // Reload CollectiveService from the same context_path to verify persistence.
        let reloaded = crate::collective::CollectiveService::new(&runtime.config, false)
            .await
            .unwrap();
        let persisted = reloaded.list_patterns().await;
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].name, "net-diag");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_ipc_wine_disabled_and_validation() {
        let dir =
            std::env::temp_dir().join(format!("mycel-ipc-wine-test-{}", uuid::Uuid::new_v4()));
        let mut runtime = build_test_runtime(&dir).await;
        let mut session = "wine-test".to_string();

        // 1. Disabled by default: WineStatus reports !enabled, RunWindowsApp refused cleanly.
        match process_request(&IpcRequest::WineStatus, &runtime, &mut session).await {
            IpcResponse::WineStatus {
                enabled,
                sandbox_with_bwrap,
                allow_network,
                ..
            } => {
                assert!(!enabled);
                assert!(sandbox_with_bwrap);
                assert!(!allow_network);
            }
            other => panic!("expected WineStatus, got {other:?}"),
        }

        match process_request(
            &IpcRequest::RunWindowsApp {
                app_name: "notepad".to_string(),
                exe_path: "/tmp/notepad.exe".to_string(),
                args: vec![],
            },
            &runtime,
            &mut session,
        )
        .await
        {
            IpcResponse::Error { message } => {
                assert!(
                    message.to_lowercase().contains("disabled"),
                    "expected disabled error, got: {message}"
                );
            }
            other => panic!("expected Error when wine disabled, got {other:?}"),
        }

        // 2. Enabled: path traversal in app_name or exe_path is rejected.
        runtime.config.wine.enabled = true;
        runtime.wine_service = crate::wine::WineService::new(&runtime.config);

        match process_request(
            &IpcRequest::RunWindowsApp {
                app_name: "../escape".to_string(),
                exe_path: "/tmp/app.exe".to_string(),
                args: vec![],
            },
            &runtime,
            &mut session,
        )
        .await
        {
            IpcResponse::Error { message } => {
                assert!(!message.is_empty());
            }
            other => panic!("expected Error for invalid app_name, got {other:?}"),
        }

        match process_request(
            &IpcRequest::RunWindowsApp {
                app_name: "goodapp".to_string(),
                exe_path: "../evil.exe".to_string(),
                args: vec![],
            },
            &runtime,
            &mut session,
        )
        .await
        {
            IpcResponse::Error { message } => {
                assert!(!message.is_empty());
            }
            other => panic!("expected Error for path traversal exe_path, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
