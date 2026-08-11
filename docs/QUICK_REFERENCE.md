# Mycel OS Quick Reference

## Boot Up (TL;DR)

```bash
# 1. Install Ollama + model
curl -fsSL https://ollama.com/install.sh | sh
ollama serve &
ollama pull qwen3:8b

# 2. Set up MCP system tools (optional)
./scripts/setup-mcp-servers.sh

# 3. Build + run, drops into mycel> prompt
./scripts/dev-run.sh
```

Daemon mode (for the CLI client / production-style):
```bash
./scripts/dev-run.sh --daemon          # note the IPC auth token in the log
export MYCEL_AUTH_TOKEN="<token>"
python3 tools/mycel-cli.py chat
```

## Manual Setup

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install Ollama + model
curl -fsSL https://ollama.com/install.sh | sh
ollama serve &
ollama pull qwen3:8b

# Build + run
cd mycel-runtime
cargo build --release
./target/release/mycel-runtime --dev --verbose   # interactive mycel>
```

## Runtime Flags

| Flag | Effect |
|------|--------|
| `--dev` | No root needed, local paths, socket at /tmp/mycel-dev.sock |
| `--daemon` | Background mode, no interactive CLI |
| `--verbose` | Debug logging |
| `--no-local-llm` | Cloud-only (needs OPENROUTER_API_KEY) |
| `--no-collective` | Skip collective network |

## Connection Reference

| Service | Location | Credentials |
|---------|----------|-------------|
| IPC socket (dev) | /tmp/mycel-dev.sock | token in daemon log |
| IPC socket (prod) | /run/mycel/runtime.sock | /etc/mycel/token |
| Ollama API | localhost:11434 | - |

---

## Common Commands

### Building (from mycel-runtime/)

```bash
cargo build              # debug
cargo build --release    # release
cargo test               # 60 tests
cargo clippy             # lint (should be 0 warnings)
RUST_LOG=debug cargo run -- --dev   # run with logging
```

### Running

```bash
# Interactive CLI (recommended entry point)
./scripts/dev-run.sh

# Daemon + socket
./scripts/dev-run.sh --daemon

# Or directly (from mycel-runtime/)
./target/release/mycel-runtime --dev --verbose
./target/release/mycel-runtime --dev --daemon

# Cloud-only mode (no local LLM)
OPENROUTER_API_KEY=sk-or-... ./target/release/mycel-runtime --dev --no-local-llm
```

### CLI Client (daemon mode)

```bash
export MYCEL_AUTH_TOKEN="<token from daemon log>"
python3 tools/mycel-cli.py chat      # interactive
python3 tools/mycel-cli.py status    # one-shot status
```

### Ollama

```bash
# List models
ollama list

# Pull a model
ollama pull qwen3:8b
ollama pull mistral:7b
ollama pull llama3.2:3b

# Run model directly
ollama run qwen3:8b "Hello!"

# Check API
curl http://localhost:11434/api/tags
```

### Wine (GUI Environment)

```bash
# Test Wine
wine notepad

# Install common dependencies
winetricks corefonts vcrun2019

# Run Windows app
wine /path/to/app.exe

# Configure Wine
winecfg
```

---

## Project Structure

```
mycel-os/
├── mycel-runtime/         # Core Rust daemon
│   └── src/
│       ├── main.rs        # Entry point
│       ├── ai/            # LLM integration
│       ├── collective/    # NEAR + Bittensor
│       ├── context/       # Session management
│       ├── executor/      # Code sandbox
│       ├── intent/        # Intent parsing
│       ├── ipc/           # IPC server
│       └── ui/            # Surface generation
├── config/                # Configuration files
├── docker/                # Docker environment
├── docs/                  # Documentation
├── scripts/               # Build & run scripts
└── tools/                 # CLI utilities
```

---

## Configuration

### Environment Variables

```bash
# Required for cloud AI
export OPENROUTER_API_KEY="sk-or-..."

# Optional
export OLLAMA_URL="http://localhost:11434"
export MYCEL_LOCAL_MODEL="qwen3:8b"
export RUST_LOG="info"  # debug, info, warn, error
```

### Config File Locations

- Development: `./config/config.toml`
- Production: `/etc/mycel/config.toml`
- Data (dev): `./mycel-data/`; (prod): `/var/lib/mycel/`

---

## Collective Intelligence

### NEAR (Testnet)

```bash
# Create testnet account
near create-account yourname.testnet --useFaucet

# Check balance
near state yourname.testnet

# View patterns
near view patterns.mycel.testnet find_patterns '{"domain": "coding"}'
```

### Bittensor

```bash
# Check wallet
btcli wallet overview

# View subnet
btcli subnet list

# Check rewards
btcli stake show
```

---

## Troubleshooting

### Ollama won't start
```bash
pgrep ollama          # already running?
ollama serve          # start manually, watch output
```

### First request very slow (~30-60s)
One-time model load. A background warmup hides this; slow-every-request
means Ollama is unloading the model between calls.

### void-tools MCP server failed
```bash
./scripts/setup-mcp-servers.sh   # create per-server Python venvs
```
Check the daemon log for the underlying stderr.

### Out of memory
```bash
ollama pull qwen3:4b
sed -i 's/qwen3:8b/qwen3:4b/' config/config.toml
```

### Build errors
```bash
cargo clean && cargo build   # clean rebuild
cargo update                 # update deps
# Note: rustls is used for TLS - no OpenSSL/pkg-config needed
```

---

## Getting Help

- Documentation: `/docs/` folder
- Architecture: `docs/ARCHITECTURE.md`
- Dev Plan: `docs/DEVELOPMENT_PLAN.md`
- GUI Design: `docs/EVOLVING_GUI.md`

---

*Last updated: January 2026*
