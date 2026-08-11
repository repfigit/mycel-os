# Mycel OS

**An AI-native operating system. The intelligent network beneath everything.**

Mycel OS is a fork of [Void Linux](https://voidlinux.org/) (musl) that integrates AI at the deepest level. Instead of clicking through windows and menus, you express intent through conversation. The system generates interfaces, writes programs on-the-fly, and learns from a global collective of Mycel instances.

*Named after mycelium - the underground fungal networks that connect forests, share resources, and adapt collectively. Mycel OS instances form a similar network: decentralized, resilient, and collectively intelligent.*

## Vision

Traditional operating systems are isolated machines. Mycel OS instances form a living network:

- **The AI is not an app** - it's the operating system itself
- **No fixed interface** - surfaces appear and disappear based on context
- **Collective intelligence** - instances learn from each other via NEAR/Bittensor
- **Personal mesh** - your devices sync securely, no cloud required
- **Windows compatibility** - run Windows apps through integrated Wine

## Quick Start

### Prerequisites
- Docker (for cross-platform development)
- 8GB+ RAM
- x86_64 processor

### Run the Runtime (fastest way to try it)

```bash
# Clone the repository
git clone https://github.com/mycel-os/mycel
cd mycel

# Install Ollama and pull the default model
curl -fsSL https://ollama.com/install.sh | sh
ollama serve &
ollama pull qwen3:8b

# Set up MCP system tools (optional but recommended)
./scripts/setup-mcp-servers.sh

# Build and run - drops you into a mycel> prompt
./scripts/dev-run.sh
```

See [Getting Started](docs/GETTING_STARTED.md) for daemon mode, CLI client,
model selection, and troubleshooting.

### Building a Bootable ISO

```bash
# Build the runtime
cd mycel-runtime
cargo build --release

# Build a bootable ISO (Void Linux base)
cd ..
./scripts/build-mycel-iso.sh
```

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         USER                                     │
│         "Find duplicate files in my photos"                      │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│                      MYCEL RUNTIME                               │
│       Intent Parser → AI Router → Code Generator                 │
└────────────────────────────┬────────────────────────────────────┘
                             │
          ┌──────────────────┼──────────────────┐
          ▼                  ▼                  ▼
┌──────────────────┐ ┌──────────────┐ ┌──────────────────┐
│    Local LLM     │ │  OpenRouter  │ │    Collective    │
│    (Ollama)      │ │   (Cloud)    │ │   (NEAR/BT)      │
└──────────────────┘ └──────────────┘ └──────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│                  VOID LINUX BASE (musl)                          │
│            Linux Kernel │ runit │ XBPS │ musl                    │
└─────────────────────────────────────────────────────────────────┘
```

## The Mycelium Metaphor

```
┌─────────────────────────────────────────────────────────────────┐
│                                                                  │
│   MYCELIUM (Nature)              MYCEL OS (Technology)          │
│                                                                  │
│   Underground network       ←→   Decentralized OS instances     │
│   Shares nutrients          ←→   Shares patterns & knowledge    │
│   No central control        ←→   No central server              │
│   Adapts to environment     ←→   Evolves from collective use    │
│   Connects separate trees   ←→   Connects your devices          │
│   "Wood wide web"           ←→   Collective intelligence        │
│   Survives damage           ←→   Resilient, offline-capable     │
│   Grows toward resources    ←→   AI seeks useful patterns       │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Why Void Linux?

- **Independent** - Not derived from Debian, Red Hat, or Arch
- **Simple init** - runit is ~500 lines of C, easy to understand
- **Fast boot** - Seconds, not minutes
- **musl libc** - Smaller, more secure binaries
- **Rolling release** - Always current
- **Systemd-free** - Simpler to customize

## Key Features

### Personal Mesh
Your Mycel devices sync securely over encrypted WireGuard tunnels:
- End-to-end encrypted, zero-knowledge
- Works on local network and over internet
- No central server, no cloud dependency
- Apps, config, documents, AI context - all synced

### Collective Intelligence
Instances share patterns (not private data) via decentralized networks:
- **NEAR Protocol** - Pattern registry, micropayments, identity
- **Bittensor** - AI-powered quality evaluation, rewards
- **IPFS** - Content-addressed pattern storage
- **Differential privacy** - Learn collectively without exposing individuals

### Evolving Interface
The GUI isn't designed - it emerges:
- Starts minimal (conversation panel)
- Learns your usage patterns locally
- Adopts proven patterns from the collective
- AI generates novel layouts when needed

## Documentation

| Document | Description |
|----------|-------------|
| [Architecture](docs/ARCHITECTURE.md) | System design |
| [Development Plan](docs/DEVELOPMENT_PLAN.md) | Roadmap |
| [Collective Intelligence](docs/COLLECTIVE_INTELLIGENCE.md) | NEAR + Bittensor |
| [Network Topology](docs/NETWORK_TOPOLOGY.md) | How instances connect |
| [Evolving GUI](docs/EVOLVING_GUI.md) | Self-adapting interface |
| [Quick Reference](docs/QUICK_REFERENCE.md) | Commands |

## Project Structure

```
mycel-os/
├── mycel-runtime/          # Core AI runtime (Rust)
│   └── src/
│       ├── ai/             # LLM integration
│       ├── collective/     # NEAR/Bittensor
│       ├── context/        # Session management
│       ├── executor/       # Code sandbox
│       ├── sync/           # Device mesh sync
│       └── intent/         # Intent parsing
├── docker/                 # Development containers
├── scripts/                # Build and setup
├── config/                 # Configuration
└── docs/                   # Documentation
```

## Status

🚧 **Early Development** — core runtime working end-to-end

### Working (verified)
- [x] Mycel Runtime daemon (Rust, ~10,600 lines, 60 tests passing)
- [x] Local LLM integration (Ollama, qwen3:8b default) with warmup
- [x] Cloud routing via OpenRouter with provider selection
- [x] IPC server (Unix socket, auth, rate limiting, streaming)
- [x] Policy-gated code execution (dangerous commands need confirmation)
- [x] Session memory (conversation history in prompts)
- [x] MCP system tools (void-tools: 14 tools) + built-in shell_execute
- [x] Device sync foundation (mDNS discovery, WireGuard keys, CRDT)

### In Progress
- [ ] Bootable base image (scripts exist, unverified end-to-end)
- [ ] Device mesh sync (foundation laid)

### Planned
- [ ] NEAR/Bittensor collective (stubs only)
- [ ] Windows compatibility (Wine)
- [ ] Evolving GUI (Wayland/Smithay, not started)

## Philosophy

> "A forest is not a collection of trees. It's a network connected by mycelium, sharing resources, communicating, adapting as one organism."

Mycel OS applies this insight to computing. Your devices aren't isolated machines - they're nodes in your personal network. Mycel instances worldwide aren't strangers - they're part of a collective that learns and grows together.

The AI is not an app. **The AI is the network.**

## Contributing

Early development - not yet accepting contributions. Watch for updates.

## License

TBD - Likely Apache 2.0 or MIT

---

*"The intelligent network beneath everything."*
