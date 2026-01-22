//! Mycel Runtime - Core daemon for Mycel OS
//!
//! The intelligent network beneath everything.
//!
//! This is the heart of Mycel OS, managing:
//! - Local LLM inference
//! - Cloud AI routing
//! - Intent parsing and execution
//! - Context management
//! - Code generation and sandboxed execution
//! - UI surface generation
//! - Device mesh synchronization
//! - Collective intelligence (NEAR/Bittensor)

use anyhow::Result;
use clap::Parser;
use futures::Stream;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod ai;
mod codegen;
mod collective;
mod config;
mod context;
mod events;
mod executor;
mod intent;
mod ipc;
mod mcp;
mod models;
mod policy;
mod sync;
mod ui;

use crate::config::MycelConfig;

#[derive(Parser, Debug)]
#[command(name = "mycel")]
#[command(about = "Mycel OS Runtime - The intelligent network beneath everything")]
#[command(version)]
struct Args {
    /// Configuration file path
    #[arg(short, long, default_value = "/etc/mycel/config.toml")]
    config: String,

    /// Run in development mode (no root required)
    #[arg(long)]
    dev: bool,

    /// Verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// Skip loading the local LLM (cloud-only mode)
    #[arg(long)]
    no_local_llm: bool,

    /// Skip collective network connection (local-only mode)
    #[arg(long)]
    no_collective: bool,

    /// Run as daemon (no interactive CLI)
    #[arg(long)]
    daemon: bool,
}

fn print_banner() {
    // Minimal - the OS speaks for itself
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging - quiet by default, verbose only when requested
    // Can override with RUST_LOG env var
    let default_level = if args.verbose { "debug" } else { "error" }; // Quiet by default
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("mycel_runtime={}", default_level)));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_target(args.verbose)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false),
        )
        .init();

    print_banner();

    let config = MycelConfig::load(&args.config, args.dev)?;

    // Log config status
    tracing::info!(
        "Config: prefer_cloud={}, has_openrouter_key={}",
        config.prefer_cloud,
        !config.openrouter_api_key.is_empty()
    );

    let context_manager = context::ContextManager::new(&config).await?;
    let ai_router = if args.no_local_llm {
        ai::AiRouter::cloud_only(&config).await?
    } else {
        ai::AiRouter::new(&config).await?
    };
    let executor = executor::CodeExecutor::new(&config)?;
    let policy_evaluator = policy::PolicyEvaluator::with_defaults();
    let ui_factory = ui::UiFactory::new(&config)?;

    // Create system event bus
    let (event_bus, _) = tokio::sync::broadcast::channel(100);

    // Initialize MCP manager with default void-tools config if none specified
    let runtime_path = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string());

    let mcp_config = if config.mcp.servers.is_empty() && config.mcp.enabled {
        // Use default void-tools configuration
        mcp::default_void_tools_config(&runtime_path)
    } else {
        config.mcp.clone()
    };

    let mcp_manager = mcp::McpManager::new(&mcp_config, &runtime_path, event_bus.clone()).await?;
    // Start MCP servers in the background
    if let Err(e) = mcp_manager.start_servers().await {
        tracing::warn!("Failed to start MCP servers: {}", e);
    }

    let sync_service =
        sync::SyncService::new(&config, Some(mcp_manager.clone()), event_bus.clone()).await?;
    sync_service.start().await?;

    // Create the main runtime
    let runtime = MycelRuntime {
        config,
        context_manager,
        ai_router,
        executor,
        policy_evaluator,
        ui_factory,
        sync_service,
        mcp_manager,
    };

    let ipc_server = ipc::IpcServer::new(&runtime).await?;

    if args.dev {
        // Only spawn interactive CLI if running with a tty and not in daemon mode
        let run_cli = !args.daemon && atty::is(atty::Stream::Stdin);
        if run_cli {
            tokio::spawn(run_dev_cli(runtime.clone()));
        }
    }

    // Background session cleanup
    let cleanup_context_manager = runtime.context_manager.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            cleanup_context_manager.cleanup_stale_sessions(None).await;
        }
    });

    ipc_server.run().await?;

    Ok(())
}

