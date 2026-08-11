# Mycel OS Quick Reference

## Development Environment Setup

### Option 1: Docker (Recommended)

```bash
# Clone the repository
git clone https://github.com/yourusername/mycel-os
cd mycel-os

# Start CLI development environment
./scripts/setup-dev.sh

# OR start GUI environment with VNC
./scripts/setup-dev.sh --gui

# OR with GPU support
./scripts/setup-dev.sh --gpu
```

### Option 2: VS Code Dev Container

1. Install "Dev Containers" extension
2. Open mycel-os folder
3. Click "Reopen in Container" when prompted

### Option 3: Manual Setup

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install Ollama
curl -fsSL https://ollama.com/install.sh | sh
ollama pull qwen3:8b

# Build Mycel OS
cd mycel-os
cargo build --release
```

---

## Connection Reference

| Service | Port | Credentials |
|---------|------|-------------|
| SSH (CLI) | 2222 | clay / clay |
| SSH (GUI) | 2223 | clay / clay |
| VNC | 5901 | clay |
| noVNC Web | 6080 | clay |
| Ollama API | 11434 | - |
| Dev Server | 3000 | - |

---

## Common Commands

### Building

```bash
# Build in debug mode
cargo build

# Build in release mode
cargo build --release

# Run tests
cargo test

# Run with logging
RUST_LOG=debug cargo run -- --dev
```

### Running

```bash
# Development mode (CLI)
./scripts/dev-run.sh

# Or directly
./target/release/clay-runtime --dev --verbose

# Cloud-only mode (no local LLM)
./target/release/clay-runtime --no-local-llm
```

### Docker

```bash
# Start environment
docker compose -f docker/docker-compose.yml up -d clay-dev

# View logs
docker logs -f clay-dev

# Enter container
docker exec -it clay-dev bash

# Stop environment
docker compose -f docker/docker-compose.yml down

# Rebuild images
docker compose -f docker/docker-compose.yml build --no-cache
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
├── clay-runtime/          # Core Rust daemon
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
- Production: `/etc/clay/config.toml`
- User overrides: `~/.config/clay/config.toml`

---

## Collective Intelligence

### NEAR (Testnet)

```bash
# Create testnet account
near create-account yourname.testnet --useFaucet

# Check balance
near state yourname.testnet

# View patterns
near view patterns.clay.testnet find_patterns '{"domain": "coding"}'
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
# Check if running
pgrep ollama

# View logs
journalctl -u ollama -f

# Restart
sudo systemctl restart ollama
```

### Out of memory
```bash
# Use smaller model
ollama pull qwen3:4b

# Update config
sed -i 's/qwen3:8b/qwen3:4b/' config/config.toml
```

### VNC not connecting
```bash
# Check VNC is running
docker exec clay-gui ps aux | grep vnc

# Restart VNC
docker exec clay-gui vncserver -kill :1
docker exec clay-gui vncserver :1
```

### Build errors
```bash
# Clean and rebuild
cargo clean
cargo build

# Update dependencies
cargo update
```

---

## Getting Help

- Documentation: `/docs/` folder
- Architecture: `docs/ARCHITECTURE.md`
- Dev Plan: `docs/DEVELOPMENT_PLAN.md`
- GUI Design: `docs/EVOLVING_GUI.md`

---

*Last updated: January 2026*
