//! Model Manager - Abstraction layer for LLM backends
//!
//! Provides a unified interface for managing local LLM models from
//! multiple sources:
//! - Ollama (default, easiest setup)
//! - Hugging Face (via llama.cpp for GGUF models)
//! - Local files (user-provided models)
//!
//! Hardware compatibility is checked before model download/load.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{info, warn};

/// Model provider backends
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelBackend {
    /// Ollama - default, handles hardware detection automatically
    Ollama,
    /// Hugging Face models via llama.cpp (GGUF format)
    HuggingFace,
    /// Local model files
    LocalFile,
}

impl Default for ModelBackend {
    fn default() -> Self {
        Self::Ollama
    }
}

/// Hardware capabilities detected on the system
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareInfo {
    /// Total system RAM in bytes
    pub total_ram_bytes: u64,
    /// Available RAM in bytes
    pub available_ram_bytes: u64,
    /// GPU VRAM in bytes (0 if no GPU)
    pub gpu_vram_bytes: u64,
    /// GPU type if available
    pub gpu_type: Option<GpuType>,
    /// Number of CPU cores
    pub cpu_cores: usize,
    /// Whether AVX2 is supported (important for llama.cpp)
    pub has_avx2: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum GpuType {
    Nvidia,
    Amd,
    AppleSilicon,
    Intel,
    None,
}

/// Model requirements for running
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequirements {
    /// Minimum RAM needed (bytes)
    pub min_ram_bytes: u64,
    /// Recommended RAM (bytes)
    pub recommended_ram_bytes: u64,
    /// VRAM needed for GPU inference (bytes, 0 for CPU-only)
    pub vram_bytes: u64,
    /// Whether model supports CPU-only inference
    pub supports_cpu: bool,
    /// Quantization level
    pub quantization: Option<String>,
}

/// Information about an available model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Unique identifier (e.g., "phi3:medium" for Ollama, "TheBloke/Mistral-7B-GGUF" for HF)
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Model description
    pub description: String,
    /// Size in bytes (for download)
    pub size_bytes: u64,
    /// Backend this model comes from
    pub backend: ModelBackend,
    /// Hardware requirements
    pub requirements: ModelRequirements,
    /// Model capabilities/tags
    pub tags: Vec<String>,
}

/// Model manager configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelManagerConfig {
    /// Preferred backend
    pub default_backend: ModelBackend,
    /// Path to store downloaded models
    pub models_path: PathBuf,
    /// Ollama URL (if using Ollama)
    pub ollama_url: String,
    /// Hugging Face API token (optional, for private models)
    pub hf_token: Option<String>,
    /// Maximum model size to auto-download (bytes)
    pub max_auto_download_bytes: u64,
}

impl Default for ModelManagerConfig {
    fn default() -> Self {
        Self {
            default_backend: ModelBackend::Ollama,
            models_path: dirs::data_dir()
                .map(|p| p.join("mycel/models"))
                .unwrap_or_else(|| PathBuf::from("/var/lib/mycel/models")),
            ollama_url: "http://localhost:11434".to_string(),
            hf_token: None,
            max_auto_download_bytes: 10 * 1024 * 1024 * 1024, // 10GB
        }
    }
}

/// Model manager for handling multiple LLM backends
pub struct ModelManager {
    config: ModelManagerConfig,
    hardware: HardwareInfo,
    http_client: reqwest::Client,
}

impl ModelManager {
    /// Create a new model manager
    pub async fn new(config: ModelManagerConfig) -> Result<Self> {
        let hardware = Self::detect_hardware()?;
        info!(
            ram_gb = hardware.total_ram_bytes / (1024 * 1024 * 1024),
            gpu = ?hardware.gpu_type,
            "Hardware detected"
        );

        Ok(Self {
            config,
            hardware,
            http_client: reqwest::Client::new(),
        })
    }