/// The main runtime struct that ties everything together
#[derive(Clone)]
pub struct MycelRuntime {
    pub config: MycelConfig,
    pub context_manager: context::ContextManager,
    pub ai_router: ai::AiRouter,
    pub executor: executor::CodeExecutor,
    pub policy_evaluator: policy::PolicyEvaluator,
    pub ui_factory: ui::UiFactory,
    pub sync_service: sync::SyncService,
    pub mcp_manager: mcp::McpManager,
}

impl MycelRuntime {
    /// Process user input - the LLM is the interface between user and OS
    pub async fn process_input(&self, input: &str, session_id: &str) -> Result<RuntimeResponse> {
        let context = self.context_manager.get_context(session_id).await?;

        // 1. Handle pending confirmations
        if let Some(pending_code) = &context.pending_command {
            let input_lower = input.to_lowercase();
            if input_lower == "yes"
                || input_lower == "y"
                || input_lower == "confirm"
                || input_lower == "ok"
            {
                // User confirmed - clear and execute
                self.context_manager
                    .clear_pending_command(session_id)
                    .await?;
                let output = self.executor.run(pending_code).await?;
                return Ok(RuntimeResponse::Text(output));
            } else if input_lower == "no" || input_lower == "n" || input_lower == "cancel" {
                // User denied - clear and inform
                self.context_manager
                    .clear_pending_command(session_id)
                    .await?;
                return Ok(RuntimeResponse::Text("action cancelled.".to_string()));
            } else {
                // User typed something else - inform them they have a pending action
                return Ok(RuntimeResponse::Text(format!(
                    "you have a pending action. type 'yes' to confirm or 'no' to cancel.\ncode: {}",
                    pending_code
                )));
            }
        }

        // 2. Normal processing
        let input_trimmed = input.trim();
        let first_word = input_trimmed.split_whitespace().next().unwrap_or("");

        // Check if it looks like a command (single word or starts with common command pattern)
        if !first_word.is_empty()
            && !first_word.contains(' ')
            && first_word
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            // Check if command exists
            let check = self
                .executor
                .run(&format!("which {} 2>/dev/null", first_word))
                .await;
            if let Ok(result) = &check {
                if result.trim().is_empty() {
                    // Command not found - search for package
                    return self.handle_missing_command(first_word).await;
                }
            }
        }

        // The LLM decides what to do - use MCP tools if available
        let response = self
            .ai_router
            .process_with_tools(input, &context, &self.mcp_manager)
            .await?;

        // Check if LLM wants to execute code
        if response.starts_with("#!exec\n") || response.starts_with("#!exec ") {
            let code = response.trim_start_matches("#!exec").trim();
            self.execute_code_with_policy(code, session_id).await
        } else if response.starts_with("```") {
            let code = extract_code_block(&response);
            self.execute_code_with_policy(&code, session_id).await
        } else {
            // Return the response from process_with_tools directly
            Ok(RuntimeResponse::Text(response))
        }
    }

    /// Update history and sync with mesh
    pub async fn record_interaction(
        &self,
        session_id: &str,
        user: &str,
        assistant: &str,
    ) -> Result<()> {
        let turn = self
            .context_manager
            .update_session(session_id, user, assistant)
            .await?;

        let _ = self
            .sync_service
            .create_event(crate::sync::SyncOperation::AddConversationTurn {
                session_id: session_id.to_string(),
                user: turn.user,
                assistant: turn.assistant,
            })
            .await;

        Ok(())
    }

    /// Execute code after checking with policy (Legacy, needs update if used with streaming)
    async fn execute_code_with_policy(
        &self,
        code: &str,
        session_id: &str,
    ) -> Result<RuntimeResponse> {
        use crate::policy::ActionPolicy;

        match self.policy_evaluator.evaluate_code(code) {
            ActionPolicy::Allow => {
                let output = self.executor.run(code).await?;

                // Check if command not found in the output
                if output.contains("command not found") || output.contains("not found") {
                    let cmd = code.split_whitespace().next().unwrap_or("");
                    if !cmd.is_empty() {
                        return self.handle_missing_command(cmd).await;
                    }
                }

                Ok(RuntimeResponse::Text(output))
            }
            ActionPolicy::RequiresConfirmation { message, .. } => {
                // Store in session and ask user
                self.context_manager
                    .set_pending_command(session_id, Some(code.to_string()))
                    .await?;
                Ok(RuntimeResponse::Text(format!(
                    "{}\ncode: {}",
                    message, code
                )))
            }
            ActionPolicy::Deny { reason } => {
                Ok(RuntimeResponse::Text(format!("blocked: {}", reason)))
            }
        }
    }

    /// Handle missing command - search repos and offer to install
    async fn handle_missing_command(&self, cmd: &str) -> Result<RuntimeResponse> {
        // Search for package (works on Debian/Ubuntu - devcontainer)
        let search_result = self.executor.run(&format!(
            "apt-cache search '^{}$' 2>/dev/null | head -5 || apt-cache search '{}' 2>/dev/null | head -5",
            cmd, cmd
        )).await?;

        if search_result.trim().is_empty() {
            // Try broader search
            let broad_search = self
                .executor
                .run(&format!("apt-cache search '{}' 2>/dev/null | head -5", cmd))
                .await?;

            if broad_search.trim().is_empty() {
                return Ok(RuntimeResponse::Text(format!(
                    "'{}' not found and no package available. check spelling or install manually.",
                    cmd
                )));
            }

            return Ok(RuntimeResponse::Text(format!(
                "'{}' not installed. related packages:\n{}\ninstall with: sudo apt install <package>",
                cmd, broad_search.trim()
            )));
        }

        // Found exact or close match
        let first_package = search_result
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().next())
            .unwrap_or(cmd);

        Ok(RuntimeResponse::Text(format!(
            "'{}' not installed. found: {}\ninstall? run: sudo apt install {}",
            cmd,
            search_result.trim(),
            first_package
        )))
    }
}

