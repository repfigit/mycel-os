//! Multi-provider cloud AI inference registry, wire protocols, and AWS SigV4 signing.
//!
//! Supports 45 built-in providers plus user-defined `[providers.<name>]` entries
//! across 5 wire protocols:
//! - `openai-completions`
//! - `openai-responses`
//! - `anthropic-messages`
//! - `google-generative-ai`
//! - `bedrock-converse`

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use futures::Stream;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use tracing::debug;

use crate::config::{CustomProviderConfig, MycelConfig};

const DEFAULT_OPENROUTER_MODEL: &str = "anthropic/claude-3.5-sonnet";
const DEFAULT_AZURE_API_VERSION: &str = "2024-10-21";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Wire protocol spoken by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderApi {
    OpenAiCompletions,
    OpenAiResponses,
    AnthropicMessages,
    GoogleGenerativeAi,
    BedrockConverse,
}

impl ProviderApi {
    /// Canonical kebab-case identifier.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderApi::OpenAiCompletions => "openai-completions",
            ProviderApi::OpenAiResponses => "openai-responses",
            ProviderApi::AnthropicMessages => "anthropic-messages",
            ProviderApi::GoogleGenerativeAi => "google-generative-ai",
            ProviderApi::BedrockConverse => "bedrock-converse",
        }
    }

    /// Parse a wire protocol name from config or user input.
    pub fn from_str_loose(s: &str) -> Option<Self> {
        let normalized = s.trim().to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "openai-completions" | "openai-chat" | "openai" => Some(ProviderApi::OpenAiCompletions),
            "openai-responses" | "responses" => Some(ProviderApi::OpenAiResponses),
            "anthropic-messages" | "anthropic" => Some(ProviderApi::AnthropicMessages),
            "google-generative-ai" | "google" | "gemini" => Some(ProviderApi::GoogleGenerativeAi),
            "bedrock-converse" | "bedrock" | "amazon-bedrock" => Some(ProviderApi::BedrockConverse),
            _ => None,
        }
    }
}

/// Public, secret-free summary of a configured or available provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSummary {
    pub id: String,
    pub name: String,
    pub api: String,
    pub configured: bool,
    pub env_var: String,
    pub default_model: String,
    pub base_url: String,
}

/// Result of a non-streaming generation call.
#[derive(Debug, Clone)]
pub struct GenerationOutput {
    pub text: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// Static definition of a built-in provider.
struct BuiltinProviderSpec {
    id: &'static str,
    name: &'static str,
    api: ProviderApi,
    base_url: &'static str,
    env_var: &'static str,
    default_model: &'static str,
    #[allow(dead_code)]
    requires_auth: bool,
    extra_headers: &'static [(&'static str, &'static str)],
}

const OPENROUTER_HEADERS: &[(&str, &str)] = &[
    ("HTTP-Referer", "https://mycel.os"),
    ("X-Title", "Mycel OS"),
];

const BUILTIN_PROVIDERS: &[BuiltinProviderSpec] = &[
    BuiltinProviderSpec {
        id: "venice",
        name: "Venice AI",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.venice.ai/api/v1",
        env_var: "VENICE_API_KEY",
        default_model: "llama-3.3-70b",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "openrouter",
        name: "OpenRouter",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://openrouter.ai/api/v1",
        env_var: "OPENROUTER_API_KEY",
        default_model: DEFAULT_OPENROUTER_MODEL,
        requires_auth: true,
        extra_headers: OPENROUTER_HEADERS,
    },
    BuiltinProviderSpec {
        id: "anthropic",
        name: "Anthropic",
        api: ProviderApi::AnthropicMessages,
        base_url: "https://api.anthropic.com",
        env_var: "ANTHROPIC_API_KEY",
        default_model: "claude-sonnet-4-20250514",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "openai",
        name: "OpenAI",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.openai.com/v1",
        env_var: "OPENAI_API_KEY",
        default_model: "gpt-4o-mini",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "openai-codex",
        name: "OpenAI Codex",
        api: ProviderApi::OpenAiResponses,
        base_url: "https://chatgpt.com/backend-api/codex",
        env_var: "OPENAI_API_KEY",
        default_model: "gpt-5.1-codex",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "azure",
        name: "Azure OpenAI",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://{AZURE_OPENAI_RESOURCE_NAME}.openai.azure.com/openai",
        env_var: "AZURE_OPENAI_API_KEY",
        default_model: "gpt-4o",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "google",
        name: "Google Gemini",
        api: ProviderApi::GoogleGenerativeAi,
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        env_var: "GEMINI_API_KEY",
        default_model: "gemini-2.5-flash",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "google-vertex",
        name: "Google Vertex AI",
        api: ProviderApi::GoogleGenerativeAi,
        base_url: "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google",
        env_var: "GOOGLE_CLOUD_API_KEY",
        default_model: "gemini-2.5-flash",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "amazon-bedrock",
        name: "Amazon Bedrock",
        api: ProviderApi::BedrockConverse,
        base_url: "https://bedrock-runtime.{region}.amazonaws.com",
        env_var: "AWS_BEARER_TOKEN_BEDROCK",
        default_model: "anthropic.claude-3-5-sonnet-20241022-v2:0",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "deepseek",
        name: "DeepSeek",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.deepseek.com/v1",
        env_var: "DEEPSEEK_API_KEY",
        default_model: "deepseek-chat",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "groq",
        name: "Groq",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.groq.com/openai/v1",
        env_var: "GROQ_API_KEY",
        default_model: "llama-3.3-70b-versatile",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "cerebras",
        name: "Cerebras",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.cerebras.ai/v1",
        env_var: "CEREBRAS_API_KEY",
        default_model: "llama-3.3-70b",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "xai",
        name: "xAI",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.x.ai/v1",
        env_var: "XAI_API_KEY",
        default_model: "grok-3-mini",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "mistral",
        name: "Mistral AI",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.mistral.ai/v1",
        env_var: "MISTRAL_API_KEY",
        default_model: "mistral-large-latest",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "fireworks",
        name: "Fireworks AI",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.fireworks.ai/inference/v1",
        env_var: "FIREWORKS_API_KEY",
        default_model: "accounts/fireworks/models/llama-v3p3-70b-instruct",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "together",
        name: "Together AI",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.together.xyz/v1",
        env_var: "TOGETHER_API_KEY",
        default_model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "nvidia",
        name: "NVIDIA NIM",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://integrate.api.nvidia.com/v1",
        env_var: "NVIDIA_API_KEY",
        default_model: "meta/llama-3.3-70b-instruct",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "huggingface",
        name: "Hugging Face",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://router.huggingface.co/v1",
        env_var: "HF_TOKEN",
        default_model: "meta-llama/Llama-3.3-70B-Instruct",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "baseten",
        name: "Baseten",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://inference.baseten.co/v1",
        env_var: "BASETEN_API_KEY",
        default_model: "deepseek-ai/DeepSeek-V3",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "github-copilot",
        name: "GitHub Copilot",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.githubcopilot.com",
        env_var: "GITHUB_TOKEN",
        default_model: "gpt-4o",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "vercel-ai-gateway",
        name: "Vercel AI Gateway",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://ai-gateway.vercel.sh/v1",
        env_var: "AI_GATEWAY_API_KEY",
        default_model: "anthropic/claude-sonnet-4",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "cloudflare-workers-ai",
        name: "Cloudflare Workers AI",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1",
        env_var: "CLOUDFLARE_API_TOKEN",
        default_model: "@cf/meta/llama-3.3-70b-instruct-fp8-fast",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "cloudflare-ai-gateway",
        name: "Cloudflare AI Gateway",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat",
        env_var: "CLOUDFLARE_API_TOKEN",
        default_model: "openai/gpt-4o-mini",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "minimax",
        name: "MiniMax",
        api: ProviderApi::AnthropicMessages,
        base_url: "https://api.minimax.io/anthropic",
        env_var: "MINIMAX_API_KEY",
        default_model: "MiniMax-M2.5",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "minimax-cn",
        name: "MiniMax (CN)",
        api: ProviderApi::AnthropicMessages,
        base_url: "https://api.minimaxi.com/anthropic",
        env_var: "MINIMAX_CN_API_KEY",
        default_model: "MiniMax-M2.5",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "moonshotai",
        name: "Moonshot AI",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.moonshot.ai/v1",
        env_var: "MOONSHOT_API_KEY",
        default_model: "kimi-k2.5",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "moonshotai-cn",
        name: "Moonshot AI (CN)",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.moonshot.cn/v1",
        env_var: "MOONSHOT_API_KEY",
        default_model: "kimi-k2.5",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "kimi-coding",
        name: "Kimi Coding",
        api: ProviderApi::AnthropicMessages,
        base_url: "https://api.kimi.com/coding",
        env_var: "KIMI_API_KEY",
        default_model: "k2p5",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "ant-ling",
        name: "Ant Ling",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.ling.antgroup.com/v1",
        env_var: "ANT_LING_API_KEY",
        default_model: "ling-1t",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "meta",
        name: "Meta Llama API",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.llama.com/compat/v1",
        env_var: "LLAMA_API_KEY",
        default_model: "Llama-4-Maverick-17B-128E-Instruct-FP8",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "zai",
        name: "Z.ai",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.z.ai/api/paas/v4",
        env_var: "ZAI_API_KEY",
        default_model: "glm-4.5",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "zai-coding-cn",
        name: "Z.ai Coding (CN)",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
        env_var: "ZAI_API_KEY",
        default_model: "glm-4.5",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "opencode",
        name: "OpenCode Zen",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://opencode.ai/zen/v1",
        env_var: "OPENCODE_API_KEY",
        default_model: "claude-sonnet-4",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "opencode-go",
        name: "OpenCode Go",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://opencode.ai/zen/go/v1",
        env_var: "OPENCODE_API_KEY",
        default_model: "glm-5",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "radius",
        name: "Radius",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.radiustech.xyz/v1",
        env_var: "RADIUS_API_KEY",
        default_model: "radius-default",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "typesafe",
        name: "TypeSafe",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.typesafe.ai/v1",
        env_var: "TYPESAFE_API_KEY",
        default_model: "typesafe-default",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "qwen-token-plan",
        name: "Qwen Token Plan",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        env_var: "DASHSCOPE_API_KEY",
        default_model: "qwen-plus",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "qwen-token-plan-cn",
        name: "Qwen Token Plan (CN)",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        env_var: "DASHSCOPE_API_KEY",
        default_model: "qwen-plus",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "qwen-token-plan-individual",
        name: "Qwen Token Plan Individual",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        env_var: "DASHSCOPE_API_KEY",
        default_model: "qwen-plus",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "qwen-token-plan-individual-cn",
        name: "Qwen Token Plan Individual (CN)",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        env_var: "DASHSCOPE_API_KEY",
        default_model: "qwen-plus",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "xiaomi",
        name: "Xiaomi MiMo",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.xiaomimimo.com/v1",
        env_var: "XIAOMI_API_KEY",
        default_model: "mimo-v2-flash",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "xiaomi-token-plan-cn",
        name: "Xiaomi Token Plan (CN)",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://api.xiaomimimo.com/v1",
        env_var: "XIAOMI_API_KEY",
        default_model: "mimo-v2-flash",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "xiaomi-token-plan-ams",
        name: "Xiaomi Token Plan (AMS)",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://ams.api.xiaomimimo.com/v1",
        env_var: "XIAOMI_API_KEY",
        default_model: "mimo-v2-flash",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "xiaomi-token-plan-sgp",
        name: "Xiaomi Token Plan (SGP)",
        api: ProviderApi::OpenAiCompletions,
        base_url: "https://sgp.api.xiaomimimo.com/v1",
        env_var: "XIAOMI_API_KEY",
        default_model: "mimo-v2-flash",
        requires_auth: true,
        extra_headers: &[],
    },
    BuiltinProviderSpec {
        id: "llama-cpp",
        name: "llama.cpp Server",
        api: ProviderApi::OpenAiCompletions,
        base_url: "http://127.0.0.1:8080/v1",
        env_var: "LLAMA_CPP_API_KEY",
        default_model: "local-model",
        requires_auth: false,
        extra_headers: &[],
    },
];

#[derive(Debug, Clone)]
struct AzureSettings {
    api_version: String,
    deployment_map: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct AwsSigV4Credentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    region: String,
}

#[derive(Debug, Clone)]
enum AuthMethod {
    None,
    Bearer(String),
    Header { name: String, value: String },
    AwsSigV4(AwsSigV4Credentials),
}

#[derive(Debug, Clone)]
struct ResolvedProvider {
    id: String,
    name: String,
    api: ProviderApi,
    base_url: String,
    env_var: String,
    default_model: String,
    #[allow(dead_code)]
    requires_auth: bool,
    configured: bool,
    explicitly_configured: bool,
    auth: AuthMethod,
    extra_headers: HashMap<String, String>,
    azure: Option<AzureSettings>,
}

/// Registry of all built-in and custom AI providers.
#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<String, ResolvedProvider>,
    configured_cloud_provider: String,
    configured_cloud_model: String,
    legacy_openrouter_key: String,
}

