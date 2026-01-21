# Mycel OS - Claude Code Implementation Guide

> **The intelligent network beneath everything.**

This is the definitive guide for Claude Code to build Mycel OS. Read completely before starting.

---

## Project Summary

**Mycel OS** is an AI-native operating system forked from Void Linux (musl). Users interact through natural language instead of traditional GUIs. The AI generates interfaces, writes code, and learns from a global collective of Mycel instances.

**Named after mycelium** - fungal networks connecting forests underground, sharing resources, adapting collectively.

---

## Current State (Accurate Assessment)

### Code Completeness: ~70% Scaffolded

| Module            | Lines  | Status    | Notes                            |
| ----------------- | ------ | --------- | -------------------------------- |
| `main.rs`         | 256    | ✅ Working | Entry point, CLI, runtime struct |
| `config/mod.rs`   | 140    | ✅ Working | Config loading, defaults         |
| `ai/mod.rs`       | 357    | ✅ Working | Ollama + Claude API clients      |
| `context/mod.rs`  | 201    | ✅ Working | Session management               |
| `intent/mod.rs`   | 152    | ✅ Working | Intent parsing types             |
| `executor/mod.rs` | 284    | ⚠️ Partial | Sandbox scaffolded               |
| `ipc/mod.rs`      | 229    | ⚠️ Partial | Socket server scaffolded         |
| `ui/mod.rs`       | 233    | ⚠️ Partial | Surface generation scaffolded    |
| `codegen/mod.rs`  | 217    | ⚠️ Partial | Code generation scaffolded       |
| `collective/`     | 282+   | ⚠️ Partial | NEAR/Bittensor stubs             |
| `sync/`           | 0      | ❌ Missing | Device mesh not started          |
| **Total**         | ~2,350 |           |                                  |

### What Works (Once Compiled)
- Config loading from TOML with env var overrides
- Ollama API client (generate, check availability)
- Claude API client (messages endpoint)
- Intent routing between local/cloud
- Session context management
- Conversation history tracking

### What Needs Work
1. **Verify compilation** - May have type mismatches or missing imports
2. **Test Ollama integration** - Need live Ollama instance
3. **IPC server** - Has structure but needs testing
4. **Executor sandbox** - Firejail/bubblewrap integration incomplete
5. **Collective** - NEAR/Bittensor are stubs only
6. **Sync** - Module doesn't exist yet

---

## Directory Structure

```
mycel-os/
├── mycel-runtime/              # Core Rust daemon (main focus)
│   ├── Cargo.toml              # Dependencies
│   └── src/
│       ├── main.rs             # Entry point, MycelRuntime struct
│       ├── config/mod.rs       # MycelConfig, loading
│       ├── ai/mod.rs           # AiRouter, Ollama, Claude
│       ├── context/mod.rs      # ContextManager, sessions
│       ├── intent/mod.rs       # Intent, ActionType
│       ├── executor/mod.rs     # CodeExecutor, sandbox
│       ├── ipc/mod.rs          # IpcServer, protocol
│       ├── ui/mod.rs           # UiFactory, Surface
│       ├── codegen/mod.rs      # Code generation
│       └── collective/         # Decentralized features
│           ├── mod.rs          # Main collective module
│           ├── near.rs         # NEAR Protocol client
│           ├── bittensor.rs    # Bittensor client
│           ├── patterns.rs     # Pattern storage
│           ├── privacy.rs      # Differential privacy
│           └── discovery.rs    # Pattern discovery
├── docker/                     # Development environment
│   ├── Dockerfile.void         # Void Linux container
│   ├── docker-compose.yml      # Service definitions
│   └── entrypoint-mycel.sh     # Container startup
├── scripts/
│   ├── setup-dev.sh            # Start dev environment
│   └── build-mycel-iso.sh      # Build bootable ISO
├── config/
│   └── config.toml             # Default configuration
├── tools/
│   └── mycel-cli.py            # Python CLI (dev testing)
└── docs/                       # Architecture documentation
```

---

## Development Environment

### Option 1: Docker (Recommended)

```bash
# Clone and start the dev container
git clone <repo> mycel-os
cd mycel-os
docker compose -f docker/docker-compose.yml up -d mycel-dev
docker compose -f docker/docker-compose.yml exec mycel-dev bash

# Inside container:
cd /workspace/mycel-os/mycel-runtime
cargo build
cargo run -- --dev
```

