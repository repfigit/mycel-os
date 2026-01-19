# Codespace Development Guide

This guide covers developing Mycel OS in GitHub Codespaces.

---

## Quick Start

### 1. Create Codespace

Click the green "Code" button on GitHub, then "Create codespace on main".

**Recommended machine type:** 4-core (minimum) or 8-core (better)

### 2. Wait for Setup

The first time takes ~3-5 minutes to:
- Install Rust toolchain
- Install Ollama
- Install dependencies
- Set up aliases

### 3. Pull an AI Model

```bash
ollama pull phi3:mini      # Fast, 2GB
# OR
ollama pull phi3:medium    # Better, 8GB
```

### 4. Build and Run

```bash
# Build
mb                         # Alias for cargo build

# Run in dev mode
mr                         # Alias for cargo run -- --dev --verbose
```

---

## Useful Aliases

These are set up automatically:

| Alias | Command | Description |
|-------|---------|-------------|
| `mb` | `cargo build` | Build the project |
| `mr` | `cargo run -- --dev --verbose` | Run in dev mode |
| `mt` | `cargo test` | Run tests |
| `mc` | `cargo check` | Fast compile check |
| `mw` | `cargo watch -x check` | Watch mode |
| `ollama-start` | Start Ollama service | |
| `ollama-stop` | Stop Ollama service | |
| `ollama-status` | Check Ollama status | |
| `ollama-logs` | View Ollama logs | |
| `ollama-pull MODEL` | Pull a model | |
| `mycel ARGS` | Run CLI tool | |

---

## Machine Type Recommendations

| Type | Cores | RAM | Use Case |
|------|-------|-----|----------|
| Basic | 2-core | 4GB | ❌ Too slow |
| Standard | 4-core | 8GB | ✅ Minimum viable |
| Large | 8-core | 16GB | ✅ Recommended |
| XL | 16-core | 32GB | ✅ Fast builds + large models |

**Why 4+ cores?**
- Rust compilation is CPU-intensive
- Ollama needs resources for inference
- Concurrent build + AI testing

---

## Port Forwarding

These ports are automatically forwarded:

| Port | Service | Notes |
|------|---------|-------|
| 11434 | Ollama API | Auto-forwarded, silent |
| 3000 | Dev server | If you add one |
| 8080 | Alternative | General purpose |

---

## Persisted Data

These survive Codespace rebuilds:

| Data | Location | Volume |
|------|----------|--------|
| Cargo registry | `/usr/local/cargo/registry` | `mycel-cargo-cache` |
| Ollama models | `~/.ollama` | `mycel-ollama-models` |

So you won't re-download dependencies or models after rebuild.

---

## Common Workflows

### Daily Development

```bash
# Start Ollama (if not running)
ollama-start

# Build and watch for changes
cd mycel-runtime
cargo watch -x check

# In another terminal, run tests
cargo test
```

### Testing with CLI

```bash
# Terminal 1: Run runtime
mr

# Terminal 2: Use CLI
mycel status
mycel "hello"
mycel "list files in /home"
```

### Testing IPC Directly

```bash
# Terminal 1: Run runtime
mr

# Terminal 2: Send raw JSON
echo '{"type":"ping"}' | nc -U /tmp/mycel-dev.sock
```

### Building ISO (Advanced)

```bash
# Requires Docker-in-Docker
./scripts/build-mycel-iso.sh
```

---

## Troubleshooting

### "Ollama not responding"

```bash
# Check if running
ollama-status

# Restart
ollama-stop
ollama-start

# Check logs
ollama-logs
```

### "cargo build fails"

```bash
# Check for errors
cargo check 2>&1 | head -50

# Clean and rebuild
cargo clean
cargo build
```

### "Out of disk space"

```bash
# Check usage
df -h

# Clean Cargo cache
cargo clean

# Remove unused Docker images
docker system prune -a
```

### "Codespace is slow"

- Upgrade to larger machine type
- Close unused browser tabs
- Stop running processes you don't need

---

## Environment Variables

Set in `devcontainer.json`:

```bash
RUST_BACKTRACE=1       # Show backtraces on panic
RUST_LOG=info          # Log level (debug, info, warn, error)
OLLAMA_HOST=127.0.0.1:11434
```

Add your own in Codespace secrets:

```bash
ANTHROPIC_API_KEY=sk-ant-...   # For cloud AI features
```

---

## VS Code Extensions

Pre-installed:
- **rust-analyzer** - Rust language support
- **Even Better TOML** - Config file support
- **CodeLLDB** - Rust debugging
- **GitLens** - Git integration
- **GitHub Copilot** - AI assistance (if you have access)

---

## Tips

### Use Split Terminals

- Left: `cargo watch -x check`
- Right: Manual testing

### Use Tasks

Create `.vscode/tasks.json`:
```json
{
  "version": "2.0.0",
  "tasks": [
    {
      "label": "Build",
      "type": "cargo",
      "command": "build",
      "problemMatcher": ["$rustc"],
      "group": { "kind": "build", "isDefault": true }
    },
    {
      "label": "Run Dev",
      "type": "shell",
      "command": "cargo run -- --dev --verbose",
      "options": { "cwd": "${workspaceFolder}/mycel-runtime" }
    }
  ]
}
```

### Keyboard Shortcuts

- `Ctrl+Shift+B` - Build
- `F5` - Debug
- `Ctrl+Shift+P` - Command palette
- `Ctrl+`` ` - Toggle terminal

---

## Getting Help

1. Read `CLAUDE.md` - Project overview
2. Read `TODO.md` - What to work on
3. Read `TROUBLESHOOTING.md` - Common issues
4. Check `DECISIONS.md` - Why things are the way they are
