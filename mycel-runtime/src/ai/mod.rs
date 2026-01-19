//! AI Router - Manages local and cloud LLM communication
//!
//! The router decides when to use the local model vs escalating to cloud,
//! handles prompt construction, and manages model inference.

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::MycelConfig;
use crate::context::Context;
use crate::intent::{ActionType, Intent};

/// Main AI router that handles all LLM interactions
#[derive(Clone)]
pub struct AiRouter {
    config: MycelConfig,
    http_client: Client,
    local_available: bool,
}

impl AiRouter {
    /// Create a new AI router with both local and cloud capabilities
    pub async fn new(config: &MycelConfig) -> Result<Self> {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;

        // Check if local model (Ollama) is available
        let local_available = Self::check_local_availability(&http_client, config).await;

        if local_available {
            info!("Local LLM (Ollama) is available");
        } else {
            warn!("Local LLM not available, will use cloud-only mode");
        }

        Ok(Self {
            config: config.clone(),
            http_client,
            local_available,
        })
    }

    /// Create a cloud-only router
    pub async fn cloud_only(config: &MycelConfig) -> Result<Self> {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;

        Ok(Self {
            config: config.clone(),
            http_client,
            local_available: false,
        })
    }

    async fn check_local_availability(client: &Client, config: &MycelConfig) -> bool {
        let url = format!("{}/api/tags", config.ollama_url);
        client.get(&url).send().await.is_ok()
    }

    /// Parse user input into a structured intent
    pub async fn parse_intent(&self, input: &str, context: &Context) -> Result<Intent> {
        let prompt = format!(
            r#"You are an intent parser for Mycel OS, an AI-native operating system.
Analyze the user's input and determine what type of action is needed.

User input: "{}"

Current context:
- Working directory: {}
- Recent files: {:?}
- Time: {}

Respond with JSON only:
{{
    "action": "brief description of what to do",
    "action_type": "simple_response|generate_code|generate_ui|system_action|cloud_escalate",
    "confidence": 0.0-1.0,
    "parameters": {{}},
    "requires_cloud": true/false (set true for complex reasoning, analysis, or creative tasks)
}}

Guidelines:
- simple_response: Questions, explanations, simple information
- generate_code: User wants something computed, automated, or transformed
- generate_ui: User needs a visual interface, comparison view, or interactive element
- system_action: File operations, settings changes, system commands
- cloud_escalate: Complex analysis, nuanced writing, or when uncertain"#,
            input,
            context.working_directory,
            context.recent_files,
            context.timestamp
        );

        let response = self.local_generate(&prompt).await?;
        
        // Parse the JSON response
        let intent: IntentResponse = serde_json::from_str(&response)
            .map_err(|e| anyhow!("Failed to parse intent response: {} - Response was: {}", e, response))?;

        Ok(Intent {
            action: intent.action,
            action_type: match intent.action_type.as_str() {
                "simple_response" => ActionType::SimpleResponse,
                "generate_code" => ActionType::GenerateCode,
                "generate_ui" => ActionType::GenerateUi,
                "system_action" => ActionType::SystemAction,
                "cloud_escalate" => ActionType::CloudEscalate,
                _ => ActionType::SimpleResponse,
            },
            confidence: intent.confidence,
            parameters: intent.parameters,
            requires_cloud: intent.requires_cloud,
        })
    }

    /// Generate a simple text response
    pub async fn generate_response(&self, input: &str, context: &Context) -> Result<String> {
        let prompt = format!(
            r#"You are Mycel, the AI assistant embedded in Mycel OS.
You are helpful, concise, and focused on getting things done.

Context:
- User's working directory: {}
- Recent activity: {:?}

User says: {}

Respond naturally and helpfully. If you need to take action, describe what you would do.
Keep responses concise unless detail is specifically requested."#,
            context.working_directory, context.recent_files, input
        );

        self.smart_generate(&prompt, false).await
    }

