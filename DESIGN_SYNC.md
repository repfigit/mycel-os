# Device Sync Design Document

Detailed implementation guide for the `sync` module.

---

## Overview

Users have multiple Mycel devices (laptop, desktop, phone). They want:
- Same config everywhere
- Documents sync automatically
- AI context shared
- Encrypted, no central server
- Works offline, syncs when connected

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    PERSONAL MYCEL MESH                          │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│   Desktop                Laptop                   Phone          │
│  ┌────────┐            ┌────────┐              ┌────────┐       │
│  │ Mycel  │◄──────────►│ Mycel  │◄────────────►│ Mycel  │       │
│  │        │ WireGuard  │        │  WireGuard   │        │       │
│  │ Node A │   Mesh     │ Node B │    Mesh      │ Node C │       │
│  └────────┘            └────────┘              └────────┘       │
│       │                     │                       │            │
│       └─────────────────────┼───────────────────────┘            │
│                             │                                    │
│                    ┌────────────────┐                           │
│                    │  Shared State  │                           │
│                    │  (CRDT Merge)  │                           │
│                    └────────────────┘                           │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Components

### 1. Mesh Network (WireGuard)

```rust
// sync/mesh.rs

use std::net::SocketAddr;

pub struct MeshNetwork {
    /// WireGuard interface name (e.g., "mycel0")
    interface: String,
    
    /// This node's WireGuard private key
    private_key: [u8; 32],
    
    /// This node's WireGuard public key
    public_key: [u8; 32],
    
    /// Known peers
    peers: Vec<MeshPeer>,
    
    /// Listen port
    port: u16,
}

pub struct MeshPeer {
    /// Peer's public key
    public_key: [u8; 32],
    
    /// Human-readable name
    name: String,
    
    /// Known endpoints (may be multiple)
    endpoints: Vec<SocketAddr>,
    
    /// Allowed IPs (mesh subnet)
    allowed_ips: Vec<String>,
    
    /// Last seen timestamp
    last_seen: Option<chrono::DateTime<chrono::Utc>>,
    
    /// Is currently connected
    connected: bool,
}

impl MeshNetwork {
    /// Create new mesh network
    pub fn new(interface: &str, port: u16) -> Result<Self> {
        // Generate keypair if not exists
        // Configure WireGuard interface
    }
    
    /// Add a peer to the mesh
    pub fn add_peer(&mut self, peer: MeshPeer) -> Result<()> {
        // Add to WireGuard config
        // wg set mycel0 peer <pubkey> allowed-ips <ips> endpoint <endpoint>
    }
    
    /// Remove a peer
    pub fn remove_peer(&mut self, public_key: &[u8; 32]) -> Result<()> {
        // Remove from WireGuard config
    }
    
    /// Check peer connectivity
    pub async fn check_peers(&mut self) -> Vec<PeerStatus> {
        // Ping each peer, update status
    }
}
```

### 2. Pairing Protocol

```rust
// sync/pairing.rs

use qrcode::QrCode;

/// Pairing information exchanged between devices
#[derive(Serialize, Deserialize)]
pub struct PairingInfo {
    /// Mesh identifier (shared across all user's devices)
    mesh_id: String,
    
    /// Temporary pairing key (for initial handshake)
    temp_key: [u8; 32],
    
    /// Bootstrap peer endpoint
    bootstrap_endpoint: SocketAddr,
    
    /// Expiration time
    expires_at: chrono::DateTime<chrono::Utc>,
}

/// Generate a pairing code for display
pub fn generate_pairing_code(info: &PairingInfo) -> String {
    // Encode to base32 for human readability
    // Format: MYCEL-XXXX-XXXX-XXXX-XXXX
    let encoded = base32::encode(&bincode::serialize(info)?);
    format_pairing_code(&encoded)
}

/// Generate QR code for pairing
pub fn generate_qr_code(info: &PairingInfo) -> Result<String> {
    let json = serde_json::to_string(info)?;
    let code = QrCode::new(json)?;
    Ok(code.render::<char>().build())
}

/// Parse a pairing code
pub fn parse_pairing_code(code: &str) -> Result<PairingInfo> {
    let cleaned = code.replace("-", "").replace(" ", "");
    let bytes = base32::decode(&cleaned)?;
    Ok(bincode::deserialize(&bytes)?)
}

/// Execute pairing handshake
pub async fn execute_pairing(
    local_mesh: &mut MeshNetwork,
    pairing_info: PairingInfo,
) -> Result<()> {
    // 1. Connect to bootstrap peer
    // 2. Exchange public keys
    // 3. Add each other as peers
    // 4. Verify connectivity
    // 5. Start initial sync
}
```