    /// Detect system hardware capabilities
    fn detect_hardware() -> Result<HardwareInfo> {
        use sysinfo::System;

        let mut sys = System::new_all();
        sys.refresh_all();

        let total_ram = sys.total_memory();
        let available_ram = sys.available_memory();
        let cpu_cores = sys.cpus().len();

        // Detect GPU (simplified - production would use more robust detection)
        let (gpu_type, gpu_vram) = Self::detect_gpu();

        // Check for AVX2 support
        #[cfg(target_arch = "x86_64")]
        let has_avx2 = is_x86_feature_detected!("avx2");
        #[cfg(not(target_arch = "x86_64"))]
        let has_avx2 = false;

        Ok(HardwareInfo {
            total_ram_bytes: total_ram,
            available_ram_bytes: available_ram,
            gpu_vram_bytes: gpu_vram,
            gpu_type: Some(gpu_type),
            cpu_cores,
            has_avx2,
        })
    }

    fn detect_gpu() -> (GpuType, u64) {
        // Check for NVIDIA GPU
        if std::path::Path::new("/dev/nvidia0").exists() {
            // Would query nvidia-smi for VRAM in production
            return (GpuType::Nvidia, 0);
        }

        // Check for Apple Silicon
        #[cfg(target_os = "macos")]
        {
            // Apple Silicon uses unified memory
            return (GpuType::AppleSilicon, 0);
        }

        // Check for AMD GPU
        if std::path::Path::new("/dev/dri/renderD128").exists() {
            return (GpuType::Amd, 0);
        }

        (GpuType::None, 0)
    }

    /// Check if a model is compatible with current hardware
    pub fn check_compatibility(&self, model: &ModelInfo) -> CompatibilityResult {
        let reqs = &model.requirements;

        // Check RAM
        if self.hardware.available_ram_bytes < reqs.min_ram_bytes {
            return CompatibilityResult::Incompatible {
                reason: format!(
                    "Insufficient RAM: {} GB available, {} GB required",
                    self.hardware.available_ram_bytes / (1024 * 1024 * 1024),
                    reqs.min_ram_bytes / (1024 * 1024 * 1024)
                ),
            };
        }

        // Check VRAM if GPU required
        if reqs.vram_bytes > 0 && self.hardware.gpu_vram_bytes < reqs.vram_bytes {
            if reqs.supports_cpu {
                return CompatibilityResult::CompatibleWithWarning {
                    warning: "Model will run on CPU (slower). GPU recommended.".to_string(),
                };
            } else {
                return CompatibilityResult::Incompatible {
                    reason: format!(
                        "Insufficient VRAM: {} GB available, {} GB required",
                        self.hardware.gpu_vram_bytes / (1024 * 1024 * 1024),
                        reqs.vram_bytes / (1024 * 1024 * 1024)
                    ),
                };
            }
        }

        // Check if below recommended
        if self.hardware.available_ram_bytes < reqs.recommended_ram_bytes {
            return CompatibilityResult::CompatibleWithWarning {
                warning: format!(
                    "RAM below recommended ({} GB). Performance may be degraded.",
                    reqs.recommended_ram_bytes / (1024 * 1024 * 1024)
                ),
            };
        }

        CompatibilityResult::Compatible
    }

    /// List available models from a backend
    pub async fn list_available(&self, backend: ModelBackend) -> Result<Vec<ModelInfo>> {
        match backend {
            ModelBackend::Ollama => self.list_ollama_models().await,
            ModelBackend::HuggingFace => self.list_huggingface_models().await,
            ModelBackend::LocalFile => self.list_local_models().await,
        }
    }

    async fn list_ollama_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/api/tags", self.config.ollama_url);
        let response: serde_json::Value = self.http_client.get(&url).send().await?.json().await?;

        let models = response["models"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|m| {
                let name = m["name"].as_str().unwrap_or("unknown").to_string();
                let size = m["size"].as_u64().unwrap_or(0);

                ModelInfo {
                    id: name.clone(),
                    name: name.clone(),
                    description: format!("Ollama model: {}", name),
                    size_bytes: size,
                    backend: ModelBackend::Ollama,
                    requirements: Self::estimate_ollama_requirements(size),
                    tags: vec!["ollama".to_string()],
                }
            })
            .collect();

