//! Device mesh synchronization
//!
//! Syncs config, patterns, and files between user's Mycel devices
//! using WireGuard for transport and CRDTs for conflict-free merge.

use crate::config::MycelConfig;
use crate::events::SystemEvent;
use crate::mcp::{McpEvolver, McpManager};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, warn};
use x25519_dalek::{PublicKey, StaticSecret};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305,
};

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    pub mesh_port: u16,
    pub discovery_enabled: bool,
    pub device_name: String,
    pub blockchain_sync: bool,
    pub near_account: Option<String>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            mesh_port: 51820,
            discovery_enabled: true,
            device_name: "mycel-device".to_string(),
            blockchain_sync: false,
            near_account: None,
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
            info!("Generating new WireGuard device keys...");
            let mut rng = rand::thread_rng();
            let private = StaticSecret::random_from_rng(&mut rng);
            let public = PublicKey::from(&private);
            let _ = std::fs::create_dir_all(path);
            std::fs::write(&key_path, private.to_bytes())?;
            Ok(Self { private, public })
        }
    }
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
    socket: Arc<UdpSocket>,
    event_bus: broadcast::Sender<SystemEvent>,
    runtime_path: String,
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
        let keys = DeviceKeys::load_or_generate(&config.context_path)?;
        let sync_config = SyncConfig {
            mesh_port: 51820,
            discovery_enabled: true,
            device_name: "mycel-device".to_string(),
            blockchain_sync: config.blockchain_sync,
            near_account: config.near_account.clone(),
        };

        let runtime_path = std::env::current_dir()?
            .to_string_lossy()
            .to_string();

        let socket = match UdpSocket::bind(format!("0.0.0.0:{}", sync_config.mesh_port)).await {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    "Failed to bind to port {}: {}, falling back to random",
                    sync_config.mesh_port, e
                );
                UdpSocket::bind("0.0.0.0:0").await?
            }
        };

        Ok(Self {
            sync_config: sync_config.clone(),
            state: Arc::new(RwLock::new(SyncState::default())),
            keys: Arc::new(keys),
            mdns: if sync_config.discovery_enabled {
                Some(ServiceDaemon::new()?)
            } else {
                None
            },
            mcp_manager: Arc::new(mcp_manager),
            socket: Arc::new(socket),
            event_bus,
            runtime_path,
        })
    }

    pub async fn start(&self) -> Result<()> {
        let pubkey_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            self.keys.public.as_bytes(),
        );
        let port = self.socket.local_addr()?.port();
        info!(
            "Sync service starting on port {}. Mycel ID: {}",
            port, pubkey_b64
        );

        if let Some(mdns) = &self.mdns {
            self.start_discovery(mdns).await?;
        }

        if self.sync_config.blockchain_sync {
            self.start_blockchain_sync().await?;
        }

        let service = self.clone();
        tokio::spawn(async move {
            if let Err(e) = service.listen_loop().await {
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

    async fn listen_loop(&self) -> Result<()> {
        let mut buf = [0u8; 65535];
        loop {
            let (len, addr) = self.socket.recv_from(&mut buf).await?;
            let data = &buf[..len];

            match serde_json::from_slice::<MeshPacket>(data) {
                Ok(MeshPacket::Handshake { public_key }) => {
                    if public_key.len() == 32 {
                        let peer_id = base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            &public_key,
                        );

                        let mut state = self.state.write().await;
                        state.peers.entry(peer_id.clone()).or_insert_with(|| PeerInfo {
                            id: peer_id,
                            name: format!("peer-{}", addr),
                            status: PeerStatus::Connected,
                            addresses: vec![addr.to_string()],
                        });
                        debug!("Received handshake from {}", addr);
                    }
                }
                Ok(MeshPacket::Event {
                    nonce,
                    encrypted_data,
                }) => {
                    let peers = self.state.read().await.peers.clone();
                    for (peer_id, _info) in peers {
                        if let Ok(peer_pk_bytes) = base64::Engine::decode(
                            &base64::engine::general_purpose::STANDARD,
                            &peer_id,
                        ) {
                            if peer_pk_bytes.len() == 32 {
                                let mut pk_bytes = [0u8; 32];
                                pk_bytes.copy_from_slice(&peer_pk_bytes);
                                let peer_pk = PublicKey::from(pk_bytes);

                                let shared_secret = self.keys.private.diffie_hellman(&peer_pk);
                                let cipher = ChaCha20Poly1305::new(shared_secret.as_bytes().into());

                                if let Ok(decrypted) = cipher.decrypt(
                                    &nonce.into(),
                                    Payload {
                                        msg: &encrypted_data,
                                        aad: &[],
                                    },
                                ) {
                                    if let Ok(event) = serde_json::from_slice::<SyncEvent>(&decrypted)
                                    {
                                        let _ = self.apply_event(event).await;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!("Received invalid mesh packet from {}: {}", addr, e);
                }
            }
        }
    }

    async fn start_discovery(&self, mdns: &ServiceDaemon) -> Result<()> {
        let service_type = "_mycel._udp.local.";
        let instance_name = format!("{}.{}", self.sync_config.device_name, uuid::Uuid::new_v4());
        let host_name = format!("{}.local.", self.sync_config.device_name);
        let port = self.socket.local_addr()?.port();

        let pub_key_base64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            self.keys.public.as_bytes(),
        );

        let properties = [("pubkey", pub_key_base64)];

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
                match event {
                    mdns_sd::ServiceEvent::ServiceResolved(info) => {
                        debug!("Found Mycel device via mDNS: {:?}", info.get_fullname());
                        if let Some(pubkey) = info.get_property_val_str("pubkey") {
                            let mut state = service.state.write().await;
                            let addresses: Vec<String> = info
                                .get_addresses()
                                .iter()
                                .map(|a| format!("{}:{}", a, info.get_port()))
                                .collect();

                            state.peers.entry(pubkey.to_string()).or_insert_with(|| PeerInfo {
                                id: pubkey.to_string(),
                                name: info.get_fullname().to_string(),
                                status: PeerStatus::Connected,
                                addresses: addresses.clone(),
                            });

                            for addr_str in addresses {
                                if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                                    let _ = service.send_handshake(addr).await;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        Ok(())
    }

    async fn send_handshake(&self, addr: SocketAddr) -> Result<()> {
        let packet = MeshPacket::Handshake {
            public_key: self.keys.public.as_bytes().to_vec(),
        };
        let data = serde_json::to_vec(&packet)?;
        self.socket.send_to(&data, addr).await?;
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

                    // 1. Poll for Peers
                    let peer_args = HashMap::from([("accountId".to_string(), serde_json::json!(acc))]);
                    if let Ok(result) = mcp.call_tool("near_get_peers", peer_args).await {
                        for content in result.content {
                            if let crate::mcp::protocol::ToolContent::Text { text } = content {
                                if let Ok(peers) = serde_json::from_str::<Vec<PeerInfo>>(&text) {
                                    let mut state = service.state.write().await;
                                    for peer in peers {
                                        state.peers.entry(peer.id.clone()).or_insert(peer.clone());
                                        for addr_str in &peer.addresses {
                                            if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                                                let _ = service.send_handshake(addr).await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // 2. Poll for Shared Capabilities
                    let cap_args = HashMap::from([("query".to_string(), serde_json::json!(acc))]);
                    if let Ok(result) = mcp.call_tool("near_discover_capabilities", cap_args).await {
                        for content in result.content {
                            if let crate::mcp::protocol::ToolContent::Text { text } = content {
                                if let Ok(caps) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                                    for cap in caps {
                                        // If discovery returns code, we could auto-install.
                                        // For now, discovery just lists them.
                                        // The AI can decide to install via evolve_os_install_capability
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

    pub async fn create_event(&self, operation: SyncOperation) -> Result<SyncEvent> {
        let mut state = self.state.write().await;

        let device_id = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            self.keys.public.as_bytes(),
        );

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

        Ok(event)
    }

    async fn send_event(&self, peer: &PeerInfo, event: &SyncEvent) -> Result<()> {
        let peer_pk_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &peer.id)?;
        if peer_pk_bytes.len() != 32 {
            return Err(anyhow!("Invalid peer public key"));
        }

        let mut pk_bytes = [0u8; 32];
        pk_bytes.copy_from_slice(&peer_pk_bytes);
        let peer_pk = PublicKey::from(pk_bytes);

        let shared_secret = self.keys.private.diffie_hellman(&peer_pk);
        let cipher = ChaCha20Poly1305::new(shared_secret.as_bytes().into());

        let (nonce_bytes, encrypted) = {
            let mut nonce_bytes = [0u8; 12];
            let mut rng = rand::thread_rng();
            use rand::RngCore;
            rng.fill_bytes(&mut nonce_bytes);

            let event_json = serde_json::to_vec(event)?;
            let encrypted = cipher
                .encrypt(
                    &nonce_bytes.into(),
                    Payload {
                        msg: &event_json,
                        aad: &[],
                    },
                )
                .map_err(|e| anyhow!("Encryption error: {}", e))?;
            (nonce_bytes, encrypted)
        };

        let packet = MeshPacket::Event {
            nonce: nonce_bytes,
            encrypted_data: encrypted,
        };

        let packet_data = serde_json::to_vec(&packet)?;

        for addr_str in &peer.addresses {
            if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                let _ = self.socket.send_to(&packet_data, addr).await;
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

        info!(event_id = %event.id, "Event integrated into local mesh log");

        // 5. React to the event
        match event.operation {
            SyncOperation::AddCapability {
                name,
                language,
                code,
            } => {
                if let Some(mcp) = &*self.mcp_manager {
                    info!("Installing shared capability from mesh: {}", name);
                    let evolver = McpEvolver::new(mcp.clone(), &self.runtime_path);
                    let _ = evolver.create_server(&name, &language, &code, false).await;
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
}