### 3. CRDT Data Structures

```rust
// sync/crdt.rs

use std::collections::{HashMap, HashSet};

/// Hybrid Logical Clock for ordering events
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HLC {
    /// Physical timestamp (milliseconds)
    physical: u64,
    
    /// Logical counter (for same-millisecond ordering)
    logical: u32,
    
    /// Node ID (tiebreaker)
    node_id: String,
}

impl HLC {
    pub fn now(node_id: &str) -> Self {
        Self {
            physical: chrono::Utc::now().timestamp_millis() as u64,
            logical: 0,
            node_id: node_id.to_string(),
        }
    }
    
    pub fn tick(&mut self) {
        let now = chrono::Utc::now().timestamp_millis() as u64;
        if now > self.physical {
            self.physical = now;
            self.logical = 0;
        } else {
            self.logical += 1;
        }
    }
    
    pub fn merge(&mut self, other: &HLC) {
        let now = chrono::Utc::now().timestamp_millis() as u64;
        if now > self.physical && now > other.physical {
            self.physical = now;
            self.logical = 0;
        } else if self.physical > other.physical {
            self.logical += 1;
        } else if other.physical > self.physical {
            self.physical = other.physical;
            self.logical = other.logical + 1;
        } else {
            self.logical = std::cmp::max(self.logical, other.logical) + 1;
        }
    }
}

/// Last-Writer-Wins Register
#[derive(Clone, Serialize, Deserialize)]
pub struct LWWRegister<T> {
    value: T,
    timestamp: HLC,
}

impl<T: Clone> LWWRegister<T> {
    pub fn new(value: T, node_id: &str) -> Self {
        Self {
            value,
            timestamp: HLC::now(node_id),
        }
    }
    
    pub fn set(&mut self, value: T, node_id: &str) {
        self.timestamp = HLC::now(node_id);
        self.value = value;
    }
    
    pub fn get(&self) -> &T {
        &self.value
    }
    
    pub fn merge(&mut self, other: &Self) {
        if other.timestamp > self.timestamp {
            self.value = other.value.clone();
            self.timestamp = other.timestamp.clone();
        }
    }
}

/// Grow-Only Set (elements can only be added)
#[derive(Clone, Serialize, Deserialize)]
pub struct GSet<T: Eq + std::hash::Hash> {
    elements: HashSet<T>,
}

impl<T: Eq + std::hash::Hash + Clone> GSet<T> {
    pub fn new() -> Self {
        Self { elements: HashSet::new() }
    }
    
    pub fn add(&mut self, element: T) {
        self.elements.insert(element);
    }
    
    pub fn contains(&self, element: &T) -> bool {
        self.elements.contains(element)
    }
    
    pub fn merge(&mut self, other: &Self) {
        self.elements.extend(other.elements.iter().cloned());
    }
}

/// Observed-Remove Set (elements can be added and removed)
#[derive(Clone, Serialize, Deserialize)]
pub struct ORSet<T: Eq + std::hash::Hash> {
    /// Element -> set of (node_id, timestamp) pairs that added it
    elements: HashMap<T, HashSet<(String, u64)>>,
    
    /// Element -> set of (node_id, timestamp) pairs that removed it
    tombstones: HashMap<T, HashSet<(String, u64)>>,
}

impl<T: Eq + std::hash::Hash + Clone> ORSet<T> {
    pub fn new() -> Self {
        Self {
            elements: HashMap::new(),
            tombstones: HashMap::new(),
        }
    }
    
    pub fn add(&mut self, element: T, node_id: &str) {
        let tag = (node_id.to_string(), chrono::Utc::now().timestamp_millis() as u64);
        self.elements
            .entry(element)
            .or_insert_with(HashSet::new)
            .insert(tag);
    }
    
    pub fn remove(&mut self, element: &T, node_id: &str) {
        if let Some(tags) = self.elements.get(element) {
            // Move all current tags to tombstones
            let tombstone_tags = self.tombstones
                .entry(element.clone())
                .or_insert_with(HashSet::new);
            tombstone_tags.extend(tags.iter().cloned());
        }
    }
    
    pub fn contains(&self, element: &T) -> bool {
        if let Some(add_tags) = self.elements.get(element) {
            let remove_tags = self.tombstones.get(element);
            // Element exists if any add tag is not in tombstones
            add_tags.iter().any(|tag| {
                remove_tags.map_or(true, |rt| !rt.contains(tag))
            })
        } else {
            false
        }
    }
    
    pub fn merge(&mut self, other: &Self) {
        // Merge elements
        for (elem, tags) in &other.elements {
            self.elements
                .entry(elem.clone())
                .or_insert_with(HashSet::new)
                .extend(tags.iter().cloned());
        }
        
        // Merge tombstones
        for (elem, tags) in &other.tombstones {
            self.tombstones
                .entry(elem.clone())
                .or_insert_with(HashSet::new)
                .extend(tags.iter().cloned());
        }
    }
}
```