impl ProviderRegistry {
    /// Build a registry from `MycelConfig` using the process environment.
    pub fn from_config(config: &MycelConfig) -> Self {
        Self::from_config_with_env(config, |k| std::env::var(k).ok())
    }

    /// Build a registry with an injectable environment lookup (deterministic in tests).
    pub fn from_config_with_env(
        config: &MycelConfig,
        env: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let mut providers = HashMap::new();

        for spec in BUILTIN_PROVIDERS {
            let custom = config.providers.get(spec.id);
            let resolved = resolve_builtin(spec, custom, config, &env);
            providers.insert(spec.id.to_string(), resolved);
        }

        for (id, custom) in &config.providers {
            if !providers.contains_key(id) {
                let resolved = resolve_custom(id, custom, &env);
                providers.insert(id.clone(), resolved);
            }
        }

        Self {
            providers,
            configured_cloud_provider: config.cloud_provider.trim().to_string(),
            configured_cloud_model: config.cloud_model.trim().to_string(),
            legacy_openrouter_key: config.openrouter_api_key.trim().to_string(),
        }
    }

    /// List all known providers sorted by ID without exposing any secrets.
    pub fn list_summaries(&self) -> Vec<ProviderSummary> {
        let mut items: Vec<ProviderSummary> = self
            .providers
            .values()
            .map(|p| ProviderSummary {
                id: p.id.clone(),
                name: p.name.clone(),
                api: p.api.as_str().to_string(),
                configured: p.configured,
                env_var: p.env_var.clone(),
                default_model: p.default_model.clone(),
                base_url: p.base_url.clone(),
            })
            .collect();
        items.sort_by(|a, b| a.id.cmp(&b.id));
        items
    }

    /// Auto-select the active `(provider_id, model)` according to config and available credentials.
    pub fn auto_select(&self) -> Option<(String, String)> {
        if !self.configured_cloud_provider.is_empty() {
            if let Some(p) = self.providers.get(&self.configured_cloud_provider) {
                if p.configured {
                    let model = self.model_for_provider(p);
                    return Some((p.id.clone(), model));
                }
            }
            return None;
        }

        if !self.legacy_openrouter_key.is_empty() {
            if let Some(p) = self.providers.get("openrouter") {
                if p.configured {
                    let model = if self.configured_cloud_model.is_empty() {
                        p.default_model.clone()
                    } else {
                        self.configured_cloud_model.clone()
                    };
                    return Some(("openrouter".to_string(), model));
                }
            }
        }

        const PRIORITY: &[&str] = &["venice", "anthropic", "openai", "google", "openrouter"];
        for id in PRIORITY {
            if let Some(p) = self.providers.get(*id) {
                if p.configured {
                    return Some((p.id.clone(), self.model_for_provider(p)));
                }
            }
        }

        let mut rest: Vec<&ResolvedProvider> = self
            .providers
            .values()
            .filter(|p| {
                p.configured
                    && !PRIORITY.contains(&p.id.as_str())
                    && (p.id != "llama-cpp" || p.explicitly_configured)
            })
            .collect();
        rest.sort_by(|a, b| a.id.cmp(&b.id));
        rest.into_iter()
            .next()
            .map(|p| (p.id.clone(), self.model_for_provider(p)))
    }

