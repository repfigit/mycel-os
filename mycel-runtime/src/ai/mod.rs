//! AI Router - Manages local and cloud LLM communication
//!
//! The router decides when to use the local model vs escalating to cloud,
//! handles prompt construction, and manages model inference.

use anyhow::{anyhow, Result};
use futures::Stream;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::config::{LocalBackend, MycelConfig};
use crate::context::Context;

pub mod providers;
pub mod router;
pub mod spend;
use crate::intent::{ActionType, Intent};
use crate::mcp::{self, McpManager};
use router::{looks_inadequate, AutoRouter, Route};
use spend::SpendTracker;

/// Strip markdown code blocks and extract JSON from a string
/// Handles cases like: ```json\n{...}\n``` or just ```\n{...}\n```
fn strip_markdown_code_blocks(text: &str) -> String {
    let mut cleaned = text.trim().to_string();

    // Remove opening ```json or ``` (with optional language tag)
    if cleaned.starts_with("```") {
        // Find the first newline after ```
        if let Some(newline_pos) = cleaned.find('\n') {
            cleaned = cleaned[newline_pos + 1..].to_string();
        } else {
            // No newline, just remove ```
            cleaned = cleaned[3..].to_string();
        }
    }

    // Remove closing ``` (may be on same line or separate line)
    if cleaned.ends_with("```") {
        let len = cleaned.len();
        cleaned = cleaned[..len - 3].to_string();
    }

    // Also check for ``` on its own line at the end
    if cleaned.ends_with("\n```") {
        let len = cleaned.len();
        cleaned = cleaned[..len - 4].to_string();
    }

    // Try to extract JSON object if there's extra text
    // Look for first { and last }
    if let Some(start) = cleaned.find('{') {
        if let Some(end) = cleaned.rfind('}') {
            if end > start {
                cleaned = cleaned[start..=end].to_string();
            }
        }
    }

    cleaned.trim().to_string()
}

/// Strip markdown formatting from plain text responses
/// Removes markdown code blocks, bold/italic markers, headers, etc.
fn strip_markdown_formatting(text: &str) -> String {
    let mut cleaned = text.trim().to_string();

    // Remove code blocks (```...```)
    cleaned = strip_markdown_code_blocks(&cleaned);

    // Remove markdown headers (# ## ### etc.) - remove # and following space
    let lines: Vec<&str> = cleaned.lines().collect();
    let cleaned_lines: Vec<String> = lines
        .iter()
        .map(|line| {
            let mut l = line.trim().to_string();
            // Remove header markers
            while l.starts_with('#') {
                l = l[1..].trim().to_string();
            }
            l
        })
        .collect();
    cleaned = cleaned_lines.join("\n");

    // Remove bold markers (**text** -> text)
    cleaned = cleaned.replace("**", "");

    // Remove italic markers (*text* -> text) but preserve asterisks in lists
    // Simple approach: replace standalone * with nothing, but be careful
    // For now, just remove double asterisks (bold) which we already did

    // Remove markdown links [text](url) -> text
    // Use a simple approach: find [text](url) patterns
    let mut result = String::new();
    let chars: Vec<char> = cleaned.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            // Look for matching ]
            let link_start = i + 1;
            let mut link_end = None;
            for (j, &c) in chars.iter().enumerate().skip(i + 1) {
                if c == ']' {
                    link_end = Some(j);
                    break;
                }
            }
            if let Some(end) = link_end {
                // Check if next char is (
                if end + 1 < chars.len() && chars[end + 1] == '(' {
                    // Find matching )
                    let mut url_end = None;
                    for (k, &c) in chars.iter().enumerate().skip(end + 2) {
                        if c == ')' {
                            url_end = Some(k);
                            break;
                        }
                    }
                    if let Some(url_end_pos) = url_end {
                        // Extract link text
                        let link_text: String = chars[link_start..end].iter().collect();
                        result.push_str(&link_text);
                        i = url_end_pos + 1;
                        continue;
                    }
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }

    result.trim().to_string()
}

/// Which local LLM backend is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveLocalBackend {
    Ollama,
    LlamaCpp,
}

impl ActiveLocalBackend {
    fn as_str(self) -> &'static str {
        match self {
            ActiveLocalBackend::Ollama => "ollama",
            ActiveLocalBackend::LlamaCpp => "llama-cpp",
        }
    }
}

/// Main AI router that handles all LLM interactions
#[derive(Clone)]
pub struct AiRouter {
    config: MycelConfig,
    http_client: Client,
    active_local: Option<ActiveLocalBackend>,
    spend_tracker: SpendTracker,
    provider_registry: Arc<providers::ProviderRegistry>,
    active_cloud: Arc<RwLock<Option<(String, String)>>>,
}

use std::pin::Pin;

#[derive(Deserialize)]
struct OllamaStreamResponse {
    response: Option<String>,
    #[allow(dead_code)]
    done: bool,
    error: Option<String>,
}

impl AiRouter {
    /// Create a new AI router with both local and cloud capabilities
    pub async fn new(config: &MycelConfig) -> Result<Self> {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(300)) // 5 min for slow CPU inference
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()?;

        // Short-timeout probe client so unreachable endpoints on offline VMs
        // fail immediately instead of stalling startup.
        let probe_client = Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .connect_timeout(std::time::Duration::from_secs(2))
            .build()?;

        let active_local = match config.local_backend {
            LocalBackend::Ollama => {
                if Self::probe_ollama(&probe_client, &config.ollama_url).await {
                    Some(ActiveLocalBackend::Ollama)
                } else {
                    info!("Ollama not running, attempting to start...");
                    if Self::try_start_ollama().await
                        && Self::probe_ollama(&probe_client, &config.ollama_url).await
                    {
                        Some(ActiveLocalBackend::Ollama)
                    } else {
                        None
                    }
                }
            }
            LocalBackend::LlamaCpp => {
                if Self::probe_llama_cpp(&probe_client, &config.llama_cpp_url).await {
                    Some(ActiveLocalBackend::LlamaCpp)
                } else {
                    None
                }
            }
            LocalBackend::Auto => {
                if Self::probe_llama_cpp(&probe_client, &config.llama_cpp_url).await {
                    Some(ActiveLocalBackend::LlamaCpp)
                } else if Self::probe_ollama(&probe_client, &config.ollama_url).await {
                    Some(ActiveLocalBackend::Ollama)
                } else {
                    info!("Ollama not running, attempting to start...");
                    if Self::try_start_ollama().await
                        && Self::probe_ollama(&probe_client, &config.ollama_url).await
                    {
                        Some(ActiveLocalBackend::Ollama)
                    } else {
                        None
                    }
                }
            }
        };