### Option 2: Local Development

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install Ollama
curl -fsSL https://ollama.com/install.sh | sh
ollama pull tinydolphin

# Build
cd mycel-runtime
cargo build
```

---

## Building and Testing ISOs

### Quick ISO Build

```bash
# Build minimal bootable ISO (~5-10 min)
./scripts/build-iso.sh quick
```

### Full ISO Build

```bash
# Build complete ISO with all packages (~15-30 min)
./scripts/build-iso.sh full
```

### Test ISO in QEMU

```bash
# Serial console (headless)
./scripts/test-iso.sh

# VNC mode (if you need GUI)
./scripts/test-iso.sh output/mycel-os-*.iso vnc
```

### SSH into Running VM

```bash
ssh -p 2222 root@localhost
```

See `docker/` for the Docker-based build environment.

---

## Phase 1: Get It Running (Priority)

### Step 1: Verify Compilation

```bash
cd mycel-runtime
cargo build 2>&1 | head -100
```

Fix any errors. Common issues:
- Missing imports (add `use` statements)
- Type mismatches (check return types)
- Lifetime issues (add `'static` or clone)

### Step 2: Start Ollama

```bash
# Install Ollama
curl -fsSL https://ollama.com/install.sh | sh

# Pull a model
ollama pull phi3:mini    # Fast, small
# OR
ollama pull phi3:medium  # Better quality

# Verify running
curl http://localhost:11434/api/tags
```

### Step 3: Run in Dev Mode

```bash
cargo run -- --dev --verbose
```

Should see:
```
    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗
    ...
    Mycel Runtime starting...
    Configuration loaded...
    Local LLM (Ollama) is available
    Mycel Runtime ready. The network grows.
```

### Step 4: Test with CLI

```bash
# In another terminal
python3 tools/mycel-cli.py
mycel> hello
mycel> list files in current directory
mycel> quit
```

---

## Phase 2: Core Functionality

### 2.1 IPC Server (src/ipc/mod.rs)

The IPC module needs to:
- Create Unix socket at `/run/mycel/runtime.sock` (or `/tmp/mycel-dev.sock` in dev)
- Accept JSON requests
- Route to MycelRuntime.process_input()
- Return JSON responses

Test:
```bash
# After runtime is running
echo '{"type":"chat","session_id":"test","input":"hello"}' | nc -U /tmp/mycel-dev.sock
```

### 2.2 Code Executor (src/executor/mod.rs)

The executor needs to:
- Take generated code (Python/Bash)
- Run in sandbox (firejail or bubblewrap)
- Capture stdout/stderr
- Enforce timeout
- Return output

Test:
```bash
mycel> count files in my home directory
# Should generate Python, run it safely, return count
```

### 2.3 Intent Parsing

Current flow:
1. User input → AiRouter.parse_intent()
2. LLM returns JSON with action_type
3. Route to appropriate handler

Improve:
- Better prompt engineering
- Handle malformed JSON
- Confidence thresholds

---

## Phase 3: Device Sync (New Module)

Create `src/sync/mod.rs`:

```rust
//! Device mesh synchronization
//!
//! Syncs config, patterns, and files between user's Mycel devices
//! using WireGuard for transport and CRDTs for conflict-free merge.

use anyhow::Result;

pub struct SyncService {
    // WireGuard mesh
    // Hypercore logs
    // CRDT state
}

impl SyncService {
    pub async fn new(config: &MycelConfig) -> Result<Self> { todo!() }
    pub async fn start(&mut self) -> Result<()> { todo!() }
    pub async fn pair_device(&self, code: &str) -> Result<PeerInfo> { todo!() }
    pub async fn sync_now(&self) -> Result<SyncStatus> { todo!() }
}
```

Key components:
- WireGuard mesh network
- Device pairing (QR code / recovery phrase)
- Hypercore append-only logs
- CRDT merge (LWW, GSet, ORSet)

---

## Phase 4: Collective Intelligence

### NEAR Protocol (src/collective/near.rs)

Current: Stub with contract interface
Needed:
- Actual NEAR SDK integration
- Pattern registry contract calls
- Micropayment handling

### Bittensor (src/collective/bittensor.rs)

Current: Stub with synapse types
Needed:
- Subnet registration
- Miner/validator logic
- TAO rewards

### Privacy (src/collective/privacy.rs)

Current: Basic PII detection
Needed:
- Differential privacy (Gaussian mechanism)
- Secure aggregation
- Privacy budget tracking