    fn model_for_provider(&self, p: &ResolvedProvider) -> String {
        if self.configured_cloud_model.is_empty() {
            return p.default_model.clone();
        }
        if p.id != "openrouter" && self.configured_cloud_model == DEFAULT_OPENROUTER_MODEL {
            return p.default_model.clone();
        }
        self.configured_cloud_model.clone()
    }

    /// Split a `provider/model` specification if the prefix is a registered provider ID.
    pub fn split_provider_model(&self, spec: &str) -> Option<(String, String)> {
        let trimmed = spec.trim();
        let (prefix, rest) = trimmed.split_once('/')?;
        if prefix.is_empty() || rest.is_empty() {
            return None;
        }
        if self.providers.contains_key(prefix) {
            Some((prefix.to_string(), rest.to_string()))
        } else {
            None
        }
    }

    /// Validate that a provider (or `provider/model` spec) exists and is configured.
    pub fn validate_selection(
        &self,
        provider_or_spec: &str,
        model_override: Option<&str>,
    ) -> Result<(String, String)> {
        let trimmed = provider_or_spec.trim();
        if trimmed.is_empty() {
            bail!("Provider name cannot be empty");
        }

        let (provider_id, spec_model) = if let Some((p, m)) = self.split_provider_model(trimmed) {
            (p, Some(m))
        } else {
            (trimmed.to_string(), None)
        };

        let provider = self
            .providers
            .get(&provider_id)
            .ok_or_else(|| anyhow!("Unknown provider '{}'", provider_id))?;

        if !provider.configured {
            let hint = if provider.env_var.is_empty() {
                "configure api_key or requires_auth = false in [providers]".to_string()
            } else {
                format!(
                    "set {} or configure it in [providers.{}]",
                    provider.env_var, provider.id
                )
            };
            bail!("Provider '{}' is not configured ({})", provider.id, hint);
        }

        let model = model_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .or(spec_model)
            .unwrap_or_else(|| provider.default_model.clone());

        Ok((provider.id.clone(), model))
    }

    /// Perform a non-streaming inference call against the selected provider.
    pub async fn generate(
        &self,
        client: &Client,
        provider_id: &str,
        model: &str,
        prompt: &str,
    ) -> Result<GenerationOutput> {
        let provider = self.get_configured_provider(provider_id)?;
        let (url, body_bytes, mut headers) = build_request_parts(provider, model, prompt, false)?;

        apply_auth_headers(provider, "POST", &url, &body_bytes, &mut headers)?;

        debug!(
            provider = %provider.id,
            api = %provider.api.as_str(),
            model = %model,
            "Sending cloud generation request"
        );

        let resp = client
            .post(&url)
            .headers(headers)
            .body(body_bytes)
            .send()
            .await
            .with_context(|| format!("Failed to reach provider '{}'", provider.id))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "Provider '{}' returned HTTP {}: {}",
                provider.id,
                status,
                truncate_error(&text)
            );
        }

        let json: Value = serde_json::from_str(&text)
            .with_context(|| format!("Invalid JSON response from provider '{}'", provider.id))?;

        parse_non_streaming_response(provider.api, &json)
            .with_context(|| format!("Failed to parse response from '{}'", provider.id))
    }

    /// Perform a streaming inference call yielding text chunks.
    pub async fn generate_stream(
        &self,
        client: &Client,
        provider_id: &str,
        model: &str,
        prompt: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        let provider = self.get_configured_provider(provider_id)?;

        if provider.api == ProviderApi::BedrockConverse {
            let output = self.generate(client, provider_id, model, prompt).await?;
            let stream = futures::stream::once(async move { Ok(output.text) });
            return Ok(Box::pin(stream));
        }

        let (url, body_bytes, mut headers) = build_request_parts(provider, model, prompt, true)?;
        apply_auth_headers(provider, "POST", &url, &body_bytes, &mut headers)?;

        debug!(
            provider = %provider.id,
            api = %provider.api.as_str(),
            model = %model,
            "Sending cloud streaming request"
        );

        let resp = client
            .post(&url)
            .headers(headers)
            .body(body_bytes)
            .send()
            .await
            .with_context(|| format!("Failed to reach provider '{}'", provider.id))?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            bail!(
                "Provider '{}' returned HTTP {}: {}",
                provider.id,
                status,
                truncate_error(&err_body)
            );
        }

        let api = provider.api;
        let byte_stream = resp.bytes_stream();

        let text_stream = async_stream_sse(byte_stream, api);
        Ok(Box::pin(text_stream))
    }

    fn get_configured_provider(&self, provider_id: &str) -> Result<&ResolvedProvider> {
        let provider = self
            .providers
            .get(provider_id)
            .ok_or_else(|| anyhow!("Unknown provider '{}'", provider_id))?;
        if !provider.configured {
            bail!("Provider '{}' is not configured", provider_id);
        }
        Ok(provider)
    }
}

fn non_empty_env(env: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    env(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn expand_template(template: &str, env: &impl Fn(&str) -> Option<String>) -> String {
    let mut out = template.to_string();

    if out.contains("{CLOUDFLARE_ACCOUNT_ID}") {
        if let Some(v) = non_empty_env(env, "CLOUDFLARE_ACCOUNT_ID") {
            out = out.replace("{CLOUDFLARE_ACCOUNT_ID}", &v);
        }
    }
    if out.contains("{CLOUDFLARE_GATEWAY_ID}") {
        if let Some(v) = non_empty_env(env, "CLOUDFLARE_GATEWAY_ID") {
            out = out.replace("{CLOUDFLARE_GATEWAY_ID}", &v);
        }
    }
    if out.contains("{location}") {
        let loc = non_empty_env(env, "GOOGLE_CLOUD_LOCATION")
            .unwrap_or_else(|| "us-central1".to_string());
        out = out.replace("{location}", &loc);
    }
    if out.contains("{project}") {
        if let Some(proj) = non_empty_env(env, "GOOGLE_CLOUD_PROJECT") {
            out = out.replace("{project}", &proj);
        }
    }
    if out.contains("{region}") {
        let region = non_empty_env(env, "AWS_REGION")
            .or_else(|| non_empty_env(env, "AWS_DEFAULT_REGION"))
            .unwrap_or_else(|| "us-east-1".to_string());
        out = out.replace("{region}", &region);
    }
    if out.contains("{AZURE_OPENAI_RESOURCE_NAME}") {
        if let Some(res) = non_empty_env(env, "AZURE_OPENAI_RESOURCE_NAME") {
            out = out.replace("{AZURE_OPENAI_RESOURCE_NAME}", &res);
        }
    }

    out.trim_end_matches('/').to_string()
}

fn parse_azure_deployment_map(raw: Option<String>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(s) = raw.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) else {
        return map;
    };

    if s.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<HashMap<String, String>>(&s) {
            return parsed;
        }
    }

    for pair in s.split(',') {
        if let Some((model, dep)) = pair.split_once('=') {
            let m = model.trim();
            let d = dep.trim();
            if !m.is_empty() && !d.is_empty() {
                map.insert(m.to_string(), d.to_string());
            }
        }
    }
    map
}

