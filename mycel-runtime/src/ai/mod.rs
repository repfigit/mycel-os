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
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::MycelConfig;
use crate::context::Context;
use crate::intent::{ActionType, Intent};
use crate::mcp::{self, McpManager};

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
            for j in (i + 1)..chars.len() {
                if chars[j] == ']' {
                    link_end = Some(j);
                    break;
                }
            }
            if let Some(end) = link_end {
                // Check if next char is (
                if end + 1 < chars.len() && chars[end + 1] == '(' {
                    // Find matching )
                    let mut url_end = None;
                    for k in (end + 2)..chars.len() {
                        if chars[k] == ')' {
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

/// Main AI router that handles all LLM interactions
#[derive(Clone)]
pub struct AiRouter {
    config: MycelConfig,
    http_client: Client,
    local_available: bool,
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

        // Check if local model (Ollama) is available
        let mut local_available = Self::check_local_availability(&http_client, config).await;

        // If not available, try to start it
        if !local_available {
            info!("Ollama not running, attempting to start...");
            if Self::try_start_ollama().await {
                local_available = Self::check_local_availability(&http_client, config).await;
            }
        }

        if local_available {
            info!("🧠 Local LLM online - this is the kernel's brain");
        } else {
            warn!("⚠️  Local LLM not available! Running in degraded cloud-only mode. Start Ollama for full capability.");
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
            .timeout(std::time::Duration::from_secs(300)) // 5 min for slow CPU inference
            .connect_timeout(std::time::Duration::from_secs(30))
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
        if self.local_available && !force_cloud {
            match self.local_generate_stream(prompt).await {
                Ok(stream) => return Ok(Box::pin(stream)),
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

        // Build the enhanced prompt with tools
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

cwd: {cwd}
user: {input}

Reply (use <tool_call>{{...}}</tool_call> for tools):"#,
            tools_prompt = tools_prompt,
            cwd = context.working_directory,
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

    /// Generate using local Ollama with streaming
    async fn local_generate_stream(
        &self,
        prompt: &str,
    ) -> Result<impl Stream<Item = Result<String>> + Send> {
        debug!("🧠 Streaming with local LLM (kernel brain)");

        let request = OllamaRequest {
            model: self.config.local_model.clone(),
            prompt: prompt.to_string(),
            stream: true,
        };

        let url = format!("{}/api/generate", self.config.ollama_url);
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

        Ok(stream)
    }

    /// Generate using cloud API with streaming
    async fn cloud_generate_stream(
        &self,
        prompt: &str,
    ) -> Result<impl Stream<Item = Result<String>> + Send> {
        debug!("☁️  Streaming with cloud LLM via OpenRouter");

        if self.config.openrouter_api_key.is_empty() {
            return Err(anyhow!("OpenRouter API key not configured"));
        }

        // Mock streaming for cloud by returning the whole thing as one chunk
        let response = self.cloud_generate(prompt).await?;

        Ok(futures::stream::once(futures::future::ready(Ok(response))))
    }

    pub fn build_basic_prompt(&self, input: &str, context: &Context) -> String {
        format!(
            r#"You are Mycel OS, an AI assistant. Answer the user's question or help with their task.

Current directory: {}
User: {}

Respond directly and helpfully:"#,
            context.working_directory, input
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

        // Build the enhanced prompt with tools
        let prompt = format!(
            r#"You are Mycel OS, an AI assistant with system access.

{tools_prompt}

IMPORTANT:
- For simple questions (jokes, explanations, chat), just answer directly - no tools needed.
- Use tools only when the user asks for system info, file operations, or commands.
- Be concise and helpful.

Current directory: {cwd}
User: {input}

Respond:"#,
            tools_prompt = tools_prompt,
            cwd = context.working_directory,
            input = input
        );

        // Get initial response from LLM
        let response = self.smart_generate(&prompt, false).await?;

        // Parse for tool calls
        let parsed = mcp::parse_tool_calls(&response);

        if !parsed.has_tool_calls() {
            // No tool calls - return the response directly
            return Ok(strip_markdown_formatting(&response));
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
        let use_cloud_first = force_cloud || (self.config.prefer_cloud && self.has_cloud_api());

        info!(
            "AI routing: prefer_cloud={}, has_api={}, using_cloud={}",
            self.config.prefer_cloud,
            self.has_cloud_api(),
            use_cloud_first
        );

        let result = if use_cloud_first {
            // Cloud first mode
            match self.cloud_generate(prompt).await {
                Ok(response) => Ok(response),
                Err(e) => {
                    if self.local_available {
                        warn!("Cloud failed, falling back to local: {}", e);
                        self.local_generate(prompt).await
                    } else {
                        Err(e)
                    }
                }
            }
        } else {
            // Local first mode
            if self.local_available {
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

    /// Generate using local Ollama - the primary brain of Mycel OS    /// Generate using local Ollama - the primary brain of Mycel OS
    async fn local_generate(&self, prompt: &str) -> Result<String> {
        debug!("🧠 Generating with local LLM (kernel brain)");

        let request = OllamaRequest {
            model: self.config.local_model.clone(),
            prompt: prompt.to_string(),
            stream: false,
        };

        let url = format!("{}/api/generate", self.config.ollama_url);
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

    /// Generate using cloud API via OpenRouter
    async fn cloud_generate(&self, prompt: &str) -> Result<String> {
        if self.config.openrouter_api_key.is_empty() {
            return Err(anyhow!(
                "No cloud API configured. Set OPENROUTER_API_KEY environment variable."
            ));
        }

        self.openrouter_generate(prompt).await
    }

    /// Generate using OpenRouter API
    async fn openrouter_generate(&self, prompt: &str) -> Result<String> {
        info!("☁️  Generating with cloud LLM: {}", self.config.cloud_model);

        let request = OpenRouterRequest {
            model: self.config.cloud_model.clone(),
            messages: vec![OpenRouterMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
            max_tokens: Some(4096),
        };

        let response = self
            .http_client
            .post("https://openrouter.ai/api/v1/chat/completions")
            .header(
                "Authorization",
                format!("Bearer {}", self.config.openrouter_api_key),
            )
            .header("HTTP-Referer", "https://mycel-os.dev")
            .header("X-Title", "Mycel OS")
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(anyhow!("OpenRouter API error: {}", error_text));
        }

        let response: OpenRouterResponse = response.json().await?;

        response
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .ok_or_else(|| anyhow!("Empty response from OpenRouter"))
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
            LlmProvider::Auto => self.smart_generate(prompt, false).await,
            LlmProvider::Local => {
                if !self.local_available {
                    return Err(anyhow!("Local LLM (Ollama) is not available"));
                }
                self.local_generate(prompt).await
            }
            LlmProvider::Cloud => {
                if !self.has_cloud_api() {
                    return Err(anyhow!(
                        "Cloud LLM is not configured. Set OPENROUTER_API_KEY."
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

        let prompt = format!(
            r#"You are Mycel OS. You ARE the operating system.

{tools_prompt}

RULES:
- TERSE responses only.
- Use tools when helpful for the task.
- For simple questions, just respond directly.
- After getting tool results, provide a final response.

cwd: {cwd}
user: {input}

Reply:"#,
            tools_prompt = tools_prompt,
            cwd = context.working_directory,
            input = input
        );

        let response = self.generate_with_provider(&prompt, provider).await?;
        let parsed = mcp::parse_tool_calls(&response);

        if !parsed.has_tool_calls() {
            return Ok(strip_markdown_formatting(&response));
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
        self.local_available
    }

    /// Check if cloud API is available
    fn has_cloud_api(&self) -> bool {
        !self.config.openrouter_api_key.is_empty()
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
    #[serde(default)]
    response: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

// Request/Response types for OpenRouter (OpenAI-compatible)
#[derive(Serialize)]
struct OpenRouterRequest {
    model: String,
    messages: Vec<OpenRouterMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Serialize)]
struct OpenRouterMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OpenRouterResponse {
    choices: Vec<OpenRouterChoice>,
}

#[derive(Deserialize)]
struct OpenRouterChoice {
    message: OpenRouterResponseMessage,
}

#[derive(Deserialize)]
struct OpenRouterResponseMessage {
    content: Option<String>,
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
}