        if let Some(backend) = active_local {
            info!(
                "🧠 Local LLM online ({}) - this is the kernel's brain",
                backend.as_str()
            );
        } else {
            warn!("⚠️  Local LLM not available! Running in degraded cloud-only mode. Start llama.cpp or Ollama for full capability.");
        }

        let spend_tracker = SpendTracker::load(Self::spend_path(config)).await;
        let provider_registry = Arc::new(providers::ProviderRegistry::from_config(config));
        let active_cloud = Arc::new(RwLock::new(provider_registry.auto_select()));

        let router = Self {
            config: config.clone(),
            http_client,
            active_local,
            spend_tracker,
            provider_registry,
            active_cloud,
        };

        // Preload the model in the background so the first user request
        // doesn't pay the cold-start model-load cost (tens of seconds on CPU).
        router.spawn_warmup();

        Ok(router)
    }

    /// Path to the persistent spend record, alongside other runtime state.
    fn spend_path(config: &MycelConfig) -> std::path::PathBuf {
        std::path::PathBuf::from(&config.context_path).join("cloud_spend.json")
    }

    /// Spawn a background task that preloads the local model into memory and
    /// pins it with keep_alive so subsequent requests are fast.
    fn spawn_warmup(&self) {
        if self.active_local != Some(ActiveLocalBackend::Ollama) {
            return;
        }
        let client = self.http_client.clone();
        let url = format!(
            "{}/api/generate",
            self.config.ollama_url.trim_end_matches('/')
        );
        let model = self.config.local_model.clone();
        tokio::spawn(async move {
            debug!("Warming up local model '{}'...", model);
            let start = std::time::Instant::now();
            let body = serde_json::json!({
                "model": model,
                "prompt": "hi",
                "stream": false,
                "think": false,
                // Pin the model in memory for 30 minutes after load.
                "keep_alive": "30m",
                "options": {"num_predict": 1}
            });
            match client.post(&url).json(&body).send().await {
                Ok(_) => info!("Local model '{}' warmed up in {:?}", model, start.elapsed()),
                Err(e) => debug!("Model warmup failed (non-fatal): {}", e),
            }
        });
    }

    /// Create a cloud-only router
    pub async fn cloud_only(config: &MycelConfig) -> Result<Self> {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(300)) // 5 min for slow CPU inference
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()?;

        let provider_registry = Arc::new(providers::ProviderRegistry::from_config(config));
        let active_cloud = Arc::new(RwLock::new(provider_registry.auto_select()));

        Ok(Self {
            config: config.clone(),
            http_client,
            active_local: None,
            spend_tracker: SpendTracker::load(Self::spend_path(config)).await,
            provider_registry,
            active_cloud,
        })
    }

    async fn probe_ollama(client: &Client, base_url: &str) -> bool {
        let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
        match client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    async fn probe_llama_cpp(client: &Client, base_url: &str) -> bool {
        let base = base_url.trim_end_matches('/');
        let models_url = format!("{}/models", base);
        if let Ok(resp) = client.get(&models_url).send().await {
            if resp.status().is_success() {
                return true;
            }
        }
        let origin = base
            .strip_suffix("/v1")
            .unwrap_or(base)
            .trim_end_matches('/');
        let health_url = format!("{}/health", origin);
        match client.get(&health_url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    /// Try to start Ollama if it's not running
    async fn try_start_ollama() -> bool {
        // Check if ollama binary exists
        let ollama_exists = Command::new("which")
            .arg("ollama")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !ollama_exists {
            debug!("Ollama binary not found in PATH");
            return false;
        }

        info!("🚀 Starting Ollama service...");

        // Start ollama serve in background
        let child = Command::new("ollama")
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        match child {
            Ok(_) => {
                // Wait for Ollama to become available (up to 10 seconds)
                let client = Client::new();
                for i in 0..20 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    if client
                        .get("http://localhost:11434/api/tags")
                        .send()
                        .await
                        .is_ok()
                    {
                        info!("✅ Ollama started successfully (took {}ms)", (i + 1) * 500);
                        return true;
                    }
                }
                warn!("Ollama process started but API not responding after 10s");
                false
            }
            Err(e) => {
                warn!("Failed to start Ollama: {}", e);
                false
            }
        }
    }

    /// Smart routing for streaming
    pub async fn smart_generate_stream(
        &self,
        prompt: &str,
        force_cloud: bool,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        if self.is_local_available() && !force_cloud {
            match self.local_generate_stream(prompt).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    warn!("Local LLM streaming failed, escalating to cloud: {}", e);
                }
            }
        }

        let stream = self.cloud_generate_stream(prompt).await?;
        Ok(Box::pin(stream))
    }

    /// Process user input with MCP tools available and streaming final response
    pub async fn process_with_tools_stream(
        &self,
        input: &str,
        context: &Context,
        mcp_manager: &McpManager,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        // Get available tools
        let tools_prompt = mcp_manager.get_tools_prompt().await;

        // If no tools available, just stream directly
        if tools_prompt.is_empty() {
            return self
                .smart_generate_stream(&self.build_basic_prompt(input, context), false)
                .await;
        }

        // Build the enhanced prompt with tools, including recent history
        let history = Self::format_history(context);

        let prompt = format!(
            r#"You are Mycel OS - an AI-native operating system assistant.

{tools_prompt}

WHEN TO USE TOOLS:
- Use tools proactively to get real data instead of guessing
- Use 'shell_command' for system commands, 'xbps_*' for packages
- Use 'system_info' for hardware/OS info

HOW TO RESPOND:
- Be helpful and specific with commands, paths, details
- After tool results, summarize what you found
- For simple questions, answer directly without tools

cwd: {cwd}{history}
user: {input}

Reply (use <tool_call>{{...}}</tool_call> for tools):"#,
            tools_prompt = tools_prompt,
            cwd = context.working_directory,
            history = history,
            input = input
        );

        // Get initial response from LLM (non-streaming for tool handling)
        let response = self.smart_generate(&prompt, false).await?;

        // Parse for tool calls
        let parsed = mcp::parse_tool_calls(&response);

        if !parsed.has_tool_calls() {
            // No tool calls - stream the response directly
            let cleaned = strip_markdown_formatting(&response);
            return Ok(Box::pin(futures::stream::once(futures::future::ready(Ok(
                cleaned,
            )))));
        }

        // Built-in shell execution: emit the #!exec marker as a single-chunk
        // stream. The IPC layer detects the marker in the accumulated response
        // and routes it through the policy-gated executor.
        if let Some(call) = parsed.tool_calls.iter().find(|c| c.name == "shell_execute") {
            if let Some(cmd) = call.arguments.get("command").and_then(|v| v.as_str()) {
                let marker = format!("#!exec\n{}", cmd);
                return Ok(Box::pin(futures::stream::once(futures::future::ready(Ok(
                    marker,
                )))));
            }
        }

        // Process tool calls
        let mut tool_results = Vec::new();
        for call in &parsed.tool_calls {
            if mcp_manager.requires_confirmation(&call.name).await {
                tool_results.push(format!("Tool '{}' requires confirmation.", call.name));
            } else {
                match mcp_manager.process_tool_call(call).await {
                    Ok(result) => tool_results.push(result),
                    Err(e) => tool_results.push(format!("Tool error: {}", e)),
                }
            }
        }

        // Build continuation prompt with tool results
        let continuation_prompt = format!(
            r#"User asked: {}

Tool results:
{}

Provide a helpful response based on these results. Include relevant details, commands, or next steps."#,
            input,
            tool_results.join("\n\n")
        );

        // Stream the final response
        self.smart_generate_stream(&continuation_prompt, false)
            .await
    }

    /// Generate using the active local LLM backend with streaming.
    async fn local_generate_stream(
        &self,
        prompt: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        match self.active_local {
            Some(ActiveLocalBackend::LlamaCpp) => self.llama_cpp_generate_stream(prompt).await,
            Some(ActiveLocalBackend::Ollama) => self.ollama_generate_stream(prompt).await,
            None => Err(anyhow!("Local LLM is not available")),
        }
    }

    async fn ollama_generate_stream(
        &self,
        prompt: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        debug!("🧠 Streaming with local Ollama (kernel brain)");

        let request = OllamaRequest {
            model: self.config.local_model.clone(),
            prompt: prompt.to_string(),
            stream: true,
            think: false,
            options: OllamaOptions { num_predict: 2048 },
        };

        let url = format!(
            "{}/api/generate",
            self.config.ollama_url.trim_end_matches('/')
        );
        let response = self.http_client.post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(anyhow!("Ollama API error: {}", error_text));
        }

        let stream = response.bytes_stream().map(|result| {
            match result {
                Ok(bytes) => {
                    // Ollama sends multiple JSON objects, one per line
                    let text = String::from_utf8_lossy(&bytes);
                    let mut combined = String::new();
                    for line in text.lines() {
                        if let Ok(chunk) = serde_json::from_str::<OllamaStreamResponse>(line) {
                            if let Some(err) = chunk.error {
                                return Err(anyhow!("Ollama error: {}", err));
                            }
                            if let Some(resp) = chunk.response {
                                combined.push_str(&resp);
                            }
                        }
                    }
                    Ok(combined)
                }
                Err(e) => Err(anyhow!("Stream error: {}", e)),
            }
        });

        Ok(Box::pin(stream))
    }

    async fn llama_cpp_generate_stream(
        &self,
        prompt: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        debug!("🧠 Streaming with local llama.cpp (kernel brain)");

        let body = serde_json::json!({
            "model": self.config.local_model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": self.config.local_max_tokens,
            "stream": true,
        });

        let url = format!(
            "{}/chat/completions",
            self.config.llama_cpp_url.trim_end_matches('/')
        );
        let response = self.http_client.post(&url).json(&body).send().await?;
        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("llama.cpp API error ({}): {}", status, error_text));
        }

        struct SseState<S> {
            inner: S,
            buf: Vec<u8>,
            pending: std::collections::VecDeque<String>,
            done: bool,
        }

        let state = SseState {
            inner: response.bytes_stream(),
            buf: Vec::new(),
            pending: std::collections::VecDeque::new(),
            done: false,
        };

        let stream = futures::stream::unfold(state, |mut st| async move {
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
                        if let Some(delta) = extract_llama_cpp_sse_delta(data) {
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
                    Some(Ok(bytes)) => st.buf.extend_from_slice(&bytes),
                    Some(Err(e)) => {
                        st.done = true;
                        return Some((Err(anyhow!("llama.cpp stream error: {}", e)), st));
                    }
                    None => {
                        st.done = true;
                        if !st.buf.is_empty() {
                            let line = String::from_utf8_lossy(&st.buf).to_string();
                            st.buf.clear();
                            if let Some(data) =
                                line.trim_end_matches(['\r', '\n']).strip_prefix("data:")
                            {
                                let data = data.trim_start();
                                if data != "[DONE]" {
                                    if let Some(delta) = extract_llama_cpp_sse_delta(data) {
                                        return Some((Ok(delta), st));
                                    }
                                }
                            }
                        }
                        return None;
                    }
                }
            }
        });

        Ok(Box::pin(stream))
    }

    /// Generate using cloud API with streaming
    async fn cloud_generate_stream(
        &self,
        prompt: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        let (provider_id, model) = self
            .active_cloud_selection()
            .await
            .ok_or_else(|| anyhow!("No cloud provider configured"))?;

        debug!("☁️  Streaming with cloud LLM: {}/{}", provider_id, model);
        self.provider_registry
            .generate_stream(&self.http_client, &provider_id, &model, prompt)
            .await
    }

    /// Render recent conversation history for injection into prompts.
    /// Caps at the last 10 turns to bound prompt size on small local models.
    fn format_history(context: &Context) -> String {
        if context.conversation_history.is_empty() {
            return String::new();
        }
        let turns: Vec<String> = context
            .conversation_history
            .iter()
            .rev()
            .take(10)
            .rev()
            .map(|t| format!("User: {}\nAssistant: {}", t.user, t.assistant))
            .collect();
        format!("\n\nConversation so far:\n{}", turns.join("\n"))
    }

    pub fn build_basic_prompt(&self, input: &str, context: &Context) -> String {
        let history = Self::format_history(context);

        format!(
            r#"You are Mycel OS, an AI assistant. Answer the user's question or help with their task.

Current directory: {}{}
User: {}

Respond directly and helpfully:"#,
            context.working_directory, history, input
        )
    }

    /// Main interface - processes user input (legacy non-streaming)
    pub async fn process(&self, input: &str, context: &Context) -> Result<String> {
        // 1. Parse the user's intent using the AI
        // This ensures the AI understands the request rather than relying on brittle string matching
        let intent = self.parse_intent(input, context).await?;

        info!(
            action = %intent.action,
            action_type = ?intent.action_type,
            confidence = %intent.confidence,
            "Parsed user intent"
        );

        // 2. Act based on the intent type
        match intent.action_type {
            ActionType::SimpleResponse => {
                // Just talk back to the user
                self.generate_response(input, context).await
            }

            ActionType::SystemAction | ActionType::GenerateCode => {
                // Generate code to fulfill the request
                let code = self.generate_code(&intent, context).await?;
                // Format for the runtime to execute
                Ok(format!("#!exec\n{}", code))
            }

            ActionType::GenerateUi => {
                // For now, UI generation falls back to code/text,
                // but in the future this would return a UI spec
                self.generate_response(input, context).await
            }

            ActionType::CloudEscalate => {
                // Explicitly route to cloud for complex tasks
                self.cloud_request(input, context).await
            }
        }
    }

    /// Process user input with MCP tools available
    /// This method injects available tools into the prompt and handles tool calls
    pub async fn process_with_tools(
        &self,
        input: &str,
        context: &Context,
        mcp_manager: &McpManager,
    ) -> Result<String> {
        // Get available tools
        let tools_prompt = mcp_manager.get_tools_prompt().await;

        // If no tools available, fall back to regular processing
        if tools_prompt.is_empty() {
            return self.process(input, context).await;
        }

        // Build the enhanced prompt with tools, including recent history
        let history = Self::format_history(context);

        let prompt = format!(
            r#"You are Mycel OS, an AI assistant with system access.

{tools_prompt}

IMPORTANT:
- For simple questions (jokes, explanations, chat), just answer directly - no tools needed.
- Use tools only when the user asks for system info, file operations, or commands.
- Be concise and helpful.

Current directory: {cwd}{history}
User: {input}

Respond:"#,
            tools_prompt = tools_prompt,
            cwd = context.working_directory,
            history = history,
            input = input
        );

        // Get initial response from LLM via the auto-router (local vs cloud)
        let response = self.smart_generate_routed(&prompt, input, &history).await?;

        // Parse for tool calls
        let parsed = mcp::parse_tool_calls(&response);

        if !parsed.has_tool_calls() {
            // No tool calls - return the response directly
            return Ok(strip_markdown_formatting(&response));
        }

        // Built-in shell execution: route to the runtime's policy-gated
        // executor via the #!exec marker instead of running ungated here.
        if let Some(call) = parsed.tool_calls.iter().find(|c| c.name == "shell_execute") {
            if let Some(cmd) = call.arguments.get("command").and_then(|v| v.as_str()) {
                return Ok(format!("#!exec\n{}", cmd));
            }
        }

        // Process tool calls
        let mut tool_results = Vec::new();
        for call in &parsed.tool_calls {
            debug!(
                "Executing MCP tool: {} with args: {:?}",
                call.name, call.arguments
            );

            // Check if confirmation is required
            if mcp_manager.requires_confirmation(&call.name).await {
                // Return information about what would be done
                let tool_info = format!(
                    "Tool '{}' requires confirmation.\nArguments: {:?}\n\nTo proceed, confirm the action.",
                    call.name, call.arguments
                );
                tool_results.push(tool_info);
            } else {
                // Execute the tool
                match mcp_manager.process_tool_call(call).await {
                    Ok(result) => tool_results.push(result),
                    Err(e) => tool_results.push(format!("Tool '{}' error: {}", call.name, e)),
                }
            }
        }

        // Build continuation prompt with tool results
        let continuation_prompt = format!(
            r#"You are Mycel OS. Continue the conversation with tool results.

Previous context:
User asked: {}
You responded: {}

Tool results:
{}

Now provide a concise final response to the user based on these tool results. Be terse."#,
            input,
            parsed.prefix_text.trim(),
            tool_results.join("\n\n")
        );

        // Get final response
        let final_response = self.smart_generate(&continuation_prompt, false).await?;
        Ok(strip_markdown_formatting(&final_response))
    }

    /// Process with tools but allow multiple tool call rounds (agentic loop)
    pub async fn process_with_tools_loop(
        &self,
        input: &str,
        context: &Context,
        mcp_manager: &McpManager,
        max_iterations: usize,
    ) -> Result<String> {
        let tools_prompt = mcp_manager.get_tools_prompt().await;

        if tools_prompt.is_empty() {
            return self.process(input, context).await;
        }

        let mut conversation = format!(
            r#"You are Mycel OS. You ARE the operating system.

{tools_prompt}

EVOLUTION RULES:
- If the user asks for a capability you don't have, USE 'evolve_os_add_capability' to write a new MCP server.
- You can write servers in JavaScript (Node.js) or Python.
- Always provide complete, production-ready code for new servers.
- You can also publish these to the global registry using 'near_publish_capability'.

GENERAL RULES:
- TERSE responses only.
- Use tools when helpful.
- After tool results, either use another tool or give a final response.
- When done, just respond normally without tool calls.

cwd: {cwd}
user: {input}

Reply:"#,
            tools_prompt = tools_prompt,
            cwd = context.working_directory,
            input = input
        );

        for iteration in 0..max_iterations {
            let response = self.smart_generate(&conversation, false).await?;
            let parsed = mcp::parse_tool_calls(&response);

            if !parsed.has_tool_calls() {
                // No more tool calls - we're done
                return Ok(strip_markdown_formatting(&response));
            }

            // Built-in shell execution: route to the runtime's policy-gated
            // executor via the #!exec marker instead of running ungated here.
            if let Some(call) = parsed.tool_calls.iter().find(|c| c.name == "shell_execute") {
                if let Some(cmd) = call.arguments.get("command").and_then(|v| v.as_str()) {
                    return Ok(format!("#!exec\n{}", cmd));
                }
            }

            debug!(
                "Tool call iteration {}: {} calls",
                iteration + 1,
                parsed.tool_calls.len()
            );

            // Process all tool calls
            let mut tool_results = Vec::new();
            for call in &parsed.tool_calls {
                if mcp_manager.requires_confirmation(&call.name).await {
                    tool_results.push(format!(
                        "Tool '{}' requires user confirmation. Cannot proceed automatically.",
                        call.name
                    ));
                } else {
                    match mcp_manager.process_tool_call(call).await {
                        Ok(result) => tool_results.push(result),
                        Err(e) => tool_results.push(format!("Tool error: {}", e)),
                    }
                }
            }

            // Add to conversation
            conversation.push_str(&format!(
                "\n\nAssistant: {}\n\nTool results:\n{}\n\nContinue (use more tools or give final response):",
                parsed.prefix_text.trim(),
                tool_results.join("\n\n")
            ));
        }

        // Max iterations reached
        warn!("MCP tool loop reached max iterations ({})", max_iterations);
        Ok("Max tool iterations reached. Please try a simpler query.".to_string())
    }

    /// Parse user input into a structured intent (legacy, kept for compatibility)
    pub async fn parse_intent(&self, input: &str, context: &Context) -> Result<Intent> {
        let prompt = format!(
            r#"Parse intent. Respond with JSON only, no other text.

input: "{}"
cwd: {}

JSON format:
{{"action":"what to do","action_type":"simple_response|generate_code|system_action","confidence":0.9,"parameters":{{}},"requires_cloud":false}}

action_type:
- simple_response: questions, info
- generate_code: compute, automate, transform
- system_action: files, commands
- cloud_escalate: complex analysis"#,
            input, context.working_directory
        );

        let response = self.smart_generate(&prompt, false).await?;
        let cleaned_response = strip_markdown_code_blocks(&response);

        // Parse JSON - if it fails, default to simple response (don't crash)
        match serde_json::from_str::<IntentResponse>(&cleaned_response) {
            Ok(intent) => Ok(Intent {
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
            }),
            Err(_) => {
                // LLM returned garbage - just treat as simple response
                Ok(Intent {
                    action: input.to_string(),
                    action_type: ActionType::SimpleResponse,
                    confidence: 0.5,
                    parameters: serde_json::Value::Null,
                    requires_cloud: false,
                })
            }
        }
    }

    /// Generate a simple text response
    pub async fn generate_response(&self, input: &str, context: &Context) -> Result<String> {
        let prompt = format!(
            r#"You are Mycel OS - an AI operating system assistant.

Your capabilities: run commands, file operations, package management, system info.

Be HELPFUL and SPECIFIC:
- For questions: give direct, informative answers
- For tasks: explain what command or action would accomplish it
- Include relevant file paths, commands, or configuration details
- If something needs clarification, ask

cwd: {}
user: {}

Reply:"#,
            context.working_directory, input
        );

        let response = self.smart_generate(&prompt, false).await?;
        // Strip any markdown formatting that might have been added
        Ok(strip_markdown_formatting(&response))
    }

    /// Generate code to accomplish a task
    pub async fn generate_code(&self, intent: &Intent, context: &Context) -> Result<String> {
        let prompt = format!(
            r#"You are the OS kernel. Generate code to execute the user's intent.

Task: {}
Current Directory: {}

Rules:
1. Choose the best language: Bash (for file/system ops) or Python (for logic/data).
2. Output ONLY the code. No markdown, no explanation.
3. If using Python, print results to stdout.
4. If using Bash, just write the command.
5. Be safe.

Code:"#,
            intent.action, context.working_directory
        );

        self.smart_generate(&prompt, intent.requires_cloud).await
    }

    /// Generate a UI specification
    pub async fn generate_ui_spec(&self, intent: &Intent, context: &Context) -> Result<UiSpec> {
        let prompt = format!(
            r#"Generate UI. JSON only, no text.

need: {}
cwd: {}

{{"type":"html","title":"...","width":800,"height":600,"content":"<html>...</html>","interactive":true,"data_bindings":[]}}"#,
            intent.action, context.working_directory
        );

        let response = self.smart_generate(&prompt, true).await?;
        let cleaned_response = strip_markdown_code_blocks(&response);
        serde_json::from_str(&cleaned_response)
            .map_err(|e| anyhow!("Failed to parse UI spec: {}", e))
    }

    /// Request from cloud AI (for complex tasks)
    pub async fn cloud_request(&self, input: &str, context: &Context) -> Result<String> {
        let prompt = format!(
            r#"You are Mycel OS. Terse responses only. No fluff.

cwd: {}
user: {}

Reply (1-2 sentences max):"#,
            context.working_directory, input
        );

        let response = self.cloud_generate(&prompt).await?;
        Ok(strip_markdown_formatting(&response))
    }

    /// Smart routing between local and cloud
    async fn smart_generate(&self, prompt: &str, force_cloud: bool) -> Result<String> {
        let start = std::time::Instant::now();

        // If prefer_cloud is set and we have a cloud API, use cloud first
        let has_cloud = self.has_cloud_api().await;
        let use_cloud_first = force_cloud || (self.config.prefer_cloud && has_cloud);

        info!(
            "AI routing: prefer_cloud={}, has_api={}, using_cloud={}",
            self.config.prefer_cloud, has_cloud, use_cloud_first
        );

        let result = if use_cloud_first {
            // Cloud first mode
            match self.cloud_generate(prompt).await {
                Ok(response) => Ok(response),
                Err(e) => {
                    if self.is_local_available() {
                        warn!("Cloud failed, falling back to local: {}", e);
                        self.local_generate(prompt).await
                    } else {
                        Err(e)
                    }
                }
            }
        } else {
            // Local first mode
            if self.is_local_available() {
                match self.local_generate(prompt).await {
                    Ok(response) => Ok(response),
                    Err(e) => {
                        warn!("Local LLM failed, escalating to cloud: {}", e);
                        self.cloud_generate(prompt).await
                    }
                }
            } else {
                self.cloud_generate(prompt).await
            }
        };

        let elapsed = start.elapsed();
        let source = if use_cloud_first { "cloud" } else { "local" };
        info!("AI response time: {:?} ({})", elapsed, source);

        result
    }

    /// Routed generation: decide local-vs-cloud up front with the decision
    /// list, then escalate on low-confidence local answers. `query` is the
    /// user's raw input, `context_text` the assembled prompt context (history,
    /// tools, cwd). `prompt` is the full text actually sent to the model.
    async fn smart_generate_routed(
        &self,
        prompt: &str,
        query: &str,
        context_text: &str,
    ) -> Result<String> {
        let start = std::time::Instant::now();
        let router = AutoRouter::new(
            self.config.router.clone(),
            self.is_local_available(),
            self.has_cloud_api().await,
        );
        let spent_today = self.spend_tracker.spent_today_usd().await;
        let route = router.route(query, context_text, spent_today);

        info!(
            "Auto-router decision: {:?} (cloud spend today: ${:.4})",
            route, spent_today
        );

        let result = match route {
            Route::Cloud => self.cloud_generate(prompt).await,
            Route::LocalOnly => self.local_generate(prompt).await,
            Route::LocalWithEscalation => match self.local_generate(prompt).await {
                Ok(answer) => {
                    if looks_inadequate(&answer) && self.has_cloud_api().await {
                        info!("Local answer inadequate, escalating to cloud");
                        match self.cloud_generate(prompt).await {
                            Ok(cloud_answer) => Ok(cloud_answer),
                            Err(e) => {
                                warn!("Cloud escalation failed, using local answer: {}", e);
                                Ok(answer)
                            }
                        }
                    } else {
                        Ok(answer)
                    }
                }
                Err(e) => {
                    if self.has_cloud_api().await {
                        warn!("Local LLM failed, escalating to cloud: {}", e);
                        self.cloud_generate(prompt).await
                    } else {
                        Err(e)
                    }
                }
            },
        };

        info!(
            "AI response time: {:?} (route: {:?})",
            start.elapsed(),
            route
        );
        result
    }

    /// Generate using the active local LLM backend - the primary brain of Mycel OS
    async fn local_generate(&self, prompt: &str) -> Result<String> {
        match self.active_local {
            Some(ActiveLocalBackend::LlamaCpp) => self.llama_cpp_generate(prompt).await,
            Some(ActiveLocalBackend::Ollama) => self.ollama_generate(prompt).await,
            None => Err(anyhow!("Local LLM is not available")),
        }
    }

    async fn ollama_generate(&self, prompt: &str) -> Result<String> {
        debug!("🧠 Generating with local Ollama (kernel brain)");

        let request = OllamaRequest {
            model: self.config.local_model.clone(),
            prompt: prompt.to_string(),
            stream: false,
            think: false,
            options: OllamaOptions { num_predict: 2048 },
        };

        let url = format!(
            "{}/api/generate",
            self.config.ollama_url.trim_end_matches('/')
        );
        let response = self.http_client.post(&url).json(&request).send().await?;

        // Save status code before consuming response
        let status = response.status();

        // Check HTTP status first
        if !status.is_success() {
            let error_text = response.text().await?;
            return Err(anyhow!("Ollama API error ({}): {}", status, error_text));
        }

        // Try to parse as success response
        let ollama_response: OllamaResponse = response.json().await?;

        // Check if it's an error response (Ollama sometimes returns 200 with error field)
        if let Some(error) = ollama_response.error {
            return Err(anyhow!("Ollama error: {}", error));
        }

        // Return the response text
        ollama_response
            .response
            .ok_or_else(|| anyhow!("Ollama returned empty response"))
    }

    async fn llama_cpp_generate(&self, prompt: &str) -> Result<String> {
        debug!("🧠 Generating with local llama.cpp (kernel brain)");

        let body = serde_json::json!({
            "model": self.config.local_model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": self.config.local_max_tokens,
            "stream": false,
        });

        let url = format!(
            "{}/chat/completions",
            self.config.llama_cpp_url.trim_end_matches('/')
        );
        let response = self.http_client.post(&url).json(&body).send().await?;
        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("llama.cpp API error ({}): {}", status, error_text));
        }

        let payload: serde_json::Value = response.json().await?;
        let content = payload
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|t| t.as_str())
            .map(str::to_string);

        match content {
            Some(text) if !text.is_empty() => Ok(text),
            Some(_) => Err(anyhow!("llama.cpp returned empty content")),
            None => Err(anyhow!(
                "llama.cpp response missing choices[0].message.content"
            )),
        }
    }

    /// Generate using the active cloud provider
    async fn cloud_generate(&self, prompt: &str) -> Result<String> {
        let (provider_id, model) = self
            .active_cloud_selection()
            .await
            .ok_or_else(|| {
                anyhow!(
                    "No cloud API configured. Set an API key (e.g. VENICE_API_KEY, OPENROUTER_API_KEY, ANTHROPIC_API_KEY, OPENAI_API_KEY) or configure [providers.<name>]."
                )
            })?;

        info!("☁️  Generating with cloud LLM: {}/{}", provider_id, model);
        let out = self
            .provider_registry
            .generate(&self.http_client, &provider_id, &model, prompt)
            .await?;

        // Record token usage for budget enforcement (best-effort).
        if out.prompt_tokens > 0 || out.completion_tokens > 0 {
            if let Err(e) = self
                .spend_tracker
                .record(&model, out.prompt_tokens, out.completion_tokens)
                .await
            {
                debug!("Failed to record cloud spend: {}", e);
            }
        }

        Ok(out.text)
    }

    /// Return the active `(provider_id, model)` cloud selection, if any.
    pub async fn active_cloud_selection(&self) -> Option<(String, String)> {
        self.active_cloud.read().await.clone()
    }

    /// List all known cloud providers without exposing any secrets.
    pub fn list_providers(&self) -> Vec<providers::ProviderSummary> {
        self.provider_registry.list_summaries()
    }

    /// Validate and switch the active cloud provider (and optional model) at runtime.
    pub async fn set_cloud_provider(
        &self,
        provider_or_spec: &str,
        model_override: Option<&str>,
    ) -> Result<(String, String)> {
        let selection = self
            .provider_registry
            .validate_selection(provider_or_spec, model_override)?;
        info!(
            "Active cloud provider set to {}/{}",
            selection.0, selection.1
        );
        *self.active_cloud.write().await = Some(selection.clone());
        Ok(selection)
    }

    /// Current cloud spend today (USD), for status reporting.
    pub async fn cloud_spend_today(&self) -> f64 {
        self.spend_tracker.spent_today_usd().await
    }

    /// Human-readable cloud spend summary for today.
    pub async fn cloud_spend_summary(&self) -> String {
        self.spend_tracker.today_summary().await
    }

    /// Generate with a specific provider (for IPC provider selection)
    pub async fn generate_with_provider(
        &self,
        prompt: &str,
        provider: crate::ipc::LlmProvider,
    ) -> Result<String> {
        use crate::ipc::LlmProvider;
        let start = std::time::Instant::now();

        let result = match provider {
            // Auto: use the decision-list router. The full prompt serves as
            // both query and context for sensitivity/size checks (more
            // conservative for privacy, which is the safe direction).
            LlmProvider::Auto => self.smart_generate_routed(prompt, prompt, "").await,
            LlmProvider::Local => {
                if !self.is_local_available() {
                    return Err(anyhow!("Local LLM is not available"));
                }
                self.local_generate(prompt).await
            }
            LlmProvider::Cloud => {
                if !self.has_cloud_api().await {
                    return Err(anyhow!(
                        "Cloud LLM is not configured. Set an API key or configure [providers.<name>]."
                    ));
                }
                self.cloud_generate(prompt).await
            }
        };

        let elapsed = start.elapsed();
        let source = match provider {
            LlmProvider::Auto => "auto",
            LlmProvider::Local => "local",
            LlmProvider::Cloud => "cloud",
        };
        info!("AI response time: {:?} ({})", elapsed, source);

        result
    }

    /// Process with tools using a specific provider
    pub async fn process_with_tools_provider(
        &self,
        input: &str,
        context: &Context,
        mcp_manager: &McpManager,
        provider: crate::ipc::LlmProvider,
    ) -> Result<String> {
        let tools_prompt = mcp_manager.get_tools_prompt().await;

        if tools_prompt.is_empty() {
            return self
                .generate_with_provider(&self.build_basic_prompt(input, context), provider)
                .await;
        }

        let history = Self::format_history(context);

        let prompt = format!(
            r#"You are Mycel OS. You ARE the operating system.

{tools_prompt}

RULES:
- TERSE responses only.
- Use tools when helpful for the task.
- For simple questions, just respond directly.
- After getting tool results, provide a final response.

cwd: {cwd}{history}
user: {input}

Reply:"#,
            tools_prompt = tools_prompt,
            cwd = context.working_directory,
            history = history,
            input = input
        );

        let response = self.generate_with_provider(&prompt, provider).await?;
        let parsed = mcp::parse_tool_calls(&response);

        if !parsed.has_tool_calls() {
            return Ok(strip_markdown_formatting(&response));
        }

        // Built-in shell execution: route to the runtime's policy-gated
        // executor via the #!exec marker instead of running ungated here.
        if let Some(call) = parsed.tool_calls.iter().find(|c| c.name == "shell_execute") {
            if let Some(cmd) = call.arguments.get("command").and_then(|v| v.as_str()) {
                return Ok(format!("#!exec\n{}", cmd));
            }
        }

        // Process tool calls
        let mut tool_results = Vec::new();
        for call in &parsed.tool_calls {
            if mcp_manager.requires_confirmation(&call.name).await {
                tool_results.push(format!("Tool '{}' requires user confirmation.", call.name));
            } else {
                match mcp_manager.process_tool_call(call).await {
                    Ok(result) => tool_results.push(result),
                    Err(e) => tool_results.push(format!("Tool error: {}", e)),
                }
            }
        }

        // Continuation prompt
        let continuation_prompt = format!(
            r#"Previous context:
User asked: {}
You responded: {}

Tool results:
{}

Provide a concise final response based on these tool results."#,
            input,
            parsed.prefix_text.trim(),
            tool_results.join("\n\n")
        );

        let final_response = self
            .generate_with_provider(&continuation_prompt, provider)
            .await?;
        Ok(strip_markdown_formatting(&final_response))
    }

    /// Check if local LLM is available
    pub fn is_local_available(&self) -> bool {
        self.active_local.is_some()
    }

    /// Name of the active local LLM backend, if any (`"ollama"` or `"llama-cpp"`).
    pub fn active_local_backend(&self) -> Option<&'static str> {
        self.active_local.map(ActiveLocalBackend::as_str)
    }

    /// Check if cloud API is available
    async fn has_cloud_api(&self) -> bool {
        self.active_cloud.read().await.is_some()
    }
}

