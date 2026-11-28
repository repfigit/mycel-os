use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::protocol::{
    IpcRequest, IpcResponse, LlmProvider, PeerInfo, ProviderSummary, SystemEvent,
};

/// Maximum NDJSON frame size in bytes (1 MiB), matching `mycel-runtime`.
pub const MAX_IPC_FRAME_BYTES: usize = 1024 * 1024;

const SYSTEM_SOCKET_PATH: &str = "/run/mycel/runtime.sock";
const DEV_SOCKET_PATH: &str = "/tmp/mycel-dev.sock";

/// Resolve the default Unix socket path for `mycel-runtime`.
pub fn default_socket_path() -> String {
    if let Ok(val) = std::env::var("MYCEL_SOCKET") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    if Path::new(SYSTEM_SOCKET_PATH).exists() {
        SYSTEM_SOCKET_PATH.to_string()
    } else {
        DEV_SOCKET_PATH.to_string()
    }
}

/// Compute the auth token file path corresponding to a Unix socket path.
pub fn token_path_for_socket(socket_path: &str) -> PathBuf {
    Path::new(socket_path).with_extension("token")
}

/// Dynamically load the auth token for `socket_path` from environment or disk.
///
/// Precedence:
/// 1. `MYCEL_AUTH_TOKEN` (if non-empty)
/// 2. `MYCEL_TOKEN_FILE` (if non-empty)
/// 3. `token_path_for_socket(socket_path)`
pub fn load_auth_token(socket_path: &str) -> Option<String> {
    if let Ok(token) = std::env::var("MYCEL_AUTH_TOKEN") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    if let Ok(token_file) = std::env::var("MYCEL_TOKEN_FILE") {
        let trimmed = token_file.trim();
        if !trimmed.is_empty() {
            return read_trimmed_token(Path::new(trimmed));
        }
    }

    let path = token_path_for_socket(socket_path);
    read_trimmed_token(&path)
}

fn read_trimmed_token(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn has_env_auth_token() -> bool {
    std::env::var("MYCEL_AUTH_TOKEN")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Configuration for `IpcRequestClient`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcClientConfig {
    pub socket_path: String,
    pub session_id: String,
}

/// Snapshot of runtime status returned by `IpcRequestClient::fetch_status`.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusSnapshot {
    pub version: String,
    pub uptime: u64,
    pub sessions: usize,
    pub llm_model: String,
    pub cloud_spend_usd: f64,
}

/// Snapshot of mesh sync status returned by `IpcRequestClient::fetch_sync_status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSnapshot {
    pub enabled: bool,
    pub device_id: String,
    pub transport: String,
    pub interface: Option<String>,
    pub listen_addr: Option<String>,
    pub trusted_peers: usize,
    pub connected_peers: usize,
    pub event_log_len: usize,
    pub peers: Vec<PeerInfo>,
}

/// Snapshot of cloud AI providers returned by `IpcRequestClient::fetch_providers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvidersSnapshot {
    pub active_provider: Option<String>,
    pub active_model: Option<String>,
    pub providers: Vec<ProviderSummary>,
}

/// Snapshot of a generated pairing code returned by `IpcRequestClient::generate_pairing_code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCodeSnapshot {
    pub device_id: String,
    pub code: String,
    pub endpoint: Option<String>,
}

/// Snapshot of a newly paired peer returned by `IpcRequestClient::pair_peer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddedSnapshot {
    pub peer: PeerInfo,
    pub synced_events: usize,
}

/// Events emitted by the dedicated push-only `Subscribe` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriberEvent {
    Connected,
    Event(SystemEvent),
    Disconnected(String),
}

/// Async request/response IPC client for `mycel-runtime`.
#[derive(Debug, Clone)]
pub struct IpcRequestClient {
    config: IpcClientConfig,
}

