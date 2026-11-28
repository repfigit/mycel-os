//! Device mesh synchronization
//!
//! Syncs preferences, patterns, and commands between the user's own Mycel
//! devices using x25519 + ChaCha20-Poly1305 over UDP, with vector clocks for
//! causal ordering.
//!
//! Trust model:
//! - The mesh is off unless `[mesh] enabled = true`. When off, no socket is
//!   bound, nothing is discovered, and no events are produced.
//! - Only devices whose Mycel ID (base64 x25519 public key) is listed in
//!   `[mesh] trusted_peers` are accepted. Handshakes, mDNS results, and NEAR
//!   peer lists naming any other key are ignored.
//! - Handshakes and mDNS records are unauthenticated hints about *where* a
//!   trusted device can be reached. Authenticity comes from the AEAD on every
//!   event: a packet that decrypts under the shared secret with a trusted peer
//!   could only have been produced by that peer (or by us), so the event's
//!   `device_id` must equal that peer's ID.
//! - Executable code received from the mesh is never installed or run
//!   automatically.
//! - Conversation history is only replicated when `[mesh] sync_ai_context`
//!   is explicitly enabled.

pub mod log;
pub mod nat;
pub mod relay;
pub mod wireguard;

use crate::config::{MeshTransportMode, MycelConfig};
use crate::events::SystemEvent;
use crate::mcp::McpManager;
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305,
};
use chrono::{DateTime, Utc};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, warn};
use x25519_dalek::{PublicKey, StaticSecret};

/// Upper bound on remembered addresses per peer. Handshakes and mDNS records
/// are unauthenticated, so this keeps spoofed hints from growing without bound.
const MAX_PEER_ADDRESSES: usize = 8;

/// Maximum number of events retained in the durable sync log.
const EVENT_LOG_CAP: usize = 512;

/// Maximum size of a single newline-delimited encrypted TCP sync frame (1 MiB).
const MAX_SYNC_FRAME_BYTES: usize = 1024 * 1024;

/// Timeout for establishing outbound TCP connections to peers.
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for reading/writing frames on an established TCP sync stream.
const TCP_IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Vector Clock for tracking causality across devices
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VectorClock {
    pub map: HashMap<String, u64>,
}

impl VectorClock {
    pub fn increment(&mut self, device_id: &str) {
        let count = self.map.entry(device_id.to_string()).or_insert(0);
        *count += 1;
    }

    pub fn merge(&mut self, other: &VectorClock) {
        for (device_id, &count) in &other.map {
            let entry = self.map.entry(device_id.clone()).or_insert(0);
            if count > *entry {
                *entry = count;
            }
        }
    }

    pub fn is_ahead_of(&self, other: &VectorClock) -> bool {
        let mut ahead = false;

        // 1. Check all keys in self against other
        for (device_id, &count) in &self.map {
            let other_count = other.map.get(device_id).cloned().unwrap_or(0);
            if count < other_count {
                return false;
            }
            if count > other_count {
                ahead = true;
            }
        }

        // 2. Check if other has keys self doesn't have
        for device_id in other.map.keys() {
            if !self.map.contains_key(device_id) {
                return false;
            }
        }

        ahead
    }
}

/// A single event in the synchronization log
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEvent {
    pub id: String,
    pub device_id: String,
    pub timestamp: DateTime<Utc>,
    pub clock: VectorClock,
    pub operation: SyncOperation,
    /// Reserved. Sender authenticity is currently provided by the per-peer
    /// AEAD (see module docs); this field is not populated or checked.
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncOperation {
    AddConversationTurn {
        session_id: String,
        user: String,
        assistant: String,
    },
    UpdatePreference {
        key: String,
        value: String,
    },
    AddLearnedPattern {
        trigger: String,
        action: String,
    },
    AddCapability {
        name: String,
        language: String,
        code: String,
    },
    /// A user-defined command to replicate across the personal mesh.
    DefineCommand {
        name: String,
        description: String,
        /// JSON-serialized Vec<CommandStep> (kept opaque to avoid a sync->commands dep).
        steps_json: String,
        params: Vec<String>,
        script: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    pub enabled: bool,
    pub mesh_port: u16,
    pub discovery_enabled: bool,
    pub device_name: String,
    pub blockchain_sync: bool,
    pub near_account: Option<String>,
    /// Mycel IDs allowed to exchange events with this device.
    pub trusted_peers: HashSet<String>,
    /// Replicate conversation turns to trusted peers.
    pub sync_ai_context: bool,
    pub transport: MeshTransportMode,
    pub interface: String,
    pub sync_port: u16,
    pub wg_address: Option<String>,
    pub peer_endpoints: HashMap<String, String>,
    pub stun_servers: Vec<String>,
    pub persistent_keepalive_secs: u16,
    pub relay_enabled: bool,
    pub relay_addr: Option<String>,
    pub relay_server_enabled: bool,
    pub relay_listen_addr: Option<String>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mesh_port: 51820,
            discovery_enabled: false,
            device_name: "mycel-device".to_string(),
            blockchain_sync: false,
            near_account: None,
            trusted_peers: HashSet::new(),
            sync_ai_context: false,
            transport: MeshTransportMode::Auto,
            interface: "mycel0".to_string(),
            sync_port: 51821,
            wg_address: None,
            peer_endpoints: HashMap::new(),
            stun_servers: Vec::new(),
            persistent_keepalive_secs: 25,
            relay_enabled: false,
            relay_addr: None,
            relay_server_enabled: false,
            relay_listen_addr: None,
        }
    }
}

#[derive(Clone)]
struct DeviceKeys {
    pub private: StaticSecret,
    pub public: PublicKey,
}

impl std::fmt::Debug for DeviceKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceKeys")
            .field("public", &self.public)
            .field("private", &"[REDACTED]")
            .finish()
    }
}

impl DeviceKeys {
    pub fn load_or_generate(path: &str) -> Result<Self> {
        let key_path = std::path::Path::new(path).join("device_key");
        if key_path.exists() {
            restrict_to_owner(&key_path)?;
            let bytes = std::fs::read(&key_path)?;
            if bytes.len() != 32 {
                return Err(anyhow!("Invalid key file length"));
            }
            let mut key_bytes = [0u8; 32];
            key_bytes.copy_from_slice(&bytes);
            let private = StaticSecret::from(key_bytes);
            let public = PublicKey::from(&private);
            Ok(Self { private, public })
        } else {
            info!("Generating new device keys...");
            let mut rng = rand::thread_rng();
            let private = StaticSecret::random_from_rng(&mut rng);
            let public = PublicKey::from(&private);
            std::fs::create_dir_all(path)?;
            write_owner_only(&key_path, &private.to_bytes())?;
            Ok(Self { private, public })
        }
    }

    fn id(&self) -> String {
        encode_id(self.public.as_bytes())
    }
}

/// Create a new file readable and writable only by the owner (0600).
fn write_owner_only(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

/// Tighten an existing secret file to 0600 if it is group/world accessible.
fn restrict_to_owner(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        warn!(
            "Device key {:?} had mode {:o}; restricting to 0600",
            path,
            mode & 0o777
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn encode_id(public_key: &[u8]) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, public_key)
}

fn decode_id(id: &str) -> Option<PublicKey> {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, id).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    Some(PublicKey::from(arr))
}

fn cipher_for(own: &StaticSecret, peer: &PublicKey) -> ChaCha20Poly1305 {
    let shared = own.diffie_hellman(peer);
    ChaCha20Poly1305::new(shared.as_bytes().into())
}

/// Encrypt an event for one peer.
fn seal_event(own: &StaticSecret, peer: &PublicKey, event: &SyncEvent) -> Result<MeshPacket> {
    use rand::RngCore;
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let plaintext = serde_json::to_vec(event)?;
    let encrypted_data = cipher_for(own, peer)
        .encrypt(
            &nonce.into(),
            Payload {
                msg: &plaintext,
                aad: &[],
            },
        )
        .map_err(|e| anyhow!("Encryption error: {}", e))?;
    Ok(MeshPacket::Event {
        nonce,
        encrypted_data,
    })
}

#[derive(Debug, Clone, Default)]
struct PeerWireGuardMeta {
    wg_ip: Option<String>,
    wg_endpoint: Option<String>,
}

