//! Configuration for Mycel Runtime

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Main configuration struct
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MycelConfig {
    /// URL for local Ollama instance
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,

    /// Local model to use (Ollama model name)
    #[serde(default = "default_local_model")]
    pub local_model: String,

    /// Cloud model to use (Anthropic model name)
    #[serde(default = "default_cloud_model")]
    pub cloud_model: String,

    /// Anthropic API key
    #[serde(default)]
    pub anthropic_api_key: String,

    /// Path to store context and state
    #[serde(default = "default_context_path")]
    pub context_path: String,

    /// Path to store generated code
    #[serde(default = "default_code_path")]
    pub code_path: String,

    /// IPC socket path
    #[serde(default = "default_ipc_path")]
    pub ipc_socket_path: String,

    /// Enable sandbox for code execution
    #[serde(default = "default_true")]
    pub sandbox_enabled: bool,

    /// Maximum tokens for local model
    #[serde(default = "default_max_tokens")]
    pub local_max_tokens: u32,

    /// Prefer cloud for complex tasks
    #[serde(default = "default_true")]
    pub prefer_cloud_for_complex: bool,
}

fn default_ollama_url() -> String {
    "http://localhost:11434".to_string()
}

fn default_local_model() -> String {
    "phi3:medium".to_string()
}

fn default_cloud_model() -> String {
    "claude-sonnet-4-20250514".to_string()
}

fn default_context_path() -> String {
    dirs::data_dir()
        .map(|p| p.join("mycel").to_string_lossy().to_string())
        .unwrap_or_else(|| "/var/lib/mycel".to_string())
}

fn default_code_path() -> String {
    dirs::cache_dir()
        .map(|p| p.join("clay/code").to_string_lossy().to_string())
        .unwrap_or_else(|| "/tmp/mycel/code".to_string())
}

fn default_ipc_path() -> String {
    "/tmp/mycel.sock".to_string()
}

fn default_true() -> bool {
    true
}

fn default_max_tokens() -> u32 {
    2048
}

impl Default for MycelConfig {
    fn default() -> Self {
        Self {
            ollama_url: default_ollama_url(),
            local_model: default_local_model(),
            cloud_model: default_cloud_model(),
            anthropic_api_key: String::new(),
            context_path: default_context_path(),
            code_path: default_code_path(),
            ipc_socket_path: default_ipc_path(),
            sandbox_enabled: true,
            local_max_tokens: 2048,
            prefer_cloud_for_complex: true,
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
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            config.anthropic_api_key = key;
        }
        if let Ok(url) = std::env::var("OLLAMA_URL") {
            config.ollama_url = url;
        }
        if let Ok(model) = std::env::var("MYCEL_LOCAL_MODEL") {
            config.local_model = model;
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
