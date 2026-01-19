# Mycel OS - Claude Code Context

> The intelligent network beneath everything.

## Project Overview

Mycel OS is an AI-native operating system forked from Void Linux (musl). Named after mycelium - the underground fungal networks that connect forests and share resources - Mycel OS instances form a similar network: decentralized, resilient, and collectively intelligent.

## Key Architectural Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Base OS | Void Linux (musl) | Independent, simple init (runit), fast, systemd-free |
| Language | Rust | Safety, performance, good async |
| Local AI | Ollama | Easy model management, good API |
| Cloud AI | Claude API | Best reasoning capabilities |
| Coordination | NEAR Protocol | Low fees, human-readable accounts, Rust SDK |
| AI Evaluation | Bittensor | Built for AI workloads, economic incentives |
| Storage | IPFS + Hypercore | Content-addressed, local-first sync |
| Device Mesh | WireGuard | Fast, secure, NAT traversal |

## Project Structure

```
mycel-os/
├── mycel-runtime/          # Core daemon (Rust)
│   └── src/
│       ├── ai/             # LLM integration (Ollama, Claude)
│       ├── collective/     # NEAR, Bittensor, patterns
│       ├── context/        # Session management
│       ├── executor/       # Sandboxed code execution
│       ├── intent/         # Intent parsing
│       ├── ipc/            # Unix socket server
│       ├── sync/           # Device mesh sync (TODO)
│       └── ui/             # Surface generation
├── docker/                 # Development environment
├── scripts/                # Build and setup scripts
├── config/                 # Default configuration
├── tools/                  # CLI tools (Python, for dev)
└── docs/                   # Documentation
```

## Build Commands

```bash
# Start development environment
./scripts/setup-dev.sh

# Build runtime
cd mycel-runtime
cargo build --release

# Build ISO (in container, requires privileged)
./scripts/build-mycel-iso.sh

# Run tests
cargo test
```

## Current Focus

Phase 0-1: Getting bootable base image and working runtime

### Immediate Tasks
- [ ] Fix Rust compilation (config module needs MycelConfig)
- [ ] Implement basic Ollama client
- [ ] Test IPC server
- [ ] Create runit service files

## Key Concepts

### The Mycelium Metaphor
- **Underground network** → Decentralized OS instances
- **Shares nutrients** → Shares patterns and knowledge
- **No central control** → No central server
- **Connects trees** → Connects your devices (mesh)
- **Adapts to environment** → Evolves from collective use

### Three-Layer Intelligence
1. **Local** - Ollama running on device (fast, private)
2. **Cloud** - Claude API for complex reasoning (smart, costs money)
3. **Collective** - Learned patterns from network (shared knowledge)

### Device Mesh
Your devices sync securely via WireGuard:
- End-to-end encrypted
- Works offline, syncs when connected
- No central server
- Apps, config, AI context all sync

### Collective Intelligence
Instances share patterns (not private data):
- NEAR: Registry, micropayments, identity
- Bittensor: Quality evaluation, rewards
- IPFS: Pattern storage
- Differential privacy protects individuals

## Environment Variables

```bash
ANTHROPIC_API_KEY=sk-ant-...  # For cloud AI
RUST_LOG=info                  # Logging level
RUST_BACKTRACE=1              # Debug backtraces
```

## File Locations

- Config: `/etc/mycel/config.toml`
- Data: `/var/lib/mycel/`
- Logs: `/var/log/mycel/`
- Socket: `/run/mycel/runtime.sock`

## Development Notes

- Docker container is for development only, not the target
- Target is bootable Void Linux ISO
- Use `ssh mycel@localhost -p 2222` (password: mycel)
- Ollama runs on port 11434

## Links

- Void Linux: https://voidlinux.org/
- NEAR Protocol: https://near.org/
- Bittensor: https://bittensor.com/
- Ollama: https://ollama.com/