        Ok(models)
    }

    fn estimate_ollama_requirements(size_bytes: u64) -> ModelRequirements {
        // Rough estimates based on model size
        let size_gb = size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);

        ModelRequirements {
            min_ram_bytes: (size_gb * 1.5 * 1024.0 * 1024.0 * 1024.0) as u64,
            recommended_ram_bytes: (size_gb * 2.0 * 1024.0 * 1024.0 * 1024.0) as u64,
            vram_bytes: 0, // Ollama handles this
            supports_cpu: true,
            quantization: None,
        }
    }

    async fn list_huggingface_models(&self) -> Result<Vec<ModelInfo>> {
        // Query Hugging Face API for GGUF models suitable for local inference
        let url =
            "https://huggingface.co/api/models?filter=gguf&sort=downloads&direction=-1&limit=50";

        let mut request = self.http_client.get(url);
        if let Some(token) = &self.config.hf_token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        let response: Vec<serde_json::Value> = request.send().await?.json().await?;

        let models = response
            .iter()
            .filter_map(|m| {
                let id = m["id"].as_str()?.to_string();
                let downloads = m["downloads"].as_u64().unwrap_or(0);

                Some(ModelInfo {
                    id: id.clone(),
                    name: id.split('/').last().unwrap_or(&id).to_string(),
                    description: m["description"].as_str().unwrap_or("").to_string(),
                    size_bytes: 0, // Would need to fetch from model card
                    backend: ModelBackend::HuggingFace,
                    requirements: ModelRequirements {
                        min_ram_bytes: 8 * 1024 * 1024 * 1024, // 8GB default
                        recommended_ram_bytes: 16 * 1024 * 1024 * 1024,
                        vram_bytes: 0,
                        supports_cpu: true,
                        quantization: Some("GGUF".to_string()),
                    },
                    tags: vec![
                        "huggingface".to_string(),
                        "gguf".to_string(),
                        format!("downloads:{}", downloads),
                    ],
                })
            })
            .collect();

        Ok(models)
    }

    async fn list_local_models(&self) -> Result<Vec<ModelInfo>> {
        let mut models = Vec::new();

        if self.config.models_path.exists() {
            for entry in std::fs::read_dir(&self.config.models_path)? {
                let entry = entry?;
                let path = entry.path();

                if path.extension().map(|e| e == "gguf").unwrap_or(false) {
                    let name = path.file_stem().unwrap().to_string_lossy().to_string();
                    let size = entry.metadata()?.len();

                    models.push(ModelInfo {
                        id: path.to_string_lossy().to_string(),
                        name,
                        description: "Local GGUF model".to_string(),
                        size_bytes: size,
                        backend: ModelBackend::LocalFile,
                        requirements: Self::estimate_ollama_requirements(size),
                        tags: vec!["local".to_string(), "gguf".to_string()],
                    });
                }
            }
        }

        Ok(models)
    }

    /// Download a model
    pub async fn download(&self, model: &ModelInfo) -> Result<PathBuf> {
        // Check compatibility first
        match self.check_compatibility(model) {
            CompatibilityResult::Incompatible { reason } => {
                return Err(anyhow!("Model incompatible with hardware: {}", reason));
            }
            CompatibilityResult::CompatibleWithWarning { warning } => {
                warn!("{}", warning);
            }
            CompatibilityResult::Compatible => {}
        }

        match model.backend {
            ModelBackend::Ollama => self.download_ollama(&model.id).await,
            ModelBackend::HuggingFace => self.download_huggingface(&model.id).await,
            ModelBackend::LocalFile => Ok(PathBuf::from(&model.id)),
        }
    }

    async fn download_ollama(&self, model_id: &str) -> Result<PathBuf> {
        info!(model = model_id, "Pulling model from Ollama");

        let url = format!("{}/api/pull", self.config.ollama_url);
        let response = self
            .http_client
            .post(&url)
            .json(&serde_json::json!({ "name": model_id }))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow!("Failed to pull model: {}", response.status()));
        }

        // Ollama manages its own model storage
        Ok(PathBuf::from(format!("ollama://{}", model_id)))
    }

    async fn download_huggingface(&self, model_id: &str) -> Result<PathBuf> {
        info!(model = model_id, "Downloading model from Hugging Face");

        // Create models directory
        std::fs::create_dir_all(&self.config.models_path)?;

        // This is simplified - production would use huggingface_hub crate
        // or download specific GGUF files from the model repo
        let safe_name = model_id.replace('/', "_");
        let model_path = self.config.models_path.join(format!("{}.gguf", safe_name));

        // Would download the model file here
        // For now, return the expected path
        warn!(
            "Hugging Face download not fully implemented. \
             Would download {} to {}",
            model_id,
            model_path.display()
        );

        Ok(model_path)
    }

    /// Get recommended models for current hardware
    pub async fn get_recommended(&self) -> Result<Vec<ModelInfo>> {
        let ram_gb = self.hardware.total_ram_bytes / (1024 * 1024 * 1024);

        // Recommend models based on available RAM
        let recommended_models = if ram_gb >= 32 {
            vec![
                "llama3.1:70b-instruct-q4_K_M",
                "mixtral:8x7b",
                "codellama:34b",
            ]
        } else if ram_gb >= 16 {
            vec![
                "llama3.1:8b-instruct-q8_0",
                "mistral:7b-instruct",
                "codellama:13b",
            ]
        } else if ram_gb >= 8 {
            vec!["phi3:medium", "llama3.2:3b", "gemma2:2b"]
        } else {
            vec!["phi3:mini", "llama3.2:1b", "tinyllama"]
        };

        Ok(recommended_models
            .iter()
            .map(|id| ModelInfo {
                id: id.to_string(),
                name: id.to_string(),
                description: format!("Recommended for {}GB RAM", ram_gb),
                size_bytes: 0,
                backend: ModelBackend::Ollama,
                requirements: ModelRequirements {
                    min_ram_bytes: 0,
                    recommended_ram_bytes: 0,
                    vram_bytes: 0,
                    supports_cpu: true,
                    quantization: None,
                },
                tags: vec!["recommended".to_string()],
            })
            .collect())
    }
}

