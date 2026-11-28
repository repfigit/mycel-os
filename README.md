# Mycel

**A distro-agnostic Linux AI agent governance & cost-routing sidecar.**

Mycel (`mycel-runtime`) is a Rust daemon that sits between your AI workflows and your Linux host—enforcing **bubblewrap (`bwrap`)** sandboxing, **tiered LLM cost routing** (`Local -> Cheap -> Frontier`), **3-layer spend governance**, and **structured policy confirmations** over a local Unix-domain socket.

## Core Capabilities

- **Bubblewrap (`bwrap`) Sandbox**: Untrusted AI-generated commands run inside isolated Linux namespaces (`--unshare-net`, read-only binds, `RLIMIT_AS`/`RLIMIT_NPROC`, execution timeouts) with explicit user confirmation for dangerous actions.
- **45+ LLM Providers + Local Inference**: Route across 45+ cloud providers (including OpenRouter, Anthropic, OpenAI, Venice.ai) and local backends (native `llama-cpp`, Ollama).
- **Tiered Routing & 3-Layer Spend Governance**:
  - Automatic `Local -> Cheap -> Frontier` tiering.
  - **Per-request cap**, **daily soft/hard budget**, and **monthly ceiling** enforced before requests leave the host.
- **Unix-Socket IPC & Policy Confirmations**: Authenticated local IPC (`$XDG_RUNTIME_DIR/mycel/runtime.sock` with `0700` parent directory and `0600` socket permissions), rate limiting, and structured confirmation prompts.
- **Personal Device Sync**: Transport-agnostic sync across your devices (Tailscale/LAN direct TCP by default; opt-in kernel WireGuard + STUN + zero-knowledge relay).

## Quick Start

### Prerequisites
- Linux (any distro with `bubblewrap` / `bwrap` and Rust toolchain)
- Optional: Ollama or `llama-cpp` for local inference

### Build & Run the Sidecar

```bash
git clone https://github.com/mycel-os/mycel
cd mycel/mycel-runtime

# Build and test
cargo build --release
cargo test

# Run in development mode
cargo run -- --dev --verbose
```

## Optional & Experimental Components

The following components are **optional or experimental** and are not required to use `mycel-runtime` as a Linux sidecar:

- **Void Linux (musl) ISO**: Optional offline-capable reference appliance (`scripts/build-mycel-iso.sh`).
- **Wayland Compositor & Shell (`mycel-compositor`, `mycel-shell`)**: Experimental Smithay-based Wayland compositor and shell.
- **Wine Integration (`--features wine`)**: Quarantined behind a compile-time feature flag (off by default).
- **Collective Intelligence (`--features collective`)**: Quarantined behind a compile-time feature flag (off by default); strict no-coin / no-token-speculation posture.

## Documentation

| Document | Description |
|----------|-------------|
| [Architecture](docs/ARCHITECTURE.md) | System design |
| [90-Day Roadmap](docs/ROADMAP_90_DAY.md) | Locked 90-day Dev + GTM plan & 3-layer spend governance |
| [Threat Model](docs/THREAT_MODEL.md) | Trust boundaries, `bwrap` isolation, and threat mitigations |
| [Decisions (ADRs)](DECISIONS.md) | Architecture Decision Records & amendments |
| [Quick Reference](docs/QUICK_REFERENCE.md) | Commands and configuration |

## License

MIT — see [LICENSE](LICENSE).
