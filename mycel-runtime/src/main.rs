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
mod recovery;
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
    let confirmations = policy::ConfirmationStore::new();
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
        confirmations,
        ui_factory,
        sync_service,
        mcp_manager,
        selfdev,
        command_registry,
        event_bus,
    };

    let ipc_server = ipc::IpcServer::new(&runtime).await?;

    if args.dev {
        // Only spawn interactive CLI if running with a tty and not in daemon mode
        let run_cli = !args.daemon && std::io::IsTerminal::is_terminal(&std::io::stdin());
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

    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            res = ipc_server.run() => {
                res?;
            }
            res = tokio::signal::ctrl_c() => {
                res?;
                tracing::info!("Received SIGINT, shutting down IPC server");
            }
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, shutting down IPC server");
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            res = ipc_server.run() => {
                res?;
            }
            res = tokio::signal::ctrl_c() => {
                res?;
                tracing::info!("Received SIGINT, shutting down IPC server");
            }
        }
    }

    ipc_server.cleanup();

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
    pub confirmations: policy::ConfirmationStore,
    pub ui_factory: ui::UiFactory,
    pub sync_service: sync::SyncService,
    pub mcp_manager: mcp::McpManager,
    pub selfdev: selfdev::SelfDevService,
    pub command_registry: commands::CommandRegistry,
    pub event_bus: tokio::sync::broadcast::Sender<events::SystemEvent>,
}

impl MycelRuntime {
    /// Process user input with the default provider. Thin alias over
    /// [`Self::process_input_with_provider`] so every entry point (dev CLI, IPC)
    /// runs the same routing core: pending confirmations, command management,
    /// user commands, the missing-command probe, then the LLM.
    pub async fn process_input(&self, input: &str, session_id: &str) -> Result<RuntimeResponse> {
        self.process_input_with_provider(input, session_id, ipc::LlmProvider::Auto)
            .await
    }