fn resolve_builtin(
    spec: &BuiltinProviderSpec,
    custom: Option<&CustomProviderConfig>,
    config: &MycelConfig,
    env: &impl Fn(&str) -> Option<String>,
) -> ResolvedProvider {
    let api = custom
        .and_then(|c| c.api.as_deref())
        .and_then(ProviderApi::from_str_loose)
        .unwrap_or(spec.api);

    let env_var = custom
        .and_then(|c| c.api_key_env.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(spec.env_var)
        .to_string();

    let default_model = custom
        .and_then(|c| c.default_model.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(spec.default_model)
        .to_string();

    let requires_auth = custom
        .and_then(|c| c.requires_auth)
        .unwrap_or(spec.requires_auth);

    let mut base_url = custom
        .and_then(|c| c.base_url.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(spec.base_url)
        .to_string();

    if spec.id == "azure" {
        if let Some(azure_base) = non_empty_env(env, "AZURE_OPENAI_BASE_URL") {
            base_url = azure_base;
        } else if let Some(res) = non_empty_env(env, "AZURE_OPENAI_RESOURCE_NAME") {
            if custom.and_then(|c| c.base_url.as_ref()).is_none() {
                base_url = format!("https://{}.openai.azure.com/openai", res);
            }
        }
    }

    let base_url = expand_template(&base_url, env);

    let mut extra_headers = HashMap::new();
    for (k, v) in spec.extra_headers {
        extra_headers.insert((*k).to_string(), (*v).to_string());
    }
    if let Some(c) = custom {
        for (k, v) in &c.headers {
            extra_headers.insert(k.clone(), v.clone());
        }
    }

    let azure = if spec.id == "azure" {
        Some(AzureSettings {
            api_version: non_empty_env(env, "AZURE_OPENAI_API_VERSION")
                .unwrap_or_else(|| DEFAULT_AZURE_API_VERSION.to_string()),
            deployment_map: parse_azure_deployment_map(non_empty_env(
                env,
                "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
            )),
        })
    } else {
        None
    };

    let custom_key = custom
        .and_then(|c| c.api_key.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);

    let (auth, has_creds) = resolve_auth_for_provider(spec.id, &env_var, custom_key, config, env);

    let explicitly_configured = custom.is_some();
    let configured = if !requires_auth { true } else { has_creds };

    ResolvedProvider {
        id: spec.id.to_string(),
        name: spec.name.to_string(),
        api,
        base_url,
        env_var,
        default_model,
        requires_auth,
        configured,
        explicitly_configured,
        auth,
        extra_headers,
        azure,
    }
}

fn resolve_custom(
    id: &str,
    custom: &CustomProviderConfig,
    env: &impl Fn(&str) -> Option<String>,
) -> ResolvedProvider {
    let api = custom
        .api
        .as_deref()
        .and_then(ProviderApi::from_str_loose)
        .unwrap_or(ProviderApi::OpenAiCompletions);
    let env_var = custom
        .api_key_env
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let base_url = custom
        .base_url
        .as_deref()
        .map(|s| expand_template(s.trim(), env))
        .unwrap_or_default();
    let default_model = custom
        .default_model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string();
    let requires_auth = custom.requires_auth.unwrap_or(true);

    let key = custom
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| non_empty_env(env, &env_var));

    let configured = if base_url.is_empty() {
        false
    } else if !requires_auth {
        true
    } else {
        key.is_some()
    };

    let auth = match key {
        Some(k) => match api {
            ProviderApi::AnthropicMessages => AuthMethod::Header {
                name: "x-api-key".to_string(),
                value: k,
            },
            ProviderApi::GoogleGenerativeAi => AuthMethod::Header {
                name: "x-goog-api-key".to_string(),
                value: k,
            },
            _ => AuthMethod::Bearer(k),
        },
        None => AuthMethod::None,
    };

    ResolvedProvider {
        id: id.to_string(),
        name: id.to_string(),
        api,
        base_url,
        env_var,
        default_model,
        requires_auth,
        configured,
        explicitly_configured: true,
        auth,
        extra_headers: custom.headers.clone(),
        azure: None,
    }
}

fn resolve_auth_for_provider(
    id: &str,
    env_var: &str,
    custom_key: Option<String>,
    config: &MycelConfig,
    env: &impl Fn(&str) -> Option<String>,
) -> (AuthMethod, bool) {
    if let Some(k) = custom_key {
        let auth = match id {
            "anthropic" | "minimax" | "minimax-cn" | "kimi-coding" => AuthMethod::Header {
                name: "x-api-key".to_string(),
                value: k,
            },
            "azure" => AuthMethod::Header {
                name: "api-key".to_string(),
                value: k,
            },
            "google" | "google-vertex" => AuthMethod::Header {
                name: "x-goog-api-key".to_string(),
                value: k,
            },
            _ => AuthMethod::Bearer(k),
        };
        return (auth, true);
    }

    match id {
        "openrouter" => {
            let key = non_empty_env(env, env_var).or_else(|| {
                let k = config.openrouter_api_key.trim();
                if k.is_empty() {
                    None
                } else {
                    Some(k.to_string())
                }
            });
            match key {
                Some(k) => (AuthMethod::Bearer(k), true),
                None => (AuthMethod::None, false),
            }
        }
        "anthropic" => {
            if let Some(k) = non_empty_env(env, env_var) {
                return (
                    AuthMethod::Header {
                        name: "x-api-key".to_string(),
                        value: k,
                    },
                    true,
                );
            }
            if let Some(tok) = non_empty_env(env, "ANTHROPIC_AUTH_TOKEN")
                .or_else(|| non_empty_env(env, "ANTHROPIC_OAUTH_TOKEN"))
            {
                return (AuthMethod::Bearer(tok), true);
            }
            (AuthMethod::None, false)
        }
        "minimax" | "minimax-cn" | "kimi-coding" => {
            let key = non_empty_env(env, env_var).or_else(|| {
                (id == "minimax-cn")
                    .then(|| non_empty_env(env, "MINIMAX_API_KEY"))
                    .flatten()
            });
            match key {
                Some(k) => (
                    AuthMethod::Header {
                        name: "x-api-key".to_string(),
                        value: k,
                    },
                    true,
                ),
                None => (AuthMethod::None, false),
            }
        }
        "azure" => match non_empty_env(env, env_var) {
            Some(k) => (
                AuthMethod::Header {
                    name: "api-key".to_string(),
                    value: k,
                },
                true,
            ),
            None => (AuthMethod::None, false),
        },
        "google" => {
            let key = non_empty_env(env, env_var).or_else(|| non_empty_env(env, "GOOGLE_API_KEY"));
            match key {
                Some(k) => (
                    AuthMethod::Header {
                        name: "x-goog-api-key".to_string(),
                        value: k,
                    },
                    true,
                ),
                None => (AuthMethod::None, false),
            }
        }
        "google-vertex" => {
            if let Some(tok) = non_empty_env(env, "GOOGLE_VERTEX_ACCESS_TOKEN")
                .or_else(|| non_empty_env(env, "GOOGLE_OAUTH_ACCESS_TOKEN"))
            {
                return (AuthMethod::Bearer(tok), true);
            }
            if let Some(k) = non_empty_env(env, env_var) {
                return (
                    AuthMethod::Header {
                        name: "x-goog-api-key".to_string(),
                        value: k,
                    },
                    true,
                );
            }
            (AuthMethod::None, false)
        }
        "amazon-bedrock" => {
            if let Some(tok) = non_empty_env(env, "AWS_BEARER_TOKEN_BEDROCK") {
                return (AuthMethod::Bearer(tok), true);
            }
            if let (Some(ak), Some(sk)) = (
                non_empty_env(env, "AWS_ACCESS_KEY_ID"),
                non_empty_env(env, "AWS_SECRET_ACCESS_KEY"),
            ) {
                let region = non_empty_env(env, "AWS_REGION")
                    .or_else(|| non_empty_env(env, "AWS_DEFAULT_REGION"))
                    .unwrap_or_else(|| "us-east-1".to_string());
                let session_token = non_empty_env(env, "AWS_SESSION_TOKEN");
                return (
                    AuthMethod::AwsSigV4(AwsSigV4Credentials {
                        access_key_id: ak,
                        secret_access_key: sk,
                        session_token,
                        region,
                    }),
                    true,
                );
            }
            (AuthMethod::None, false)
        }
        "huggingface" => {
            let key =
                non_empty_env(env, env_var).or_else(|| non_empty_env(env, "HUGGINGFACE_HUB_TOKEN"));
            match key {
                Some(k) => (AuthMethod::Bearer(k), true),
                None => (AuthMethod::None, false),
            }
        }
        _ => match non_empty_env(env, env_var) {
            Some(k) => (AuthMethod::Bearer(k), true),
            None => (AuthMethod::None, false),
        },
    }
}

fn build_request_parts(
    provider: &ResolvedProvider,
    model: &str,
    prompt: &str,
    stream: bool,
) -> Result<(String, Vec<u8>, HeaderMap)> {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    for (k, v) in &provider.extra_headers {
        let name = HeaderName::from_bytes(k.as_bytes()).with_context(|| {
            format!("Invalid header name '{}' on provider '{}'", k, provider.id)
        })?;
        let val = HeaderValue::from_str(v).with_context(|| {
            format!(
                "Invalid header value for '{}' on provider '{}'",
                k, provider.id
            )
        })?;
        headers.insert(name, val);
    }

    let base = provider.base_url.trim_end_matches('/');

    let (url, body_json) = match provider.api {
        ProviderApi::OpenAiCompletions => {
            let url = if let Some(azure) = &provider.azure {
                let deployment = azure
                    .deployment_map
                    .get(model)
                    .map(String::as_str)
                    .unwrap_or(model);
                if base.contains("/deployments/") {
                    format!(
                        "{}/chat/completions?api-version={}",
                        base, azure.api_version
                    )
                } else {
                    format!(
                        "{}/deployments/{}/chat/completions?api-version={}",
                        base, deployment, azure.api_version
                    )
                }
            } else {
                format!("{}/chat/completions", base)
            };

            let body = json!({
                "model": model,
                "messages": [{ "role": "user", "content": prompt }],
                "stream": stream
            });
            (url, body)
        }
        ProviderApi::OpenAiResponses => {
            let url = format!("{}/responses", base);
            let body = json!({
                "model": model,
                "input": prompt,
                "stream": stream
            });
            (url, body)
        }
        ProviderApi::AnthropicMessages => {
            headers.insert(
                HeaderName::from_static("anthropic-version"),
                HeaderValue::from_static(ANTHROPIC_API_VERSION),
            );
            let url = if base.ends_with("/v1") {
                format!("{}/messages", base)
            } else {
                format!("{}/v1/messages", base)
            };
            let body = json!({
                "model": model,
                "max_tokens": 4096,
                "messages": [{ "role": "user", "content": prompt }],
                "stream": stream
            });
            (url, body)
        }
        ProviderApi::GoogleGenerativeAi => {
            let url = if stream {
                format!("{}/models/{}:streamGenerateContent?alt=sse", base, model)
            } else {
                format!("{}/models/{}:generateContent", base, model)
            };
            let body = json!({
                "contents": [{
                    "role": "user",
                    "parts": [{ "text": prompt }]
                }]
            });
            (url, body)
        }
        ProviderApi::BedrockConverse => {
            let encoded_model = uri_encode_segment(model);
            let url = format!("{}/model/{}/converse", base, encoded_model);
            let body = json!({
                "messages": [{
                    "role": "user",
                    "content": [{ "text": prompt }]
                }]
            });
            (url, body)
        }
    };

    let body_bytes = serde_json::to_vec(&body_json)?;
    Ok((url, body_bytes, headers))
}

fn apply_auth_headers(
    provider: &ResolvedProvider,
    method: &str,
    url: &str,
    body: &[u8],
    headers: &mut HeaderMap,
) -> Result<()> {
    match &provider.auth {
        AuthMethod::None => Ok(()),
        AuthMethod::Bearer(token) => {
            let val = HeaderValue::from_str(&format!("Bearer {}", token))
                .context("Invalid bearer token characters")?;
            headers.insert(reqwest::header::AUTHORIZATION, val);
            Ok(())
        }
        AuthMethod::Header { name, value } => {
            let hname = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("Invalid auth header name '{}'", name))?;
            let hval = HeaderValue::from_str(value).context("Invalid auth header value")?;
            headers.insert(hname, hval);
            Ok(())
        }
        AuthMethod::AwsSigV4(creds) => {
            let now = Utc::now();
            let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
            let sig_headers = sign_aws_sigv4(
                method,
                url,
                body,
                "bedrock",
                &creds.region,
                &creds.access_key_id,
                &creds.secret_access_key,
                creds.session_token.as_deref(),
                &amz_date,
            )?;
            for (k, v) in sig_headers {
                let hname = HeaderName::from_bytes(k.as_bytes())?;
                let hval = HeaderValue::from_str(&v)?;
                headers.insert(hname, hval);
            }
            Ok(())
        }
    }
}

fn parse_non_streaming_response(api: ProviderApi, json: &Value) -> Result<GenerationOutput> {
    match api {
        ProviderApi::OpenAiCompletions => {
            let text = json["choices"][0]["message"]["content"]
                .as_str()
                .ok_or_else(|| anyhow!("Missing choices[0].message.content in response"))?
                .to_string();
            let prompt_tokens = json["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
            let completion_tokens = json["usage"]["completion_tokens"].as_u64().unwrap_or(0);
            Ok(GenerationOutput {
                text,
                prompt_tokens,
                completion_tokens,
            })
        }
        ProviderApi::OpenAiResponses => {
            let text = if let Some(s) = json["output_text"].as_str() {
                s.to_string()
            } else {
                let mut collected = String::new();
                if let Some(outputs) = json["output"].as_array() {
                    for item in outputs {
                        if let Some(parts) = item["content"].as_array() {
                            for part in parts {
                                if let Some(t) = part["text"].as_str() {
                                    collected.push_str(t);
                                }
                            }
                        }
                    }
                }
                if collected.is_empty() {
                    bail!("Missing output_text or output[].content[].text in response");
                }
                collected
            };
            let prompt_tokens = json["usage"]["input_tokens"].as_u64().unwrap_or(0);
            let completion_tokens = json["usage"]["output_tokens"].as_u64().unwrap_or(0);
            Ok(GenerationOutput {
                text,
                prompt_tokens,
                completion_tokens,
            })
        }
        ProviderApi::AnthropicMessages => {
            let mut collected = String::new();
            if let Some(blocks) = json["content"].as_array() {
                for b in blocks {
                    if let Some(t) = b["text"].as_str() {
                        collected.push_str(t);
                    }
                }
            }
            if collected.is_empty() {
                bail!("Missing content[].text in Anthropic response");
            }
            let prompt_tokens = json["usage"]["input_tokens"].as_u64().unwrap_or(0);
            let completion_tokens = json["usage"]["output_tokens"].as_u64().unwrap_or(0);
            Ok(GenerationOutput {
                text: collected,
                prompt_tokens,
                completion_tokens,
            })
        }
        ProviderApi::GoogleGenerativeAi => {
            let mut collected = String::new();
            if let Some(parts) = json["candidates"][0]["content"]["parts"].as_array() {
                for p in parts {
                    if let Some(t) = p["text"].as_str() {
                        collected.push_str(t);
                    }
                }
            }
            if collected.is_empty() {
                bail!("Missing candidates[0].content.parts[].text in Google response");
            }
            let prompt_tokens = json["usageMetadata"]["promptTokenCount"]
                .as_u64()
                .unwrap_or(0);
            let completion_tokens = json["usageMetadata"]["candidatesTokenCount"]
                .as_u64()
                .unwrap_or(0);
            Ok(GenerationOutput {
                text: collected,
                prompt_tokens,
                completion_tokens,
            })
        }
        ProviderApi::BedrockConverse => {
            let mut collected = String::new();
            if let Some(blocks) = json["output"]["message"]["content"].as_array() {
                for b in blocks {
                    if let Some(t) = b["text"].as_str() {
                        collected.push_str(t);
                    }
                }
            }
            if collected.is_empty() {
                bail!("Missing output.message.content[].text in Bedrock response");
            }
            let prompt_tokens = json["usage"]["inputTokens"].as_u64().unwrap_or(0);
            let completion_tokens = json["usage"]["outputTokens"].as_u64().unwrap_or(0);
            Ok(GenerationOutput {
                text: collected,
                prompt_tokens,
                completion_tokens,
            })
        }
    }
}

fn extract_sse_delta(api: ProviderApi, data: &str) -> Option<String> {
    let trimmed = data.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return None;
    }
    let json: Value = serde_json::from_str(trimmed).ok()?;

    match api {
        ProviderApi::OpenAiCompletions => json["choices"][0]["delta"]["content"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(ToString::to_string),
        ProviderApi::OpenAiResponses => {
            if json["type"].as_str() == Some("response.output_text.delta") {
                json["delta"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string)
            } else {
                json["choices"][0]["delta"]["content"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string)
            }
        }
        ProviderApi::AnthropicMessages => {
            if json["type"].as_str() == Some("content_block_delta") {
                json["delta"]["text"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string)
            } else {
                None
            }
        }
        ProviderApi::GoogleGenerativeAi => {
            let mut out = String::new();
            if let Some(parts) = json["candidates"][0]["content"]["parts"].as_array() {
                for p in parts {
                    if let Some(t) = p["text"].as_str() {
                        out.push_str(t);
                    }
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        ProviderApi::BedrockConverse => None,
    }
}

fn async_stream_sse<S, B>(
    byte_stream: S,
    api: ProviderApi,
) -> impl Stream<Item = Result<String>> + Send
where
    B: AsRef<[u8]> + Send + 'static,
    S: Stream<Item = Result<B, reqwest::Error>> + Send + Unpin + 'static,
{
    struct SseState<S> {
        inner: S,
        buf: Vec<u8>,
        pending: std::collections::VecDeque<String>,
        done: bool,
        api: ProviderApi,
    }

    let state = SseState {
        inner: byte_stream,
        buf: Vec::new(),
        pending: std::collections::VecDeque::new(),
        done: false,
        api,
    };

    futures::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(chunk) = st.pending.pop_front() {
                return Some((Ok(chunk), st));
            }
            if st.done {
                return None;
            }

            while let Some(pos) = st.buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = st.buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim_end_matches(['\r', '\n']);
                if let Some(data) = line.strip_prefix("data:") {
                    let data = data.trim_start();
                    if data == "[DONE]" {
                        st.done = true;
                        break;
                    }
                    if let Some(delta) = extract_sse_delta(st.api, data) {
                        st.pending.push_back(delta);
                    }
                }
            }

            if let Some(chunk) = st.pending.pop_front() {
                return Some((Ok(chunk), st));
            }
            if st.done {
                return None;
            }

            match st.inner.next().await {
                Some(Ok(bytes)) => {
                    st.buf.extend_from_slice(bytes.as_ref());
                }
                Some(Err(e)) => {
                    st.done = true;
                    return Some((Err(anyhow!("Stream read error: {}", e)), st));
                }
                None => {
                    st.done = true;
                    if !st.buf.is_empty() {
                        let line = String::from_utf8_lossy(&st.buf).to_string();
                        st.buf.clear();
                        if let Some(data) =
                            line.trim_end_matches(['\r', '\n']).strip_prefix("data:")
                        {
                            if let Some(delta) = extract_sse_delta(st.api, data) {
                                return Some((Ok(delta), st));
                            }
                        }
                    }
                    return None;
                }
            }
        }
    })
}

fn truncate_error(s: &str) -> String {
    const MAX: usize = 300;
    if s.len() <= MAX {
        s.to_string()
    } else {
        format!("{}...", &s[..MAX])
    }
}

// ---------------------------------------------------------------------------
// Pure-Rust RFC 2104 HMAC-SHA256 & AWS SigV4 signing (zero new dependencies)
// ---------------------------------------------------------------------------

fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let hex = sha256::digest(data);
    let bytes = hex.as_bytes();
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        if let (Some(&hi), Some(&lo)) = (bytes.get(2 * i), bytes.get(2 * i + 1)) {
            *slot = (hex_nibble(hi) << 4) | hex_nibble(lo);
        }
    }
    out
}

