//! Configuration for Mycel Runtime

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Main configuration struct
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MycelConfig {
    /// URL for local Ollama instance
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,

    /// Local model to use (Ollama model name)
    #[serde(default = "default_local_model")]
    pub local_model: String,

    /// Cloud model to use (OpenRouter model name, e.g. "anthropic/claude-3.5-sonnet")
    #[serde(default = "default_cloud_model")]
    pub cloud_model: String,

    /// OpenRouter API key (get one at https://openrouter.ai/keys)
    #[serde(default)]
    pub openrouter_api_key: String,

    /// Anthropic API key (direct Claude access - faster than OpenRouter)
    #[serde(default)]
    pub anthropic_api_key: String,

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

    /// MCP (Model Context Protocol) configuration
    #[serde(default)]
    pub mcp: McpConfig,
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
    "tinydolphin".to_string() // Uncensored + fast (636MB) - won't refuse commands
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
            cloud_model: default_cloud_model(),
            openrouter_api_key: String::new(),
            anthropic_api_key: String::new(),
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
            mcp: McpConfig::default(),
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
        }
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            config.anthropic_api_key = key;
            // Auto-prefer cloud when Anthropic key is set
            config.prefer_cloud = true;
        }
        if let Ok(url) = std::env::var("OLLAMA_URL") {
            config.ollama_url = url;
        }
        if let Ok(model) = std::env::var("MYCEL_LOCAL_MODEL") {
            config.local_model = model;
        }
        if std::env::var("MYCEL_PREFER_CLOUD").is_ok() {
            config.prefer_cloud = true;
        }

        // Dev mode adjustments
        if dev_mode {
            config.context_path = "./mycel-data".to_string();
            config.code_path = "./mycel-code".to_string();
            config.ipc_socket_path = "/tmp/mycel-dev.sock".to_string();
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
        assert_eq!(config.local_model, "tinydolphin");
        assert_eq!(config.ollama_url, "http://localhost:11434");
        assert!(!config.force_cloud_for_complex);
    }

    #[test]
    fn test_dev_mode_adjustments() {
        // We can't easily test file loading without creating a file,
        // but we can test the load function with a non-existent file
        let config = MycelConfig::load("non_existent_config.toml", true).unwrap();
        assert_eq!(config.context_path, "./mycel-data");
        assert_eq!(config.ipc_socket_path, "/tmp/mycel-dev.sock");
    }
}