### 4. Sync State

```rust
// sync/state.rs

/// What gets synced
#[derive(Clone, Serialize, Deserialize)]
pub struct SyncState {
    /// System configuration (LWW - latest config wins)
    pub config: LWWRegister<MycelConfig>,
    
    /// Installed apps (GSet - apps only added, never auto-removed)
    pub installed_apps: GSet<String>,
    
    /// Learned patterns (ORSet - can be added/removed)
    pub patterns: ORSet<LearnedPattern>,
    
    /// AI preferences (LWW per key)
    pub preferences: HashMap<String, LWWRegister<String>>,
    
    /// Sync metadata
    pub last_sync: HashMap<String, chrono::DateTime<chrono::Utc>>,
}

impl SyncState {
    pub fn merge(&mut self, other: &SyncState) {
        self.config.merge(&other.config);
        self.installed_apps.merge(&other.installed_apps);
        self.patterns.merge(&other.patterns);
        
        for (key, value) in &other.preferences {
            self.preferences
                .entry(key.clone())
                .or_insert_with(|| LWWRegister::new(String::new(), ""))
                .merge(value);
        }
    }
}
```

### 5. Sync Service

```rust
// sync/mod.rs

pub mod mesh;
pub mod pairing;
pub mod crdt;
pub mod state;

use mesh::MeshNetwork;
use state::SyncState;

pub struct SyncService {
    /// This node's identity
    node_id: String,
    
    /// WireGuard mesh
    mesh: MeshNetwork,
    
    /// Current state
    state: SyncState,
    
    /// Configuration
    config: MeshConfig,
    
    /// Event log (Hypercore-style append-only)
    log: Vec<SyncEvent>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SyncEvent {
    timestamp: crdt::HLC,
    node_id: String,
    event_type: SyncEventType,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum SyncEventType {
    ConfigChanged(MycelConfig),
    AppInstalled(String),
    PatternAdded(LearnedPattern),
    PatternRemoved(String),
    PreferenceSet(String, String),
}

impl SyncService {
    pub async fn new(config: &MycelConfig) -> Result<Self> {
        // Load state from disk
        // Initialize mesh network
        // Connect to known peers
    }
    
    pub async fn start(&mut self) -> Result<()> {
        // Start sync loop
        tokio::spawn(self.run_sync_loop());
    }
    
    async fn run_sync_loop(&mut self) {
        loop {
            tokio::select! {
                // Local change detected
                event = self.watch_local_changes() => {
                    self.append_event(event).await;
                    self.broadcast_to_peers().await;
                }
                
                // Peer data received
                data = self.receive_from_peers() => {
                    self.merge_peer_data(data).await;
                }
                
                // Periodic sync
                _ = tokio::time::sleep(Duration::from_secs(30)) => {
                    self.sync_with_all_peers().await;
                }
            }
        }
    }
    
    pub async fn pair_device(&self, code: &str) -> Result<PeerInfo> {
        let info = pairing::parse_pairing_code(code)?;
        pairing::execute_pairing(&mut self.mesh, info).await
    }
    
    pub fn generate_pairing(&self) -> (String, String) {
        let info = PairingInfo { /* ... */ };
        let code = pairing::generate_pairing_code(&info);
        let qr = pairing::generate_qr_code(&info).unwrap_or_default();
        (code, qr)
    }
}
```