impl IpcRequestClient {
    pub fn new(config: IpcClientConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &IpcClientConfig {
        &self.config
    }

    async fn connect_and_authenticate(&self) -> Result<(BufReader<OwnedReadHalf>, OwnedWriteHalf)> {
        connect_and_authenticate_socket(&self.config.socket_path).await
    }

    async fn connect_session(&self) -> Result<(BufReader<OwnedReadHalf>, OwnedWriteHalf)> {
        let (mut reader, mut writer) = self.connect_and_authenticate().await?;
        send_request(
            &mut writer,
            &IpcRequest::SetSession {
                id: self.config.session_id.clone(),
            },
        )
        .await?;

        match read_response(&mut reader).await? {
            Some(IpcResponse::Ok { .. }) => Ok((reader, writer)),
            Some(IpcResponse::Error { message }) => Err(anyhow!("SetSession failed: {}", message)),
            Some(other) => Err(anyhow!("Unexpected response to SetSession: {:?}", other)),
            None => Err(anyhow!("Connection closed while setting session")),
        }
    }

    /// Fetch runtime status.
    pub async fn fetch_status(&self) -> Result<StatusSnapshot> {
        let (mut reader, mut writer) = self.connect_session().await?;
        send_request(&mut writer, &IpcRequest::Status).await?;

        match read_response(&mut reader).await? {
            Some(IpcResponse::Status {
                version,
                uptime,
                sessions,
                llm_model,
                cloud_spend_usd,
            }) => Ok(StatusSnapshot {
                version,
                uptime,
                sessions,
                llm_model,
                cloud_spend_usd,
            }),
            Some(IpcResponse::Error { message }) => Err(anyhow!("Status error: {}", message)),
            Some(other) => Err(anyhow!("Unexpected response to Status: {:?}", other)),
            None => Err(anyhow!("Connection closed before Status response")),
        }
    }

    /// Fetch mesh sync status.
    pub async fn fetch_sync_status(&self) -> Result<SyncSnapshot> {
        let (mut reader, mut writer) = self.connect_session().await?;
        send_request(&mut writer, &IpcRequest::SyncStatus).await?;

        match read_response(&mut reader).await? {
            Some(IpcResponse::SyncStatus {
                enabled,
                device_id,
                transport,
                interface,
                listen_addr,
                trusted_peers,
                connected_peers,
                event_log_len,
                peers,
            }) => Ok(SyncSnapshot {
                enabled,
                device_id,
                transport,
                interface,
                listen_addr,
                trusted_peers,
                connected_peers,
                event_log_len,
                peers,
            }),
            Some(IpcResponse::Error { message }) => Err(anyhow!("SyncStatus error: {}", message)),
            Some(other) => Err(anyhow!("Unexpected response to SyncStatus: {:?}", other)),
            None => Err(anyhow!("Connection closed before SyncStatus response")),
        }
    }

    /// Fetch available cloud AI providers and the active selection.
    pub async fn fetch_providers(&self) -> Result<ProvidersSnapshot> {
        let (mut reader, mut writer) = self.connect_session().await?;
        send_request(&mut writer, &IpcRequest::ListProviders).await?;

        match read_response(&mut reader).await? {
            Some(IpcResponse::Providers {
                active_provider,
                active_model,
                providers,
            }) => Ok(ProvidersSnapshot {
                active_provider,
                active_model,
                providers,
            }),
            Some(IpcResponse::Error { message }) => {
                Err(anyhow!("ListProviders error: {}", message))
            }
            Some(other) => Err(anyhow!("Unexpected response to ListProviders: {:?}", other)),
            None => Err(anyhow!("Connection closed before ListProviders response")),
        }
    }

    /// Switch the active cloud AI provider and optional model override.
    pub async fn set_provider(&self, provider: String, model: Option<String>) -> Result<String> {
        let (mut reader, mut writer) = self.connect_session().await?;
        send_request(&mut writer, &IpcRequest::SetProvider { provider, model }).await?;

        match read_response(&mut reader).await? {
            Some(IpcResponse::Ok { message }) => Ok(message),
            Some(IpcResponse::Error { message }) => Err(anyhow!("SetProvider error: {}", message)),
            Some(other) => Err(anyhow!("Unexpected response to SetProvider: {:?}", other)),
            None => Err(anyhow!("Connection closed before SetProvider response")),
        }
    }

    /// Generate a mesh pairing code for this device.
    pub async fn generate_pairing_code(
        &self,
        endpoint: Option<String>,
    ) -> Result<PairingCodeSnapshot> {
        let (mut reader, mut writer) = self.connect_session().await?;
        send_request(&mut writer, &IpcRequest::GeneratePairingCode { endpoint }).await?;

        match read_response(&mut reader).await? {
            Some(IpcResponse::PairingCode {
                device_id,
                code,
                endpoint,
            }) => Ok(PairingCodeSnapshot {
                device_id,
                code,
                endpoint,
            }),
            Some(IpcResponse::Error { message }) => {
                Err(anyhow!("GeneratePairingCode error: {}", message))
            }
            Some(other) => Err(anyhow!(
                "Unexpected response to GeneratePairingCode: {:?}",
                other
            )),
            None => Err(anyhow!(
                "Connection closed before GeneratePairingCode response"
            )),
        }
    }

    /// Pair a peer using a pairing code or peer ID.
    pub async fn pair_peer(&self, code: String) -> Result<PeerAddedSnapshot> {
        let (mut reader, mut writer) = self.connect_session().await?;
        send_request(&mut writer, &IpcRequest::PairPeer { code }).await?;

        match read_response(&mut reader).await? {
            Some(IpcResponse::PeerAdded {
                peer,
                synced_events,
            }) => Ok(PeerAddedSnapshot {
                peer,
                synced_events,
            }),
            Some(IpcResponse::Error { message }) => Err(anyhow!("PairPeer error: {}", message)),
            Some(other) => Err(anyhow!("Unexpected response to PairPeer: {:?}", other)),
            None => Err(anyhow!("Connection closed before PairPeer response")),
        }
    }

    /// Trigger an immediate mesh sync with one or all peers.
    pub async fn trigger_sync(&self, peer_id: Option<String>) -> Result<String> {
        let (mut reader, mut writer) = self.connect_session().await?;
        send_request(&mut writer, &IpcRequest::TriggerSync { peer_id }).await?;

        match read_response(&mut reader).await? {
            Some(IpcResponse::Ok { message }) => Ok(message),
            Some(IpcResponse::Error { message }) => Err(anyhow!("TriggerSync error: {}", message)),
            Some(other) => Err(anyhow!("Unexpected response to TriggerSync: {:?}", other)),
            None => Err(anyhow!("Connection closed before TriggerSync response")),
        }
    }

    /// Confirm or deny a pending policy-gated action.
    pub async fn confirm_action(&self, id: String, approved: bool) -> Result<IpcResponse> {
        let (mut reader, mut writer) = self.connect_session().await?;
        send_request(&mut writer, &IpcRequest::ConfirmAction { id, approved }).await?;

        match read_response(&mut reader).await? {
            Some(resp) => Ok(resp),
            None => Err(anyhow!("Connection closed before ConfirmAction response")),
        }
    }

    /// Send a chat message and stream `ChatChunk` deltas into `chunk_tx`, returning the terminal response.
    pub async fn send_chat_streaming(
        &self,
        message: String,
        provider: LlmProvider,
        chunk_tx: UnboundedSender<String>,
    ) -> Result<IpcResponse> {
        let (mut reader, mut writer) = self.connect_session().await?;
        send_request(&mut writer, &IpcRequest::Chat { message, provider }).await?;

        loop {
            match read_response(&mut reader).await? {
                Some(IpcResponse::ChatChunk { delta }) => {
                    let _ = chunk_tx.send(delta);
                }
                Some(
                    resp @ (IpcResponse::Chat { .. }
                    | IpcResponse::ConfirmationRequired { .. }
                    | IpcResponse::CodeResult { .. }
                    | IpcResponse::Error { .. }),
                ) => {
                    return Ok(resp);
                }
                Some(other) => {
                    return Err(anyhow!(
                        "Unexpected response during Chat stream: {:?}",
                        other
                    ));
                }
                None => {
                    return Err(anyhow!("Connection closed before terminal Chat response"));
                }
            }
        }
    }
}

/// Spawn a dedicated push-only `Subscribe` connection with automatic reconnect and capped exponential backoff.
pub fn spawn_event_subscriber(
    socket_path: String,
    filter: Option<Vec<String>>,
    initial_backoff: Duration,
    max_backoff: Duration,
) -> (UnboundedReceiver<SubscriberEvent>, JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();

    let handle = tokio::spawn(async move {
        let mut backoff = initial_backoff;
        let max_backoff = max_backoff.max(initial_backoff);

        loop {
            if tx.is_closed() {
                break;
            }

            match run_subscribe_session(&socket_path, &filter, &tx).await {
                Ok(()) => {
                    // Receiver dropped.
                    break;
                }
                Err(reason) => {
                    debug!("Event subscriber disconnected: {}", reason);
                    if tx.send(SubscriberEvent::Disconnected(reason)).is_err() {
                        break;
                    }
                }
            }

            if tx.is_closed() {
                break;
            }

            if !backoff.is_zero() {
                tokio::time::sleep(backoff).await;
            }
            backoff = next_backoff(backoff, max_backoff);
        }
    });

    (rx, handle)
}

fn next_backoff(current: Duration, max_backoff: Duration) -> Duration {
    if current.is_zero() {
        return Duration::ZERO;
    }
    current.saturating_mul(2).min(max_backoff)
}

async fn run_subscribe_session(
    socket_path: &str,
    filter: &Option<Vec<String>>,
    tx: &UnboundedSender<SubscriberEvent>,
) -> std::result::Result<(), String> {
    let (mut reader, mut writer) = connect_and_authenticate_socket(socket_path)
        .await
        .map_err(|e| e.to_string())?;

    send_request(
        &mut writer,
        &IpcRequest::Subscribe {
            events: filter.clone(),
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    match read_response(&mut reader)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(IpcResponse::Ok { .. }) => {}
        Some(IpcResponse::Error { message }) => {
            return Err(format!("Subscribe rejected: {}", message));
        }
        Some(other) => {
            return Err(format!("Unexpected Subscribe ack: {:?}", other));
        }
        None => {
            return Err("Connection closed before Subscribe ack".to_string());
        }
    }

    if tx.send(SubscriberEvent::Connected).is_err() {
        return Ok(());
    }

    loop {
        match read_response(&mut reader).await {
            Ok(Some(IpcResponse::Event { event })) => {
                if tx.send(SubscriberEvent::Event(event)).is_err() {
                    return Ok(());
                }
            }
            Ok(Some(IpcResponse::Error { message })) => {
                return Err(format!("Subscriber stream error: {}", message));
            }
            Ok(Some(other)) => {
                warn!(
                    "Ignoring unexpected response on Subscribe stream: {:?}",
                    other
                );
            }
            Ok(None) => {
                return Err("EOF on Subscribe stream".to_string());
            }
            Err(e) => {
                return Err(format!("Read error on Subscribe stream: {}", e));
            }
        }
    }
}

async fn connect_and_authenticate_socket(
    socket_path: &str,
) -> Result<(BufReader<OwnedReadHalf>, OwnedWriteHalf)> {
    let first_attempt = try_connect_and_auth_once(socket_path).await;
    match first_attempt {
        Ok(streams) => Ok(streams),
        Err(AuthFailure::Rejected(msg)) if !has_env_auth_token() => {
            debug!(
                "Auth rejected ({}), re-reading token file and retrying once",
                msg
            );
            try_connect_and_auth_once(socket_path)
                .await
                .map_err(AuthFailure::into_anyhow)
        }
        Err(err) => Err(err.into_anyhow()),
    }
}

enum AuthFailure {
    Rejected(String),
    Other(anyhow::Error),
}

impl AuthFailure {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            AuthFailure::Rejected(msg) => anyhow!("Authentication failed: {}", msg),
            AuthFailure::Other(err) => err,
        }
    }
}