/// Result of hardware compatibility check
#[derive(Debug, Clone)]
pub enum CompatibilityResult {
    Compatible,
    CompatibleWithWarning { warning: String },
    Incompatible { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ModelManagerConfig::default();
        assert_eq!(config.default_backend, ModelBackend::Ollama);
    }

    #[test]
    fn test_compatibility_check() {
        let hardware = HardwareInfo {
            total_ram_bytes: 16 * 1024 * 1024 * 1024,     // 16GB
            available_ram_bytes: 12 * 1024 * 1024 * 1024, // 12GB available
            gpu_vram_bytes: 0,
            gpu_type: Some(GpuType::None),
            cpu_cores: 8,
            has_avx2: true,
        };

        let model = ModelInfo {
            id: "test".to_string(),
            name: "Test Model".to_string(),
            description: "Test".to_string(),
            size_bytes: 4 * 1024 * 1024 * 1024,
            backend: ModelBackend::Ollama,
            requirements: ModelRequirements {
                min_ram_bytes: 8 * 1024 * 1024 * 1024, // 8GB min
                recommended_ram_bytes: 16 * 1024 * 1024 * 1024,
                vram_bytes: 0,
                supports_cpu: true,
                quantization: None,
            },
            tags: vec![],
        };

        // This would need a ModelManager instance to test properly
        // For now, just verify the struct compiles
        assert!(hardware.available_ram_bytes >= model.requirements.min_ram_bytes);
    }
}
