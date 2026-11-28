//! Configuration for Mycel Runtime

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Configuration for a custom or overridden AI inference provider.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CustomProviderConfig {
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub requires_auth: Option<bool>,
}

/// Main configuration struct
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MycelConfig {
    /// URL for local Ollama instance
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,

    /// Local model to use (Ollama model name)
    #[serde(default = "default_local_model")]
    pub local_model: String,

    /// Cloud provider ID (empty = auto-detect from configured keys)
    #[serde(default)]
    pub cloud_provider: String,

    /// Cloud model to use (OpenRouter model name, e.g. "anthropic/claude-3.5-sonnet")
    #[serde(default = "default_cloud_model")]
    pub cloud_model: String,

    /// OpenRouter API key (get one at https://openrouter.ai/keys)
    #[serde(default)]
    pub openrouter_api_key: String,

    /// Custom or overridden AI provider definitions
    #[serde(default)]
    pub providers: HashMap<String, CustomProviderConfig>,

    /// Prefer cloud over local LLM (useful in low-resource environments)
    #[serde(default)]
    pub prefer_cloud: bool,

    /// Path to store context and state
    #[serde(default = "default_context_path")]
    pub context_path: String,

    /// Path to store generated code
    #[serde(default = "default_code_path")]
    pub code_path: String,

    /// IPC socket path
    #[serde(default = "default_ipc_path")]
    pub ipc_socket_path: String,

    /// Maximum tokens for local model
    #[serde(default = "default_max_tokens")]
    pub local_max_tokens: u32,

    /// Force cloud for complex tasks (default: false - local LLM is primary)
    #[serde(default = "default_false")]
    pub force_cloud_for_complex: bool,

    /// Execution timeout in seconds (default: 30)
    #[serde(default = "default_execution_timeout")]
    pub execution_timeout_secs: u64,

    /// Memory limit for code execution in MB (default: 512)
    #[serde(default = "default_execution_memory")]
    pub execution_memory_mb: u32,

    /// Blockchain synchronization settings
    #[serde(default)]
    pub blockchain_sync: bool,

    /// NEAR account for identity and global mesh
    #[serde(default)]
    pub near_account: Option<String>,

    /// Auto-router configuration (local-vs-cloud decision list)
    #[serde(default)]
    pub router: crate::ai::router::RouterConfig,

    /// Self-development loop (auto-filing GitHub issues for limitations)
    #[serde(default)]
    pub selfdev: crate::selfdev::SelfDevConfig,

    /// MCP (Model Context Protocol) configuration
    #[serde(default)]
    pub mcp: McpConfig,

    /// Personal device mesh (sync between the user's own devices)
    #[serde(default)]
    pub mesh: MeshConfig,

    /// Isolation for AI-generated code execution
    #[serde(default)]
    pub sandbox: SandboxConfig,

    /// Execution-recovery policy for failed sandboxed runs
    #[serde(default)]
    pub recovery: crate::recovery::RecoveryConfig,
}

/// How AI-generated code is isolated from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// Run under bubblewrap: no network, read-only filesystem, secrets
    /// masked, empty environment. Refuse to run if bwrap is unavailable.
    #[default]
    Bwrap,
    /// Run directly on the host with the daemon's privileges. Resource
    /// limits still apply. Opt-in only.
    Host,
}

impl SandboxMode {
    /// Stable lowercase name used in events and diagnostics.
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxMode::Bwrap => "bwrap",
            SandboxMode::Host => "host",
        }
    }
}