---

## Key APIs

### Ollama API

```rust
// POST http://localhost:11434/api/generate
{
    "model": "phi3:medium",
    "prompt": "...",
    "stream": false
}
// Response: { "response": "..." }
```

### Claude API

```rust
// POST https://api.anthropic.com/v1/messages
// Headers: x-api-key, anthropic-version: 2023-06-01
{
    "model": "claude-sonnet-4-20250514",
    "max_tokens": 4096,
    "messages": [{"role": "user", "content": "..."}]
}
// Response: { "content": [{"text": "..."}] }
```

### IPC Protocol

```json
// Request
{"type": "chat", "session_id": "abc123", "input": "hello"}

// Response
{"type": "text", "content": "Hello! How can I help?"}
// OR
{"type": "code", "code": "print('hi')", "output": "hi"}
// OR
{"type": "error", "message": "..."}
```

---

## Environment Variables

```bash
# Required for cloud AI
ANTHROPIC_API_KEY=sk-ant-...

# Optional overrides
OLLAMA_URL=http://localhost:11434
MYCEL_LOCAL_MODEL=phi3:medium
RUST_LOG=debug
RUST_BACKTRACE=1
```

---

## Common Tasks

### Add a new module

1. Create `src/newmodule/mod.rs`
2. Add `mod newmodule;` to `main.rs`
3. Add types/functions
4. Use in MycelRuntime

### Add a dependency

1. Add to `Cargo.toml`
2. Run `cargo build`
3. Import with `use`

### Test a single module

```bash
cargo test --lib module_name
```

### Build release

```bash
cargo build --release
# Binary at target/release/mycel-runtime
```

---

## File Paths

| Purpose | Dev Mode               | Production                |
| ------- | ---------------------- | ------------------------- |
| Config  | `./config/config.toml` | `/etc/mycel/config.toml`  |
| Data    | `./mycel-data/`        | `/var/lib/mycel/`         |
| Code    | `./mycel-code/`        | `/var/cache/mycel/code/`  |
| Socket  | `/tmp/mycel-dev.sock`  | `/run/mycel/runtime.sock` |
| Logs    | stdout                 | `/var/log/mycel/`         |

---

## Success Milestones

### Milestone 1: Compiles and Runs ⬜
- [ ] `cargo build` succeeds
- [ ] `cargo run -- --dev` starts without crash
- [ ] Connects to Ollama

### Milestone 2: Basic Chat ⬜
- [ ] CLI connects via IPC
- [ ] Messages route to Ollama
- [ ] Responses return to CLI
- [ ] Conversation history works

### Milestone 3: Code Execution ⬜
- [ ] AI generates Python code
- [ ] Code runs in sandbox
- [ ] Output captured and returned
- [ ] Timeouts enforced

### Milestone 4: Device Sync ⬜
- [ ] Two instances pair
- [ ] Config syncs
- [ ] Works offline

### Milestone 5: Collective ⬜
- [ ] Patterns stored on IPFS
- [ ] Registered on NEAR testnet
- [ ] Privacy layer functional

---

## Architecture Decisions

| Decision     | Choice                | Why                       |
| ------------ | --------------------- | ------------------------- |
| Base OS      | Void Linux (musl)     | Independent, simple, fast |
| Language     | Rust                  | Safe, fast, good async    |
| Local AI     | Ollama                | Easy setup, good models   |
| Cloud AI     | Claude                | Best reasoning            |
| Config       | TOML                  | Human readable            |
| IPC          | Unix socket + JSON    | Simple, fast, secure      |
| Sandbox      | Firejail/bubblewrap   | Linux native              |
| Sync         | WireGuard + Hypercore | Encrypted, CRDT-friendly  |
| Coordination | NEAR                  | Low fees, Rust SDK        |
| AI eval      | Bittensor             | Built for AI              |

---

## Quick Reference

```bash
# Development
cargo build                    # Compile
cargo run -- --dev            # Run dev mode
cargo test                    # Run tests
cargo clippy                  # Lint

# Docker
./scripts/setup-dev.sh        # Start environment
ssh mycel@localhost -p 2222   # Connect (password: mycel)

# ISO
./scripts/build-mycel-iso.sh  # Build bootable ISO
```

---

## Next Action

**Start here:**

```bash
cd mycel-runtime
cargo build
```

Fix any compilation errors, then proceed to testing with Ollama.

---

*The network grows. Start with compilation.*
