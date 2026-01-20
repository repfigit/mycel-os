# Mycel OS - Agent Instructions

This document provides essential instructions for AI agents operating in the Mycel OS repository.

## 1. Project Context
**Mycel OS** is an AI-native operating system (forked from Void Linux) where users interact via natural language.
- **Core Runtime**: Rust daemon (`mycel-runtime`).
- **Architecture**: Async event loop, local LLM (Ollama) + Cloud LLM (Claude), Linux namespaces for sandboxing.
- **Target**: Linux (musl).

## 2. Build & Test Commands

All Rust commands must be run from the `mycel-runtime/` directory.

### Build
- **Standard Build**: `cargo build`
- **Check (Fast)**: `cargo check`
- **Release Build**: `cargo build --release`
- **Watch Mode**: `cargo watch -x check`

### Run
- **Dev Mode (Required for agents)**: `cargo run -- --dev --verbose`
  - `--dev`: Runs without root privileges, uses local paths.
  - `--verbose`: Enables debug logging.

### Test
- **Run All Tests**: `cargo test`
- **Run Single Test**: `cargo test --lib <module_name> -- <test_name>`
  - Example: `cargo test --lib intent -- parse_simple_intent`
- **Lint**: `cargo clippy` (Fix warnings immediately)
- **Format**: `cargo fmt`

## 3. Code Style & Conventions

### General Rust
- **Formatting**: Strictly follow `cargo fmt`.
- **Linting**: Ensure `cargo clippy` passes without warnings.
- **Async**: Use `tokio` runtime. Most I/O-bound functions should be `async`.
- **Paths**: Use `std::path::PathBuf` for file system paths.

### Architecture & patterns
- **Error Handling**: 
  - Application logic: Use `anyhow::Result` for easy error propagation.
  - Library/Modules: Use `thiserror` for structured, typed errors.
  - Never use `.unwrap()` in production code; use `?` or `expect("context")`.
- **Logging**: Use `tracing` crate (`info!`, `debug!`, `error!`, `warn!`). Do not use `println!` for logging.
- **Configuration**: Use `config` crate. Define structs in `src/config/mod.rs` with `serde`.

### Module Structure
- **New Modules**: Create `src/<module_name>/mod.rs`. Register in `main.rs`.
- **Imports**: Group imports: `std`, external crates, internal modules (`crate::...`).
- **Visibility**: Default to private. Use `pub` only when necessary for external API.

## 4. Specific Implementation Details

### IPC & Communication
- **Protocol**: JSON over Unix Domain Sockets (`/tmp/mycel-dev.sock` in dev).
- **Serialization**: Use `serde` and `serde_json`.

### AI & Intents
- **Router**: `AiRouter` in `src/ai/mod.rs` handles switching between Local and Cloud LLMs.
- **Intents**: Define new actions in `src/intent/mod.rs`.

### Sandbox Execution
- Code execution happens in `src/executor/mod.rs`.
- **Safety**: Always assume code is untrusted. Use `nix` crate for isolation.

## 5. Workflow Rules for Agents

1.  **Verification**: After every code change, run `cargo check` inside `mycel-runtime`.
2.  **Safety**: Never run `cargo run` without the `--dev` flag.
3.  **Context**: Always read `CLAUDE.md` and `README.md` if unsure about architecture.
4.  **Dependencies**: Check `Cargo.toml` before adding new crates. Prefer existing dependencies.
5.  **Paths**: Always use absolute paths when using file tools (e.g., `/workspaces/mycel-os/...`).
6.  **Refactoring**: Do not refactor generic logic without understanding the specific Mycel context (e.g., this is a Linux OS, not a generic web app).

## 6. Common Issues & Fixes

- **Missing Imports**: If `cargo check` fails with "unknown type", check if `use crate::...` is needed.
- **Async Mismatch**: If "future cannot be sent between threads", check for non-Send types held across `.await` points.
- **Ollama Connection**: If LLM fails, verify Ollama is running (`curl http://localhost:11434/api/tags`).