/// Sandbox configuration for the code executor. Secure by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub mode: SandboxMode,

    /// Path to the bubblewrap binary
    #[serde(default = "default_bwrap_path")]
    pub bwrap_path: String,

    /// Allow network access from sandboxed code (default: no network)
    #[serde(default)]
    pub allow_network: bool,

    /// Directories the sandboxed code may write to (bind-mounted read-write).
    /// Everything else is read-only.
    #[serde(default)]
    pub writable_paths: Vec<String>,

    /// Extra paths hidden from sandboxed code (in addition to the built-in
    /// list of credential locations). `~` expands to the user's home.
    #[serde(default)]
    pub hidden_paths: Vec<String>,

    /// Size of the private /tmp inside the sandbox, in MB
    #[serde(default = "default_sandbox_tmp_mb")]
    pub tmp_size_mb: u32,

    /// Maximum number of tasks the sandboxed code may have running
    /// (`RLIMIT_NPROC`), which stops fork bombs. The limit counts every task
    /// (thread) of the sandbox's real user across the host, so it must sit
    /// above the usual desktop task count; root bypasses it entirely.
    #[serde(default = "default_max_processes")]
    pub max_processes: u32,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Bwrap,
            bwrap_path: default_bwrap_path(),
            allow_network: false,
            writable_paths: Vec::new(),
            hidden_paths: Vec::new(),
            tmp_size_mb: default_sandbox_tmp_mb(),
            max_processes: default_max_processes(),
        }
    }
}

fn default_bwrap_path() -> String {
    "bwrap".to_string()
}

fn default_sandbox_tmp_mb() -> u32 {
    64
}

fn default_max_processes() -> u32 {
    4096
}

/// Transport mode for the personal device mesh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MeshTransportMode {
    #[default]
    Auto,
    WireGuard,
    Direct,
}

/// Personal device mesh configuration.
///
/// Secure by default: the mesh is off, and when on, only explicitly trusted
/// device keys can exchange events with this node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshConfig {
    /// Enable the mesh (UDP listener, discovery, event exchange)
    #[serde(default)]
    pub enabled: bool,

    /// UDP port for mesh traffic (0 = OS-assigned)
    #[serde(default = "default_mesh_port")]
    pub listen_port: u16,

    /// Advertise and discover devices via mDNS on the local network
    #[serde(default = "default_true")]
    pub mdns_enabled: bool,

    /// Mycel IDs (base64 x25519 public keys) of the user's own devices.
    /// Only these peers can send or receive mesh events.
    #[serde(default)]
    pub trusted_peers: Vec<String>,

    /// Replicate conversation history to trusted peers (opt-in)
    #[serde(default)]
    pub sync_ai_context: bool,

    /// Kernel WireGuard interface name
    #[serde(default = "default_mesh_interface")]
    pub interface: String,

    /// Mesh transport mode (auto, wireguard, direct)
    #[serde(default)]
    pub transport: MeshTransportMode,

    /// TCP/UDP port for sync protocol traffic inside the mesh
    #[serde(default = "default_sync_port")]
    pub sync_port: u16,

    /// Optional explicit WireGuard IPv4 address (derived from public key if None)
    #[serde(default)]
    pub wg_address: Option<String>,

    /// Static peer endpoints keyed by Mycel ID (base64 public key) -> "ip:port"
    #[serde(default)]
    pub peer_endpoints: HashMap<String, String>,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_port: default_mesh_port(),
            mdns_enabled: true,
            trusted_peers: Vec::new(),
            sync_ai_context: false,
            interface: default_mesh_interface(),
            transport: MeshTransportMode::Auto,
            sync_port: default_sync_port(),
            wg_address: None,
            peer_endpoints: HashMap::new(),
        }
    }
}

fn default_mesh_port() -> u16 {
    51820
}

fn default_mesh_interface() -> String {
    "mycel0".to_string()
}

fn default_sync_port() -> u16 {
    51821
}

/// MCP (Model Context Protocol) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    /// Enable MCP server integration
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// List of MCP servers to connect to
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            servers: Vec::new(),
        }
    }
}

/// Configuration for a single MCP server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Server name (used for identification)
    pub name: String,

    /// Command to run the server
    pub command: String,

    /// Arguments for the command
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Tools that require user confirmation before execution
    #[serde(default)]
    pub requires_confirmation: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_ollama_url() -> String {
    "http://localhost:11434".to_string()
}