fn hex_nibble(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(LUT[(b >> 4) as usize] as char);
        out.push(LUT[(b & 0x0f) as usize] as char);
    }
    out
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let hashed = sha256_bytes(key);
        key_block[..32].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }

    let mut inner = Vec::with_capacity(BLOCK + data.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(data);
    let inner_hash = sha256_bytes(&inner);

    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    sha256_bytes(&outer)
}

fn uri_encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn sign_aws_sigv4(
    method: &str,
    url: &str,
    body: &[u8],
    service: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    amz_date: &str,
) -> Result<Vec<(String, String)>> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("Invalid AWS URL '{}'", url))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("Missing host in AWS URL"))?;
    let host_header = match parsed.port() {
        Some(p) => format!("{}:{}", host, p),
        None => host.to_string(),
    };

    let canonical_uri = if parsed.path().is_empty() {
        "/".to_string()
    } else {
        parsed.path().to_string()
    };

    let mut query_pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(k, v)| (uri_encode_segment(&k), uri_encode_segment(&v)))
        .collect();
    query_pairs.sort();
    let canonical_query: String = query_pairs
        .into_iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    let payload_hash = sha256::digest(body);
    let date_stamp = amz_date
        .get(..8)
        .ok_or_else(|| anyhow!("Invalid amz_date '{}'", amz_date))?;

    let mut hdrs: BTreeMap<String, String> = BTreeMap::new();
    hdrs.insert("host".to_string(), host_header);
    hdrs.insert("x-amz-content-sha256".to_string(), payload_hash.clone());
    hdrs.insert("x-amz-date".to_string(), amz_date.to_string());
    if let Some(tok) = session_token {
        hdrs.insert("x-amz-security-token".to_string(), tok.to_string());
    }

    let canonical_headers: String = hdrs
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v.trim()))
        .collect();
    let signed_headers: String = hdrs.keys().cloned().collect::<Vec<_>>().join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method, canonical_uri, canonical_query, canonical_headers, signed_headers, payload_hash
    );

    let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, region, service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        credential_scope,
        sha256::digest(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(
        format!("AWS4{}", secret_key).as_bytes(),
        date_stamp.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = to_hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        access_key, credential_scope, signed_headers, signature
    );

    let mut out = vec![
        ("x-amz-date".to_string(), amz_date.to_string()),
        ("x-amz-content-sha256".to_string(), payload_hash),
        ("Authorization".to_string(), authorization),
    ];
    if let Some(tok) = session_token {
        out.push(("x-amz-security-token".to_string(), tok.to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn empty_env(_: &str) -> Option<String> {
        None
    }

    fn map_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn test_all_45_builtin_providers_present_and_valid() {
        let cfg = MycelConfig::default();
        let reg = ProviderRegistry::from_config_with_env(&cfg, empty_env);
        let summaries = reg.list_summaries();
        assert_eq!(summaries.len(), 45, "expected 45 built-in providers");

        let expected = [
            "venice",
            "openrouter",
            "anthropic",
            "openai",
            "openai-codex",
            "azure",
            "google",
            "google-vertex",
            "amazon-bedrock",
            "deepseek",
            "groq",
            "cerebras",
            "xai",
            "mistral",
            "fireworks",
            "together",
            "nvidia",
            "huggingface",
            "baseten",
            "github-copilot",
            "vercel-ai-gateway",
            "cloudflare-workers-ai",
            "cloudflare-ai-gateway",
            "minimax",
            "minimax-cn",
            "moonshotai",
            "moonshotai-cn",
            "kimi-coding",
            "ant-ling",
            "meta",
            "zai",
            "zai-coding-cn",
            "opencode",
            "opencode-go",
            "radius",
            "typesafe",
            "qwen-token-plan",
            "qwen-token-plan-cn",
            "qwen-token-plan-individual",
            "qwen-token-plan-individual-cn",
            "xiaomi",
            "xiaomi-token-plan-cn",
            "xiaomi-token-plan-ams",
            "xiaomi-token-plan-sgp",
            "llama-cpp",
        ];
        for id in expected {
            assert!(
                summaries.iter().any(|s| s.id == id),
                "missing provider {}",
                id
            );
        }
    }

    #[test]
    fn test_url_template_expansion_and_azure_normalization() {
        let cfg = MycelConfig::default();
        let env = map_env(&[
            ("CLOUDFLARE_ACCOUNT_ID", "acc-123"),
            ("CLOUDFLARE_GATEWAY_ID", "gw-456"),
            ("GOOGLE_CLOUD_LOCATION", "europe-west4"),
            ("GOOGLE_CLOUD_PROJECT", "my-proj"),
            ("AWS_REGION", "us-west-2"),
            ("AZURE_OPENAI_RESOURCE_NAME", "my-res"),
            ("AZURE_OPENAI_API_KEY", "az-secret"),
            ("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", "gpt-4o=prod-gpt4o"),
        ]);
        let reg = ProviderRegistry::from_config_with_env(&cfg, env);
        let summaries = reg.list_summaries();
        let by_id = |id: &str| summaries.iter().find(|s| s.id == id).unwrap().clone();

        assert_eq!(
            by_id("cloudflare-workers-ai").base_url,
            "https://api.cloudflare.com/client/v4/accounts/acc-123/ai/v1"
        );
        assert_eq!(
            by_id("cloudflare-ai-gateway").base_url,
            "https://gateway.ai.cloudflare.com/v1/acc-123/gw-456/compat"
        );
        assert_eq!(
            by_id("google-vertex").base_url,
            "https://europe-west4-aiplatform.googleapis.com/v1/projects/my-proj/locations/europe-west4/publishers/google"
        );
        assert_eq!(
            by_id("amazon-bedrock").base_url,
            "https://bedrock-runtime.us-west-2.amazonaws.com"
        );
        assert_eq!(
            by_id("azure").base_url,
            "https://my-res.openai.azure.com/openai"
        );

        let azure = reg.providers.get("azure").unwrap();
        let (url, _, headers) = build_request_parts(azure, "gpt-4o", "hi", false).unwrap();
        assert_eq!(
            url,
            "https://my-res.openai.azure.com/openai/deployments/prod-gpt4o/chat/completions?api-version=2024-10-21"
        );
        assert!(headers.get("Content-Type").is_some());
    }

    #[test]
    fn test_split_provider_model_and_validate_selection() {
        let cfg = MycelConfig::default();
        let env = map_env(&[
            ("VENICE_API_KEY", "v-key"),
            ("OPENROUTER_API_KEY", "or-key"),
        ]);
        let reg = ProviderRegistry::from_config_with_env(&cfg, env);

        assert_eq!(
            reg.split_provider_model("venice/llama-3.3-70b"),
            Some(("venice".to_string(), "llama-3.3-70b".to_string()))
        );
        assert_eq!(
            reg.split_provider_model("openrouter/anthropic/claude-3.5-sonnet"),
            Some((
                "openrouter".to_string(),
                "anthropic/claude-3.5-sonnet".to_string()
            ))
        );
        assert_eq!(
            reg.split_provider_model("groq/openai/gpt-oss-120b"),
            Some(("groq".to_string(), "openai/gpt-oss-120b".to_string()))
        );
        assert_eq!(reg.split_provider_model("unknown/model"), None);

        assert_eq!(
            reg.validate_selection("venice/llama-3.3-70b", None)
                .unwrap(),
            ("venice".to_string(), "llama-3.3-70b".to_string())
        );
        assert!(reg.validate_selection("anthropic", None).is_err());
        assert!(reg.validate_selection("nonexistent", None).is_err());
    }

    #[test]
    fn test_auto_selection_priority_and_no_cloud_keys() {
        let cfg = MycelConfig::default();
        // No keys -> auto_select returns None (llama-cpp is NOT auto-selected unless explicit)
        let reg_none = ProviderRegistry::from_config_with_env(&cfg, empty_env);
        assert_eq!(reg_none.auto_select(), None);

        // Venice beats Anthropic/OpenAI when no legacy openrouter_api_key in config
        let reg_venice = ProviderRegistry::from_config_with_env(
            &cfg,
            map_env(&[("OPENAI_API_KEY", "o"), ("VENICE_API_KEY", "v")]),
        );
        assert_eq!(
            reg_venice.auto_select(),
            Some(("venice".to_string(), "llama-3.3-70b".to_string()))
        );

        // Legacy openrouter_api_key in config preserves backwards-compat openrouter selection
        let legacy_cfg = MycelConfig {
            openrouter_api_key: "or-legacy".to_string(),
            ..Default::default()
        };
        let reg_legacy = ProviderRegistry::from_config_with_env(
            &legacy_cfg,
            map_env(&[("VENICE_API_KEY", "v")]),
        );
        assert_eq!(
            reg_legacy.auto_select(),
            Some((
                "openrouter".to_string(),
                "anthropic/claude-3.5-sonnet".to_string()
            ))
        );

        // Explicit cloud_provider wins
        let explicit_cfg = MycelConfig {
            cloud_provider: "llama-cpp".to_string(),
            ..Default::default()
        };
        let reg_explicit = ProviderRegistry::from_config_with_env(&explicit_cfg, empty_env);
        assert_eq!(
            reg_explicit.auto_select(),
            Some(("llama-cpp".to_string(), "local-model".to_string()))
        );
    }

    #[test]
    fn test_custom_toml_provider() {
        let toml_src = r#"
cloud_provider = "local-vllm"
cloud_model = "Qwen/Qwen2.5-72B-Instruct"

[providers.local-vllm]
api = "openai-completions"
base_url = "http://localhost:8000/v1"
default_model = "Qwen/Qwen2.5-72B-Instruct"
requires_auth = false
"#;
        let cfg: MycelConfig = toml::from_str(toml_src).unwrap();
        let reg = ProviderRegistry::from_config_with_env(&cfg, empty_env);
        assert_eq!(
            reg.auto_select(),
            Some((
                "local-vllm".to_string(),
                "Qwen/Qwen2.5-72B-Instruct".to_string()
            ))
        );
    }

    #[test]
    fn test_aws_sigv4_deterministic_vector() {
        let sig_headers = sign_aws_sigv4(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse",
            br#"{"messages":[]}"#,
            "bedrock",
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            Some("AQoDYXdzEJr"),
            "20261128T060000Z",
        )
        .unwrap();

        let auth = sig_headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20261128/us-east-1/bedrock/aws4_request"
        ));
        assert!(auth
            .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token"));
        assert!(auth.contains("Signature="));

        // Deterministic: calling again with same inputs produces identical signature
        let sig_headers_2 = sign_aws_sigv4(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse",
            br#"{"messages":[]}"#,
            "bedrock",
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            Some("AQoDYXdzEJr"),
            "20261128T060000Z",
        )
        .unwrap();
        assert_eq!(sig_headers, sig_headers_2);
    }

    async fn spawn_mock_server(
        content_type: &'static str,
        response_body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = tx.send(req);

                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    content_type,
                    response_body.len(),
                    response_body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        (format!("http://{}", addr), rx)
    }

    #[tokio::test]
    async fn test_live_mock_http_all_5_protocols_and_streaming() {
        let client = Client::new();

        // 1. OpenAI Completions (Venice) JSON + OpenRouter headers
        let (venice_url, venice_rx) = spawn_mock_server(
            "application/json",
            r#"{"choices":[{"message":{"content":"hello from venice"}}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
        )
        .await;
        let (or_url, or_rx) = spawn_mock_server(
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: [DONE]\n\n",
        )
        .await;

        let mut cfg = MycelConfig::default();
        cfg.providers.insert(
            "venice".to_string(),
            CustomProviderConfig {
                base_url: Some(venice_url),
                api_key: Some("venice-secret".to_string()),
                ..Default::default()
            },
        );
        cfg.providers.insert(
            "openrouter".to_string(),
            CustomProviderConfig {
                base_url: Some(or_url),
                api_key: Some("or-secret".to_string()),
                ..Default::default()
            },
        );

        let reg = ProviderRegistry::from_config_with_env(&cfg, empty_env);
        let out = reg
            .generate(&client, "venice", "llama-3.3-70b", "hi")
            .await
            .unwrap();
        assert_eq!(out.text, "hello from venice");
        assert_eq!(out.prompt_tokens, 10);
        assert_eq!(out.completion_tokens, 5);
        let req = venice_rx.await.unwrap();
        assert!(req.contains("POST /chat/completions "));
        assert!(req
            .to_ascii_lowercase()
            .contains("authorization: bearer venice-secret"));

        let mut stream = reg
            .generate_stream(&client, "openrouter", "anthropic/claude-3.5-sonnet", "hi")
            .await
            .unwrap();
        let mut collected = String::new();
        while let Some(chunk) = stream.next().await {
            collected.push_str(&chunk.unwrap());
        }
        assert_eq!(collected, "hello");
        let or_req = or_rx.await.unwrap().to_ascii_lowercase();
        assert!(or_req.contains("http-referer: https://mycel.os"));
        assert!(or_req.contains("x-title: mycel os"));

        // 2. OpenAI Responses (openai-codex) JSON + SSE
        let (resp_url, resp_rx) = spawn_mock_server(
            "application/json",
            r#"{"output_text":"responses ok","usage":{"input_tokens":7,"output_tokens":3}}"#,
        )
        .await;
        let (resp_sse_url, _) = spawn_mock_server(
            "text/event-stream",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"stream \"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
        )
        .await;
        cfg.providers.insert(
            "openai-codex".to_string(),
            CustomProviderConfig {
                base_url: Some(resp_url),
                api_key: Some("codex-key".to_string()),
                ..Default::default()
            },
        );
        let reg2 = ProviderRegistry::from_config_with_env(&cfg, empty_env);
        let out2 = reg2
            .generate(&client, "openai-codex", "gpt-5.1-codex", "hi")
            .await
            .unwrap();
        assert_eq!(out2.text, "responses ok");
        assert!(resp_rx.await.unwrap().contains("POST /responses "));

        cfg.providers.insert(
            "openai-codex".to_string(),
            CustomProviderConfig {
                base_url: Some(resp_sse_url),
                api_key: Some("codex-key".to_string()),
                ..Default::default()
            },
        );
        let reg2s = ProviderRegistry::from_config_with_env(&cfg, empty_env);
        let mut s2 = reg2s
            .generate_stream(&client, "openai-codex", "gpt-5.1-codex", "hi")
            .await
            .unwrap();
        let mut c2 = String::new();
        while let Some(ch) = s2.next().await {
            c2.push_str(&ch.unwrap());
        }
        assert_eq!(c2, "stream ok");

        // 3. Anthropic Messages JSON + SSE
        let (anth_url, anth_rx) = spawn_mock_server(
            "application/json",
            r#"{"content":[{"type":"text","text":"claude ok"}],"usage":{"input_tokens":4,"output_tokens":2}}"#,
        )
        .await;
        let (anth_sse_url, _) = spawn_mock_server(
            "text/event-stream",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"clau\"}}\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"de\"}}\n\n",
        )
        .await;
        cfg.providers.insert(
            "anthropic".to_string(),
            CustomProviderConfig {
                base_url: Some(anth_url),
                api_key: Some("anth-key".to_string()),
                ..Default::default()
            },
        );
        let reg3 = ProviderRegistry::from_config_with_env(&cfg, empty_env);
        let out3 = reg3
            .generate(&client, "anthropic", "claude-sonnet-4-20250514", "hi")
            .await
            .unwrap();
        assert_eq!(out3.text, "claude ok");
        let anth_req = anth_rx.await.unwrap().to_ascii_lowercase();
        assert!(anth_req.contains("x-api-key: anth-key"));
        assert!(anth_req.contains("anthropic-version: 2023-06-01"));

        cfg.providers.insert(
            "anthropic".to_string(),
            CustomProviderConfig {
                base_url: Some(anth_sse_url),
                api_key: Some("anth-key".to_string()),
                ..Default::default()
            },
        );
        let reg3s = ProviderRegistry::from_config_with_env(&cfg, empty_env);
        let mut s3 = reg3s
            .generate_stream(&client, "anthropic", "claude-sonnet-4-20250514", "hi")
            .await
            .unwrap();
        let mut c3 = String::new();
        while let Some(ch) = s3.next().await {
            c3.push_str(&ch.unwrap());
        }
        assert_eq!(c3, "claude");

        // 4. Google Generative AI JSON + SSE
        let (goog_url, goog_rx) = spawn_mock_server(
            "application/json",
            r#"{"candidates":[{"content":{"parts":[{"text":"gemini ok"}]}}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3}}"#,
        )
        .await;
        let (goog_sse_url, _) = spawn_mock_server(
            "text/event-stream",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"gem\"}]}}]}\n\ndata: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ini\"}]}}]}\n\n",
        )
        .await;
        cfg.providers.insert(
            "google".to_string(),
            CustomProviderConfig {
                base_url: Some(goog_url),
                api_key: Some("gem-key".to_string()),
                ..Default::default()
            },
        );
        let reg4 = ProviderRegistry::from_config_with_env(&cfg, empty_env);
        let out4 = reg4
            .generate(&client, "google", "gemini-2.5-flash", "hi")
            .await
            .unwrap();
        assert_eq!(out4.text, "gemini ok");
        let goog_req = goog_rx.await.unwrap().to_ascii_lowercase();
        assert!(goog_req.contains("x-goog-api-key: gem-key"));

        cfg.providers.insert(
            "google".to_string(),
            CustomProviderConfig {
                base_url: Some(goog_sse_url),
                api_key: Some("gem-key".to_string()),
                ..Default::default()
            },
        );
        let reg4s = ProviderRegistry::from_config_with_env(&cfg, empty_env);
        let mut s4 = reg4s
            .generate_stream(&client, "google", "gemini-2.5-flash", "hi")
            .await
            .unwrap();
        let mut c4 = String::new();
        while let Some(ch) = s4.next().await {
            c4.push_str(&ch.unwrap());
        }
        assert_eq!(c4, "gemini");

        // 5. Bedrock Converse (Bearer + SigV4)
        let (bed_bearer_url, bed_bearer_rx) = spawn_mock_server(
            "application/json",
            r#"{"output":{"message":{"content":[{"text":"bedrock bearer ok"}]}},"usage":{"inputTokens":6,"outputTokens":4}}"#,
        )
        .await;
        cfg.providers.insert(
            "amazon-bedrock".to_string(),
            CustomProviderConfig {
                base_url: Some(bed_bearer_url),
                ..Default::default()
            },
        );
        let reg5b = ProviderRegistry::from_config_with_env(
            &cfg,
            map_env(&[("AWS_BEARER_TOKEN_BEDROCK", "bed-tok")]),
        );
        let out5b = reg5b
            .generate(
                &client,
                "amazon-bedrock",
                "anthropic.claude-3-5-sonnet-20241022-v2:0",
                "hi",
            )
            .await
            .unwrap();
        assert_eq!(out5b.text, "bedrock bearer ok");
        assert!(bed_bearer_rx
            .await
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer bed-tok"));

        let (bed_sig_url, bed_sig_rx) = spawn_mock_server(
            "application/json",
            r#"{"output":{"message":{"content":[{"text":"bedrock sigv4 ok"}]}},"usage":{"inputTokens":6,"outputTokens":4}}"#,
        )
        .await;
        cfg.providers.insert(
            "amazon-bedrock".to_string(),
            CustomProviderConfig {
                base_url: Some(bed_sig_url),
                ..Default::default()
            },
        );
        let reg5s = ProviderRegistry::from_config_with_env(
            &cfg,
            map_env(&[
                ("AWS_ACCESS_KEY_ID", "AKIATEST"),
                ("AWS_SECRET_ACCESS_KEY", "SECRETTEST"),
                ("AWS_REGION", "us-east-1"),
            ]),
        );
        let mut s5 = reg5s
            .generate_stream(
                &client,
                "amazon-bedrock",
                "anthropic.claude-3-5-sonnet-20241022-v2:0",
                "hi",
            )
            .await
            .unwrap();
        let mut c5 = String::new();
        while let Some(ch) = s5.next().await {
            c5.push_str(&ch.unwrap());
        }
        assert_eq!(c5, "bedrock sigv4 ok");
        let sig_req = bed_sig_rx.await.unwrap();
        assert!(sig_req.contains("AWS4-HMAC-SHA256"));
    }
}