/// Extract code from markdown code block
fn extract_code_block(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().collect();

    // Remove opening ``` line
    if !lines.is_empty() && lines[0].starts_with("```") {
        lines.remove(0);
    }

    // Remove closing ``` line
    if !lines.is_empty() && lines.last().map(|l| l.trim()) == Some("```") {
        lines.pop();
    }

    lines.join("\n")
}

use std::pin::Pin;

/// Response from the runtime - text or stream
pub enum RuntimeResponse {
    Text(String),
    Stream(Pin<Box<dyn Stream<Item = Result<String>> + Send>>),
}

impl std::fmt::Debug for RuntimeResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(t) => f.debug_tuple("Text").field(t).finish(),
            Self::Stream(_) => f.debug_tuple("Stream").finish(),
        }
    }
}

/// Development CLI for testing
async fn run_dev_cli(runtime: MycelRuntime) {
    use std::io::{self, BufRead, Write};

    let session_id = uuid::Uuid::new_v4().to_string();

    println!("mycel os");

    let stdin = io::stdin();
    loop {
        print!("mycel> ");
        io::stdout().flush().unwrap();

        let mut line = String::new();
        if stdin.lock().read_line(&mut line).is_err() {
            break;
        }

        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "quit" || input == "exit" {
            break;
        }

        if input.starts_with("near-link ") {
            let account_id = input.trim_start_matches("near-link ").trim();
            if !account_id.is_empty() {
                println!("linking to NEAR account: {}...", account_id);
                // Update config
                let mut new_config = runtime.config.clone();
                new_config.blockchain_sync = true;
                new_config.near_account = Some(account_id.to_string());

                // In a real implementation, we would save this to the config file
                // and probably restart the sync service polling.
                // For now, let's just update the runtime's local copy if possible
                // (though MycelRuntime holds it by value, so we'd need a Mutex if we wanted it truly dynamic)

                println!("successfully linked (simulated). please restart to enable polling.");
            }
            continue;
        }

        match runtime.process_input(input, &session_id).await {
            Ok(RuntimeResponse::Text(text)) => {
                if !text.is_empty() {
                    println!("{}", text);
                    let _ = runtime.record_interaction(&session_id, input, &text).await;
                }
            }
            Ok(RuntimeResponse::Stream(mut stream)) => {
                use futures_util::StreamExt;
                use std::io::{self, Write};
                let mut full_response = String::new();
                while let Some(chunk_result) = stream.next().await {
                    if let Ok(chunk) = chunk_result {
                        print!("{}", chunk);
                        full_response.push_str(&chunk);
                        io::stdout().flush().unwrap();
                    }
                }
                println!();
                let _ = runtime
                    .record_interaction(&session_id, input, &full_response)
                    .await;
            }
            Err(e) => eprintln!("error: {}", e),
        }
    }
}