fn default_local_model() -> String {
    // qwen3:8b - best balance for Mycel's core loop: native tool-calling
    // (required by MCP), strong code generation, 40K context, runs on CPU
    // in a pinch. Safety is enforced by the policy evaluator, not the model.
    "qwen3:8b".to_string()
}

fn default_cloud_model() -> String {
    "anthropic/claude-3.5-sonnet".to_string() // OpenRouter model format
}

fn default_context_path() -> String {
    dirs::data_dir()
        .map(|p| p.join("mycel").to_string_lossy().to_string())
        .unwrap_or_else(|| "/var/lib/mycel".to_string())
}

fn default_code_path() -> String {
    dirs::cache_dir()
        .map(|p| p.join("mycel/code").to_string_lossy().to_string())
        .unwrap_or_else(|| "/tmp/mycel/code".to_string())
}

fn default_ipc_path() -> String {
    "/tmp/mycel.sock".to_string()
}

fn default_false() -> bool {
    false
}

fn default_max_tokens() -> u32 {
    2048
}

fn default_execution_timeout() -> u64 {
    30
}

fn default_execution_memory() -> u32 {
    512
}

impl Default for MycelConfig {
    fn default() -> Self {
        Self {
            ollama_url: default_ollama_url(),
            local_model: default_local_model(),
            cloud_provider: String::new(),
            cloud_model: default_cloud_model(),
            openrouter_api_key: String::new(),
            providers: HashMap::new(),
            prefer_cloud: false,
            context_path: default_context_path(),
            code_path: default_code_path(),
            ipc_socket_path: default_ipc_path(),
            local_max_tokens: 2048,
            force_cloud_for_complex: false, // Local LLM is the primary brain
            execution_timeout_secs: default_execution_timeout(),
            execution_memory_mb: default_execution_memory(),
            blockchain_sync: false,
            near_account: None,
            router: crate::ai::router::RouterConfig::default(),
            selfdev: crate::selfdev::SelfDevConfig::default(),
            mcp: McpConfig::default(),
            mesh: MeshConfig::default(),
            sandbox: SandboxConfig::default(),
            recovery: crate::recovery::RecoveryConfig::default(),
        }
    }
}

impl MycelConfig {
    /// Load configuration from file, with environment variable overrides
    pub fn load(path: &str, dev_mode: bool) -> Result<Self> {
        let mut config = if std::path::Path::new(path).exists() {
            let content = std::fs::read_to_string(path)?;
            toml::from_str(&content)?
        } else {
            Self::default()
        };

        // Environment variable overrides
        if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
            config.openrouter_api_key = key;
            // Auto-prefer cloud when OpenRouter key is set
            config.prefer_cloud = true;
        }
        if let Ok(url) = std::env::var("OLLAMA_URL") {
            config.ollama_url = url;
        }
        if let Ok(model) = std::env::var("MYCEL_LOCAL_MODEL") {
            config.local_model = model;
        }
        if let Ok(provider) = std::env::var("MYCEL_CLOUD_PROVIDER") {
            config.cloud_provider = provider;
        }
        if let Ok(model) = std::env::var("MYCEL_CLOUD_MODEL") {
            config.cloud_model = model;
        }
        if std::env::var("MYCEL_PREFER_CLOUD").is_ok() {
            config.prefer_cloud = true;
        }

        // Dev mode adjustments
        if dev_mode {
            config.context_path = "./mycel-data".to_string();
            config.code_path = "./mycel-code".to_string();
            config.ipc_socket_path = "/tmp/mycel-dev.sock".to_string();
            config.mesh.transport = MeshTransportMode::Direct;
        }