struct SyncState {
    peers: HashMap<String, PeerInfo>,
    peer_wg: HashMap<String, PeerWireGuardMeta>,
    trusted_peers: HashSet<String>,
    log: log::SyncLog,
    local_clock: VectorClock,
    active_transport: String,
    wg_interface: Option<String>,
    wg: Option<wireguard::KernelWireGuard>,
    tcp_listen_addr: Option<SocketAddr>,
    public_endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedPeerMeta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    addresses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wg_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wg_endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedTrustedPeers {
    #[serde(default)]
    trusted_peers: Vec<String>,
    #[serde(default)]
    peers: HashMap<String, PersistedPeerMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum TrustedPeersFile {
    Structured(PersistedTrustedPeers),
    LegacyList(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingPayload {
    device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wg_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sync_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireFrame {
    sender_id: String,
    nonce: [u8; 12],
    encrypted_payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum SyncMessage {
    PushEvent {
        event: SyncEvent,
    },
    PullSince {
        clock: VectorClock,
    },
    EventBatch {
        events: Vec<SyncEvent>,
        sender_clock: VectorClock,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshStatusInfo {
    pub enabled: bool,
    pub device_id: String,
    pub transport: String,
    pub interface: Option<String>,
    pub listen_addr: Option<String>,
    pub trusted_peers: usize,
    pub connected_peers: usize,
    pub event_log_len: usize,
    pub peers: Vec<PeerInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_endpoint: Option<String>,
    #[serde(default)]
    pub relay_connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingCodeInfo {
    pub device_id: String,
    pub code: String,
    pub endpoint: Option<String>,
}

fn seal_sync_message(
    own: &StaticSecret,
    own_id: &str,
    peer: &PublicKey,
    msg: &SyncMessage,
) -> Result<WireFrame> {
    use rand::RngCore;
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let plaintext = serde_json::to_vec(msg)?;
    let encrypted_payload = cipher_for(own, peer)
        .encrypt(
            &nonce.into(),
            Payload {
                msg: &plaintext,
                aad: &[],
            },
        )
        .map_err(|e| anyhow!("Sync message encryption error: {}", e))?;
    Ok(WireFrame {
        sender_id: own_id.to_string(),
        nonce,
        encrypted_payload,
    })
}

async fn write_wire_frame(stream: &mut TcpStream, frame: &WireFrame) -> Result<()> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    tokio::time::timeout(TCP_IO_TIMEOUT, stream.write_all(line.as_bytes()))
        .await
        .map_err(|_| anyhow!("Timed out writing sync frame"))??;
    tokio::time::timeout(TCP_IO_TIMEOUT, stream.flush())
        .await
        .map_err(|_| anyhow!("Timed out flushing sync frame"))??;
    Ok(())
}

async fn read_wire_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<WireFrame>> {
    let mut line = String::new();
    let n = tokio::time::timeout(
        TCP_IO_TIMEOUT,
        reader
            .take((MAX_SYNC_FRAME_BYTES + 1) as u64)
            .read_line(&mut line),
    )
    .await
    .map_err(|_| anyhow!("Timed out reading sync frame"))??;

    if n == 0 {
        return Ok(None);
    }
    if n > MAX_SYNC_FRAME_BYTES {
        return Err(anyhow!(
            "Sync frame exceeds MAX_SYNC_FRAME_BYTES ({} bytes)",
            MAX_SYNC_FRAME_BYTES
        ));
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let frame: WireFrame =
        serde_json::from_str(trimmed).context("Failed to deserialize WireFrame")?;
    Ok(Some(frame))
}

fn load_persisted_trusted_peers(path: &Path, own_id: &str) -> Result<PersistedTrustedPeers> {
    if !path.exists() {
        return Ok(PersistedTrustedPeers::default());
    }
    restrict_to_owner(path)?;
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read trusted peers at {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(PersistedTrustedPeers::default());
    }

    let parsed = match serde_json::from_str::<TrustedPeersFile>(&raw) {
        Ok(TrustedPeersFile::Structured(s)) => s,
        Ok(TrustedPeersFile::LegacyList(list)) => PersistedTrustedPeers {
            trusted_peers: list,
            peers: HashMap::new(),
        },
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "Failed to parse trusted_peers.json; ignoring corrupt file"
            );
            return Ok(PersistedTrustedPeers::default());
        }
    };

    let mut clean = PersistedTrustedPeers::default();
    for raw_id in parsed.trusted_peers {
        let id = raw_id.trim();
        if id.is_empty() || id == own_id || decode_id(id).is_none() {
            continue;
        }
        if !clean.trusted_peers.iter().any(|existing| existing == id) {
            clean.trusted_peers.push(id.to_string());
        }
    }
    for (raw_id, meta) in parsed.peers {
        let id = raw_id.trim();
        if id.is_empty() || id == own_id || decode_id(id).is_none() {
            continue;
        }
        clean.peers.insert(id.to_string(), meta);
    }

    Ok(clean)
}

fn write_trusted_peers_atomically(path: &Path, data: &PersistedTrustedPeers) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "Failed to create trusted peers directory {}",
            parent.display()
        )
    })?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("trusted_peers.json");
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", uuid::Uuid::new_v4()));

    let json =
        serde_json::to_string_pretty(data).context("Failed to serialize trusted_peers.json")?;

    let write_res = (|| -> Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&tmp_path)
            .with_context(|| format!("Failed to create temp file {}", tmp_path.display()))?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp_path, path)
            .with_context(|| format!("Failed to rename temp file to {}", path.display()))?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    Ok(())
}

fn format_short_peer_name(peer_id: &str) -> String {
    let short: String = peer_id.chars().take(8).collect();
    format!("peer-{short}")
}

#[derive(Clone)]
pub struct SyncService {
    sync_config: SyncConfig,
    context_path: PathBuf,
    state: Arc<RwLock<SyncState>>,
    keys: Arc<DeviceKeys>,
    mdns: Option<ServiceDaemon>,
    mcp_manager: Arc<Option<McpManager>>,
    /// Bound only when the mesh is enabled in Direct mode.
    socket: Arc<RwLock<Option<Arc<UdpSocket>>>>,
    event_bus: broadcast::Sender<SystemEvent>,
    /// Optional command registry for replicating user-defined commands.
    command_registry: Arc<RwLock<Option<crate::commands::CommandRegistry>>>,
    /// Optional context manager for replicating preferences and conversation turns.
    context_manager: Arc<RwLock<Option<crate::context::ContextManager>>>,
    /// Shutdown signal for background listener loops.
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// Optional zero-knowledge relay client for NAT-traversed fallback.
    relay_client: Arc<RwLock<Option<relay::RelayClient>>>,
}

#[derive(Debug, Serialize, Deserialize)]
enum MeshPacket {
    Handshake {
        public_key: Vec<u8>,
    },
    Event {
        nonce: [u8; 12],
        encrypted_data: Vec<u8>,
    },
}

impl SyncService {
    pub async fn new(
        config: &MycelConfig,
        mcp_manager: Option<McpManager>,
        event_bus: broadcast::Sender<SystemEvent>,
    ) -> Result<Self> {
        // Keys exist even while the mesh is off so the user can read their
        // Mycel ID and add it to their other devices before enabling.
        let keys = DeviceKeys::load_or_generate(&config.context_path)?;
        let own_id = keys.id();
        let mesh = &config.mesh;
        let context_path = PathBuf::from(&config.context_path);

        let log_path = context_path.join("sync_log.jsonl");
        let sync_log = log::SyncLog::load(log_path, EVENT_LOG_CAP)?;
        let local_clock = sync_log.reconstructed_clock();

        let mut trusted_peers = HashSet::new();
        for raw in &mesh.trusted_peers {
            let id = raw.trim();
            if id.is_empty() || id == own_id {
                continue;
            }
            if decode_id(id).is_none() {
                warn!("Ignoring invalid trusted_peers entry: {:?}", id);
                continue;
            }
            trusted_peers.insert(id.to_string());
        }

        let persisted_path = context_path.join("trusted_peers.json");
        let persisted = load_persisted_trusted_peers(&persisted_path, &own_id)?;
        for id in persisted.trusted_peers {
            trusted_peers.insert(id);
        }

        let mut peers: HashMap<String, PeerInfo> = HashMap::new();
        let mut peer_wg: HashMap<String, PeerWireGuardMeta> = HashMap::new();
        for peer_id in &trusted_peers {
            let mut addresses = Vec::new();
            if let Some(ep) = mesh.peer_endpoints.get(peer_id) {
                let trimmed = ep.trim();
                if !trimmed.is_empty() {
                    addresses.push(trimmed.to_string());
                }
            }
            let mut name = None;
            if let Some(meta) = persisted.peers.get(peer_id) {
                if meta.name.is_some() {
                    name = meta.name.clone();
                }
                for addr in &meta.addresses {
                    if !addresses.iter().any(|a| a == addr) && addresses.len() < MAX_PEER_ADDRESSES
                    {
                        addresses.push(addr.clone());
                    }
                }
                if meta.wg_ip.is_some() || meta.wg_endpoint.is_some() {
                    peer_wg.insert(
                        peer_id.clone(),
                        PeerWireGuardMeta {
                            wg_ip: meta.wg_ip.clone(),
                            wg_endpoint: meta.wg_endpoint.clone(),
                        },
                    );
                }
            }
            peers.insert(
                peer_id.clone(),
                PeerInfo {
                    id: peer_id.clone(),
                    name: name.unwrap_or_else(|| format_short_peer_name(peer_id)),
                    status: PeerStatus::Disconnected,
                    addresses,
                },
            );
        }

        let sync_config = SyncConfig {
            enabled: mesh.enabled,
            mesh_port: mesh.listen_port,
            discovery_enabled: mesh.enabled && mesh.mdns_enabled,
            device_name: "mycel-device".to_string(),
            blockchain_sync: config.blockchain_sync,
            near_account: config.near_account.clone(),
            trusted_peers: trusted_peers.clone(),
            sync_ai_context: mesh.sync_ai_context,
            transport: mesh.transport,
            interface: mesh.interface.clone(),
            sync_port: mesh.sync_port,
            wg_address: mesh.wg_address.clone(),
            peer_endpoints: mesh.peer_endpoints.clone(),
            stun_servers: mesh.stun_servers.clone(),
            persistent_keepalive_secs: mesh.persistent_keepalive_secs,
            relay_enabled: mesh.relay_enabled,
            relay_addr: mesh.relay_addr.clone(),
            relay_server_enabled: mesh.relay_server_enabled,
            relay_listen_addr: mesh.relay_listen_addr.clone(),
        };

        // Do NOT bind UDP mesh_port here when transport is WireGuard or Auto,
        // because kernel WireGuard needs to bind `listen-port` on mesh_port.
        let socket = if sync_config.enabled {
            if trusted_peers.is_empty() {
                warn!("Mesh enabled but [mesh] trusted_peers is empty; no device will be accepted");
            }
            if matches!(mesh.transport, MeshTransportMode::Direct) {
                let bound = Self::bind_direct_udp_socket(sync_config.mesh_port).await?;
                Some(bound)
            } else {
                None
            }
        } else {
            None
        };

        let mdns = if sync_config.discovery_enabled {
            Some(ServiceDaemon::new()?)
        } else {
            None
        };

        let initial_transport = if sync_config.enabled {
            match mesh.transport {
                MeshTransportMode::Auto => "auto",
                MeshTransportMode::WireGuard => "wireguard",
                MeshTransportMode::Direct => "direct",
            }
            .to_string()
        } else {
            "disabled".to_string()
        };

        let sync_state = SyncState {
            peers,
            peer_wg,
            trusted_peers,
            log: sync_log,
            local_clock,
            active_transport: initial_transport,
            wg_interface: None,
            wg: None,
            tcp_listen_addr: None,
            public_endpoint: None,
        };

        let (shutdown_tx, _) = tokio::sync::watch::channel(false);

        Ok(Self {
            sync_config,
            context_path,
            state: Arc::new(RwLock::new(sync_state)),
            keys: Arc::new(keys),
            mdns,
            mcp_manager: Arc::new(mcp_manager),
            socket: Arc::new(RwLock::new(socket)),
            event_bus,
            command_registry: Arc::new(RwLock::new(None)),
            context_manager: Arc::new(RwLock::new(None)),
            shutdown_tx,
            relay_client: Arc::new(RwLock::new(None)),
        })
    }

    async fn bind_direct_udp_socket(mesh_port: u16) -> Result<Arc<UdpSocket>> {
        let bound = match UdpSocket::bind(format!("0.0.0.0:{}", mesh_port)).await {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    "Failed to bind to port {}: {}, falling back to random",
                    mesh_port, e
                );
                UdpSocket::bind("0.0.0.0:0").await?
            }
        };
        Ok(Arc::new(bound))
    }

    /// Signal background listener loops to stop.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Return this device's Mycel ID (base64-encoded x25519 public key).
    pub fn device_id(&self) -> String {
        self.keys.id()
    }

    /// Return the bound TCP sync listener address, if active.
    pub async fn tcp_listen_addr(&self) -> Option<SocketAddr> {
        self.state.read().await.tcp_listen_addr
    }

    /// Attach the command registry so DefineCommand events can be applied.
    pub async fn set_command_registry(&self, registry: crate::commands::CommandRegistry) {
        *self.command_registry.write().await = Some(registry);
    }

    /// Attach the context manager so preference and conversation turn events can be applied.
    pub async fn set_context_manager(&self, cm: crate::context::ContextManager) {
        *self.context_manager.write().await = Some(cm);
    }

    pub async fn status(&self) -> MeshStatusInfo {
        let (
            active_transport,
            wg_interface,
            tcp_listen_addr,
            trusted_peers_len,
            event_log_len,
            public_endpoint,
            peers,
        ) = {
            let state = self.state.read().await;
            let mut peers: Vec<PeerInfo> = state.peers.values().cloned().collect();
            peers.sort_by(|a, b| a.id.cmp(&b.id));
            (
                state.active_transport.clone(),
                state.wg_interface.clone(),
                state.tcp_listen_addr.map(|a| a.to_string()),
                state.trusted_peers.len(),
                state.log.len(),
                state.public_endpoint.clone(),
                peers,
            )
        };
        let connected_peers = peers
            .iter()
            .filter(|p| matches!(p.status, PeerStatus::Connected))
            .count();
        let relay_connected = self
            .relay_client
            .read()
            .await
            .as_ref()
            .is_some_and(|c| c.is_connected());

        MeshStatusInfo {
            enabled: self.sync_config.enabled,
            device_id: self.keys.id(),
            transport: active_transport,
            interface: wg_interface,
            listen_addr: tcp_listen_addr,
            trusted_peers: trusted_peers_len,
            connected_peers,
            event_log_len,
            peers,
            public_endpoint,
            relay_connected,
        }
    }

    fn ensure_endpoint_port(ep: &str, default_port: u16) -> String {
        let trimmed = ep.trim();
        if trimmed.parse::<SocketAddr>().is_ok() {
            return trimmed.to_string();
        }
        if let Ok(ip) = trimmed.parse::<std::net::IpAddr>() {
            return SocketAddr::new(ip, default_port).to_string();
        }
        if trimmed.contains(':') {
            trimmed.to_string()
        } else {
            format!("{trimmed}:{default_port}")
        }
    }

    pub async fn generate_pairing_code(&self, endpoint: Option<String>) -> PairingCodeInfo {
        let (wg_local_ip, tcp_listen_addr, public_endpoint) = {
            let state = self.state.read().await;
            (
                state.wg.as_ref().map(|w| w.local_ip().to_string()),
                state.tcp_listen_addr,
                state.public_endpoint.clone(),
            )
        };

        let (resolved_endpoint, wg_ip, sync_port) = if let Some(local_ip) = wg_local_ip {
            let ep = endpoint
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .map(|e| Self::ensure_endpoint_port(&e, self.sync_config.mesh_port))
                .or(public_endpoint)
                .or_else(|| {
                    tcp_listen_addr.map(|a| {
                        let host = if a.ip().is_unspecified() {
                            "127.0.0.1".to_string()
                        } else {
                            a.ip().to_string()
                        };
                        format!("{host}:{}", self.sync_config.mesh_port)
                    })
                });
            let sp = tcp_listen_addr
                .map(|a| a.port())
                .or(Some(self.sync_config.sync_port));
            (ep, Some(local_ip), sp)
        } else {
            let ep = endpoint
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .or(public_endpoint)
                .or_else(|| {
                    tcp_listen_addr.map(|a| {
                        if a.ip().is_unspecified() {
                            format!("127.0.0.1:{}", a.port())
                        } else {
                            a.to_string()
                        }
                    })
                });
            (ep, None, None)
        };

        let payload = PairingPayload {
            device_id: self.keys.id(),
            endpoint: resolved_endpoint.clone(),
            name: Some(self.sync_config.device_name.clone()),
            wg_ip,
            sync_port,
        };

        let json = serde_json::to_vec(&payload).unwrap_or_default();
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);
        let code = format!("mycel-pair-v1:{encoded}");

