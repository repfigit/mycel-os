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
use tracing::{info, instrument};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod ai;
mod codegen;
mod collective;
mod config;
mod context;
mod executor;
mod intent;
mod ipc;
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
}

fn print_banner() {
    println!(
        r#"
    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗     
    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║     
    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║     
    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║     
    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗
    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝
    
    The intelligent network beneath everything.
"#
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize structured logging with EnvFilter support
    // Can override with RUST_LOG env var (e.g., RUST_LOG=mycel_runtime=debug)
    let default_level = if args.verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("mycel_runtime={}", default_level)));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_target(true)
                .with_thread_ids(args.verbose)
                .with_file(args.verbose)
                .with_line_number(args.verbose),
        )
        .init();

    print_banner();
    info!("Mycel Runtime starting...");

    // Load configuration
    let config = MycelConfig::load(&args.config, args.dev)?;
    info!("Configuration loaded from {}", args.config);

    // Initialize the context manager
    let context_manager = context::ContextManager::new(&config).await?;
    info!("Context manager initialized");

    // Initialize AI backends
    let ai_router = if args.no_local_llm {
        info!("Running in cloud-only mode");
        ai::AiRouter::cloud_only(&config).await?
    } else {
        info!("Initializing local LLM...");
        ai::AiRouter::new(&config).await?
    };
    info!("AI router ready");

    // Initialize code executor (sandboxed)
    let executor = executor::CodeExecutor::new(&config)?;
    info!("Code executor initialized");

    // Initialize UI factory
    let ui_factory = ui::UiFactory::new(&config)?;
    info!("UI factory ready");

    // Initialize sync service
    let sync_service = sync::SyncService::new(&config).await?;
    sync_service.start().await?;
    info!("Sync service ready");

    // Create the main runtime
    let runtime = MycelRuntime {
        config,
        context_manager,
        ai_router,
        executor,
        ui_factory,
        sync_service,
    };

    // Start the IPC server (for UI and other components to connect)
    let ipc_server = ipc::IpcServer::new(&runtime).await?;
    info!("IPC server listening");

    // If in dev mode, also start a simple CLI interface
    if args.dev {
        info!("Starting development CLI...");
        tokio::spawn(run_dev_cli(runtime.clone()));
    }

    // Start background session cleanup task (every hour)
    let cleanup_context_manager = runtime.context_manager.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            let removed = cleanup_context_manager.cleanup_stale_sessions(None).await;
            if removed > 0 {
                info!(
                    removed_sessions = removed,
                    "Periodic session cleanup completed"
                );
            }
        }
    });

    // Main event loop
    info!("Mycel Runtime ready. The network grows.");
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
    pub ui_factory: ui::UiFactory,
    pub sync_service: sync::SyncService,
}

impl MycelRuntime {
    /// Process a user input and generate a response
    #[instrument(
        skip(self, input),
        fields(
            request_id = %uuid::Uuid::new_v4(),
            input_len = input.len(),
        )
    )]
    pub async fn process_input(&self, input: &str, session_id: &str) -> Result<RuntimeResponse> {
        info!("Processing user input");

        // Get current context
        let context = self.context_manager.get_context(session_id).await?;

        // Parse intent
        let intent = self.ai_router.parse_intent(input, &context).await?;

        // Route to appropriate handler
        match intent.action_type {
            intent::ActionType::SimpleResponse => {
                // Just needs a text response
                let response = self.ai_router.generate_response(input, &context).await?;
                Ok(RuntimeResponse::Text(response))
            }
            intent::ActionType::GenerateCode => {
                // Needs to write and execute code
                let code = self.ai_router.generate_code(&intent, &context).await?;
                let result = self.executor.run(&code).await?;
                Ok(RuntimeResponse::CodeResult {
                    code,
                    output: result,
                })
            }
            intent::ActionType::GenerateUi => {
                // Needs to create a UI surface
                let ui_spec = self.ai_router.generate_ui_spec(&intent, &context).await?;
                let surface = self.ui_factory.create_surface(&ui_spec)?;
                Ok(RuntimeResponse::UiSurface(surface))
            }
            intent::ActionType::SystemAction => {
                // Needs to interact with the system
                self.handle_system_action(&intent).await
            }
            intent::ActionType::CloudEscalate => {
                // Local model decided this needs cloud AI
                let response = self.ai_router.cloud_request(input, &context).await?;
                Ok(RuntimeResponse::Text(response))
            }
        }
    }

    async fn handle_system_action(&self, intent: &intent::Intent) -> Result<RuntimeResponse> {
        // Handle system-level actions (file operations, settings, etc.)
        Ok(RuntimeResponse::Text(format!(
            "System action '{}' would be executed here",
            intent.action
        )))
    }
}

/// Possible responses from the runtime
#[derive(Debug, Clone)]
pub enum RuntimeResponse {
    Text(String),
    CodeResult { code: String, output: String },
    UiSurface(ui::Surface),
    Error(String),
}

/// Development CLI for testing
async fn run_dev_cli(runtime: MycelRuntime) {
    use std::io::{self, BufRead, Write};

    let session_id = uuid::Uuid::new_v4().to_string();

    println!("\n=== Mycel OS Development CLI ===");
    println!("The intelligent network beneath everything.");
    println!("Type your requests, or 'quit' to exit.\n");

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
            println!("The network rests. Goodbye!");
            break;
        }

        match runtime.process_input(input, &session_id).await {
            Ok(response) => match response {
                RuntimeResponse::Text(text) => println!("\n{}\n", text),
                RuntimeResponse::CodeResult { code, output } => {
                    println!(
                        "\n--- Generated Code ---\n{}\n--- Output ---\n{}\n",
                        code, output
                    );
                }
                RuntimeResponse::UiSurface(surface) => {
                    println!("\n[UI Surface created: {}]\n", surface.id);
                }
                RuntimeResponse::Error(err) => {
                    println!("\nError: {}\n", err);
                }
            },
            Err(e) => {
                println!("\nRuntime error: {}\n", e);
            }
        }
    }
}