        Ok(config)
    }

    /// Save configuration to file
    pub fn save(&self, path: &str) -> Result<()> {
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = MycelConfig::default();
        assert_eq!(config.local_model, "qwen3:8b");
        assert_eq!(config.ollama_url, "http://localhost:11434");
        assert!(!config.force_cloud_for_complex);
        assert!(!config.mesh.enabled);
        assert_eq!(config.mesh.interface, "mycel0");
        assert_eq!(config.mesh.transport, MeshTransportMode::Auto);
        assert_eq!(config.mesh.sync_port, 51821);
        assert_eq!(config.mesh.wg_address, None);
        assert!(config.mesh.peer_endpoints.is_empty());
    }

    #[test]
    fn test_dev_mode_adjustments() {
        // We can't easily test file loading without creating a file,
        // but we can test the load function with a non-existent file
        let config = MycelConfig::load("non_existent_config.toml", true).unwrap();
        assert_eq!(config.context_path, "./mycel-data");
        assert_eq!(config.ipc_socket_path, "/tmp/mycel-dev.sock");
        assert_eq!(config.mesh.transport, MeshTransportMode::Direct);
    }

    #[test]
    fn test_mesh_config_backward_compatible_toml() {
        let legacy_toml = r#"
[mesh]
enabled = true
listen_port = 55555
mdns_enabled = false
trusted_peers = ["peerA"]
sync_ai_context = true
"#;
        let parsed: MycelConfig = toml::from_str(legacy_toml).unwrap();
        assert!(parsed.mesh.enabled);
        assert_eq!(parsed.mesh.listen_port, 55555);
        assert!(!parsed.mesh.mdns_enabled);
        assert_eq!(parsed.mesh.trusted_peers, vec!["peerA"]);
        assert!(parsed.mesh.sync_ai_context);
        assert_eq!(parsed.mesh.interface, "mycel0");
        assert_eq!(parsed.mesh.transport, MeshTransportMode::Auto);
        assert_eq!(parsed.mesh.sync_port, 51821);
        assert_eq!(parsed.mesh.wg_address, None);
        assert!(parsed.mesh.peer_endpoints.is_empty());
    }

    #[test]
    fn test_mesh_config_full_toml_and_dev_override() {
        let full_toml = r#"
[mesh]
enabled = true
interface = "wg-mycel"
transport = "wireguard"
sync_port = 52000
wg_address = "10.100.1.2"

[mesh.peer_endpoints]
"peerA" = "203.0.113.5:51820"
"#;
        let parsed: MycelConfig = toml::from_str(full_toml).unwrap();
        assert_eq!(parsed.mesh.interface, "wg-mycel");
        assert_eq!(parsed.mesh.transport, MeshTransportMode::WireGuard);
        assert_eq!(parsed.mesh.sync_port, 52000);
        assert_eq!(parsed.mesh.wg_address.as_deref(), Some("10.100.1.2"));
        assert_eq!(
            parsed.mesh.peer_endpoints.get("peerA").map(String::as_str),
            Some("203.0.113.5:51820")
        );
    }

    #[test]
    fn test_providers_config_backward_compatible_and_custom() {
        let legacy: MycelConfig = toml::from_str("").unwrap();
        assert_eq!(legacy.cloud_provider, "");
        assert!(legacy.providers.is_empty());

        let custom_toml = r#"
cloud_provider = "venice"
cloud_model = "llama-3.3-70b"

[providers.my-vllm]
api = "openai-completions"
base_url = "http://localhost:8000/v1"
default_model = "Qwen/Qwen2.5-72B-Instruct"
requires_auth = false

[providers.my-vllm.headers]
X-Custom = "val"
"#;
        let parsed: MycelConfig = toml::from_str(custom_toml).unwrap();
        assert_eq!(parsed.cloud_provider, "venice");
        assert_eq!(parsed.cloud_model, "llama-3.3-70b");
        let p = parsed.providers.get("my-vllm").unwrap();
        assert_eq!(p.api.as_deref(), Some("openai-completions"));
        assert_eq!(p.base_url.as_deref(), Some("http://localhost:8000/v1"));
        assert_eq!(
            p.default_model.as_deref(),
            Some("Qwen/Qwen2.5-72B-Instruct")
        );
        assert_eq!(p.requires_auth, Some(false));
        assert_eq!(p.headers.get("X-Custom").map(String::as_str), Some("val"));
    }
}