        PairingCodeInfo {
            device_id: self.keys.id(),
            code,
            endpoint: resolved_endpoint,
        }
    }

    pub async fn add_trusted_peer(
        &self,
        peer_id: &str,
        name: Option<String>,
        endpoint: Option<String>,
    ) -> Result<(PeerInfo, usize)> {
        self.add_trusted_peer_with_wg(peer_id, name, endpoint, None, None)
            .await
    }

    async fn add_trusted_peer_with_wg(
        &self,
        peer_id: &str,
        name: Option<String>,
        endpoint: Option<String>,
        wg_ip: Option<String>,
        sync_port: Option<u16>,
    ) -> Result<(PeerInfo, usize)> {
        let peer_id = peer_id.trim();
        if peer_id.is_empty() {
            return Err(anyhow!("Peer ID cannot be empty"));
        }
        if peer_id == self.keys.id() {
            return Err(anyhow!("Cannot add own device ID as a trusted peer"));
        }
        let peer_pk = decode_id(peer_id)
            .ok_or_else(|| anyhow!("Invalid peer ID: expected base64-encoded 32-byte key"))?;

        let clean_name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
        let clean_endpoint = endpoint
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty());
        let clean_wg_ip = wg_ip
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty());

        let (peer_info, persisted_snapshot, wg_registration, connected_count) = {
            let mut state = self.state.write().await;
            state.trusted_peers.insert(peer_id.to_string());

            let wg_opt = state.wg.clone();
            let wg_registration = if let Some(wg) = wg_opt {
                let allowed_ip = match &clean_wg_ip {
                    Some(ip_str) => wireguard::parse_wg_cidr(ip_str)
                        .map(|(ip, _)| ip)
                        .with_context(|| format!("Invalid peer wg_ip '{ip_str}'"))?,
                    None => wireguard::derive_mesh_ip(peer_pk.as_bytes()),
                };
                let wg_endpoint = clean_endpoint
                    .as_deref()
                    .map(|ep| Self::ensure_endpoint_port(ep, self.sync_config.mesh_port));
                let peer_sync_port = sync_port.unwrap_or(self.sync_config.sync_port);
                let overlay_tcp_addr = format!("{allowed_ip}:{peer_sync_port}");

                state.peer_wg.insert(
                    peer_id.to_string(),
                    PeerWireGuardMeta {
                        wg_ip: Some(allowed_ip.to_string()),
                        wg_endpoint: wg_endpoint.clone(),
                    },
                );

                let entry = state
                    .peers
                    .entry(peer_id.to_string())
                    .or_insert_with(|| PeerInfo {
                        id: peer_id.to_string(),
                        name: clean_name
                            .clone()
                            .unwrap_or_else(|| format_short_peer_name(peer_id)),
                        status: PeerStatus::Disconnected,
                        addresses: Vec::new(),
                    });

                if let Some(new_name) = &clean_name {
                    entry.name = new_name.clone();
                }

                // Store overlay TCP address FIRST so anti-entropy traffic routes through mycel0.
                entry.addresses.retain(|a| a != &overlay_tcp_addr);
                entry.addresses.insert(0, overlay_tcp_addr);
                if entry.addresses.len() > MAX_PEER_ADDRESSES {
                    entry.addresses.truncate(MAX_PEER_ADDRESSES);
                }

                Some((wg, allowed_ip, wg_endpoint))
            } else {
                let entry = state
                    .peers
                    .entry(peer_id.to_string())
                    .or_insert_with(|| PeerInfo {
                        id: peer_id.to_string(),
                        name: clean_name
                            .clone()
                            .unwrap_or_else(|| format_short_peer_name(peer_id)),
                        status: PeerStatus::Disconnected,
                        addresses: Vec::new(),
                    });

                if let Some(new_name) = &clean_name {
                    entry.name = new_name.clone();
                }
                if let Some(ep) = &clean_endpoint {
                    if !entry.addresses.iter().any(|a| a == ep) {
                        if entry.addresses.len() >= MAX_PEER_ADDRESSES {
                            entry.addresses.remove(0);
                        }
                        entry.addresses.push(ep.clone());
                    }
                }

                None
            };

            let peer_info = state
                .peers
                .get(peer_id)
                .cloned()
                .ok_or_else(|| anyhow!("Peer missing from state after insert"))?;
            let snapshot = Self::build_persisted_peers(&state);
            let connected_count = state
                .peers
                .values()
                .filter(|p| matches!(p.status, PeerStatus::Connected))
                .count();
            (peer_info, snapshot, wg_registration, connected_count)
        };

        let persisted_path = self.context_path.join("trusted_peers.json");
        write_trusted_peers_atomically(&persisted_path, &persisted_snapshot)?;

        if let Some((wg, allowed_ip, wg_endpoint)) = wg_registration {
            if let Err(e) = wg
                .set_peer(
                    peer_id,
                    allowed_ip,
                    wg_endpoint.as_deref(),
                    self.sync_config.persistent_keepalive_secs,
                )
                .await
            {
                warn!(
                    peer = %peer_id,
                    error = %e,
                    "Failed to register peer on WireGuard interface"
                );
            }
        }

        let _ = self.event_bus.send(SystemEvent::SyncStateChanged {
            peers_connected: connected_count,
            state: "peer_paired".to_string(),
        });

        let relay_ready = self
            .relay_client
            .read()
            .await
            .as_ref()
            .is_some_and(|c| c.is_connected());
        let synced_events =
            if (!peer_info.addresses.is_empty() || relay_ready) && self.sync_config.enabled {
                match self.sync_with_peer(peer_id).await {
                    Ok(n) => n,
                    Err(e) => {
                        debug!(peer = %peer_id, error = %e, "Initial sync after pairing failed");
                        0
                    }
                }
            } else {
                0
            };

        let updated_peer = self
            .state
            .read()
            .await
            .peers
            .get(peer_id)
            .cloned()
            .unwrap_or(peer_info);

        Ok((updated_peer, synced_events))
    }

    pub async fn pair_peer(&self, code_or_id: &str) -> Result<(PeerInfo, usize)> {
        let trimmed = code_or_id.trim();
        if let Some(encoded) = trimmed.strip_prefix("mycel-pair-v1:") {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .context("Invalid base64 in pairing code")?;
            let payload: PairingPayload =
                serde_json::from_slice(&raw).context("Invalid JSON in pairing code")?;
            self.add_trusted_peer_with_wg(
                &payload.device_id,
                payload.name,
                payload.endpoint,
                payload.wg_ip,
                payload.sync_port,
            )
            .await
        } else {
            self.add_trusted_peer(trimmed, None, None).await
        }
    }

    fn build_persisted_peers(state: &SyncState) -> PersistedTrustedPeers {
        let mut trusted_peers: Vec<String> = state.trusted_peers.iter().cloned().collect();
        trusted_peers.sort();

        let mut peers = HashMap::new();
        for (id, info) in &state.peers {
            if state.trusted_peers.contains(id) {
                let wg_meta = state.peer_wg.get(id);
                peers.insert(
                    id.clone(),
                    PersistedPeerMeta {
                        name: Some(info.name.clone()),
                        addresses: info.addresses.clone(),
                        wg_ip: wg_meta.and_then(|m| m.wg_ip.clone()),
                        wg_endpoint: wg_meta.and_then(|m| m.wg_endpoint.clone()),
                    },
                );
            }
        }

        PersistedTrustedPeers {
            trusted_peers,
            peers,
        }
    }

    async fn setup_wireguard_dataplane(&self) -> Result<(wireguard::KernelWireGuard, TcpListener)> {
        let (local_ip, prefix_len) = match &self.sync_config.wg_address {
            Some(addr_str) => wireguard::parse_wg_cidr(addr_str)
                .with_context(|| format!("Invalid [mesh] wg_address '{}'", addr_str))?,
            None => (wireguard::derive_mesh_ip(self.keys.public.as_bytes()), 16),
        };

        let wg = wireguard::KernelWireGuard::new(
            self.sync_config.interface.clone(),
            local_ip,
            prefix_len,
        )?;
        let private_b64 = encode_id(&self.keys.private.to_bytes());
        wg.configure_interface(&private_b64, self.sync_config.mesh_port)
            .await?;

        let peers_snapshot = {
            let state = self.state.read().await;
            state
                .trusted_peers
                .iter()
                .map(|id| {
                    let wg_meta = state.peer_wg.get(id);
                    let wg_ip = wg_meta.and_then(|m| m.wg_ip.clone());
                    let ep = wg_meta.and_then(|m| m.wg_endpoint.clone()).or_else(|| {
                        state
                            .peers
                            .get(id)
                            .and_then(|p| p.addresses.first().cloned())
                    });
                    (id.clone(), wg_ip, ep)
                })
                .collect::<Vec<_>>()
        };

        for (peer_id, wg_ip_opt, endpoint) in peers_snapshot {
            if let Some(pk) = decode_id(&peer_id) {
                let allowed_ip = match wg_ip_opt {
                    Some(ip_str) => match wireguard::parse_wg_cidr(&ip_str) {
                        Ok((ip, _)) => ip,
                        Err(e) => {
                            warn!(peer = %peer_id, error = %e, "Invalid persisted wg_ip; deriving fallback");
                            wireguard::derive_mesh_ip(pk.as_bytes())
                        }
                    },
                    None => wireguard::derive_mesh_ip(pk.as_bytes()),
                };
                // Avoid passing the overlay TCP address itself as the WireGuard UDP endpoint.
                let wg_endpoint = endpoint.filter(|ep| !ep.starts_with(&format!("{allowed_ip}:")));
                if let Err(e) = wg
                    .set_peer(
                        &peer_id,
                        allowed_ip,
                        wg_endpoint.as_deref(),
                        self.sync_config.persistent_keepalive_secs,
                    )
                    .await
                {
                    warn!(
                        peer = %peer_id,
                        error = %e,
                        "Failed to configure WireGuard peer"
                    );
                }
            }
        }

        let listener = TcpListener::bind(("0.0.0.0", self.sync_config.sync_port))
            .await
            .with_context(|| {
                format!(
                    "Failed to bind TCP sync listener on port {}",
                    self.sync_config.sync_port
                )
            })?;

        Ok((wg, listener))
    }

    pub async fn start(&self) -> Result<()> {
        if !self.sync_config.enabled {
            info!(
                "Mesh disabled. Mycel ID: {} (add it to [mesh] trusted_peers on your other devices)",
                self.keys.id()
            );
            return Ok(());
        }

        let (active_transport, wg_opt, tcp_listener, udp_socket_opt) =
            match self.sync_config.transport {
                MeshTransportMode::Direct => {
                    let socket = match self.socket.read().await.clone() {
                        Some(s) => s,
                        None => {
                            let s =
                                Self::bind_direct_udp_socket(self.sync_config.mesh_port).await?;
                            *self.socket.write().await = Some(s.clone());
                            s
                        }
                    };
                    let listener = TcpListener::bind(("0.0.0.0", self.sync_config.sync_port))
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to bind Direct TCP sync listener on port {}",
                                self.sync_config.sync_port
                            )
                        })?;
                    ("direct".to_string(), None, listener, Some(socket))
                }
                MeshTransportMode::WireGuard => {
                    let (wg, listener) = self.setup_wireguard_dataplane().await?;
                    ("wireguard".to_string(), Some(wg), listener, None)
                }
                MeshTransportMode::Auto => match self.setup_wireguard_dataplane().await {
                    Ok((wg, listener)) => ("wireguard".to_string(), Some(wg), listener, None),
                    Err(e) => {
                        warn!(
                        "Kernel WireGuard unavailable ({}); falling back to Direct encrypted TCP",
                        e
                    );
                        let socket = match self.socket.read().await.clone() {
                            Some(s) => s,
                            None => {
                                let s = Self::bind_direct_udp_socket(self.sync_config.mesh_port)
                                    .await?;
                                *self.socket.write().await = Some(s.clone());
                                s
                            }
                        };
                        let listener = TcpListener::bind(("0.0.0.0", self.sync_config.sync_port))
                            .await
                            .with_context(|| {
                                format!(
                                    "Failed to bind fallback Direct TCP sync listener on port {}",
                                    self.sync_config.sync_port
                                )
                            })?;
                        ("direct".to_string(), None, listener, Some(socket))
                    }
                },
            };

        let tcp_addr = tcp_listener.local_addr()?;
        {
            let mut state = self.state.write().await;
            state.active_transport = active_transport.clone();
            state.wg_interface = wg_opt.as_ref().map(|w| w.interface().to_string());
            state.wg = wg_opt;
            state.tcp_listen_addr = Some(tcp_addr);
        }

        let udp_port = udp_socket_opt
            .as_ref()
            .and_then(|s| s.local_addr().ok())
            .map(|a| a.port())
            .unwrap_or(self.sync_config.mesh_port);
        info!(
            "Sync service starting (transport={}, tcp={}, udp_port={}). Mycel ID: {}",
            active_transport,
            tcp_addr,
            udp_port,
            self.keys.id()
        );

        if let Some(mdns) = &self.mdns {
            self.start_discovery(mdns, udp_port).await?;
        }

        if self.sync_config.blockchain_sync {
            self.start_blockchain_sync().await?;
        }

        // Spawn UDP listener loop only in Direct mode
        if let Some(socket) = udp_socket_opt {
            let service = self.clone();
            tokio::spawn(async move {
                if let Err(e) = service.listen_loop(socket).await {
                    error!("Mesh UDP listener loop error: {}", e);
                }
            });
        }

        // Best-effort STUN public endpoint discovery (never fails startup)
        if !self.sync_config.stun_servers.is_empty() {
            let stun_bind = SocketAddr::from(([0, 0, 0, 0], 0));
            match nat::discover_public_endpoint(
                stun_bind,
                &self.sync_config.stun_servers,
                Duration::from_secs(2),
            )
            .await
            {
                Ok(public_addr) => {
                    info!("Discovered public endpoint via STUN: {}", public_addr);
                    self.state.write().await.public_endpoint = Some(public_addr.to_string());
                }
                Err(e) => {
                    warn!(
                        "STUN public endpoint discovery failed (continuing without public endpoint): {}",
                        e
                    );
                }
            }
        }

        // Optional embedded zero-knowledge relay server
        if self.sync_config.relay_server_enabled {
            if let Some(listen_addr) = self
                .sync_config
                .relay_listen_addr
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let relay_server = relay::MeshRelayServer::bind(listen_addr).await?;
                let relay_shutdown_rx = self.shutdown_tx.subscribe();
                tokio::spawn(async move {
                    if let Err(e) = relay_server.run(relay_shutdown_rx).await {
                        error!("Embedded MeshRelayServer error: {}", e);
                    }
                });
            }
        }

        // Optional zero-knowledge relay client fallback
        if self.sync_config.relay_enabled {
            if let Some(relay_addr) = self
                .sync_config
                .relay_addr
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            {
                let (client, mut incoming_rx) = relay::RelayClient::start(
                    relay_addr,
                    self.device_id(),
                    self.shutdown_tx.subscribe(),
                );
                *self.relay_client.write().await = Some(client);

                let service = self.clone();
                let mut relay_shutdown_rx = self.shutdown_tx.subscribe();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            maybe_msg = incoming_rx.recv() => {
                                let Some((from_device_id, payload)) = maybe_msg else {
                                    break;
                                };
                                if let Err(e) = service.handle_relay_payload(&from_device_id, &payload).await {
                                    debug!(from = %from_device_id, error = %e, "Rejected or failed incoming relay payload");
                                }
                            }
                            _ = relay_shutdown_rx.changed() => {
                                if *relay_shutdown_rx.borrow() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        }

        // Spawn TCP anti-entropy listener loop
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(e) = service.tcp_listen_loop(tcp_listener).await {
                error!("Mesh TCP listener loop error: {}", e);
            }
        });

        // Spawn periodic anti-entropy catch-up loop on startup/reconnect
        let service = self.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        return;
                    }
                }
            }
            service.sync_all_peers().await;

            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        service.sync_all_peers().await;
                    }
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });

        // Start event bus listener
        let service = self.clone();
        let mut receiver = self.event_bus.subscribe();
        tokio::spawn(async move {
            while let Ok(event) = receiver.recv().await {
                match event {
                    SystemEvent::CapabilityCreated {
                        name,
                        language,
                        source_code,
                    } => {
                        info!("Broadcasting new capability to mesh: {}", name);
                        let _ = service
                            .create_event(SyncOperation::AddCapability {
                                name,
                                language,
                                code: source_code,
                            })
                            .await;
                    }
                    SystemEvent::ToolCalled { .. }
                    | SystemEvent::McpServerRestarted { .. }
                    | SystemEvent::ExecutionFailed { .. }
                    | SystemEvent::RecoveryDecided { .. }
                    | SystemEvent::ConfirmationRequired { .. }
                    | SystemEvent::ProviderHealthChanged { .. }
                    | SystemEvent::SyncStateChanged { .. } => {}
                }
            }
        });

        Ok(())
    }

    async fn is_trusted(&self, peer_id: &str) -> bool {
        self.state.read().await.trusted_peers.contains(peer_id)
    }

    async fn set_peer_status(&self, peer_id: &str, status: PeerStatus) {
        let (changed, connected_count, state_label) = {
            let mut state = self.state.write().await;
            let Some(peer) = state.peers.get_mut(peer_id) else {
                return;
            };
            let was_connected = matches!(peer.status, PeerStatus::Connected);
            let now_connected = matches!(status, PeerStatus::Connected);
            if std::mem::discriminant(&peer.status) == std::mem::discriminant(&status) {
                return;
            }
            peer.status = status;
            let connected_count = state
                .peers
                .values()
                .filter(|p| matches!(p.status, PeerStatus::Connected))
                .count();
            let label = match (was_connected, now_connected) {
                (false, true) => Some("peer_connected"),
                (true, false) => Some("peer_disconnected"),
                _ if !now_connected => Some("peer_disconnected"),
                _ => None,
            };
            (true, connected_count, label)
        };

        if changed {
            if let Some(label) = state_label {
                let _ = self.event_bus.send(SystemEvent::SyncStateChanged {
                    peers_connected: connected_count,
                    state: label.to_string(),
                });
            }
        }
    }

    async fn tcp_listen_loop(&self, listener: TcpListener) -> Result<()> {
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        loop {
            tokio::select! {
                res = listener.accept() => {
                    let (stream, remote_addr) = res?;
                    let service = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = service.handle_tcp_connection(stream, remote_addr).await {
                            debug!(remote = %remote_addr, error = %e, "TCP sync connection closed");
                        }
                    });
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn decrypt_wire_frame(
        &self,
        frame: &WireFrame,
        trusted_peers: &HashSet<String>,
    ) -> Result<(String, SyncMessage)> {
        let own_id = self.keys.id();
        if frame.sender_id == own_id || !trusted_peers.contains(&frame.sender_id) {
            return Err(anyhow!(
                "Rejecting sync frame from untrusted sender {}",
                frame.sender_id
            ));
        }

        let peer_pk = decode_id(&frame.sender_id)
            .ok_or_else(|| anyhow!("Invalid sender public key {}", frame.sender_id))?;

        let plaintext = cipher_for(&self.keys.private, &peer_pk)
            .decrypt(
                &frame.nonce.into(),
                Payload {
                    msg: &frame.encrypted_payload,
                    aad: &[],
                },
            )
            .map_err(|e| {
                anyhow!(
                    "Failed to decrypt sync frame from {}: {}",
                    frame.sender_id,
                    e
                )
            })?;

        let msg: SyncMessage = serde_json::from_slice(&plaintext)
            .context("Failed to deserialize decrypted SyncMessage")?;

        Ok((frame.sender_id.clone(), msg))
    }

    fn validate_incoming_message(
        &self,
        sender_id: &str,
        msg: &SyncMessage,
        trusted_peers: &HashSet<String>,
    ) -> Result<()> {
        let own_id = self.keys.id();
        match msg {
            SyncMessage::PushEvent { event } => {
                if event.device_id != sender_id || event.device_id == own_id {
                    return Err(anyhow!(
                        "PushEvent device_id '{}' does not match authenticated sender '{}'",
                        event.device_id,
                        sender_id
                    ));
                }
            }
            SyncMessage::EventBatch { events, .. } => {
                for event in events {
                    if event.device_id == own_id || !trusted_peers.contains(&event.device_id) {
                        return Err(anyhow!(
                            "EventBatch contains event {} from untrusted or self device '{}'",
                            event.id,
                            event.device_id
                        ));
                    }
                }
            }
            SyncMessage::PullSince { .. } => {}
        }
        Ok(())
    }

    async fn handle_tcp_connection(
        &self,
        stream: TcpStream,
        remote_addr: SocketAddr,
    ) -> Result<()> {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        let Some(first_frame) = read_wire_frame(&mut reader).await? else {
            return Ok(());
        };

        let trusted_peers = self.state.read().await.trusted_peers.clone();
        let (sender_id, msg) = self.decrypt_wire_frame(&first_frame, &trusted_peers)?;
        self.validate_incoming_message(&sender_id, &msg, &trusted_peers)?;
        self.set_peer_status(&sender_id, PeerStatus::Connected)
            .await;

        let peer_pk = decode_id(&sender_id)
            .ok_or_else(|| anyhow!("Invalid peer public key {}", sender_id))?;

        match msg {
            SyncMessage::PushEvent { event } => {
                self.apply_event_internal(event).await?;
            }
            SyncMessage::PullSince { clock } => {
                let (missing, sender_clock) = {
                    let state = self.state.read().await;
                    (
                        state.log.events_missing_for(&clock),
                        state.local_clock.clone(),
                    )
                };

                let reply = SyncMessage::EventBatch {
                    events: missing,
                    sender_clock,
                };
                let reply_frame =
                    seal_sync_message(&self.keys.private, &self.keys.id(), &peer_pk, &reply)?;
                let mut line = serde_json::to_string(&reply_frame)?;
                line.push('\n');
                tokio::time::timeout(TCP_IO_TIMEOUT, write_half.write_all(line.as_bytes()))
                    .await
                    .map_err(|_| anyhow!("Timed out writing EventBatch reply"))??;
                tokio::time::timeout(TCP_IO_TIMEOUT, write_half.flush())
                    .await
                    .map_err(|_| anyhow!("Timed out flushing EventBatch reply"))??;

                // Read optional reverse EventBatch from client before closing.
                if let Some(reverse_frame) = read_wire_frame(&mut reader).await? {
                    let trusted_now = self.state.read().await.trusted_peers.clone();
                    let (rev_sender, rev_msg) =
                        self.decrypt_wire_frame(&reverse_frame, &trusted_now)?;
                    if rev_sender != sender_id {
                        return Err(anyhow!(
                            "Reverse batch sender {} changed mid-stream (expected {})",
                            rev_sender,
                            sender_id
                        ));
                    }
                    self.validate_incoming_message(&rev_sender, &rev_msg, &trusted_now)?;
                    if let SyncMessage::EventBatch { events, .. } = rev_msg {
                        for event in events {
                            self.apply_event_internal(event).await?;
                        }
                    }
                }
            }
            SyncMessage::EventBatch { events, .. } => {
                for event in events {
                    self.apply_event_internal(event).await?;
                }
            }
        }

        debug!(peer = %sender_id, remote = %remote_addr, "Handled TCP sync session");
        Ok(())
    }

    async fn handle_relay_payload(&self, from_device_id: &str, payload: &[u8]) -> Result<()> {
        let frame: WireFrame =
            serde_json::from_slice(payload).context("Invalid WireFrame in relay payload")?;

        // Enforce pre-decryption trust check before touching any AEAD state.
        let trusted_peers = self.state.read().await.trusted_peers.clone();
        let own_id = self.keys.id();
        if frame.sender_id != from_device_id
            || frame.sender_id == own_id
            || !trusted_peers.contains(&frame.sender_id)
        {
            return Err(anyhow!(
                "Rejecting untrusted or mismatched relay frame from {} (frame sender {})",
                from_device_id,
                frame.sender_id
            ));
        }

        let (sender_id, msg) = self.decrypt_wire_frame(&frame, &trusted_peers)?;
        self.validate_incoming_message(&sender_id, &msg, &trusted_peers)?;
        self.set_peer_status(&sender_id, PeerStatus::Connected)
            .await;

        match msg {
            SyncMessage::PushEvent { event } => {
                self.apply_event_internal(event).await?;
            }
            SyncMessage::PullSince { clock } => {
                let (missing, sender_clock) = {
                    let state = self.state.read().await;
                    (
                        state.log.events_missing_for(&clock),
                        state.local_clock.clone(),
                    )
                };
                let reply = SyncMessage::EventBatch {
                    events: missing,
                    sender_clock,
                };
                self.send_via_relay(&sender_id, &reply).await?;
            }
            SyncMessage::EventBatch { events, .. } => {
                for event in events {
                    self.apply_event_internal(event).await?;
                }
            }
        }

        Ok(())
    }

    async fn send_via_relay(&self, peer_id: &str, msg: &SyncMessage) -> Result<()> {
        if !self.is_trusted(peer_id).await {
            return Err(anyhow!("Refusing to relay to untrusted peer {}", peer_id));
        }
        let client = self
            .relay_client
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("Relay client not configured"))?;
        if !client.is_connected() {
            return Err(anyhow!("Relay client not connected"));
        }
        let peer_pk = decode_id(peer_id).ok_or_else(|| anyhow!("Invalid peer public key"))?;
        let frame = seal_sync_message(&self.keys.private, &self.keys.id(), &peer_pk, msg)?;
        let payload = serde_json::to_vec(&frame)?;
        client.forward(peer_id, payload).await?;
        Ok(())
    }

    pub async fn sync_with_peer(&self, peer_id: &str) -> Result<usize> {
        if !self.sync_config.enabled {
            return Err(anyhow!("Mesh sync is disabled"));
        }

        let (peer_pk, addresses, local_clock, trusted_peers) = {
            let state = self.state.read().await;
            if !state.trusted_peers.contains(peer_id) {
                return Err(anyhow!("Peer {} is not in trusted_peers", peer_id));
            }
            let pk =
                decode_id(peer_id).ok_or_else(|| anyhow!("Invalid peer public key {}", peer_id))?;
            let addrs = state
                .peers
                .get(peer_id)
                .map(|p| p.addresses.clone())
                .unwrap_or_default();
            (
                pk,
                addrs,
                state.local_clock.clone(),
                state.trusted_peers.clone(),
            )
        };

        if addresses.is_empty() {
            if self
                .send_via_relay(
                    peer_id,
                    &SyncMessage::PullSince {
                        clock: local_clock.clone(),
                    },
                )
                .await
                .is_ok()
            {
                self.set_peer_status(peer_id, PeerStatus::Connected).await;
                return Ok(0);
            }
            return Err(anyhow!("No known endpoint address for peer {}", peer_id));
        }

        let mut connected_stream: Option<TcpStream> = None;
        let mut last_err: Option<anyhow::Error> = None;
        for addr_str in &addresses {
            match tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(addr_str)).await {
                Ok(Ok(stream)) => {
                    connected_stream = Some(stream);
                    break;
                }
                Ok(Err(e)) => {
                    last_err = Some(anyhow!("Failed to connect to {}: {}", addr_str, e));
                }
                Err(_) => {
                    last_err = Some(anyhow!("Timed out connecting to {}", addr_str));
                }
            }
        }

        let mut stream = match connected_stream {
            Some(s) => s,
            None => {
                if self
                    .send_via_relay(
                        peer_id,
                        &SyncMessage::PullSince {
                            clock: local_clock.clone(),
                        },
                    )
                    .await
                    .is_ok()
                {
                    self.set_peer_status(peer_id, PeerStatus::Connected).await;
                    return Ok(0);
                }
                self.set_peer_status(peer_id, PeerStatus::Disconnected)
                    .await;
                return Err(last_err.unwrap_or_else(|| anyhow!("Failed to connect to peer")));
            }
        };

        self.set_peer_status(peer_id, PeerStatus::Connected).await;

        let own_id = self.keys.id();
        let pull_msg = SyncMessage::PullSince {
            clock: local_clock.clone(),
        };
        let pull_frame = seal_sync_message(&self.keys.private, &own_id, &peer_pk, &pull_msg)?;
        write_wire_frame(&mut stream, &pull_frame).await?;

        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        let reply_frame = read_wire_frame(&mut reader).await?.ok_or_else(|| {
            anyhow!(
                "Peer {} closed connection before sending EventBatch",
                peer_id
            )
        })?;

        let (sender_id, reply_msg) = self.decrypt_wire_frame(&reply_frame, &trusted_peers)?;
        if sender_id != peer_id {
            return Err(anyhow!(
                "Expected reply from peer {}, got {}",
                peer_id,
                sender_id
            ));
        }
        self.validate_incoming_message(&sender_id, &reply_msg, &trusted_peers)?;

        let (events, peer_clock) = match reply_msg {
            SyncMessage::EventBatch {
                events,
                sender_clock,
            } => (events, sender_clock),
            other => {
                return Err(anyhow!(
                    "Unexpected response from peer {}: {:?}",
                    peer_id,
                    std::mem::discriminant(&other)
                ));
            }
        };

        let mut applied_count = 0usize;
        for event in events {
            if self.apply_event_internal(event).await? {
                applied_count += 1;
            }
        }

        let (missing_for_peer, updated_clock) = {
            let state = self.state.read().await;
            (
                state.log.events_missing_for(&peer_clock),
                state.local_clock.clone(),
            )
        };

        if !missing_for_peer.is_empty() {
            let push_batch = SyncMessage::EventBatch {
                events: missing_for_peer,
                sender_clock: updated_clock,
            };
            let batch_frame =
                seal_sync_message(&self.keys.private, &own_id, &peer_pk, &push_batch)?;
            let mut line = serde_json::to_string(&batch_frame)?;
            line.push('\n');
            tokio::time::timeout(TCP_IO_TIMEOUT, write_half.write_all(line.as_bytes()))
                .await
                .map_err(|_| anyhow!("Timed out writing reverse EventBatch"))??;
            tokio::time::timeout(TCP_IO_TIMEOUT, write_half.flush())
                .await
                .map_err(|_| anyhow!("Timed out flushing reverse EventBatch"))??;
        }
        let _ = write_half.shutdown().await;

        let connected_count = self
            .state
            .read()
            .await
            .peers
            .values()
            .filter(|p| matches!(p.status, PeerStatus::Connected))
            .count();
        let _ = self.event_bus.send(SystemEvent::SyncStateChanged {
            peers_connected: connected_count,
            state: "synced".to_string(),
        });

        Ok(applied_count)
    }

    pub async fn sync_all_peers(&self) -> usize {
        let peer_ids: Vec<String> = {
            let state = self.state.read().await;
            state.trusted_peers.iter().cloned().collect()
        };

        let mut total = 0usize;
        for peer_id in peer_ids {
            match self.sync_with_peer(&peer_id).await {
                Ok(n) => total += n,
                Err(e) => {
                    debug!(peer = %peer_id, error = %e, "Peer sync skipped or failed");
                }
            }
        }
        total
    }

    async fn listen_loop(&self, socket: Arc<UdpSocket>) -> Result<()> {
        let mut buf = [0u8; 65535];
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        loop {
            tokio::select! {
                res = socket.recv_from(&mut buf) => {
                    let (len, addr) = res?;
                    self.handle_packet(&buf[..len], addr).await;
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_packet(&self, data: &[u8], addr: SocketAddr) {
        match serde_json::from_slice::<MeshPacket>(data) {
            Ok(MeshPacket::Handshake { public_key }) => {
                if public_key.len() == 32 {
                    let peer_id = encode_id(&public_key);
                    self.note_peer_address(&peer_id, &addr.to_string(), None)
                        .await;
                }
            }
            Ok(MeshPacket::Event {
                nonce,
                encrypted_data,
            }) => {
                let trusted_peers = self.state.read().await.trusted_peers.clone();
                match self.open_from_trusted(&nonce, &encrypted_data, &trusted_peers) {
                    Some((peer_id, event)) => {
                        if event.device_id != peer_id {
                            warn!(
                                "Dropping event {} from {}: claims device {} but was sealed by {}",
                                event.id, addr, event.device_id, peer_id
                            );
                            return;
                        }
                        let _ = self.apply_event(event).await;
                    }
                    None => debug!("Dropping event from {}: not sealed by a trusted peer", addr),
                }
            }
            Err(e) => {
                debug!("Received invalid mesh packet from {}: {}", addr, e);
            }
        }
    }

    /// Record a reachable address for a trusted peer. Returns false (and
    /// records nothing) for untrusted IDs.
    async fn note_peer_address(&self, peer_id: &str, addr: &str, name: Option<&str>) -> bool {
        if !self.is_trusted(peer_id).await {
            debug!("Ignoring untrusted device {} at {}", peer_id, addr);
            return false;
        }
        let mut state = self.state.write().await;
        let peer = state
            .peers
            .entry(peer_id.to_string())
            .or_insert_with(|| PeerInfo {
                id: peer_id.to_string(),
                name: name
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("peer-{}", addr)),
                status: PeerStatus::Connected,
                addresses: Vec::new(),
            });
        if !peer.addresses.iter().any(|a| a == addr) {
            if peer.addresses.len() >= MAX_PEER_ADDRESSES {
                peer.addresses.remove(0);
            }
            peer.addresses.push(addr.to_string());
        }
        peer.status = PeerStatus::Connected;
        debug!("Trusted peer {} reachable at {}", peer_id, addr);
        true
    }

    /// Try to open an event packet with each trusted peer's shared secret.
    fn open_from_trusted(
        &self,
        nonce: &[u8; 12],
        data: &[u8],
        trusted_peers: &HashSet<String>,
    ) -> Option<(String, SyncEvent)> {
        for peer_id in trusted_peers {
            let Some(peer_pk) = decode_id(peer_id) else {
                continue;
            };
            let cipher = cipher_for(&self.keys.private, &peer_pk);
            if let Ok(plaintext) = cipher.decrypt(
                nonce.into(),
                Payload {
                    msg: data,
                    aad: &[],
                },
            ) {
                return match serde_json::from_slice::<SyncEvent>(&plaintext) {
                    Ok(event) => Some((peer_id.clone(), event)),
                    Err(e) => {
                        warn!("Malformed event from trusted peer {}: {}", peer_id, e);
                        None
                    }
                };
            }
        }
        None
    }

    async fn start_discovery(&self, mdns: &ServiceDaemon, port: u16) -> Result<()> {
        let service_type = "_mycel._udp.local.";
        let instance_name = format!("{}.{}", self.sync_config.device_name, uuid::Uuid::new_v4());
        let host_name = format!("{}.local.", self.sync_config.device_name);

        let properties = [("pubkey", self.keys.id())];

        let my_service = ServiceInfo::new(
            service_type,
            &instance_name,
            &host_name,
            "",
            port,
            &properties[..],
        )?;

        mdns.register(my_service)?;
        info!("mDNS discovery active: {}", instance_name);

        let receiver = mdns.browse(service_type)?;
        let service = self.clone();

        tokio::spawn(async move {
            while let Ok(event) = receiver.recv_async().await {
                if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
                    let Some(pubkey) = info.get_property_val_str("pubkey") else {
                        continue;
                    };
                    let addresses: Vec<String> = info
                        .get_addresses()
                        .iter()
                        .map(|a| format!("{}:{}", a, info.get_port()))
                        .collect();
                    for addr_str in addresses {
                        if service
                            .note_peer_address(pubkey, &addr_str, Some(info.get_fullname()))
                            .await
                        {
                            if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                                let _ = service.send_handshake(addr).await;
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    async fn send_handshake(&self, addr: SocketAddr) -> Result<()> {
        let Some(socket) = self.socket.read().await.clone() else {
            return Ok(());
        };
        let packet = MeshPacket::Handshake {
            public_key: self.keys.public.as_bytes().to_vec(),
        };
        let data = serde_json::to_vec(&packet)?;
        socket.send_to(&data, addr).await?;
        Ok(())
    }

    async fn start_blockchain_sync(&self) -> Result<()> {
        let mcp = self.mcp_manager.clone();
        let account = self.sync_config.near_account.clone();
        let service = self.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                if let (Some(acc), Some(mcp)) = (&account, &*mcp) {
                    debug!("Polling NEAR for global updates for {}", acc);

                    // 1. Poll for peers. The registry only supplies addresses;
                    //    a device must still be in trusted_peers to be used.
                    let peer_args =
                        HashMap::from([("accountId".to_string(), serde_json::json!(acc))]);
                    if let Ok(result) = mcp.call_tool("near_get_peers", peer_args).await {
                        for content in result.content {
                            if let crate::mcp::protocol::ToolContent::Text { text } = content {
                                if let Ok(peers) = serde_json::from_str::<Vec<PeerInfo>>(&text) {
                                    for peer in peers {
                                        for addr_str in &peer.addresses {
                                            if service
                                                .note_peer_address(
                                                    &peer.id,
                                                    addr_str,
                                                    Some(&peer.name),
                                                )
                                                .await
                                            {
                                                if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                                                    let _ = service.send_handshake(addr).await;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // 2. Poll for shared capabilities (listed only, never installed)
                    let cap_args = HashMap::from([("query".to_string(), serde_json::json!(acc))]);
                    if let Ok(result) = mcp.call_tool("near_discover_capabilities", cap_args).await
                    {
                        for content in result.content {
                            if let crate::mcp::protocol::ToolContent::Text { text } = content {
                                if let Ok(caps) =
                                    serde_json::from_str::<Vec<serde_json::Value>>(&text)
                                {
                                    for cap in caps {
                                        debug!("Discovered global capability: {:?}", cap["name"]);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    /// Record a local change and send it to trusted peers.
    ///
    /// Returns `None` when nothing was produced: the mesh is disabled, or the
    /// operation is a conversation turn and `sync_ai_context` is off.
    pub async fn create_event(&self, operation: SyncOperation) -> Result<Option<SyncEvent>> {
        if !self.sync_config.enabled {
            return Ok(None);
        }
        if matches!(operation, SyncOperation::AddConversationTurn { .. })
            && !self.sync_config.sync_ai_context
        {
            return Ok(None);
        }

        let (event, peers) = {
            let mut state = self.state.write().await;
            let device_id = self.keys.id();
            state.local_clock.increment(&device_id);

            let event = SyncEvent {
                id: uuid::Uuid::new_v4().to_string(),
                device_id,
                timestamp: Utc::now(),
                clock: state.local_clock.clone(),
                operation,
                signature: Vec::new(),
            };

            state.log.append(event.clone())?;
            let peers = state.peers.clone();
            (event, peers)
        };

        for peer in peers.values() {
            let _ = self.send_event(peer, &event).await;
            let delivered = if !peer.addresses.is_empty() {
                self.push_event_tcp(peer, &event).await.is_ok()
            } else {
                false
            };
            if delivered
                || self
                    .send_via_relay(
                        &peer.id,
                        &SyncMessage::PushEvent {
                            event: event.clone(),
                        },
                    )
                    .await
                    .is_ok()
            {
                self.set_peer_status(&peer.id, PeerStatus::Connected).await;
            } else if !peer.addresses.is_empty() {
                debug!(peer = %peer.id, "TCP push failed and relay unavailable; marking peer disconnected");
                self.set_peer_status(&peer.id, PeerStatus::Disconnected)
                    .await;
            }
        }

        Ok(Some(event))
    }

    async fn push_event_tcp(&self, peer: &PeerInfo, event: &SyncEvent) -> Result<()> {
        if !self.is_trusted(&peer.id).await {
            return Err(anyhow!("Refusing to push to untrusted peer {}", peer.id));
        }
        let peer_pk = decode_id(&peer.id).ok_or_else(|| anyhow!("Invalid peer public key"))?;
        let frame = seal_sync_message(
            &self.keys.private,
            &self.keys.id(),
            &peer_pk,
            &SyncMessage::PushEvent {
                event: event.clone(),
            },
        )?;

        let mut last_err: Option<anyhow::Error> = None;
        for addr_str in &peer.addresses {
            match tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(addr_str)).await {
                Ok(Ok(mut stream)) => {
                    write_wire_frame(&mut stream, &frame).await?;
                    let _ = stream.shutdown().await;
                    return Ok(());
                }
                Ok(Err(e)) => {
                    last_err = Some(anyhow!("TCP connect to {} failed: {}", addr_str, e));
                }
                Err(_) => {
                    last_err = Some(anyhow!("TCP connect to {} timed out", addr_str));
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("No reachable addresses for peer {}", peer.id)))
    }

    async fn send_event(&self, peer: &PeerInfo, event: &SyncEvent) -> Result<()> {
        let Some(socket) = self.socket.read().await.clone() else {
            return Ok(());
        };
        if !self.is_trusted(&peer.id).await {
            return Err(anyhow!("Refusing to send to untrusted peer {}", peer.id));
        }
        let peer_pk = decode_id(&peer.id).ok_or_else(|| anyhow!("Invalid peer public key"))?;
        let packet_data = serde_json::to_vec(&seal_event(&self.keys.private, &peer_pk, event)?)?;

        for addr_str in &peer.addresses {
            if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                let _ = socket.send_to(&packet_data, addr).await;
            }
        }

        Ok(())
    }

    pub async fn get_peers(&self) -> Vec<PeerInfo> {
        self.state.read().await.peers.values().cloned().collect()
    }

    pub async fn apply_event(&self, event: SyncEvent) -> Result<()> {
        self.apply_event_internal(event).await?;
        Ok(())
    }

    async fn apply_event_internal(&self, event: SyncEvent) -> Result<bool> {
        debug!(event_id = %event.id, device = %event.device_id, "Applying sync event");

        {
            let mut state = self.state.write().await;
            state.local_clock.merge(&event.clock);
            if !state.log.append(event.clone())? {
                debug!(event_id = %event.id, "Duplicate sync event ignored");
                return Ok(false);
            }
        }

        info!(event_id = %event.id, "Event integrated into local mesh log");

        match event.operation {
            SyncOperation::AddCapability { name, language, .. } => {
                // Never install or execute code received over the network.
                // The event stays in the log; installing it is a user decision.
                warn!(
                    "Received capability '{}' ({}) from device {}; not installed automatically",
                    name, language, event.device_id
                );
            }
            SyncOperation::DefineCommand {
                name,
                description,
                steps_json,
                params,
                script,
            } => {
                let registry = self.command_registry.read().await.clone();
                if let Some(registry) = registry {
                    // Don't overwrite a locally-defined command of the same name.
                    if registry.contains(&name).await {
                        debug!(
                            "DefineCommand from mesh: '{}' already exists locally, skipping",
                            name
                        );
                    } else if let Ok(steps) =
                        serde_json::from_str::<Vec<crate::commands::CommandStep>>(&steps_json)
                    {
                        // Commands only run when the user invokes them by name,
                        // and every step is policy-gated at that point.
                        info!("Installing shared command from mesh: {}", name);
                        let _ = registry
                            .define_full(&name, &description, steps, params, script)
                            .await;
                    }
                }
            }
            SyncOperation::AddConversationTurn {
                session_id,
                user,
                assistant,
            } => {
                if self.sync_config.sync_ai_context {
                    let cm = self.context_manager.read().await.clone();
                    if let Some(cm) = cm {
                        if let Err(e) = cm.apply_synced_turn(&session_id, &user, &assistant).await {
                            warn!(
                                session_id = %session_id,
                                error = %e,
                                "Failed to apply synced conversation turn"
                            );
                        }
                    }
                } else {
                    debug!(
                        session_id = %session_id,
                        "Ignoring AddConversationTurn because sync_ai_context is false"
                    );
                }
            }
            SyncOperation::UpdatePreference { key, value } => {
                let cm = self.context_manager.read().await.clone();
                if let Some(cm) = cm {
                    if let Err(e) = cm.set_user_preference(&key, &value).await {
                        warn!(
                            key = %key,
                            error = %e,
                            "Failed to apply synced preference"
                        );
                    }
                }
            }
            SyncOperation::AddLearnedPattern { trigger, .. } => {
                debug!(trigger = %trigger, "Recorded AddLearnedPattern sync event");
            }
        }

        Ok(true)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
    pub status: PeerStatus,
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PeerStatus {
    Connected,
    Disconnected,
    Pairing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct SyncStatus {
    pub files_synced: usize,
    pub conflicts: usize,
    pub last_sync: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> String {
        let dir = std::env::temp_dir().join(format!("mycel-sync-test-{}", uuid::Uuid::new_v4()));
        dir.to_string_lossy().to_string()
    }

    fn peer_keys() -> (StaticSecret, String) {
        let secret = StaticSecret::random_from_rng(rand::thread_rng());
        let id = encode_id(PublicKey::from(&secret).as_bytes());
        (secret, id)
    }

    /// Build a service. `enabled` binds an ephemeral port; mDNS is always off.
    async fn service(trusted: &[String], enabled: bool, sync_ai_context: bool) -> SyncService {
        let config = MycelConfig {
            context_path: temp_dir(),
            mesh: crate::config::MeshConfig {
                enabled,
                listen_port: 0,
                mdns_enabled: false,
                trusted_peers: trusted.to_vec(),
                sync_ai_context,
                transport: MeshTransportMode::Direct,
                sync_port: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let (tx, _) = broadcast::channel(16);
        SyncService::new(&config, None, tx).await.unwrap()
    }

    fn event_from(device_id: &str, op: SyncOperation) -> SyncEvent {
        SyncEvent {
            id: uuid::Uuid::new_v4().to_string(),
            device_id: device_id.to_string(),
            timestamp: Utc::now(),
            clock: VectorClock::default(),
            operation: op,
            signature: Vec::new(),
        }
    }

    fn pref() -> SyncOperation {
        SyncOperation::UpdatePreference {
            key: "k".to_string(),
            value: "v".to_string(),
        }
    }

    fn addr() -> SocketAddr {
        "127.0.0.1:9".parse().unwrap()
    }

    async fn deliver(svc: &SyncService, packet: &MeshPacket) {
        svc.handle_packet(&serde_json::to_vec(packet).unwrap(), addr())
            .await;
    }

    #[test]
    fn test_vector_clock_merge() {
        let mut v1 = VectorClock::default();
        v1.increment("deviceA");
        v1.increment("deviceA");

        let mut v2 = VectorClock::default();
        v2.increment("deviceA");
        v2.increment("deviceB");

        v1.merge(&v2);

        assert_eq!(v1.map.get("deviceA"), Some(&2));
        assert_eq!(v1.map.get("deviceB"), Some(&1));
    }

    #[test]
    fn test_vector_clock_ordering() {
        let mut v1 = VectorClock::default();
        v1.increment("deviceA");

        let mut v2 = VectorClock::default();
        v2.increment("deviceA");
        v2.increment("deviceA");

        assert!(v2.is_ahead_of(&v1));
        assert!(!v1.is_ahead_of(&v2));

        v1.increment("deviceB");
        assert!(!v1.is_ahead_of(&v2));
        assert!(!v2.is_ahead_of(&v1));
    }

    #[test]
    fn test_define_command_operation_roundtrip() {
        let op = SyncOperation::DefineCommand {
            name: "backup".to_string(),
            description: "tar a folder".to_string(),
            steps_json: r#"[{"type":"shell","command":"tar czf {folder}.tgz {folder}"}]"#
                .to_string(),
            params: vec!["folder".to_string()],
            script: None,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: SyncOperation = serde_json::from_str(&json).unwrap();
        match back {
            SyncOperation::DefineCommand {
                name,
                params,
                script,
                ..
            } => {
                assert_eq!(name, "backup");
                assert_eq!(params, vec!["folder"]);
                assert!(script.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn test_disabled_mesh_binds_nothing_and_emits_nothing() {
        let svc = service(&[], false, true).await;
        assert!(svc.socket.read().await.is_none());
        assert!(svc.mdns.is_none());
        assert!(svc.create_event(pref()).await.unwrap().is_none());
        assert!(svc.state.read().await.log.is_empty());
    }

    #[tokio::test]
    async fn test_untrusted_handshake_ignored() {
        let (_, stranger) = peer_keys();
        let svc = service(&[], false, false).await;
        let pk = decode_id(&stranger).unwrap();
        deliver(
            &svc,
            &MeshPacket::Handshake {
                public_key: pk.as_bytes().to_vec(),
            },
        )
        .await;
        assert!(svc.get_peers().await.is_empty());
    }

    #[tokio::test]
    async fn test_trusted_handshake_records_peer() {
        let (_, friend) = peer_keys();
        let svc = service(std::slice::from_ref(&friend), false, false).await;
        let pk = decode_id(&friend).unwrap();
        deliver(
            &svc,
            &MeshPacket::Handshake {
                public_key: pk.as_bytes().to_vec(),
            },
        )
        .await;
        let peers = svc.get_peers().await;
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].id, friend);
        assert_eq!(peers[0].addresses, vec![addr().to_string()]);
    }

    #[tokio::test]
    async fn test_event_from_untrusted_key_rejected() {
        let (stranger_secret, stranger) = peer_keys();
        let svc = service(&[], false, false).await;
        let packet = seal_event(
            &stranger_secret,
            &svc.keys.public,
            &event_from(&stranger, pref()),
        )
        .unwrap();
        deliver(&svc, &packet).await;
        assert!(svc.state.read().await.log.is_empty());
    }

    #[tokio::test]
    async fn test_event_from_trusted_key_accepted() {
        let (friend_secret, friend) = peer_keys();
        let svc = service(std::slice::from_ref(&friend), false, false).await;
        let packet = seal_event(
            &friend_secret,
            &svc.keys.public,
            &event_from(&friend, pref()),
        )
        .unwrap();
        deliver(&svc, &packet).await;
        assert_eq!(svc.state.read().await.log.len(), 1);
    }

    #[tokio::test]
    async fn test_trusted_peer_cannot_impersonate_another_device() {
        let (friend_secret, friend) = peer_keys();
        let (_, other) = peer_keys();
        let svc = service(&[friend.clone(), other.clone()], false, false).await;
        let packet = seal_event(
            &friend_secret,
            &svc.keys.public,
            &event_from(&other, pref()),
        )
        .unwrap();
        deliver(&svc, &packet).await;
        assert!(svc.state.read().await.log.is_empty());
    }

    #[tokio::test]
    async fn test_capability_from_trusted_peer_is_logged_not_installed() {
        let (friend_secret, friend) = peer_keys();
        let svc = service(std::slice::from_ref(&friend), false, false).await;
        let name = format!("probe-{}", uuid::Uuid::new_v4());
        let op = SyncOperation::AddCapability {
            name: name.clone(),
            language: "python".to_string(),
            code: "import os; os.system('id')".to_string(),
        };
        let packet =
            seal_event(&friend_secret, &svc.keys.public, &event_from(&friend, op)).unwrap();
        deliver(&svc, &packet).await;
        assert_eq!(svc.state.read().await.log.len(), 1);
        let dynamic = std::env::current_dir()
            .unwrap()
            .join("mcp-servers/dynamic")
            .join(&name);
        assert!(!dynamic.exists(), "capability was written to {:?}", dynamic);
    }

    #[tokio::test]
    async fn test_conversation_turns_not_synced_by_default() {
        let svc = service(&[], true, false).await;
        let turn = SyncOperation::AddConversationTurn {
            session_id: "s".to_string(),
            user: "my password is hunter2".to_string(),
            assistant: "ok".to_string(),
        };
        assert!(svc.create_event(turn).await.unwrap().is_none());
        assert!(svc.create_event(pref()).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_conversation_turns_synced_when_opted_in() {
        let svc = service(&[], true, true).await;
        let turn = SyncOperation::AddConversationTurn {
            session_id: "s".to_string(),
            user: "hi".to_string(),
            assistant: "hello".to_string(),
        };
        assert!(svc.create_event(turn).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_own_id_and_garbage_dropped_from_trust_list() {
        let dir = temp_dir();
        let keys = DeviceKeys::load_or_generate(&dir).unwrap();
        let config = MycelConfig {
            context_path: dir,
            mesh: crate::config::MeshConfig {
                mdns_enabled: false,
                trusted_peers: vec![keys.id(), "not-a-key".to_string(), "  ".to_string()],
                ..Default::default()
            },
            ..Default::default()
        };
        let (tx, _) = broadcast::channel(4);
        let svc = SyncService::new(&config, None, tx).await.unwrap();
        assert!(svc.sync_config.trusted_peers.is_empty());
    }

    #[test]
    fn test_device_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        DeviceKeys::load_or_generate(&dir).unwrap();
        let path = std::path::Path::new(&dir).join("device_key");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // A loosened key file is tightened on next load.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        DeviceKeys::load_or_generate(&dir).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn test_event_log_is_capped_prunes_oldest_and_stays_ordered() {
        let dir = temp_dir();
        let log_path = PathBuf::from(&dir).join("sync_log.jsonl");
        let mut sync_log = log::SyncLog::load(log_path, EVENT_LOG_CAP).unwrap();
        let base = Utc::now();
        for i in 0..(EVENT_LOG_CAP + 25) {
            let mut e = event_from("dev", pref());
            e.timestamp = base + chrono::Duration::milliseconds(i as i64);
            assert!(sync_log.append(e).unwrap(), "event {i} rejected");
        }

        let events = sync_log.events();
        assert_eq!(events.len(), EVENT_LOG_CAP);
        // The 25 oldest entries were pruned; the newest is still last.
        assert_eq!(
            events[0].timestamp,
            base + chrono::Duration::milliseconds(25)
        );
        assert_eq!(
            events.last().unwrap().timestamp,
            base + chrono::Duration::milliseconds((EVENT_LOG_CAP + 24) as i64)
        );
        assert!(events.windows(2).all(|w| w[0].timestamp <= w[1].timestamp));
    }

    #[tokio::test]
    async fn test_duplicate_event_applied_once_and_order_kept() {
        let (friend_secret, friend) = peer_keys();
        let svc = service(std::slice::from_ref(&friend), false, false).await;

        let earlier = event_from(&friend, pref());
        let mut later = event_from(&friend, pref());
        later.clock.increment(&friend);

        // Deliver out of causal order, then retransmit the later event.
        for event in [&later, &earlier, &later, &later] {
            let packet = seal_event(&friend_secret, &svc.keys.public, event).unwrap();
            deliver(&svc, &packet).await;
        }

        let state = svc.state.read().await;
        let events = state.log.events();
        assert_eq!(events.len(), 2, "duplicates were not deduplicated");
        assert_eq!(events[0].id, earlier.id);
        assert_eq!(events[1].id, later.id);
    }

    async fn build_node(
        context_path: &str,
        sync_ai_context: bool,
    ) -> (
        SyncService,
        crate::context::ContextManager,
        crate::commands::CommandRegistry,
        broadcast::Receiver<SystemEvent>,
    ) {
        let config = MycelConfig {
            context_path: context_path.to_string(),
            mesh: crate::config::MeshConfig {
                enabled: true,
                listen_port: 0,
                mdns_enabled: false,
                trusted_peers: Vec::new(),
                sync_ai_context,
                transport: MeshTransportMode::Direct,
                sync_port: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let (tx, rx) = broadcast::channel(64);
        let cm = crate::context::ContextManager::new(&config).await.unwrap();
        let registry = crate::commands::CommandRegistry::load(
            PathBuf::from(context_path).join("commands.json"),
        )
        .await;
        let svc = SyncService::new(&config, None, tx).await.unwrap();
        svc.set_context_manager(cm.clone()).await;
        svc.set_command_registry(registry.clone()).await;
        svc.start().await.unwrap();
        (svc, cm, registry, rx)
    }

    #[tokio::test]
    async fn test_two_node_e2e_pairing_propagation_restart_and_security() {
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let dir_c = temp_dir();

        let (node_a, cm_a, _reg_a, mut rx_a) = build_node(&dir_a, true).await;
        let (node_b, cm_b, reg_b, mut rx_b) = build_node(&dir_b, true).await;
        let (node_c, _cm_c, _reg_c, _rx_c) = build_node(&dir_c, true).await;

        let addr_a = node_a.tcp_listen_addr().await.unwrap();
        let addr_b = node_b.tcp_listen_addr().await.unwrap();
        let ep_a = format!("127.0.0.1:{}", addr_a.port());
        let ep_b = format!("127.0.0.1:{}", addr_b.port());

        // 1. Untrusted Node C is rejected over live TCP.
        let mut c_frame = seal_sync_message(
            &node_c.keys.private,
            &node_c.device_id(),
            &node_a.keys.public,
            &SyncMessage::PullSince {
                clock: VectorClock::default(),
            },
        )
        .unwrap();
        let mut c_stream = TcpStream::connect(&ep_a).await.unwrap();
        write_wire_frame(&mut c_stream, &c_frame).await.unwrap();
        let (c_read, _) = c_stream.into_split();
        let mut c_reader = BufReader::new(c_read);
        let reply = read_wire_frame(&mut c_reader).await;
        assert!(
            matches!(reply, Ok(None) | Err(_)),
            "Untrusted node C should be rejected without an EventBatch"
        );
        // Also ensure spoofed sender_id in WireFrame is rejected.
        c_frame.sender_id = node_b.device_id();
        let mut c_stream2 = TcpStream::connect(&ep_a).await.unwrap();
        write_wire_frame(&mut c_stream2, &c_frame).await.unwrap();
        let (c_read2, _) = c_stream2.into_split();
        let mut c_reader2 = BufReader::new(c_read2);
        assert!(matches!(
            read_wire_frame(&mut c_reader2).await,
            Ok(None) | Err(_)
        ));

        // 2. Pair A <-> B via generate_pairing_code + pair_peer.
        let code_a = node_a.generate_pairing_code(Some(ep_a.clone())).await;
        let code_b = node_b.generate_pairing_code(Some(ep_b.clone())).await;

        let (peer_a_on_b, _) = node_b.pair_peer(&code_a.code).await.unwrap();
        assert_eq!(peer_a_on_b.id, node_a.device_id());
        let (peer_b_on_a, _) = node_a.pair_peer(&code_b.code).await.unwrap();
        assert_eq!(peer_b_on_a.id, node_b.device_id());

        // 3. Live propagation of UpdatePreference, DefineCommand, and AddConversationTurn.
        node_a
            .create_event(SyncOperation::UpdatePreference {
                key: "theme".to_string(),
                value: "solarized".to_string(),
            })
            .await
            .unwrap();
        node_a
            .create_event(SyncOperation::DefineCommand {
                name: "greet".to_string(),
                description: "say hi".to_string(),
                steps_json: "[]".to_string(),
                params: vec!["who".to_string()],
                script: Some("echo hi".to_string()),
            })
            .await
            .unwrap();
        node_a
            .create_event(SyncOperation::AddConversationTurn {
                session_id: "sess-1".to_string(),
                user: "hello mesh".to_string(),
                assistant: "hi from node a".to_string(),
            })
            .await
            .unwrap();

        // Give async TCP push a brief moment, or trigger sync if needed.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = node_b.sync_with_peer(&node_a.device_id()).await.unwrap();

        let b_ctx = cm_b.get_context("sess-1").await.unwrap();
        assert_eq!(
            b_ctx.user_preferences.get("theme").map(String::as_str),
            Some("solarized")
        );
        assert_eq!(b_ctx.conversation_history.len(), 1);
        assert_eq!(b_ctx.conversation_history[0].user, "hello mesh");
        assert!(reg_b.contains("greet").await);

        let b_log_path = PathBuf::from(&dir_b).join("sync_log.jsonl");
        assert!(b_log_path.exists());
        let b_log_reloaded = log::SyncLog::load(b_log_path, EVENT_LOG_CAP).unwrap();
        assert_eq!(b_log_reloaded.len(), 3);

        // 4. Offline gap + restart + reconnect catch-up.
        node_b.shutdown();
        drop(node_b);
        drop(cm_b);
        drop(reg_b);
        tokio::time::sleep(Duration::from_millis(20)).await;

        node_a
            .create_event(SyncOperation::UpdatePreference {
                key: "editor".to_string(),
                value: "nvim".to_string(),
            })
            .await
            .unwrap();
        node_a
            .create_event(SyncOperation::UpdatePreference {
                key: "shell".to_string(),
                value: "zsh".to_string(),
            })
            .await
            .unwrap();

        let (node_b2, cm_b2, _reg_b2, mut rx_b2) = build_node(&dir_b, true).await;
        assert_eq!(node_b2.status().await.trusted_peers, 1);
        assert_eq!(node_b2.status().await.event_log_len, 3);

        let synced = node_b2.sync_with_peer(&node_a.device_id()).await.unwrap();
        assert_eq!(synced, 2, "only the 2 missing events should be applied");
        assert_eq!(node_b2.status().await.event_log_len, 5);

        let b2_ctx = cm_b2.get_context("sess-1").await.unwrap();
        assert_eq!(
            b2_ctx.user_preferences.get("theme").map(String::as_str),
            Some("solarized")
        );
        assert_eq!(
            b2_ctx.user_preferences.get("editor").map(String::as_str),
            Some("nvim")
        );
        assert_eq!(
            b2_ctx.user_preferences.get("shell").map(String::as_str),
            Some("zsh")
        );
        // Zero duplicates in persisted log
        let turn_count = node_b2
            .state
            .read()
            .await
            .log
            .events()
            .iter()
            .filter(|e| matches!(e.operation, SyncOperation::AddConversationTurn { .. }))
            .count();
        assert_eq!(turn_count, 1);

        // Verify SyncStateChanged events were emitted.
        let mut saw_a_state = false;
        while let Ok(ev) = rx_a.try_recv() {
            if matches!(ev, SystemEvent::SyncStateChanged { .. }) {
                saw_a_state = true;
            }
        }
        assert!(saw_a_state, "Node A should emit SyncStateChanged");

        let mut saw_b_state = false;
        while let Ok(ev) = rx_b.try_recv() {
            if matches!(ev, SystemEvent::SyncStateChanged { .. }) {
                saw_b_state = true;
            }
        }
        while let Ok(ev) = rx_b2.try_recv() {
            if matches!(ev, SystemEvent::SyncStateChanged { .. }) {
                saw_b_state = true;
            }
        }
        assert!(saw_b_state, "Node B should emit SyncStateChanged");

        // 5. sync_ai_context = false blocks incoming AddConversationTurn even from trusted peer.
        let dir_d = temp_dir();
        let (node_d, cm_d, _reg_d, _rx_d) = build_node(&dir_d, false).await;
        node_a
            .add_trusted_peer(&node_d.device_id(), Some("node-d".to_string()), None)
            .await
            .unwrap();
        let (_peer_d, synced_d) = node_d.pair_peer(&code_a.code).await.unwrap();
        assert_eq!(synced_d, 5);
        assert_eq!(node_d.sync_with_peer(&node_a.device_id()).await.unwrap(), 0);
        let d_ctx = cm_d.get_context("sess-1").await.unwrap();
        assert!(
            d_ctx.conversation_history.is_empty(),
            "AddConversationTurn must not be applied when sync_ai_context is false"
        );
        assert_eq!(
            d_ctx.user_preferences.get("theme").map(String::as_str),
            Some("solarized")
        );
        let _ = cm_a;
    }

    #[tokio::test]
    async fn test_wg_mode_new_does_not_bind_udp_mesh_port() {
        let dir = temp_dir();
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let mesh_port = probe.local_addr().unwrap().port();
        drop(probe);

        let config = MycelConfig {
            context_path: dir,
            mesh: crate::config::MeshConfig {
                enabled: true,
                listen_port: mesh_port,
                mdns_enabled: false,
                transport: MeshTransportMode::WireGuard,
                sync_port: 0,
                wg_address: Some("10.77.0.1/24".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let (tx, _) = broadcast::channel(16);
        let svc = SyncService::new(&config, None, tx).await.unwrap();
        assert!(svc.socket.read().await.is_none());

        // Kernel WireGuard should be free to bind the same UDP port.
        let rebound = std::net::UdpSocket::bind(("0.0.0.0", mesh_port));
        assert!(
            rebound.is_ok(),
            "WireGuard mode SyncService::new must not bind UDP mesh_port"
        );
    }

    #[tokio::test]
    async fn test_wg_pairing_payload_and_persisted_meta_roundtrip() {
        let dir_a = temp_dir();
        let config_a = MycelConfig {
            context_path: dir_a,
            mesh: crate::config::MeshConfig {
                enabled: true,
                listen_port: 51820,
                mdns_enabled: false,
                transport: MeshTransportMode::WireGuard,
                sync_port: 51821,
                wg_address: Some("10.77.0.1/24".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let (tx_a, _) = broadcast::channel(16);
        let svc_a = SyncService::new(&config_a, None, tx_a).await.unwrap();
        {
            let mut state = svc_a.state.write().await;
            state.wg = Some(
                wireguard::KernelWireGuard::new(
                    "mycel0".to_string(),
                    std::net::Ipv4Addr::new(10, 77, 0, 1),
                    24,
                )
                .unwrap(),
            );
            state.tcp_listen_addr = Some("0.0.0.0:51821".parse().unwrap());
        }

        let code_info = svc_a
            .generate_pairing_code(Some("192.168.100.10".to_string()))
            .await;
        assert_eq!(code_info.endpoint.as_deref(), Some("192.168.100.10:51820"));
        let encoded = code_info.code.strip_prefix("mycel-pair-v1:").unwrap();
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let payload: PairingPayload = serde_json::from_slice(&raw).unwrap();
        assert_eq!(payload.wg_ip.as_deref(), Some("10.77.0.1"));
        assert_eq!(payload.sync_port, Some(51821));
        assert_eq!(payload.endpoint.as_deref(), Some("192.168.100.10:51820"));

        // Node B pairs with Node A's code in WireGuard mode (without invoking real `wg` binary
        // by testing state & persistence; `set_peer` failure is logged and tolerated).
        let dir_b = temp_dir();
        let config_b = MycelConfig {
            context_path: dir_b,
            mesh: crate::config::MeshConfig {
                enabled: true,
                listen_port: 51820,
                mdns_enabled: false,
                transport: MeshTransportMode::WireGuard,
                sync_port: 51821,
                wg_address: Some("10.77.0.2/24".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let (tx_b, _) = broadcast::channel(16);
        let svc_b = SyncService::new(&config_b, None, tx_b).await.unwrap();
        {
            let mut state = svc_b.state.write().await;
            state.wg = Some(
                wireguard::KernelWireGuard::new(
                    "mycel0".to_string(),
                    std::net::Ipv4Addr::new(10, 77, 0, 2),
                    24,
                )
                .unwrap(),
            );
        }

        let (peer_a, _) = svc_b.pair_peer(&code_info.code).await.unwrap();
        assert_eq!(
            peer_a.addresses.first().map(String::as_str),
            Some("10.77.0.1:51821"),
            "Overlay TCP sync address must be stored first"
        );

        // Reload from trusted_peers.json and verify wg_ip & wg_endpoint are restored.
        let (tx_b2, _) = broadcast::channel(16);
        let svc_b_reloaded = SyncService::new(&config_b, None, tx_b2).await.unwrap();
        let state_b2 = svc_b_reloaded.state.read().await;
        let restored_meta = state_b2.peer_wg.get(&svc_a.device_id()).unwrap();
        assert_eq!(restored_meta.wg_ip.as_deref(), Some("10.77.0.1"));
        assert_eq!(
            restored_meta.wg_endpoint.as_deref(),
            Some("192.168.100.10:51820")
        );
    }

    #[tokio::test]
    async fn test_two_node_relay_e2e_zero_knowledge_and_untrusted_rejection() {
        let relay_server = relay::MeshRelayServer::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay_server.local_addr().to_string();
        let mut observed_rx = relay_server.subscribe_observed_payloads();
        let (relay_shutdown_tx, relay_shutdown_rx) = tokio::sync::watch::channel(false);
        let relay_handle = tokio::spawn(async move {
            let _ = relay_server.run(relay_shutdown_rx).await;
        });

        let build_relay_node = |dir: &str| {
            let dir = dir.to_string();
            let relay_addr = relay_addr.clone();
            async move {
                let cfg = MycelConfig {
                    context_path: dir,
                    mesh: crate::config::MeshConfig {
                        enabled: true,
                        listen_port: 0,
                        mdns_enabled: false,
                        transport: MeshTransportMode::Direct,
                        sync_port: 0,
                        sync_ai_context: true,
                        relay_enabled: true,
                        relay_addr: Some(relay_addr),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let cm = crate::context::ContextManager::new(&cfg).await.unwrap();
                let reg = crate::commands::CommandRegistry::in_memory();
                let (tx, _) = broadcast::channel(32);
                let svc = SyncService::new(&cfg, None, tx).await.unwrap();
                svc.set_context_manager(cm.clone()).await;
                svc.set_command_registry(reg.clone()).await;
                svc.start().await.unwrap();
                (svc, cm, reg)
            }
        };

        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let dir_c = temp_dir();
        let (node_a, cm_a, reg_a) = build_relay_node(&dir_a).await;
        let (node_b, cm_b, reg_b) = build_relay_node(&dir_b).await;

        // Wait until both nodes report relay_connected == true.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node_a.status().await.relay_connected && node_b.status().await.relay_connected {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Timed out waiting for nodes A and B to connect to relay"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Pair Node A <-> Node B with NO direct address (endpoint: None).
        node_a
            .add_trusted_peer(&node_b.device_id(), Some("node-b".to_string()), None)
            .await
            .unwrap();
        node_b
            .add_trusted_peer(&node_a.device_id(), Some("node-a".to_string()), None)
            .await
            .unwrap();

        let cmd_name = "relay-deploy-check";
        let cmd_desc = "Verify zero-knowledge relay command replication";
        let user_prompt = "secret-relay-user-prompt-xyz";
        let assistant_reply = "secret-relay-assistant-reply-abc";

        // Broadcast DefineCommand from Node A -> assert Node B applies it.
        node_a
            .create_event(SyncOperation::DefineCommand {
                name: cmd_name.to_string(),
                description: cmd_desc.to_string(),
                steps_json: "[]".to_string(),
                params: vec!["env".to_string()],
                script: Some("echo ok".to_string()),
            })
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if reg_b.get(cmd_name).await.is_some() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Node B did not apply DefineCommand via relay"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let applied_cmd = reg_b.get(cmd_name).await.unwrap();
        assert_eq!(applied_cmd.description, cmd_desc);

        // Broadcast AddConversationTurn from Node B -> assert Node A applies it.
        node_b
            .create_event(SyncOperation::AddConversationTurn {
                session_id: "relay-sess-1".to_string(),
                user: user_prompt.to_string(),
                assistant: assistant_reply.to_string(),
            })
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let ctx = cm_a.get_context("relay-sess-1").await.unwrap();
            if !ctx.conversation_history.is_empty() {
                assert_eq!(ctx.conversation_history[0].user, user_prompt);
                assert_eq!(ctx.conversation_history[0].assistant, assistant_reply);
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Node A did not apply AddConversationTurn via relay"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Verify zero-knowledge property: relay observed >= 2 forwarded payloads and NONE
        // contain any plaintext command name, description, user prompt, or assistant reply.
        let mut observed_payloads = Vec::new();
        while let Ok(payload) = observed_rx.try_recv() {
            observed_payloads.push(payload);
        }
        assert!(
            observed_payloads.len() >= 2,
            "Expected relay to observe >= 2 forwarded payloads, got {}",
            observed_payloads.len()
        );
        for payload in &observed_payloads {
            let text = String::from_utf8_lossy(payload);
            assert!(
                !text.contains(cmd_name)
                    && !text.contains(cmd_desc)
                    && !text.contains(user_prompt)
                    && !text.contains(assistant_reply),
                "Relay observed plaintext in forwarded payload!"
            );
        }

        // Connect untrusted Node C to the same MeshRelayServer and forward spoofed/malicious
        // frames targeting Node A and Node B; both must reject them completely.
        let (node_c, _cm_c, _reg_c) = build_relay_node(&dir_c).await;
        let (c_shutdown_tx, c_shutdown_rx) = tokio::sync::watch::channel(false);
        let (c_client, _c_rx) =
            relay::RelayClient::start(relay_addr.clone(), node_c.device_id(), c_shutdown_rx);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !c_client.is_connected() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "Untrusted Node C failed to connect to relay"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let rogue_cmd = "rogue-untrusted-cmd";
        let rogue_event = SyncEvent {
            id: uuid::Uuid::new_v4().to_string(),
            device_id: node_c.device_id(),
            timestamp: Utc::now(),
            clock: VectorClock::default(),
            operation: SyncOperation::DefineCommand {
                name: rogue_cmd.to_string(),
                description: "Should be rejected".to_string(),
                steps_json: "[]".to_string(),
                params: Vec::new(),
                script: Some("echo pwned".to_string()),
            },
            signature: Vec::new(),
        };
        let rogue_turn_event = SyncEvent {
            id: uuid::Uuid::new_v4().to_string(),
            device_id: node_c.device_id(),
            timestamp: Utc::now(),
            clock: VectorClock::default(),
            operation: SyncOperation::AddConversationTurn {
                session_id: "rogue-sess".to_string(),
                user: "rogue-user".to_string(),
                assistant: "rogue-assistant".to_string(),
            },
            signature: Vec::new(),
        };

        let a_pk = decode_id(&node_a.device_id()).unwrap();
        let b_pk = decode_id(&node_b.device_id()).unwrap();
        let frame_for_a = seal_sync_message(
            &node_c.keys.private,
            &node_c.device_id(),
            &a_pk,
            &SyncMessage::PushEvent {
                event: rogue_turn_event,
            },
        )
        .unwrap();
        let frame_for_b = seal_sync_message(
            &node_c.keys.private,
            &node_c.device_id(),
            &b_pk,
            &SyncMessage::PushEvent { event: rogue_event },
        )
        .unwrap();

        c_client
            .forward(
                &node_a.device_id(),
                serde_json::to_vec(&frame_for_a).unwrap(),
            )
            .await
            .unwrap();
        c_client
            .forward(
                &node_b.device_id(),
                serde_json::to_vec(&frame_for_b).unwrap(),
            )
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            reg_b.get(rogue_cmd).await.is_none(),
            "Node B must reject DefineCommand from untrusted Node C"
        );
        assert!(
            reg_a.get(rogue_cmd).await.is_none(),
            "Node A must reject DefineCommand from untrusted Node C"
        );
        let a_rogue_ctx = cm_a.get_context("rogue-sess").await.unwrap();
        assert!(
            a_rogue_ctx.conversation_history.is_empty(),
            "Node A must reject AddConversationTurn from untrusted Node C"
        );
        let b_rogue_ctx = cm_b.get_context("rogue-sess").await.unwrap();
        assert!(
            b_rogue_ctx.conversation_history.is_empty(),
            "Node B must reject AddConversationTurn from untrusted Node C"
        );

        node_a.shutdown();
        node_b.shutdown();
        node_c.shutdown();
        let _ = c_shutdown_tx.send(true);
        let _ = relay_shutdown_tx.send(true);
        let _ = relay_handle.await;
    }
}