---

## Sync Protocol

### Initial Pairing

```
Device A (existing)                    Device B (new)
      │                                      │
      │  1. Generate pairing code            │
      │  Display: MYCEL-XXXX-...             │
      │                                      │
      │         2. User enters code          │
      │◄─────────────────────────────────────│
      │                                      │
      │  3. Exchange public keys             │
      │─────────────────────────────────────►│
      │◄─────────────────────────────────────│
      │                                      │
      │  4. Add as WireGuard peers           │
      │         (both sides)                 │
      │                                      │
      │  5. Initial state sync               │
      │─────────────────────────────────────►│
      │                                      │
      │  6. Confirm pairing complete         │
      │◄─────────────────────────────────────│
```

### Ongoing Sync

```
Device A                                Device B
      │                                      │
      │  1. Local change (e.g., new pattern) │
      │                                      │
      │  2. Append to local log              │
      │                                      │
      │  3. Broadcast change                 │
      │─────────────────────────────────────►│
      │                                      │
      │                    4. Receive change │
      │                    5. Merge (CRDT)   │
      │                    6. Append to log  │
      │                                      │
      │  7. Periodic full sync (every 30s)  │
      │◄────────────────────────────────────►│
      │     Exchange log heads               │
      │     Request missing entries          │
      │     Merge state                      │
```

---

## File Sync (Optional)

For document sync, use content-addressed chunks:

```rust
// sync/files.rs

pub struct FileSync {
    /// Content-addressed store
    store: ContentStore,
    
    /// What to sync
    paths: Vec<SyncPath>,
}

pub struct SyncPath {
    local_path: PathBuf,
    sync_enabled: bool,
    exclude_patterns: Vec<String>,
}

pub struct ContentStore {
    /// Hash -> content
    chunks: HashMap<[u8; 32], Vec<u8>>,
}

impl FileSync {
    /// Sync a file by chunking and storing
    pub async fn sync_file(&mut self, path: &Path) -> Result<FileManifest> {
        let content = tokio::fs::read(path).await?;
        let chunks = chunk_file(&content);
        
        for chunk in &chunks {
            let hash = sha256(&chunk);
            self.store.chunks.insert(hash, chunk.clone());
        }
        
        Ok(FileManifest {
            path: path.to_path_buf(),
            chunks: chunks.iter().map(|c| sha256(c)).collect(),
            size: content.len(),
            modified: fs::metadata(path)?.modified()?,
        })
    }
}
```

---

## Testing Plan

### Unit Tests
```rust
#[test]
fn test_lww_register_merge() {
    let mut a = LWWRegister::new("first", "node_a");
    let b = LWWRegister::new("second", "node_b");
    // Ensure deterministic merge
}

#[test]
fn test_orset_add_remove() {
    let mut set = ORSet::new();
    set.add("item", "node_a");
    assert!(set.contains(&"item"));
    set.remove(&"item", "node_a");
    assert!(!set.contains(&"item"));
}
```

### Integration Tests
```rust
#[tokio::test]
async fn test_two_node_sync() {
    // Spin up two nodes
    // Make change on node A
    // Verify appears on node B
}
```

### Manual Testing
```bash
# Terminal 1: Node A
cargo run -- --dev --node-name node_a

# Terminal 2: Node B
cargo run -- --dev --node-name node_b

# Pair them
# Node A: mycel mesh add-device
# Node B: mycel mesh join [code]

# Test sync
# Node A: mycel "set preference theme dark"
# Node B: mycel "show preferences"
# Should show theme=dark
```

---

## Dependencies to Add

```toml
# Cargo.toml additions for sync

# WireGuard
wireguard-control = "0.1"

# Cryptography
x25519-dalek = "2.0"
blake3 = "1.5"

# QR codes
qrcode = "0.13"

# Serialization
bincode = "1.3"
base32 = "0.4"
```

---

## Security Considerations

1. **Key Storage**: WireGuard private keys must be protected
   - Store in encrypted file
   - Use OS keyring if available

2. **Pairing Codes**: Time-limited (5 minutes default)
   - Include expiration in code
   - One-time use

3. **Network**: All traffic encrypted by WireGuard
   - No plaintext ever

4. **Relay Nodes**: If used, they see only encrypted blobs
   - Cannot read content
   - Cannot modify (authenticated encryption)
