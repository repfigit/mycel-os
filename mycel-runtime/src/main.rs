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
mod commands;
mod config;
mod context;
mod events;
mod executor;
mod intent;
mod ipc;
mod mcp;
mod models;
mod policy;
mod selfdev;
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

    // Self-development loop (auto-filing issues for limitations). State lives
    // alongside other runtime data; disabled unless [selfdev] enabled = true.
    let selfdev = selfdev::SelfDevService::new(
        config.selfdev.clone(),
        std::path::PathBuf::from(&config.context_path).join("selfdev_state.json"),
    )
    .await;

    // User-defined command registry (named sequences of shell/tool steps).
    let command_registry = commands::CommandRegistry::load(
        std::path::PathBuf::from(&config.context_path).join("commands.json"),
    )
    .await;

    // Let the sync service apply DefineCommand events from the mesh into the
    // registry, so commands follow the user across their devices.
    sync_service
        .set_command_registry(command_registry.clone())
        .await;

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
        selfdev,
        command_registry,
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
    pub selfdev: selfdev::SelfDevService,
    pub command_registry: commands::CommandRegistry,
}

impl MycelRuntime {
    /// Process user input - the LLM is the interface between user and OS
    pub async fn process_input(&self, input: &str, session_id: &str) -> Result<RuntimeResponse> {
        let context = self.context_manager.get_context(session_id).await?;

        // 1. Handle pending confirmations
        if let Some(pending_code) = &context.pending_command {
            let input_lower = input.to_lowercase();
            let confirmed = matches!(input_lower.as_str(), "yes" | "y" | "confirm" | "ok");
            let denied = matches!(input_lower.as_str(), "no" | "n" | "cancel");

            // Pending command definition (save a new user command)?
            if let Some(def_json) = pending_code.strip_prefix("#!defcmd\n") {
                if confirmed {
                    self.context_manager
                        .clear_pending_command(session_id)
                        .await?;
                    let def: commands::PendingCommandDef = serde_json::from_str(def_json)?;
                    let params = def.params.clone();
                    let script = def.script.clone();
                    let steps_json = serde_json::to_string(&def.steps)?;
                    match self
                        .command_registry
                        .define_full(
                            &def.name,
                            &def.description,
                            def.steps,
                            def.params,
                            def.script,
                        )
                        .await
                    {
                        Ok(_) => {
                            // Propagate the new command across the personal mesh.
                            let _ = self
                                .sync_service
                                .create_event(crate::sync::SyncOperation::DefineCommand {
                                    name: def.name.clone(),
                                    description: def.description.clone(),
                                    steps_json,
                                    params,
                                    script,
                                })
                                .await;
                            return Ok(RuntimeResponse::Text(format!(
                                "'{}' is ready. just say '{}' anytime.",
                                def.name, def.name
                            )));
                        }
                        Err(e) => {
                            return Ok(RuntimeResponse::Text(format!("couldn't save: {}", e)));
                        }
                    }
                } else if denied {
                    self.context_manager
                        .clear_pending_command(session_id)
                        .await?;
                    return Ok(RuntimeResponse::Text("command not saved.".to_string()));
                } else {
                    return Ok(RuntimeResponse::Text(
                        "save this command? type 'yes' to save or 'no' to discard.".to_string(),
                    ));
                }
            }

            // Pending code execution?
            if confirmed {
                // User confirmed - clear and execute
                self.context_manager
                    .clear_pending_command(session_id)
                    .await?;
                let output = self.executor.run(pending_code).await?;
                return Ok(RuntimeResponse::Text(output));
            } else if denied {
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

        // 2. Command management (conversational create/list/show/forget)
        if let Some(response) = self.handle_command_management(input, session_id).await? {
            return Ok(RuntimeResponse::Text(response));
        }

        // 3. User-defined command? Run its steps (each policy-gated).
        let input_trimmed = input.trim();
        let mut words = input_trimmed.split_whitespace();
        let first_word = words.next().unwrap_or("");
        if let Some(user_cmd) = self.command_registry.get(first_word).await {
            // Remaining words are arguments for parameterized commands.
            let args: Vec<String> = words.map(|s| s.to_string()).collect();
            return self.run_user_command(user_cmd, &args, session_id).await;
        }

        // 4. Check if it looks like a command (single word or starts with common command pattern)
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
                let out = result.trim();
                // `which` exits non-zero with no output when the command is
                // missing; the executor surfaces that as an empty string OR a
                // "Command exited with code" placeholder. Both mean "not found".
                let not_found = out.is_empty() || out.starts_with("Command exited with code");
                if not_found {
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

    /// Process user input with a specific LLM provider
    pub async fn process_input_with_provider(
        &self,
        input: &str,
        session_id: &str,
        provider: ipc::LlmProvider,
    ) -> Result<RuntimeResponse> {
        use ipc::LlmProvider;

        // If auto, use normal process_input
        if provider == LlmProvider::Auto {
            return self.process_input(input, session_id).await;
        }

        let context = self.context_manager.get_context(session_id).await?;

        // Use provider-aware processing
        let response = self
            .ai_router
            .process_with_tools_provider(input, &context, &self.mcp_manager, provider)
            .await?;

        // Check if LLM wants to execute code
        if response.starts_with("#!exec\n") || response.starts_with("#!exec ") {
            let code = response.trim_start_matches("#!exec").trim();
            self.execute_code_with_policy(code, session_id).await
        } else if response.starts_with("```") {
            let code = extract_code_block(&response);
            self.execute_code_with_policy(&code, session_id).await
        } else {
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

    /// Execute code after checking with policy
    pub(crate) async fn execute_code_with_policy(
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
        // Report the gap to the self-development loop (deduped, opt-in).
        let _ = self
            .selfdev
            .report(selfdev::Limitation {
                kind: selfdev::LimitationKind::MissingCommand,
                summary: format!("user needs the '{}' command", cmd),
                detail: format!(
                    "'{}' was requested but is not installed and no package was found.",
                    cmd
                ),
                suggestion: Some(format!(
                    "Add an MCP tool or package-install fallback for '{}'.",
                    cmd
                )),
            })
            .await;

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

    /// Handle conversational command management: make/list/show/forget.
    /// Returns Some(response) if the input was a management request, else None.
    async fn handle_command_management(
        &self,
        input: &str,
        session_id: &str,
    ) -> Result<Option<String>> {
        let lower = input.trim().to_lowercase();

        // List commands
        if matches!(
            lower.as_str(),
            "what commands do i have" | "list commands" | "my commands" | "show commands"
        ) {
            let cmds = self.command_registry.list().await;
            if cmds.is_empty() {
                return Ok(Some(
                    "no custom commands yet. create one with: make a command called <name> that <does things>".to_string(),
                ));
            }
            let mut out = String::from("your commands:\n");
            for c in cmds {
                out.push_str(&format!(
                    "  {} - {} ({} steps, used {}x)\n",
                    c.name,
                    c.description,
                    c.steps.len(),
                    c.use_count
                ));
            }
            return Ok(Some(out));
        }

        // Forget/delete a command
        for prefix in [
            "forget the ",
            "forget ",
            "delete the command ",
            "delete command ",
            "remove command ",
        ] {
            if let Some(rest) = lower.strip_prefix(prefix) {
                let name = rest.trim().trim_end_matches(" command").trim().to_string();
                return if self.command_registry.remove(&name).await? {
                    Ok(Some(format!("'{}' forgotten.", name)))
                } else {
                    Ok(Some(format!("no command called '{}'.", name)))
                };
            }
        }

        // Show a command's definition
        for prefix in ["what does ", "show ", "describe "] {
            if let Some(rest) = lower.strip_prefix(prefix) {
                let name = rest
                    .trim()
                    .trim_end_matches(" do")
                    .trim_end_matches(" command")
                    .trim()
                    .to_string();
                if let Some(cmd) = self.command_registry.get(&name).await {
                    let mut out = format!("'{}' - {}\n", cmd.name, cmd.description);
                    for (i, step) in cmd.steps.iter().enumerate() {
                        out.push_str(&format!("  {}. {}\n", i + 1, step.describe()));
                    }
                    return Ok(Some(out));
                }
                // Not a command — fall through to normal processing.
                return Ok(None);
            }
        }

        // Create a command: "make a command called X that ..."
        if let Some(def) = self.parse_command_definition(input).await? {
            // Show the expanded form and ask for confirmation before saving.
            let mut preview = format!("i'll create '{}':\n", def.name);
            if !def.params.is_empty() {
                preview.push_str(&format!("  parameters: {}\n", def.params.join(", ")));
            }
            if let Some(script) = &def.script {
                preview.push_str("  script:\n");
                for line in script.lines().take(12) {
                    preview.push_str(&format!("    {}\n", line));
                }
            } else {
                for (i, step) in def.steps.iter().enumerate() {
                    preview.push_str(&format!("  {}. {}\n", i + 1, step.describe()));
                }
            }
            // Check for collision with a real binary.
            if let Ok(which) = self
                .executor
                .run(&format!("which {} 2>/dev/null", def.name))
                .await
            {
                let out = which.trim();
                if !out.is_empty() && !out.starts_with("Command exited with code") {
                    preview.push_str(&format!(
                        "\nwarning: '{}' is also a real command at {}. your version takes precedence in chat.",
                        def.name, out
                    ));
                }
            }
            preview.push_str("save this command? (yes/no)");

            // Stash the pending definition in the session for confirmation.
            let pending = serde_json::to_string(&def)?;
            self.context_manager
                .set_pending_command(session_id, Some(format!("#!defcmd\n{}", pending)))
                .await?;
            return Ok(Some(preview));
        }

        Ok(None)
    }

    /// Parse "make a command called X that A, B, and C" into (name, desc, steps).
    /// Uses the LLM to split the step list robustly, then builds Shell steps.
    async fn parse_command_definition(
        &self,
        input: &str,
    ) -> Result<Option<commands::PendingCommandDef>> {
        let lower = input.to_lowercase();
        // Match "make a command called X that ..." / "create a command X that ..."
        let trigger = [
            "make a command called",
            "create a command called",
            "make a command",
            "create a command",
        ];
        let mut name: Option<String> = None;
        let mut rest = "";
        for t in trigger {
            if let Some(idx) = lower.find(t) {
                let after = &input[idx + t.len()..];
                let after_lower = &lower[idx + t.len()..];
                // Split on " that " to separate name from the step list.
                if let Some(tidx) = after_lower.find(" that ") {
                    name = Some(after[..tidx].trim().to_string());
                    rest = &after[tidx + 6..];
                } else {
                    // No "that": first word is the name, rest is the body.
                    let mut parts = after.trim().splitn(2, ' ');
                    name = parts.next().map(|s| s.trim().to_string());
                    rest = parts.next().unwrap_or("");
                }
                break;
            }
        }
        let Some(name) = name.filter(|n| !n.is_empty()) else {
            return Ok(None);
        };
        if rest.trim().is_empty() {
            return Ok(None);
        }
        let body = rest.trim();

        // Tier 3: raw script. Detected by explicit "script" phrasing or bash
        // control-flow keywords in the body.
        let is_script = lower.contains(" script")
            || lower.contains("bash script")
            || body.contains("\n")
            || ["for ", "while ", "if ", "until "]
                .iter()
                .any(|k| lower.contains(k));

        // Ask the LLM to produce a structured definition as JSON.
        let def_prompt = format!(
            "Turn this user request into a reusable OS command definition.\n\
Return ONLY a JSON object, no prose, with these fields:\n\
  \"params\": array of parameter names the command takes (empty if none),\n\
  \"steps\": array of shell command strings (use {{param}} for parameters),\n\
  \"script\": optional single bash script string for logic (loops/ifs), or null.\n\
Rules:\n\
- If the request takes an input (a folder, file, name), make it a param and use {{param}} in steps.\n\
- Use \"script\" only when the logic needs loops or conditionals; otherwise use \"steps\".\n\
- Keep it minimal and safe.\n\
Request: \"{}\"\n\
Example for 'backup a folder': {{\"params\":[\"folder\"],\"steps\":[\"tar czf ~/backups/{{folder}}-$(date +%F).tar.gz {{folder}}\"],\"script\":null}}",
            body
        );
        let llm_out = self
            .ai_router
            .generate_with_provider(&def_prompt, crate::ipc::LlmProvider::Auto)
            .await?;

        let (params, steps, script) = parse_command_def_json(&llm_out);

        // Force script form if Tier 3 was detected but the LLM used steps.
        let script = if is_script && script.is_none() && !steps.is_empty() {
            Some(steps.join("\n"))
        } else {
            script
        };
        let steps = if script.is_some() { Vec::new() } else { steps };

        if script.is_none() && steps.is_empty() {
            return Ok(None);
        }

        let description = body.chars().take(80).collect::<String>();
        Ok(Some(commands::PendingCommandDef {
            name,
            description,
            steps: steps
                .into_iter()
                .map(|s| commands::CommandStep::shell(&s))
                .collect(),
            params,
            script,
        }))
    }

    /// Execute a user-defined command. `args` are positional parameter values.
    /// Scripts (Tier 3) run as one body; step commands run each step. All shell
    /// execution is policy-gated; parameter values are shell-escaped as data.
    async fn run_user_command(
        &self,
        cmd: commands::UserCommand,
        args: &[String],
        session_id: &str,
    ) -> Result<RuntimeResponse> {
        // Parameter count check.
        if args.len() < cmd.params.len() {
            return Ok(RuntimeResponse::Text(format!(
                "'{}' needs {} parameter(s): {}\nusage: {} {}",
                cmd.name,
                cmd.params.len(),
                cmd.params.join(", "),
                cmd.name,
                cmd.params
                    .iter()
                    .map(|p| format!("<{}>", p))
                    .collect::<Vec<_>>()
                    .join(" ")
            )));
        }

        self.command_registry.record_use(&cmd.name).await;

        // Tier 3: raw script — substitute params, policy-gate the whole body.
        if let Some(script) = &cmd.script {
            let expanded = cmd.substitute(script, args);
            use crate::policy::ActionPolicy;
            return match self.policy_evaluator.evaluate_code(&expanded) {
                ActionPolicy::Allow => {
                    let out = self.executor.run(&expanded).await?;
                    Ok(RuntimeResponse::Text(out))
                }
                ActionPolicy::RequiresConfirmation { message, .. } => {
                    self.context_manager
                        .set_pending_command(session_id, Some(expanded.clone()))
                        .await?;
                    Ok(RuntimeResponse::Text(format!(
                        "{}\ncode: {}",
                        message, expanded
                    )))
                }
                ActionPolicy::Deny { reason } => {
                    Ok(RuntimeResponse::Text(format!("blocked: {}", reason)))
                }
            };
        }

        // Tier 1/2: run steps in order, substituting params, each policy-gated.
        let mut outputs = Vec::new();
        for (i, step) in cmd.steps.iter().enumerate() {
            match step {
                commands::CommandStep::Shell { command } => {
                    let expanded = cmd.substitute(command, args);
                    use crate::policy::ActionPolicy;
                    match self.policy_evaluator.evaluate_code(&expanded) {
                        ActionPolicy::Allow => {
                            let out = self.executor.run(&expanded).await?;
                            outputs.push(format!("{}. {}\n{}", i + 1, expanded, out.trim()));
                        }
                        ActionPolicy::RequiresConfirmation { message, .. } => {
                            self.context_manager
                                .set_pending_command(session_id, Some(expanded.clone()))
                                .await?;
                            outputs.push(format!("{}. {} — paused: {}", i + 1, expanded, message));
                            return Ok(RuntimeResponse::Text(outputs.join("\n\n")));
                        }
                        ActionPolicy::Deny { reason } => {
                            outputs.push(format!("{}. {} — blocked: {}", i + 1, expanded, reason));
                            return Ok(RuntimeResponse::Text(outputs.join("\n\n")));
                        }
                    }
                }
                commands::CommandStep::Prompt { message } => {
                    outputs.push(format!("{}. {}", i + 1, message));
                }
            }
        }

        Ok(RuntimeResponse::Text(outputs.join("\n\n")))
    }
}

/// Parse a structured command-definition JSON object from an LLM response.
/// Returns (params, steps, script). Tolerant of surrounding prose.
fn parse_command_def_json(text: &str) -> (Vec<String>, Vec<String>, Option<String>) {
    let start = text.find('{');
    let end = text.rfind('}');
    if let (Some(s), Some(e)) = (start, end) {
        if e > s {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text[s..=e]) {
                let params = v
                    .get("params")
                    .and_then(|p| p.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                let steps = v
                    .get("steps")
                    .and_then(|p| p.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                let script = v
                    .get("script")
                    .and_then(|p| p.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                return (params, steps, script);
            }
        }
    }
    (Vec::new(), Vec::new(), None)
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
        let _ = io::stdout().flush();

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
                        let _ = io::stdout().flush();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_command_def_json_full() {
        let (params, steps, script) = parse_command_def_json(
            r#"{"params":["folder"],"steps":["tar czf {folder}.tgz {folder}"],"script":null}"#,
        );
        assert_eq!(params, vec!["folder"]);
        assert_eq!(steps, vec!["tar czf {folder}.tgz {folder}"]);
        assert!(script.is_none());
    }

    #[test]
    fn test_parse_command_def_json_with_script() {
        let (params, steps, script) = parse_command_def_json(
            "Here you go:\n{\"params\":[],\"steps\":[],\"script\":\"for f in *; do echo $f; done\"}\nDone.",
        );
        assert!(params.is_empty());
        assert!(steps.is_empty());
        assert_eq!(script.unwrap(), "for f in *; do echo $f; done");
    }

    #[test]
    fn test_parse_command_def_json_invalid() {
        let (params, steps, script) = parse_command_def_json("no json");
        assert!(params.is_empty() && steps.is_empty() && script.is_none());
    }

    #[test]
    fn test_extract_code_block() {
        assert_eq!(extract_code_block("```\nls -la\n```"), "ls -la");
        assert_eq!(extract_code_block("```bash\necho hi\n```"), "echo hi");
    }
}