    /// Generate code to accomplish a task
    pub async fn generate_code(&self, intent: &Intent, context: &Context) -> Result<String> {
        let prompt = format!(
            r#"You are a code generator for Mycel OS.
Generate Python code to accomplish the following task.

Task: {}
Parameters: {:?}
Working directory: {}

Requirements:
1. Write clean, working Python code
2. Include all necessary imports
3. Handle errors gracefully
4. Print results to stdout
5. Do NOT use any dangerous operations (no rm -rf, no system modifications)

Respond with ONLY the Python code, no explanations or markdown."#,
            intent.action, intent.parameters, context.working_directory
        );

        // Code generation often benefits from cloud model
        self.smart_generate(&prompt, intent.requires_cloud).await
    }

    /// Generate a UI specification
    pub async fn generate_ui_spec(&self, intent: &Intent, context: &Context) -> Result<UiSpec> {
        let prompt = format!(
            r#"You are a UI generator for Mycel OS.
Generate a UI specification for the following need.

Need: {}
Parameters: {:?}
Context: Working in {}

Respond with JSON only:
{{
    "type": "html|react|native",
    "title": "surface title",
    "width": 800,
    "height": 600,
    "content": "the actual HTML/React/native code",
    "interactive": true/false,
    "data_bindings": []
}}"#,
            intent.action, intent.parameters, context.working_directory
        );

        let response = self.smart_generate(&prompt, true).await?;
        serde_json::from_str(&response).map_err(|e| anyhow!("Failed to parse UI spec: {}", e))
    }

    /// Request from cloud AI (for complex tasks)
    pub async fn cloud_request(&self, input: &str, context: &Context) -> Result<String> {
        let prompt = format!(
            r#"You are Mycel, an AI assistant embedded in Mycel OS.

Context:
- Working directory: {}
- Recent files: {:?}
- Timestamp: {}

User request: {}

Provide a thorough, helpful response. You have access to the user's full context and can suggest actions to take."#,
            context.working_directory, context.recent_files, context.timestamp, input
        );

        self.cloud_generate(&prompt).await
    }

    /// Smart routing between local and cloud
    async fn smart_generate(&self, prompt: &str, prefer_cloud: bool) -> Result<String> {
        if prefer_cloud || !self.local_available {
            // Try cloud first
            match self.cloud_generate(prompt).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    warn!("Cloud generation failed, falling back to local: {}", e);
                }
            }
        }

        // Use local
        if self.local_available {
            self.local_generate(prompt).await
        } else {
            Err(anyhow!("No AI backend available"))
        }
    }

    /// Generate using local Ollama
    async fn local_generate(&self, prompt: &str) -> Result<String> {
        debug!("Generating with local LLM");

        let request = OllamaRequest {
            model: self.config.local_model.clone(),
            prompt: prompt.to_string(),
            stream: false,
        };

        let url = format!("{}/api/generate", self.config.ollama_url);
        let response = self
            .http_client
            .post(&url)
            .json(&request)
            .send()
            .await?
            .json::<OllamaResponse>()
            .await?;

        Ok(response.response)
    }

    /// Generate using cloud API (Anthropic Claude)
    async fn cloud_generate(&self, prompt: &str) -> Result<String> {
        debug!("Generating with cloud LLM");

        if self.config.anthropic_api_key.is_empty() {
            return Err(anyhow!("Anthropic API key not configured"));
        }

        let request = AnthropicRequest {
            model: self.config.cloud_model.clone(),
            max_tokens: 4096,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
        };

        let response = self
            .http_client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.config.anthropic_api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(anyhow!("Anthropic API error: {}", error_text));
        }

        let response: AnthropicResponse = response.json().await?;
        
        response
            .content
            .first()
            .map(|c| c.text.clone())
            .ok_or_else(|| anyhow!("Empty response from Anthropic"))
    }
}

// Request/Response types for Ollama
#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    prompt: String,
    stream: bool,
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: String,
}

// Request/Response types for Anthropic
#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    text: String,
}

// Intent parsing response
#[derive(Deserialize)]
struct IntentResponse {
    action: String,
    action_type: String,
    confidence: f32,
    #[serde(default)]
    parameters: serde_json::Value,
    #[serde(default)]
    requires_cloud: bool,
}

/// UI specification for surface generation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiSpec {
    #[serde(rename = "type")]
    pub ui_type: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub content: String,
    pub interactive: bool,
    #[serde(default)]
    pub data_bindings: Vec<String>,
}