    /// Process user input with a specific LLM provider through the unified routing core.
    pub async fn process_input_with_provider(
        &self,
        input: &str,
        session_id: &str,
        provider: ipc::LlmProvider,
    ) -> Result<RuntimeResponse> {
        let context = self.context_manager.get_context(session_id).await?;

        // 1. Handle pending confirmations
        if let Some(pending_code) = &context.pending_command {
            let input_lower = input.trim().to_lowercase();
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
                // User confirmed - clear and execute (never auto-repaired, but
                // failures are still reported on the event bus).
                self.context_manager
                    .clear_pending_command(session_id)
                    .await?;
                self.confirmations
                    .clear_matching_code(session_id, pending_code)
                    .await;
                let report = self.executor.run_detailed(pending_code).await?;
                if !report.succeeded() {
                    let class = recovery::classify_failure(&report);
                    self.emit_execution_failed(&report, class);
                }
                return Ok(RuntimeResponse::Text(report.combined_output()));
            } else if denied {
                // User denied - clear and inform
                self.context_manager
                    .clear_pending_command(session_id)
                    .await?;
                self.confirmations
                    .clear_matching_code(session_id, pending_code)
                    .await;
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
        if let Some(response) = self
            .handle_command_management(input, session_id, provider)
            .await?
        {
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

        // 4. Narrow missing-command probe: only probe inputs that clearly look
        // like a CLI command invocation (never plain single-word chat or slash
        // commands), and always shell-escape the `which` query.
        if let Some(probe_cmd) = extract_command_probe(input_trimmed) {
            let check = self
                .executor
                .run_detailed(&build_which_command(probe_cmd))
                .await;
            if let Ok(report) = &check {
                // Skip inconclusive probes (missing sandbox, timeout, signal)
                // so ordinary chat falls through to the LLM instead of failing.
                if !report.sandbox_unavailable && !report.timed_out && report.signal.is_none() {
                    // `which` prints the resolved path on success and nothing when
                    // the command is missing.
                    let found = report.succeeded() && !report.stdout.trim().is_empty();
                    if !found {
                        // Command not found - search for package
                        return self.handle_missing_command(probe_cmd).await;
                    }
                }
            }
        }

        // 5. The LLM decides what to do - use MCP tools if available
        let response = if provider == ipc::LlmProvider::Auto {
            self.ai_router
                .process_with_tools(input, &context, &self.mcp_manager)
                .await?
        } else {
            self.ai_router
                .process_with_tools_provider(input, &context, &self.mcp_manager, provider)
                .await?
        };

        self.dispatch_llm_response(response, session_id).await
    }

    /// Route an LLM reply. Only the explicit `#!exec` marker (emitted by the
    /// router for a `shell_execute` tool call or an intent-driven code
    /// generation) reaches the executor, and it is policy-gated there. Any
    /// other reply, including one that happens to start with a fenced code
    /// block, is returned to the user as text and never executed.
    async fn dispatch_llm_response(
        &self,
        response: String,
        session_id: &str,
    ) -> Result<RuntimeResponse> {
        match exec_payload(&response) {
            Some(code) => self.execute_code_with_policy(code, session_id).await,
            None => Ok(RuntimeResponse::Text(response)),
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
            ActionPolicy::Allow => self.run_and_recover(code).await,
            ActionPolicy::RequiresConfirmation { message, .. } => {
                // Store in session and ask user
                self.context_manager.get_context(session_id).await?;
                self.context_manager
                    .set_pending_command(session_id, Some(code.to_string()))
                    .await?;
                let pending = self
                    .confirmations
                    .insert(session_id, "execute_code", code, &message)
                    .await;
                let _ = self
                    .event_bus
                    .send(events::SystemEvent::ConfirmationRequired {
                        id: pending.id,
                        session_id: session_id.to_string(),
                        command: code.to_string(),
                        reason: message.clone(),
                    });
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

    /// Run policy-approved code and, on failure, classify it, emit events, and
    /// (only when explicitly enabled and safe) attempt a verified repair.
    async fn run_and_recover(&self, code: &str) -> Result<RuntimeResponse> {
        let report = self.executor.run_detailed(code).await?;
        if report.succeeded() {
            return Ok(RuntimeResponse::Text(report.combined_output()));
        }

        let class = recovery::classify_failure(&report);
        self.emit_execution_failed(&report, class);

        // A missing command is fixed by installation, not code repair; route it
        // to the package-search handler only when the shell diagnostic confirms
        // the invoked command was missing (never guess from the first token).
        let combined = report.combined_output();
        if let Some(cmd) = detect_missing_command_in_output(code, &combined) {
            return self.handle_missing_command(&cmd).await;
        }

        if !self.config.recovery.enabled {
            return Ok(RuntimeResponse::Text(combined));
        }

        let decision = recovery::decide_recovery(&report, &self.config.recovery, 0);
        self.emit_recovery_decided(&decision, false, 0);

        match decision {
            recovery::RecoveryDecision::ProposeRepair { .. } => {
                self.attempt_repair(code, &report).await
            }
            recovery::RecoveryDecision::ReturnFailure { .. } => Ok(RuntimeResponse::Text(combined)),
            recovery::RecoveryDecision::AbortForInspection { class, reason } => {
                self.report_execution_limitation(&report, &reason).await;
                let output = if combined.trim().is_empty() {
                    format!("execution failed: {}", class.as_str())
                } else {
                    combined
                };
                Ok(RuntimeResponse::Text(output))
            }
        }
    }

    /// Emit an `ExecutionFailed` event for a failed report.
    fn emit_execution_failed(
        &self,
        report: &executor::ExecutionReport,
        class: recovery::FailureClass,
    ) {
        let _ = self.event_bus.send(events::SystemEvent::ExecutionFailed {
            language: report.language.as_str().to_string(),
            failure_class: class.as_str().to_string(),
            exit_code: report.exit_code,
            signal: report.signal,
            timed_out: report.timed_out,
            sandbox_mode: report.sandbox_mode.as_str().to_string(),
        });
    }

    /// Emit a `RecoveryDecided` event for a classification decision.
    fn emit_recovery_decided(
        &self,
        decision: &recovery::RecoveryDecision,
        verified: bool,
        attempts: u32,
    ) {
        let _ = self.event_bus.send(events::SystemEvent::RecoveryDecided {
            decision: decision.kind_str().to_string(),
            reason: decision.reason().to_string(),
            verified,
            attempts,
        });
    }

    /// Report a failed execution to the self-development loop. Only structured
    /// facts (class, exit code, sandbox mode) are shared - never raw output,
    /// which may contain secrets.
    async fn report_execution_limitation(&self, report: &executor::ExecutionReport, reason: &str) {
        let class = recovery::classify_failure(report);
        let _ = self
            .selfdev
            .report(selfdev::Limitation {
                kind: selfdev::LimitationKind::ExecutionFailure,
                summary: format!("generated code failed ({})", class.as_str()),
                detail: format!(
                    "sandbox_mode={} exit_code={:?} signal={:?} timed_out={} reason={}",
                    report.sandbox_mode.as_str(),
                    report.exit_code,
                    report.signal,
                    report.timed_out,
                    reason
                ),
                suggestion: Some(
                    "Improve code generation or expand tool coverage for this task.".to_string(),
                ),
            })
            .await;
    }

    /// Attempt opt-in, verified repairs of an eligible failure up to
    /// `config.recovery.max_repair_attempts`. The model may only *propose* a
    /// candidate; the recovery layer enforces safety preconditions and
    /// postconditions before anything is accepted.
    async fn attempt_repair(
        &self,
        original_code: &str,
        report: &executor::ExecutionReport,
    ) -> Result<RuntimeResponse> {
        let max_attempts = self.config.recovery.max_repair_attempts;
        // Require non-empty output so silent no-ops like `true` or `exit 0`
        // cannot pass verification.
        let postcondition = recovery::Postcondition {
            must_not_be_empty: true,
            ..Default::default()
        };

        let mut last_feedback: Option<String> = None;
        let mut last_reason = "repair not verified".to_string();

        for attempt in 0..max_attempts {
            if !matches!(
                recovery::decide_recovery(report, &self.config.recovery, attempt),
                recovery::RecoveryDecision::ProposeRepair { .. }
            ) {
                break;
            }

            let attempt_num = attempt + 1;
            let prompt = build_repair_prompt(original_code, report, last_feedback.as_deref());
            let raw = match self
                .ai_router
                .generate_with_provider(&prompt, ipc::LlmProvider::Auto)
                .await
            {
                Ok(raw) => raw,
                Err(e) => {
                    tracing::debug!("repair generation failed (attempt {}): {}", attempt_num, e);
                    last_reason = "repair generation failed".to_string();
                    break;
                }
            };

            let Some(candidate) = recovery::extract_repair_candidate(&raw) else {
                last_feedback = Some("previous reply had no valid fenced code block".to_string());
                last_reason = "no usable repair candidate".to_string();
                let _ = self.event_bus.send(events::SystemEvent::RecoveryDecided {
                    decision: "repair_rejected".to_string(),
                    reason: last_reason.clone(),
                    verified: false,
                    attempts: attempt_num,
                });
                continue;
            };

            let verification = recovery::verify_and_run_repair(
                original_code,
                &candidate,
                &self.executor,
                &self.policy_evaluator,
                &self.config.recovery,
                &postcondition,
            )
            .await?;

            let detail = match &verification.reject_reason {
                Some(reason) => format!("tier {}: {:?}", verification.tier_reached, reason),
                None => format!("verified at tier {}", verification.tier_reached),
            };

            let _ = self.event_bus.send(events::SystemEvent::RecoveryDecided {
                decision: if verification.accepted {
                    "repair_accepted".to_string()
                } else {
                    "repair_rejected".to_string()
                },
                reason: detail.clone(),
                verified: verification.accepted,
                attempts: attempt_num,
            });

            if verification.accepted {
                let output = verification
                    .report
                    .as_ref()
                    .map(executor::ExecutionReport::combined_output)
                    .unwrap_or_default();
                return Ok(RuntimeResponse::Text(output));
            }

            if verification.dangerous_attempt {
                // Stop immediately on any dangerous candidate rather than retrying.
                last_reason = "repair rejected: dangerous candidate".to_string();
                break;
            }

            last_feedback = Some(detail);
            last_reason = "repair not verified".to_string();
        }

        self.report_execution_limitation(report, &last_reason).await;
        Ok(RuntimeResponse::Text(report.combined_output()))
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

        let Some((exact_search_cmd, broad_search_cmd)) = build_missing_command_search(cmd) else {
            return Ok(RuntimeResponse::Text(format!(
                "'{}' is not a valid command name. check spelling or install manually.",
                cmd
            )));
        };

        // Search for package (works on Debian/Ubuntu - devcontainer)
        let search_result = self.executor.run(&exact_search_cmd).await?;

        if search_result.trim().is_empty() {
            // Try broader search
            let broad_search = self.executor.run(&broad_search_cmd).await?;

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
        provider: ipc::LlmProvider,
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
        if let Some(def) = self.parse_command_definition(input, provider).await? {
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
            if let Ok(report) = self
                .executor
                .run_detailed(&build_which_command(&def.name))
                .await
            {
                if report.succeeded() && !report.stdout.trim().is_empty() {
                    preview.push_str(&format!(
                        "\nwarning: '{}' is also a real command at {}. your version takes precedence in chat.",
                        def.name,
                        report.stdout.trim()
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
        provider: ipc::LlmProvider,
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
            .generate_with_provider(&def_prompt, provider)
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
                    let report = self.executor.run_detailed(&expanded).await?;
                    if !report.succeeded() {
                        let class = recovery::classify_failure(&report);
                        self.emit_execution_failed(&report, class);
                    }
                    Ok(RuntimeResponse::Text(report.combined_output()))
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
                            let report = self.executor.run_detailed(&expanded).await?;
                            if !report.succeeded() {
                                let class = recovery::classify_failure(&report);
                                self.emit_execution_failed(&report, class);
                            }
                            outputs.push(format!(
                                "{}. {}\n{}",
                                i + 1,
                                expanded,
                                report.combined_output().trim()
                            ));
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

/// The code carried by an explicit `#!exec` marker, if the reply is one.
fn exec_payload(response: &str) -> Option<&str> {
    if response.starts_with("#!exec\n") || response.starts_with("#!exec ") {
        Some(response.trim_start_matches("#!exec").trim())
    } else {
        None
    }
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

/// Decide whether raw user input should be probed as a CLI command via `which`.
///
/// Plain single-word natural language (e.g. "hello", "help", "thanks"),
/// slash commands (e.g. "/help"), multi-word prose, and quoted or metacharacter
/// inputs return `None` so they reach the LLM instead of `handle_missing_command`.
/// Only inputs that unambiguously look like a CLI command invocation are probed:
/// - A single valid command identifier containing a CLI-style separator or digit
///   (e.g. `docker-compose`, `python3.11`, `g++`, `my_tool`), or
/// - A valid command identifier followed by a CLI flag argument (e.g. `jq --version`,
///   `rg -i pattern`).
fn extract_command_probe(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.starts_with('/') {
        return None;
    }

    let mut words = trimmed.split_whitespace();
    let first = words.next()?;
    let safe_first = sanitize_missing_command_name(first)?;

    match words.next() {
        None => {
            // Reject purely alphabetic single words so greetings, "help", and
            // other single-word chat always reach the LLM.
            if safe_first.chars().all(|c| c.is_ascii_alphabetic()) {
                None
            } else {
                Some(safe_first)
            }
        }
        Some(second) => {
            let is_cli_flag = second.starts_with('-')
                && second.len() > 1
                && second
                    .chars()
                    .nth(1)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '-');
            if is_cli_flag {
                Some(safe_first)
            } else {
                None
            }
        }
    }
}

/// Build a shell-escaped `which` probe for `cmd`.
fn build_which_command(cmd: &str) -> String {
    format!("which -- {} 2>/dev/null", commands::shell_escape(cmd))
}

/// Detect whether shell execution output is a genuine "command not found"
/// diagnostic for the command invoked by `code`, rather than ordinary command
/// output that happens to contain the words "not found".
///
/// Recognized prefixes are the shells themselves (`sh`, `bash`, `dash`, `zsh`,
/// with or without a path) and the executor's generated script path (`*.sh`),
/// which is exactly how bash reports a missing command inside a script file.
fn detect_missing_command_in_output(code: &str, output: &str) -> Option<String> {
    let invoked = code.split_whitespace().next()?;
    // A path-qualified invocation is reported by name, and a trailing shell
    // operator is not part of the name.
    let invoked = invoked.trim_end_matches(&[';', '|', '&'][..]);
    let invoked = invoked.rsplit('/').next().unwrap_or(invoked);
    let invoked = sanitize_missing_command_name(invoked)?;

    for raw_line in output.lines() {
        let line = raw_line.trim();
        let Some((prefix, rest)) = line.split_once(':') else {
            continue;
        };
        let shell_name = prefix.rsplit('/').next().unwrap_or(prefix).trim();
        if !matches!(shell_name, "sh" | "bash" | "dash" | "zsh") && !shell_name.ends_with(".sh") {
            continue;
        }

        let reported = if let Some(before) = rest
            .strip_suffix(": command not found")
            .or_else(|| rest.strip_suffix(": not found"))
        {
            before.rsplit(':').next().map(str::trim)
        } else if let Some((_, after)) = rest.split_once("command not found:") {
            Some(after.trim())
        } else {
            None
        };

        if let Some(cmd) = reported {
            let cmd = cmd.rsplit('/').next().unwrap_or(cmd);
            if cmd == invoked {
                return Some(invoked.to_string());
            }
        }
    }

    None
}

/// Validate that a missing command name is a safe package/command identifier
/// before interpolating it into a shell search command.
fn sanitize_missing_command_name(cmd: &str) -> Option<&str> {
    if cmd.is_empty() || cmd.len() > 128 {
        return None;
    }
    let mut chars = cmd.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphanumeric() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+')) {
        return None;
    }
    Some(cmd)
}

fn build_missing_command_search(cmd: &str) -> Option<(String, String)> {
    let safe = sanitize_missing_command_name(cmd)?;
    let exact_pattern = commands::shell_escape(&format!("^{}$", safe));
    let broad_pattern = commands::shell_escape(safe);
    let exact = format!(
        "apt-cache search -- {} 2>/dev/null | head -5 || apt-cache search -- {} 2>/dev/null | head -5",
        exact_pattern, broad_pattern
    );
    let broad = format!(
        "apt-cache search -- {} 2>/dev/null | head -5",
        broad_pattern
    );
    Some((exact, broad))
}

/// Compact prompt asking the model to repair a failed program. Only the
/// model's *candidate* is ever executed, and only after the recovery layer's
/// static, safety and policy gates.
fn build_repair_prompt(
    original_code: &str,
    report: &executor::ExecutionReport,
    previous_rejection: Option<&str>,
) -> String {
    let class = recovery::classify_failure(report);
    let output = report.combined_output();
    let output = output.chars().take(2000).collect::<String>();
    let feedback = previous_rejection
        .map(|r| format!("\nPrevious candidate rejected: {r}\n"))
        .unwrap_or_default();
    format!(
        "A generated program failed. Propose a corrected version.\n\
         Failure class: {class}\n\
         Exit code: {exit:?}\n\
         Output:\n{output}\n\
         Original program:\n{original}{feedback}\n\
         Reply with ONLY a single fenced code block containing the corrected program. \
         Do not include reasoning.",
        class = class.as_str(),
        exit = report.exit_code,
        output = output,
        original = original_code,
        feedback = feedback,
    )
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
    fn test_exec_payload_requires_explicit_marker() {
        assert_eq!(exec_payload("#!exec\nls -la"), Some("ls -la"));
        assert_eq!(exec_payload("#!exec uname -a"), Some("uname -a"));
        // A fenced code block in a normal reply is text, never a command.
        assert_eq!(exec_payload("```bash\nrm -rf ~/old\n```"), None);
        assert_eq!(exec_payload("```\nls\n```"), None);
        assert_eq!(exec_payload("here is #!exec\nls"), None);
        assert_eq!(exec_payload("#!execute me"), None);
    }

    #[test]
    fn test_missing_command_sanitization_blocks_shell_injection() {
        // Legitimate command/package names are allowed.
        for valid in ["jq", "g++", "python3.11", "my_tool-1", "ripgrep"] {
            assert_eq!(sanitize_missing_command_name(valid), Some(valid));
            let (exact, broad) = build_missing_command_search(valid).unwrap();
            assert!(exact.contains(&format!("'^{valid}$'")));
            assert!(broad.contains(&format!("'{valid}'")));
        }

        // Shell metacharacters, flag injection, and whitespace must be rejected.
        for evil in [
            "",
            "foo'; id; echo '",
            "$(whoami)",
            "`id`",
            "foo | cat /etc/passwd",
            "foo && touch /tmp/pwned",
            "foo;rm -rf /",
            "foo\nid",
            "-o",
            "--version",
            "foo bar",
            "foo\"bar",
            "foo$HOME",
        ] {
            assert_eq!(
                sanitize_missing_command_name(evil),
                None,
                "expected {evil:?} to be rejected"
            );
            assert!(
                build_missing_command_search(evil).is_none(),
                "expected no shell command for {evil:?}"
            );
        }
    }

    async fn build_test_runtime(dir: &std::path::Path) -> MycelRuntime {
        std::fs::create_dir_all(dir).unwrap();
        let mut config = MycelConfig {
            context_path: dir.to_string_lossy().to_string(),
            ollama_url: "http://127.0.0.1:1".to_string(),
            openrouter_api_key: String::new(),
            ..Default::default()
        };
        config.mcp.enabled = false;
        config.sandbox.mode = config::SandboxMode::Host;

        let context_manager = context::ContextManager::new(&config).await.unwrap();
        let ai_router = ai::AiRouter::cloud_only(&config).await.unwrap();
        let executor = executor::CodeExecutor::new(&config).unwrap();
        let policy_evaluator = policy::PolicyEvaluator::with_defaults();
        let confirmations = policy::ConfirmationStore::new();
        let ui_factory = ui::UiFactory::new(&config).unwrap();
        let (event_bus, _) = tokio::sync::broadcast::channel(16);
        let mcp_manager = mcp::McpManager::new(&config.mcp, ".", event_bus.clone())
            .await
            .unwrap();
        let sync_service = sync::SyncService::new(&config, None, event_bus.clone())
            .await
            .unwrap();
        let selfdev =
            selfdev::SelfDevService::new(config.selfdev.clone(), dir.join("selfdev.json")).await;
        let command_registry = commands::CommandRegistry::load(dir.join("commands.json")).await;

        MycelRuntime {
            config,
            context_manager,
            ai_router,
            executor,
            policy_evaluator,
            confirmations,
            ui_factory,
            sync_service,
            mcp_manager,
            selfdev,
            command_registry,
            event_bus,
        }
    }

    fn temp_test_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("mycel-test-{}-{}", tag, uuid::Uuid::new_v4()))
    }

    fn unwrap_text(resp: RuntimeResponse) -> String {
        match resp {
            RuntimeResponse::Text(t) => t,
            RuntimeResponse::Stream(_) => panic!("expected RuntimeResponse::Text"),
        }
    }

    #[test]
    fn test_extract_command_probe_skips_single_word_chat_and_slash_commands() {
        for chat in [
            "hello",
            "hi",
            "hey",
            "help",
            "thanks",
            "status",
            "weather",
            "explain",
            "hello world",
            "explain rust ownership",
            "/help",
            "/status",
            "/clear",
            "/model claude",
            "",
            "   ",
        ] {
            assert_eq!(
                extract_command_probe(chat),
                None,
                "expected {chat:?} to reach the LLM instead of which/apt"
            );
        }

        // Genuine CLI-style probes are still recognized.
        assert_eq!(extract_command_probe("jq --version"), Some("jq"));
        assert_eq!(extract_command_probe("ripgrep -i foo"), Some("ripgrep"));
        assert_eq!(
            extract_command_probe("docker-compose"),
            Some("docker-compose")
        );
        assert_eq!(extract_command_probe("python3.11"), Some("python3.11"));
        assert_eq!(extract_command_probe("g++"), Some("g++"));
    }

    #[test]
    fn test_quoted_and_metacharacter_command_probes_rejected_and_escaped() {
        for evil in [
            "\"jq\" --version",
            "'jq' --version",
            "jq;id --version",
            "$(whoami) --help",
            "`id` --help",
            "foo|bar --baz",
            "foo&&bar --baz",
            "-v",
            "--help",
            "explain - why",
        ] {
            assert_eq!(
                extract_command_probe(evil),
                None,
                "expected quoted/metacharacter probe {evil:?} to be rejected"
            );
        }

        assert_eq!(
            build_which_command("foo'bar;id"),
            "which -- 'foo'\\''bar;id' 2>/dev/null"
        );
    }

    #[test]
    fn test_detect_missing_command_in_output_ignores_ordinary_output() {
        // Genuine shell diagnostics for the invoked command are detected.
        assert_eq!(
            detect_missing_command_in_output("jq --version", "sh: 1: jq: not found\n"),
            Some("jq".to_string())
        );
        assert_eq!(
            detect_missing_command_in_output(
                "my_tool --help",
                "/bin/bash: line 1: my_tool: command not found\n"
            ),
            Some("my_tool".to_string())
        );
        assert_eq!(
            detect_missing_command_in_output("rg foo", "zsh:1: command not found: rg\n"),
            Some("rg".to_string())
        );
        // The executor runs generated script files, so bash prefixes the
        // diagnostic with the script path rather than "bash".
        assert_eq!(
            detect_missing_command_in_output(
                "mycel-tool --version",
                "/home/u/.cache/mycel/code/e753.sh: line 1: mycel-tool: command not found\n"
            ),
            Some("mycel-tool".to_string())
        );
        // Path-qualified invocations and trailing operators are normalized.
        assert_eq!(
            detect_missing_command_in_output(
                "./mycel-tool --version",
                "sh: 1: ./mycel-tool: not found\n"
            ),
            Some("mycel-tool".to_string())
        );
        assert_eq!(
            detect_missing_command_in_output(
                "mycel-tool; echo done",
                "sh: 1: mycel-tool: not found\n"
            ),
            Some("mycel-tool".to_string())
        );

        // Ordinary command output containing "not found" or fake shell lines must not trigger.
        assert_eq!(
            detect_missing_command_in_output("echo 'file not found'", "file not found\n"),
            None
        );
        assert_eq!(
            detect_missing_command_in_output(
                "python3 -c 'print(\"key not found\")'",
                "key not found\n"
            ),
            None
        );
        assert_eq!(
            detect_missing_command_in_output(
                "echo 'bash: line 1: foo: command not found'",
                "bash: line 1: foo: command not found\n"
            ),
            None
        );
        assert_eq!(
            detect_missing_command_in_output(
                "curl -s https://example.com/missing",
                "404 Not Found\nresource not found\n"
            ),
            None
        );
        // A script-path prefix is only accepted for *.sh files.
        assert_eq!(
            detect_missing_command_in_output(
                "cat notes.txt",
                "notes.txt: line 1: xyz: command not found\n"
            ),
            None
        );
    }

    #[tokio::test]
    async fn test_single_word_chat_does_not_hit_missing_command_handler() {
        let dir = temp_test_dir("single-word");
        let runtime = build_test_runtime(&dir).await;

        for word in ["hello", "help", "/help"] {
            // With no LLM configured, reaching the LLM path returns an Err or
            // non-package-search reply; it must never return the missing-command
            // apt-cache message.
            if let Ok(resp) = runtime.process_input(word, "sess-chat").await {
                let text = unwrap_text(resp);
                assert!(
                    !text.contains("not found and no package available")
                        && !text.contains("not installed."),
                    "single-word input {word:?} was misrouted to handle_missing_command: {text}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_confirmation_works_on_both_input_paths() {
        let dir = temp_test_dir("confirm");
        let runtime = build_test_runtime(&dir).await;

        // Ensure sessions exist before setting pending commands.
        runtime
            .context_manager
            .get_context("sess-confirm-1")
            .await
            .unwrap();
        runtime
            .context_manager
            .get_context("sess-confirm-2")
            .await
            .unwrap();

        // Path 1: process_input — confirm with "y"
        runtime
            .context_manager
            .set_pending_command("sess-confirm-1", Some("echo confirmed_default".to_string()))
            .await
            .unwrap();
        let out = unwrap_text(runtime.process_input("y", "sess-confirm-1").await.unwrap());
        assert_eq!(out.trim(), "confirmed_default");

        // Path 1: process_input — deny with "n"
        runtime
            .context_manager
            .set_pending_command("sess-confirm-1", Some("echo should_not_run".to_string()))
            .await
            .unwrap();
        let out = unwrap_text(runtime.process_input("n", "sess-confirm-1").await.unwrap());
        assert_eq!(out, "action cancelled.");

        // Path 2: process_input_with_provider (non-Auto) — confirm with "y"
        runtime
            .context_manager
            .set_pending_command(
                "sess-confirm-2",
                Some("echo confirmed_provider".to_string()),
            )
            .await
            .unwrap();
        let out = unwrap_text(
            runtime
                .process_input_with_provider("y", "sess-confirm-2", ipc::LlmProvider::Cloud)
                .await
                .unwrap(),
        );
        assert_eq!(out.trim(), "confirmed_provider");

        // Path 2: process_input_with_provider (non-Auto) — deny with "n"
        runtime
            .context_manager
            .set_pending_command("sess-confirm-2", Some("echo should_not_run".to_string()))
            .await
            .unwrap();
        let out = unwrap_text(
            runtime
                .process_input_with_provider("n", "sess-confirm-2", ipc::LlmProvider::Cloud)
                .await
                .unwrap(),
        );
        assert_eq!(out, "action cancelled.");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_user_commands_and_command_management_work_with_explicit_provider() {
        let dir = temp_test_dir("user-cmds");
        let runtime = build_test_runtime(&dir).await;

        runtime
            .command_registry
            .define_full(
                "sayhi",
                "print a greeting",
                vec![commands::CommandStep::shell("echo hi {who}")],
                vec!["who".to_string()],
                None,
            )
            .await
            .unwrap();

        // Command management via explicit provider
        let list_out = unwrap_text(
            runtime
                .process_input_with_provider("list commands", "sess-cmd", ipc::LlmProvider::Cloud)
                .await
                .unwrap(),
        );
        assert!(list_out.contains("sayhi"));

        // User-defined command execution via explicit provider
        let run_out = unwrap_text(
            runtime
                .process_input_with_provider("sayhi world", "sess-cmd", ipc::LlmProvider::Cloud)
                .await
                .unwrap(),
        );
        assert!(run_out.contains("hi world"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_ordinary_output_looking_like_command_not_found_is_not_misrouted() {
        let dir = temp_test_dir("ordinary-output");
        let runtime = build_test_runtime(&dir).await;

        let out1 = unwrap_text(
            runtime
                .execute_code_with_policy("echo 'file not found'", "sess-out")
                .await
                .unwrap(),
        );
        assert_eq!(out1.trim(), "file not found");

        let out2 = unwrap_text(
            runtime
                .execute_code_with_policy("echo 'bash: line 1: foo: command not found'", "sess-out")
                .await
                .unwrap(),
        );
        assert_eq!(out2.trim(), "bash: line 1: foo: command not found");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end: a genuine shell "command not found" diagnostic still reaches
    /// the package-install offer through the real executor.
    #[tokio::test]
    async fn test_real_missing_command_reaches_package_offer() {
        let dir = temp_test_dir("real-missing");
        let runtime = build_test_runtime(&dir).await;

        let out = unwrap_text(
            runtime
                .execute_code_with_policy("mycel-nonexistent-tool", "sess-missing")
                .await
                .unwrap(),
        );
        assert!(
            out.contains("mycel-nonexistent-tool")
                && (out.contains("not installed")
                    || out.contains("no package available")
                    || out.contains("install with")),
            "missing command did not reach handle_missing_command: {out}"
        );
        assert!(
            !out.starts_with("bash: line"),
            "raw shell error leaked: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end: a CLI-shaped probe on the input path is still detected as a
    /// missing command, while single-word chat stays on the LLM path.
    #[tokio::test]
    async fn test_cli_shaped_probe_routes_to_missing_command_handler() {
        let dir = temp_test_dir("probe-missing");
        let runtime = build_test_runtime(&dir).await;

        let out = unwrap_text(
            runtime
                .process_input("mycel-nonexistent-tool --version", "sess-probe")
                .await
                .unwrap(),
        );
        assert!(
            out.contains("mycel-nonexistent-tool")
                && (out.contains("not installed")
                    || out.contains("no package available")
                    || out.contains("install with")),
            "CLI-shaped probe did not reach handle_missing_command: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn drain_events(
        rx: &mut tokio::sync::broadcast::Receiver<events::SystemEvent>,
    ) -> Vec<events::SystemEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn test_execute_code_failure_emits_events_without_retry() {
        let dir = temp_test_dir("recovery-events");
        let runtime = build_test_runtime(&dir).await;
        let mut rx = runtime.event_bus.subscribe();

        let out = unwrap_text(
            runtime
                .execute_code_with_policy("exit 1", "sess-recovery")
                .await
                .unwrap(),
        );
        assert!(out.contains("Some(1)"), "unexpected output: {out}");

        let events = drain_events(&mut rx);
        let failures = events
            .iter()
            .filter(|e| matches!(e, events::SystemEvent::ExecutionFailed { .. }))
            .count();
        // Exactly one failed run: the failure is reported, never silently retried.
        assert_eq!(failures, 1, "expected exactly one ExecutionFailed event");

        let decided: Vec<(String, bool, u32)> = events
            .iter()
            .filter_map(|e| match e {
                events::SystemEvent::RecoveryDecided {
                    decision,
                    verified,
                    attempts,
                    ..
                } => Some((decision.clone(), *verified, *attempts)),
                _ => None,
            })
            .collect();
        assert_eq!(decided.len(), 1, "expected one RecoveryDecided event");
        assert_eq!(decided[0].2, 0);
        assert!(!decided[0].1);
        // Host mode is never auto-repaired.
        assert_ne!(decided[0].0, "propose_repair");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_host_mode_never_proposes_repair_even_when_enabled() {
        let dir = temp_test_dir("recovery-host");
        let mut runtime = build_test_runtime(&dir).await;
        runtime.config.recovery.auto_repair = true;
        let mut rx = runtime.event_bus.subscribe();

        let out = unwrap_text(
            runtime
                .execute_code_with_policy("exit 1", "sess-host")
                .await
                .unwrap(),
        );
        assert!(!out.is_empty());

        let events = drain_events(&mut rx);
        let decisions: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                events::SystemEvent::RecoveryDecided { decision, .. } => Some(decision.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(decisions, vec!["abort_for_inspection".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_probe_with_missing_bwrap_does_not_misreport_missing_command() {
        let dir = temp_test_dir("probe-no-bwrap");
        let mut runtime = build_test_runtime(&dir).await;
        runtime.config.sandbox.mode = config::SandboxMode::Bwrap;
        runtime.config.sandbox.bwrap_path = "/nonexistent/bwrap".to_string();
        runtime.executor = executor::CodeExecutor::new(&runtime.config).unwrap();

        // Probe must skip when sandbox is unavailable rather than routing to
        // handle_missing_command (which would fail on missing bwrap or falsely
        // claim the command isn't installed). Because there is no LLM configured,
        // falling through to the LLM produces an AI error, NOT a missing-command
        // response or a sandbox-unavailable error.
        let res = runtime
            .process_input_with_provider(
                "mycel-missing-probe-cmd --version",
                "sess-probe",
                ipc::LlmProvider::Auto,
            )
            .await;
        match res {
            Ok(RuntimeResponse::Text(text)) => {
                assert!(
                    !text.contains("not installed") && !text.contains("sandbox unavailable"),
                    "probe should have been skipped, got: {text}"
                );
            }
            Ok(RuntimeResponse::Stream(_)) => {}
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    !msg.contains("sandbox unavailable"),
                    "probe failed on missing sandbox instead of falling through: {msg}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_exit_127_without_shell_diagnostic_does_not_guess_first_word() {
        let dir = temp_test_dir("exit-127");
        let runtime = build_test_runtime(&dir).await;

        let out = unwrap_text(
            runtime
                .execute_code_with_policy("cd /tmp && exit 127", "sess-127")
                .await
                .unwrap(),
        );
        assert!(
            !out.contains("'cd'") && !out.contains("install"),
            "must not misidentify 'cd' as a missing command, got: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_recovery_disabled_skips_recovery_decision_events() {
        let dir = temp_test_dir("recovery-disabled");
        let mut runtime = build_test_runtime(&dir).await;
        runtime.config.recovery.enabled = false;
        let mut rx = runtime.event_bus.subscribe();

        let out = unwrap_text(
            runtime
                .execute_code_with_policy("exit 1", "sess-disabled")
                .await
                .unwrap(),
        );
        assert!(out.contains("Some(1)"), "unexpected output: {out}");

        let events = drain_events(&mut rx);
        let decided_count = events
            .iter()
            .filter(|e| matches!(e, events::SystemEvent::RecoveryDecided { .. }))
            .count();
        assert_eq!(
            decided_count, 0,
            "expected no RecoveryDecided when disabled"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