fn extract_llama_cpp_sse_delta(data: &str) -> Option<String> {
    let val: serde_json::Value = serde_json::from_str(data).ok()?;
    val.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("delta"))
        .and_then(|d| d.get("content"))
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// Request/Response types for Ollama
#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    prompt: String,
    stream: bool,
    /// Disable qwen3-style "thinking" mode. Mycel is an OS assistant that
    /// needs direct, responsive answers — thinking burns the entire token
    /// budget on reasoning and can stall CPU inference for minutes.
    think: bool,
    options: OllamaOptions,
}

#[derive(Serialize)]
struct OllamaOptions {
    /// Cap generated tokens to prevent runaway responses on local models.
    num_predict: u32,
}

#[derive(Deserialize)]
struct OllamaResponse {
    #[serde(default)]
    response: Option<String>,
    #[serde(default)]
    error: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ollama_available() {
        // This test requires Ollama to be running.
        // We use a short timeout so tests don't hang if it's not.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .unwrap();

        let config = MycelConfig::default();
        let url = format!("{}/api/tags", config.ollama_url);

        match client.get(&url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    println!("Ollama is running!");
                } else {
                    println!("Ollama returned error: {}", resp.status());
                }
            }
            Err(e) => {
                println!("Ollama not available: {}", e);
                // We don't fail the test because CI environments might not have Ollama
            }
        }
    }

    async fn spawn_mock_llama_cpp_server(
        models_ok: bool,
        health_ok: bool,
    ) -> (std::net::SocketAddr, Arc<tokio::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded_paths = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recorded_clone = Arc::clone(&recorded_paths);

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let recorded = Arc::clone(&recorded_clone);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let header_end = loop {
                        let n = match socket.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let header_str = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let content_length = header_str
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            if k.eq_ignore_ascii_case("content-length") {
                                v.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    while buf.len() < header_end + content_length {
                        let n = match socket.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let req_line = header_str.lines().next().unwrap_or("");
                    let path = req_line.split_whitespace().nth(1).unwrap_or("");
                    let body_str = String::from_utf8_lossy(
                        &buf[header_end..buf.len().min(header_end + content_length)],
                    )
                    .to_string();
                    recorded.lock().await.push(path.to_string());

                    if path == "/v1/models" {
                        let status = if models_ok { "200 OK" } else { "404 Not Found" };
                        let body = if models_ok {
                            r#"{"data":[{"id":"qwen3:8b"}]}"#
                        } else {
                            r#"{"error":"not found"}"#
                        };
                        let resp = format!(
                            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            status,
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(resp.as_bytes()).await;
                    } else if path == "/health" {
                        let status = if health_ok {
                            "200 OK"
                        } else {
                            "503 Service Unavailable"
                        };
                        let body = if health_ok {
                            r#"{"status":"ok"}"#
                        } else {
                            r#"{"status":"unavailable"}"#
                        };
                        let resp = format!(
                            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            status,
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(resp.as_bytes()).await;
                    } else if path == "/v1/chat/completions" {
                        let is_stream = body_str.contains("\"stream\":true")
                            || body_str.contains("\"stream\": true");
                        if is_stream {
                            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
                            let _ = socket.write_all(headers.as_bytes()).await;
                            let _ = socket.flush().await;

                            // Intentionally split SSE line across two TCP writes to verify buffering.
                            let part1 = b"data: {\"choices\":[{\"delta\":{\"content\":\"hel";
                            let part2 = b"lo \"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"llama\"}}]}\n\ndata: [DONE]\n\n";
                            let _ = socket.write_all(part1).await;
                            let _ = socket.flush().await;
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            let _ = socket.write_all(part2).await;
                            let _ = socket.flush().await;
                        } else {
                            let body = r#"{"choices":[{"message":{"role":"assistant","content":"local-llamacpp-ok"}}]}"#;
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(resp.as_bytes()).await;
                        }
                    } else {
                        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = socket.write_all(resp.as_bytes()).await;
                    }
                });
            }
        });

        (addr, recorded_paths)
    }

    async fn closed_localhost_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{}", addr)
    }

    fn offline_test_config(
        backend: LocalBackend,
        llama_cpp_url: String,
        ollama_url: String,
    ) -> MycelConfig {
        MycelConfig {
            local_backend: backend,
            llama_cpp_url,
            ollama_url,
            openrouter_api_key: String::new(),
            cloud_provider: String::new(),
            providers: std::collections::HashMap::new(),
            prefer_cloud: false,
            context_path: std::env::temp_dir()
                .join(format!("mycel-test-{}", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .to_string(),
            ..MycelConfig::default()
        }
    }

    #[tokio::test]
    async fn test_llama_cpp_non_streaming_and_sse_streaming() {
        let (addr, recorded) = spawn_mock_llama_cpp_server(true, true).await;
        let closed_ollama = closed_localhost_url().await;
        let config = offline_test_config(
            LocalBackend::LlamaCpp,
            format!("http://{}/v1", addr),
            closed_ollama,
        );

        let router = AiRouter::new(&config).await.unwrap();
        assert!(router.is_local_available());
        assert_eq!(router.active_local_backend(), Some("llama-cpp"));

        let reply = router.local_generate("hello").await.unwrap();
        assert_eq!(reply, "local-llamacpp-ok");

        let via_provider = router
            .generate_with_provider("hello", crate::ipc::LlmProvider::Local)
            .await
            .unwrap();
        assert_eq!(via_provider, "local-llamacpp-ok");

        let mut stream = router.smart_generate_stream("hello", false).await.unwrap();
        let mut collected = String::new();
        while let Some(chunk) = stream.next().await {
            collected.push_str(&chunk.unwrap());
        }
        assert_eq!(collected, "hello llama");

        let paths = recorded.lock().await;
        assert!(paths.iter().any(|p| p == "/v1/models"));
        assert!(paths.iter().any(|p| p == "/v1/chat/completions"));
    }

    #[tokio::test]
    async fn test_auto_backend_falls_back_to_llama_cpp_health_when_ollama_unreachable() {
        // /v1/models returns 404, /health returns 200 -> probe_llama_cpp must succeed via /health.
        let (addr, recorded) = spawn_mock_llama_cpp_server(false, true).await;
        let closed_ollama = closed_localhost_url().await;
        let config = offline_test_config(
            LocalBackend::Auto,
            format!("http://{}/v1", addr),
            closed_ollama,
        );

        let router = AiRouter::new(&config).await.unwrap();
        assert!(router.is_local_available());
        assert_eq!(router.active_local_backend(), Some("llama-cpp"));

        let reply = router.local_generate("status").await.unwrap();
        assert_eq!(reply, "local-llamacpp-ok");

        let paths = recorded.lock().await;
        assert!(paths.iter().any(|p| p == "/v1/models"));
        assert!(paths.iter().any(|p| p == "/health"));
    }

    #[tokio::test]
    async fn test_privacy_sensitive_query_stays_local_on_llama_cpp() {
        let (addr, recorded) = spawn_mock_llama_cpp_server(true, true).await;
        let closed_ollama = closed_localhost_url().await;
        let config = offline_test_config(
            LocalBackend::Auto,
            format!("http://{}/v1", addr),
            closed_ollama,
        );

        let router = AiRouter::new(&config).await.unwrap();
        assert_eq!(router.active_local_backend(), Some("llama-cpp"));

        let reply = router
            .generate_with_provider(
                "Please inspect ~/.ssh/id_rsa permissions",
                crate::ipc::LlmProvider::Auto,
            )
            .await
            .unwrap();
        assert_eq!(reply, "local-llamacpp-ok");

        let paths = recorded.lock().await;
        assert!(paths.iter().any(|p| p == "/v1/chat/completions"));
    }
}