async fn try_connect_and_auth_once(
    socket_path: &str,
) -> std::result::Result<(BufReader<OwnedReadHalf>, OwnedWriteHalf), AuthFailure> {
    let token = load_auth_token(socket_path).ok_or_else(|| {
        AuthFailure::Other(anyhow!(
            "No auth token available for socket {}",
            socket_path
        ))
    })?;

    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("Failed to connect to {}", socket_path))
        .map_err(AuthFailure::Other)?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    send_request(&mut write_half, &IpcRequest::Authenticate { token })
        .await
        .map_err(AuthFailure::Other)?;

    match read_response(&mut reader)
        .await
        .map_err(AuthFailure::Other)?
    {
        Some(IpcResponse::Ok { .. }) => Ok((reader, write_half)),
        Some(IpcResponse::Error { message }) => Err(AuthFailure::Rejected(message)),
        Some(other) => Err(AuthFailure::Other(anyhow!(
            "Unexpected response to Authenticate: {:?}",
            other
        ))),
        None => Err(AuthFailure::Other(anyhow!(
            "Connection closed during Authenticate"
        ))),
    }
}

async fn send_request(writer: &mut OwnedWriteHalf, request: &IpcRequest) -> Result<()> {
    let mut payload = serde_json::to_string(request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_response(reader: &mut BufReader<OwnedReadHalf>) -> Result<Option<IpcResponse>> {
    let mut line = String::with_capacity(512);
    loop {
        line.clear();
        let n = reader
            .take((MAX_IPC_FRAME_BYTES + 1) as u64)
            .read_line(&mut line)
            .await?;

        if n == 0 {
            return Ok(None);
        }
        if n > MAX_IPC_FRAME_BYTES {
            return Err(anyhow!(
                "IPC frame exceeds maximum size of {} bytes",
                MAX_IPC_FRAME_BYTES
            ));
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response: IpcResponse = serde_json::from_str(trimmed)
            .with_context(|| format!("Failed to deserialize IPC response: {}", trimmed))?;
        return Ok(Some(response));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    fn temp_socket_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mycel-shell-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("runtime.sock")
    }

    async fn write_frame(writer: &mut OwnedWriteHalf, resp: &IpcResponse) {
        let mut s = serde_json::to_string(resp).unwrap();
        s.push('\n');
        writer.write_all(s.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn read_req(reader: &mut BufReader<OwnedReadHalf>) -> Option<IpcRequest> {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap();
        if n == 0 {
            return None;
        }
        Some(serde_json::from_str(line.trim()).unwrap())
    }

    #[test]
    fn token_path_replaces_extension() {
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
    async fn token_file_auto_read_and_rotation_retry() {
        let sock_path = temp_socket_path("token-rotate");
        let sock_str = sock_path.to_string_lossy().to_string();
        let tok_path = token_path_for_socket(&sock_str);

        // Start with a stale token in the token file; server expects "new-token".
        // Wait: to test rotation retry on a single client call, the mock server
        // rejects the first Authenticate attempt and rotates the token file before
        // returning Error, so the client's retry reads "new-token" and succeeds.
        std::fs::write(&tok_path, "old-token\n").unwrap();

        let listener = UnixListener::bind(&sock_path).unwrap();
        let tok_path_clone = tok_path.clone();

        let server = tokio::spawn(async move {
            // Connection 1: reject old-token, rotate token file.
            let (stream1, _) = listener.accept().await.unwrap();
            let (r1, mut w1) = stream1.into_split();
            let mut r1 = BufReader::new(r1);
            match read_req(&mut r1).await.unwrap() {
                IpcRequest::Authenticate { token } => assert_eq!(token, "old-token"),
                other => panic!("unexpected req: {:?}", other),
            }
            std::fs::write(&tok_path_clone, "new-token\n").unwrap();
            write_frame(
                &mut w1,
                &IpcResponse::Error {
                    message: "Invalid authentication token".to_string(),
                },
            )
            .await;

            // Connection 2: accept new-token, SetSession, Status.
            let (stream2, _) = listener.accept().await.unwrap();
            let (r2, mut w2) = stream2.into_split();
            let mut r2 = BufReader::new(r2);
            match read_req(&mut r2).await.unwrap() {
                IpcRequest::Authenticate { token } => assert_eq!(token, "new-token"),
                other => panic!("unexpected req: {:?}", other),
            }
            write_frame(
                &mut w2,
                &IpcResponse::Ok {
                    message: "Authenticated".to_string(),
                },
            )
            .await;

            match read_req(&mut r2).await.unwrap() {
                IpcRequest::SetSession { id } => assert_eq!(id, "sess-rot"),
                other => panic!("unexpected req: {:?}", other),
            }
            write_frame(
                &mut w2,
                &IpcResponse::Ok {
                    message: "Session set".to_string(),
                },
            )
            .await;

            match read_req(&mut r2).await.unwrap() {
                IpcRequest::Status => {}
                other => panic!("unexpected req: {:?}", other),
            }
            write_frame(
                &mut w2,
                &IpcResponse::Status {
                    version: "0.1.0".to_string(),
                    uptime: 99,
                    sessions: 1,
                    llm_model: "qwen2.5".to_string(),
                    cloud_spend_usd: 0.25,
                },
            )
            .await;
        });

        let client = IpcRequestClient::new(IpcClientConfig {
            socket_path: sock_str,
            session_id: "sess-rot".to_string(),
        });
        let status = client.fetch_status().await.unwrap();
        assert_eq!(status.version, "0.1.0");
        assert_eq!(status.uptime, 99);
        assert_eq!(status.llm_model, "qwen2.5");
        assert!((status.cloud_spend_usd - 0.25).abs() < f64::EPSILON);

        server.await.unwrap();
        let _ = std::fs::remove_dir_all(sock_path.parent().unwrap());
    }

    #[tokio::test]
    async fn live_chat_chunk_streaming_and_terminal_response() {
        let sock_path = temp_socket_path("chat-stream");
        let sock_str = sock_path.to_string_lossy().to_string();
        let tok_path = token_path_for_socket(&sock_str);
        std::fs::write(&tok_path, "stream-tok").unwrap();

        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut r = BufReader::new(r);

            assert_eq!(
                read_req(&mut r).await.unwrap(),
                IpcRequest::Authenticate {
                    token: "stream-tok".to_string()
                }
            );
            write_frame(
                &mut w,
                &IpcResponse::Ok {
                    message: "ok".to_string(),
                },
            )
            .await;

            assert_eq!(
                read_req(&mut r).await.unwrap(),
                IpcRequest::SetSession {
                    id: "sess-chat".to_string()
                }
            );
            write_frame(
                &mut w,
                &IpcResponse::Ok {
                    message: "ok".to_string(),
                },
            )
            .await;

            assert_eq!(
                read_req(&mut r).await.unwrap(),
                IpcRequest::Chat {
                    message: "tell me a joke".to_string(),
                    provider: LlmProvider::Cloud,
                }
            );

            for delta in ["Why ", "did ", "the ", "crab?"] {
                write_frame(
                    &mut w,
                    &IpcResponse::ChatChunk {
                        delta: delta.to_string(),
                    },
                )
                .await;
            }
            write_frame(
                &mut w,
                &IpcResponse::Chat {
                    response: "Why did the crab?".to_string(),
                    surface: None,
                },
            )
            .await;
        });

        let client = IpcRequestClient::new(IpcClientConfig {
            socket_path: sock_str,
            session_id: "sess-chat".to_string(),
        });
        let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel();
        let terminal = client
            .send_chat_streaming("tell me a joke".to_string(), LlmProvider::Cloud, chunk_tx)
            .await
            .unwrap();

        let mut chunks = Vec::new();
        while let Some(c) = chunk_rx.recv().await {
            chunks.push(c);
        }
        assert_eq!(chunks, vec!["Why ", "did ", "the ", "crab?"]);
        assert_eq!(
            terminal,
            IpcResponse::Chat {
                response: "Why did the crab?".to_string(),
                surface: None,
            }
        );

        server.await.unwrap();
        let _ = std::fs::remove_dir_all(sock_path.parent().unwrap());
    }

    #[tokio::test]
    async fn push_subscriber_receives_events_and_reconnects_with_rotated_token() {
        let sock_path = temp_socket_path("sub-reconnect");
        let sock_str = sock_path.to_string_lossy().to_string();
        let tok_path = token_path_for_socket(&sock_str);
        std::fs::write(&tok_path, "sub-tok-1").unwrap();

        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn(async move {
            // Generation 1: accept, auth with sub-tok-1, push 4 events, then drop connection.
            {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);

                assert_eq!(
                    read_req(&mut r).await.unwrap(),
                    IpcRequest::Authenticate {
                        token: "sub-tok-1".to_string()
                    }
                );
                write_frame(
                    &mut w,
                    &IpcResponse::Ok {
                        message: "ok".to_string(),
                    },
                )
                .await;

                assert_eq!(
                    read_req(&mut r).await.unwrap(),
                    IpcRequest::Subscribe { events: None }
                );
                write_frame(
                    &mut w,
                    &IpcResponse::Ok {
                        message: "Subscribed".to_string(),
                    },
                )
                .await;

                let gen1_events = vec![
                    SystemEvent::ExecutionFailed {
                        language: "bash".to_string(),
                        failure_class: "non_zero_exit".to_string(),
                        exit_code: Some(1),
                        signal: None,
                        timed_out: false,
                        sandbox_mode: "namespace".to_string(),
                    },
                    SystemEvent::RecoveryDecided {
                        decision: "retry".to_string(),
                        reason: "transient".to_string(),
                        verified: true,
                        attempts: 1,
                    },
                    SystemEvent::ConfirmationRequired {
                        id: "c-1".to_string(),
                        session_id: "s-1".to_string(),
                        command: "rm -rf /tmp/foo".to_string(),
                        reason: "destructive".to_string(),
                    },
                    SystemEvent::SyncStateChanged {
                        peers_connected: 2,
                        state: "connected".to_string(),
                    },
                ];
                for ev in gen1_events {
                    write_frame(&mut w, &IpcResponse::Event { event: ev }).await;
                }
                // Rotate token before dropping generation 1 so generation 2 sees the new token.
                std::fs::write(&tok_path, "sub-tok-2").unwrap();
            }

            // Generation 2: accept, auth with sub-tok-2, push remaining 3 events.
            {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);

                assert_eq!(
                    read_req(&mut r).await.unwrap(),
                    IpcRequest::Authenticate {
                        token: "sub-tok-2".to_string()
                    }
                );
                write_frame(
                    &mut w,
                    &IpcResponse::Ok {
                        message: "ok".to_string(),
                    },
                )
                .await;

                assert_eq!(
                    read_req(&mut r).await.unwrap(),
                    IpcRequest::Subscribe { events: None }
                );
                write_frame(
                    &mut w,
                    &IpcResponse::Ok {
                        message: "Subscribed".to_string(),
                    },
                )
                .await;

                let gen2_events = vec![
                    SystemEvent::ProviderHealthChanged {
                        provider: "ollama".to_string(),
                        available: true,
                        detail: "healthy".to_string(),
                    },
                    SystemEvent::McpServerRestarted {
                        name: "fs".to_string(),
                    },
                    SystemEvent::ToolCalled {
                        tool_name: "read".to_string(),
                        server_name: "fs".to_string(),
                        success: true,
                        response_time_ms: 8,
                    },
                ];
                for ev in gen2_events {
                    write_frame(&mut w, &IpcResponse::Event { event: ev }).await;
                }
                // Keep socket open briefly so client reads all 3 events before EOF.
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });

        let (mut rx, handle) = spawn_event_subscriber(
            sock_str,
            None,
            Duration::from_millis(10),
            Duration::from_millis(50),
        );

        let mut received_kinds = Vec::new();
        let mut connected_count = 0usize;
        let mut disconnected_count = 0usize;

        while received_kinds.len() < 7 {
            let msg = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("timed out waiting for subscriber event")
                .expect("subscriber channel closed early");

            match msg {
                SubscriberEvent::Connected => connected_count += 1,
                SubscriberEvent::Disconnected(_) => disconnected_count += 1,
                SubscriberEvent::Event(ev) => received_kinds.push(ev.kind().to_string()),
            }
        }

        assert_eq!(connected_count, 2);
        assert!(disconnected_count >= 1);
        assert_eq!(
            received_kinds,
            vec![
                "ExecutionFailed",
                "RecoveryDecided",
                "ConfirmationRequired",
                "SyncStateChanged",
                "ProviderHealthChanged",
                "McpServerRestarted",
                "ToolCalled",
            ]
        );

        drop(rx);
        handle.abort();
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(sock_path.parent().unwrap());
    }

    #[tokio::test]
    async fn confirmation_required_and_confirm_action_preserves_session() {
        let sock_path = temp_socket_path("confirm-action");
        let sock_str = sock_path.to_string_lossy().to_string();
        let tok_path = token_path_for_socket(&sock_str);
        std::fs::write(&tok_path, "conf-tok").unwrap();

        let listener = UnixListener::bind(&sock_path).unwrap();
        let observed_sessions = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed_clone = Arc::clone(&observed_sessions);

        let server = tokio::spawn(async move {
            for idx in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);

                assert_eq!(
                    read_req(&mut r).await.unwrap(),
                    IpcRequest::Authenticate {
                        token: "conf-tok".to_string()
                    }
                );
                write_frame(
                    &mut w,
                    &IpcResponse::Ok {
                        message: "ok".to_string(),
                    },
                )
                .await;

                match read_req(&mut r).await.unwrap() {
                    IpcRequest::SetSession { id } => {
                        observed_clone.lock().await.push(id);
                    }
                    other => panic!("expected SetSession, got {:?}", other),
                }
                write_frame(
                    &mut w,
                    &IpcResponse::Ok {
                        message: "ok".to_string(),
                    },
                )
                .await;

                if idx == 0 {
                    match read_req(&mut r).await.unwrap() {
                        IpcRequest::Chat { .. } => {}
                        other => panic!("expected Chat, got {:?}", other),
                    }
                    write_frame(
                        &mut w,
                        &IpcResponse::ConfirmationRequired {
                            id: "act-77".to_string(),
                            action: "shell".to_string(),
                            code: "rm -rf /tmp/demo".to_string(),
                            reason: "destructive path".to_string(),
                        },
                    )
                    .await;
                } else {
                    assert_eq!(
                        read_req(&mut r).await.unwrap(),
                        IpcRequest::ConfirmAction {
                            id: "act-77".to_string(),
                            approved: true,
                        }
                    );
                    write_frame(
                        &mut w,
                        &IpcResponse::CodeResult {
                            code: "rm -rf /tmp/demo".to_string(),
                            output: "removed".to_string(),
                            success: true,
                        },
                    )
                    .await;
                }
            }
        });

        let client = IpcRequestClient::new(IpcClientConfig {
            socket_path: sock_str,
            session_id: "sess-confirm-42".to_string(),
        });

        let (chunk_tx, _chunk_rx) = mpsc::unbounded_channel();
        let chat_resp = client
            .send_chat_streaming("delete /tmp/demo".to_string(), LlmProvider::Auto, chunk_tx)
            .await
            .unwrap();

        let action_id = match chat_resp {
            IpcResponse::ConfirmationRequired { id, .. } => id,
            other => panic!("expected ConfirmationRequired, got {:?}", other),
        };
        assert_eq!(action_id, "act-77");

        let confirm_resp = client.confirm_action(action_id, true).await.unwrap();
        assert_eq!(
            confirm_resp,
            IpcResponse::CodeResult {
                code: "rm -rf /tmp/demo".to_string(),
                output: "removed".to_string(),
                success: true,
            }
        );

        server.await.unwrap();
        assert_eq!(
            *observed_sessions.lock().await,
            vec!["sess-confirm-42", "sess-confirm-42"]
        );
        let _ = std::fs::remove_dir_all(sock_path.parent().unwrap());
    }

    #[tokio::test]
    async fn providers_and_mesh_pairing_ipc_roundtrips() {
        use crate::protocol::PeerStatus;

        let sock_path = temp_socket_path("providers-mesh");
        let sock_str = sock_path.to_string_lossy().to_string();
        let tok_path = token_path_for_socket(&sock_str);
        std::fs::write(&tok_path, "pm-tok").unwrap();

        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn(async move {
            for idx in 0..6 {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);

                assert_eq!(
                    read_req(&mut r).await.unwrap(),
                    IpcRequest::Authenticate {
                        token: "pm-tok".to_string()
                    }
                );
                write_frame(
                    &mut w,
                    &IpcResponse::Ok {
                        message: "ok".to_string(),
                    },
                )
                .await;

                assert_eq!(
                    read_req(&mut r).await.unwrap(),
                    IpcRequest::SetSession {
                        id: "sess-pm".to_string()
                    }
                );
                write_frame(
                    &mut w,
                    &IpcResponse::Ok {
                        message: "ok".to_string(),
                    },
                )
                .await;

                match idx {
                    0 => {
                        assert_eq!(read_req(&mut r).await.unwrap(), IpcRequest::ListProviders);
                        write_frame(
                            &mut w,
                            &IpcResponse::Providers {
                                active_provider: Some("venice".to_string()),
                                active_model: Some("llama-3.3-70b".to_string()),
                                providers: vec![ProviderSummary {
                                    id: "venice".to_string(),
                                    name: "Venice AI".to_string(),
                                    api: "openai-compatible".to_string(),
                                    configured: true,
                                    env_var: "VENICE_API_KEY".to_string(),
                                    default_model: "llama-3.3-70b".to_string(),
                                    base_url: "https://api.venice.ai/api/v1".to_string(),
                                }],
                            },
                        )
                        .await;
                    }
                    1 => {
                        assert_eq!(
                            read_req(&mut r).await.unwrap(),
                            IpcRequest::SetProvider {
                                provider: "openrouter".to_string(),
                                model: Some("anthropic/claude-sonnet-4".to_string()),
                            }
                        );
                        write_frame(
                            &mut w,
                            &IpcResponse::Ok {
                                message: "Active cloud provider set to openrouter/anthropic/claude-sonnet-4".to_string(),
                            },
                        )
                        .await;
                    }
                    2 => {
                        assert_eq!(read_req(&mut r).await.unwrap(), IpcRequest::SyncStatus);
                        write_frame(
                            &mut w,
                            &IpcResponse::SyncStatus {
                                enabled: true,
                                device_id: "node-alpha".to_string(),
                                transport: "wireguard".to_string(),
                                interface: Some("mycel0".to_string()),
                                listen_addr: Some("0.0.0.0:51820".to_string()),
                                trusted_peers: 1,
                                connected_peers: 1,
                                event_log_len: 42,
                                peers: vec![PeerInfo {
                                    id: "node-beta".to_string(),
                                    name: "beta".to_string(),
                                    status: PeerStatus::Connected,
                                    addresses: vec!["10.42.0.2:51820".to_string()],
                                }],
                            },
                        )
                        .await;
                    }
                    3 => {
                        assert_eq!(
                            read_req(&mut r).await.unwrap(),
                            IpcRequest::GeneratePairingCode {
                                endpoint: Some("10.42.0.1:51820".to_string()),
                            }
                        );
                        write_frame(
                            &mut w,
                            &IpcResponse::PairingCode {
                                device_id: "node-alpha".to_string(),
                                code: "mycel-pair-v1:node-alpha@10.42.0.1:51820".to_string(),
                                endpoint: Some("10.42.0.1:51820".to_string()),
                            },
                        )
                        .await;
                    }
                    4 => {
                        assert_eq!(
                            read_req(&mut r).await.unwrap(),
                            IpcRequest::PairPeer {
                                code: "mycel-pair-v1:node-gamma@10.42.0.3:51820".to_string(),
                            }
                        );
                        write_frame(
                            &mut w,
                            &IpcResponse::PeerAdded {
                                peer: PeerInfo {
                                    id: "node-gamma".to_string(),
                                    name: "gamma".to_string(),
                                    status: PeerStatus::Connected,
                                    addresses: vec!["10.42.0.3:51820".to_string()],
                                },
                                synced_events: 5,
                            },
                        )
                        .await;
                    }
                    _ => {
                        assert_eq!(
                            read_req(&mut r).await.unwrap(),
                            IpcRequest::TriggerSync { peer_id: None }
                        );
                        write_frame(
                            &mut w,
                            &IpcResponse::Ok {
                                message: "Synced 3 event(s) across mesh".to_string(),
                            },
                        )
                        .await;
                    }
                }
            }
        });

        let client = IpcRequestClient::new(IpcClientConfig {
            socket_path: sock_str,
            session_id: "sess-pm".to_string(),
        });

        let providers = client.fetch_providers().await.unwrap();
        assert_eq!(providers.active_provider.as_deref(), Some("venice"));
        assert_eq!(providers.active_model.as_deref(), Some("llama-3.3-70b"));
        assert_eq!(providers.providers.len(), 1);
        assert_eq!(providers.providers[0].id, "venice");

        let set_msg = client
            .set_provider(
                "openrouter".to_string(),
                Some("anthropic/claude-sonnet-4".to_string()),
            )
            .await
            .unwrap();
        assert!(set_msg.contains("openrouter"));

        let sync = client.fetch_sync_status().await.unwrap();
        assert!(sync.enabled);
        assert_eq!(sync.device_id, "node-alpha");
        assert_eq!(sync.interface.as_deref(), Some("mycel0"));
        assert_eq!(sync.event_log_len, 42);
        assert_eq!(sync.peers.len(), 1);

        let pairing = client
            .generate_pairing_code(Some("10.42.0.1:51820".to_string()))
            .await
            .unwrap();
        assert_eq!(pairing.device_id, "node-alpha");
        assert!(pairing.code.starts_with("mycel-pair-v1:"));
        assert_eq!(pairing.endpoint.as_deref(), Some("10.42.0.1:51820"));

        let paired = client
            .pair_peer("mycel-pair-v1:node-gamma@10.42.0.3:51820".to_string())
            .await
            .unwrap();
        assert_eq!(paired.peer.id, "node-gamma");
        assert_eq!(paired.synced_events, 5);

        let sync_msg = client.trigger_sync(None).await.unwrap();
        assert_eq!(sync_msg, "Synced 3 event(s) across mesh");

        server.await.unwrap();
        let _ = std::fs::remove_dir_all(sock_path.parent().unwrap());
    }
}
