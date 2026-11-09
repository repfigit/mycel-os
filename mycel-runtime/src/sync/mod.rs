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

use crate::config::MycelConfig;
use crate::events::SystemEvent;
use crate::mcp::McpManager;
use anyhow::{anyhow, Result};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305,
};
use chrono::{DateTime, Utc};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, warn};
use x25519_dalek::{PublicKey, StaticSecret};

/// Upper bound on remembered addresses per peer. Handshakes and mDNS records
/// are unauthenticated, so this keeps spoofed hints from growing without bound.
const MAX_PEER_ADDRESSES: usize = 8;

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

#[derive(Default)]
struct SyncState {
    peers: HashMap<String, PeerInfo>,
    event_log: Vec<SyncEvent>,
    local_clock: VectorClock,
}

#[derive(Clone)]
pub struct SyncService {
    sync_config: SyncConfig,
    state: Arc<RwLock<SyncState>>,
    keys: Arc<DeviceKeys>,
    mdns: Option<ServiceDaemon>,
    mcp_manager: Arc<Option<McpManager>>,
    /// Bound only when the mesh is enabled.
    socket: Option<Arc<UdpSocket>>,
    event_bus: broadcast::Sender<SystemEvent>,
    /// Optional command registry for replicating user-defined commands.
    command_registry: Arc<RwLock<Option<crate::commands::CommandRegistry>>>,
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

        let sync_config = SyncConfig {
            enabled: mesh.enabled,
            mesh_port: mesh.listen_port,
            discovery_enabled: mesh.enabled && mesh.mdns_enabled,
            device_name: "mycel-device".to_string(),
            blockchain_sync: config.blockchain_sync,
            near_account: config.near_account.clone(),
            trusted_peers,
            sync_ai_context: mesh.sync_ai_context,
        };

        let socket = if sync_config.enabled {
            if sync_config.trusted_peers.is_empty() {
                warn!("Mesh enabled but [mesh] trusted_peers is empty; no device will be accepted");
            }
            let bound = match UdpSocket::bind(format!("0.0.0.0:{}", sync_config.mesh_port)).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        "Failed to bind to port {}: {}, falling back to random",
                        sync_config.mesh_port, e
                    );
                    UdpSocket::bind("0.0.0.0:0").await?
                }
            };
            Some(Arc::new(bound))
        } else {
            None
        };

        let mdns = if sync_config.discovery_enabled {
            Some(ServiceDaemon::new()?)
        } else {
            None
        };

        Ok(Self {
            sync_config,
            state: Arc::new(RwLock::new(SyncState::default())),
            keys: Arc::new(keys),
            mdns,
            mcp_manager: Arc::new(mcp_manager),
            socket,
            event_bus,
            command_registry: Arc::new(RwLock::new(None)),
        })
    }

    /// Attach the command registry so DefineCommand events can be applied.
    pub async fn set_command_registry(&self, registry: crate::commands::CommandRegistry) {
        *self.command_registry.write().await = Some(registry);
    }

    pub async fn start(&self) -> Result<()> {
        let Some(socket) = self.socket.clone() else {
            info!(
                "Mesh disabled. Mycel ID: {} (add it to [mesh] trusted_peers on your other devices)",
                self.keys.id()
            );
            return Ok(());
        };

        let port = socket.local_addr()?.port();
        info!(
            "Sync service starting on port {}. Mycel ID: {} ({} trusted peers)",
            port,
            self.keys.id(),
            self.sync_config.trusted_peers.len()
        );

        if let Some(mdns) = &self.mdns {
            self.start_discovery(mdns, port).await?;
        }

        if self.sync_config.blockchain_sync {
            self.start_blockchain_sync().await?;
        }

        let service = self.clone();
        tokio::spawn(async move {
            if let Err(e) = service.listen_loop(socket).await {
                error!("Mesh listener loop error: {}", e);
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
                    // Tool call events are logged but not synced to mesh
                    SystemEvent::ToolCalled { .. } => {}
                    // Server restart events are logged but not synced to mesh
                    SystemEvent::McpServerRestarted { .. } => {}
                }
            }
        });

        Ok(())
    }

    fn is_trusted(&self, peer_id: &str) -> bool {
        self.sync_config.trusted_peers.contains(peer_id)
    }

    async fn listen_loop(&self, socket: Arc<UdpSocket>) -> Result<()> {
        let mut buf = [0u8; 65535];
        loop {
            let (len, addr) = socket.recv_from(&mut buf).await?;
            self.handle_packet(&buf[..len], addr).await;
        }
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
            }) => match self.open_from_trusted(&nonce, &encrypted_data) {
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
            },
            Err(e) => {
                debug!("Received invalid mesh packet from {}: {}", addr, e);
            }
        }
    }

    /// Record a reachable address for a trusted peer. Returns false (and
    /// records nothing) for untrusted IDs.
    async fn note_peer_address(&self, peer_id: &str, addr: &str, name: Option<&str>) -> bool {
        if !self.is_trusted(peer_id) {
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
    fn open_from_trusted(&self, nonce: &[u8; 12], data: &[u8]) -> Option<(String, SyncEvent)> {
        for peer_id in &self.sync_config.trusted_peers {
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
        let Some(socket) = &self.socket else {
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

        state.event_log.push(event.clone());

        let peers = state.peers.clone();
        drop(state);

        for peer in peers.values() {
            let _ = self.send_event(peer, &event).await;
        }

        Ok(Some(event))
    }

    async fn send_event(&self, peer: &PeerInfo, event: &SyncEvent) -> Result<()> {
        let Some(socket) = &self.socket else {
            return Ok(());
        };
        if !self.is_trusted(&peer.id) {
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
        debug!(event_id = %event.id, device = %event.device_id, "Applying sync event");

        let mut state = self.state.write().await;

        if state.event_log.iter().any(|e| e.id == event.id) {
            return Ok(());
        }

        state.local_clock.merge(&event.clock);

        state.event_log.push(event.clone());
        state.event_log.sort_by(|a, b| {
            if a.clock.is_ahead_of(&b.clock) {
                std::cmp::Ordering::Greater
            } else if b.clock.is_ahead_of(&a.clock) {
                std::cmp::Ordering::Less
            } else {
                a.timestamp.cmp(&b.timestamp)
            }
        });
        drop(state);

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
            _ => {}
        }

        Ok(())
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
            },
            ..Default::default()
        };
        let (tx, _) = broadcast::channel(4);
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
        assert!(svc.socket.is_none());
        assert!(svc.mdns.is_none());
        assert!(svc.create_event(pref()).await.unwrap().is_none());
        assert!(svc.state.read().await.event_log.is_empty());
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
        assert!(svc.state.read().await.event_log.is_empty());
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
        assert_eq!(svc.state.read().await.event_log.len(), 1);
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
        assert!(svc.state.read().await.event_log.is_empty());
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
        assert_eq!(svc.state.read().await.event_log.len(), 1);
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
}
