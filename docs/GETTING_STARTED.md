# Getting Started with Mycel OS

This guide walks you through setting up and running Mycel OS in development mode.

## Prerequisites

You'll need:
- A Linux machine (Ubuntu 22.04+, Debian 12+, or similar)
- 16GB RAM (8GB minimum, but 16GB recommended for local LLM)
- NVIDIA GPU with 8GB+ VRAM (optional but recommended)
- Rust toolchain (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- Python 3.10+

## Quick Start (Development Mode)

### 1. Clone and Build

```bash
git clone https://github.com/yourusername/mycel-os
cd mycel-os
```

### 2. Install Ollama (Local LLM)

```bash
curl -fsSL https://ollama.com/install.sh | sh
ollama serve &  # Start Ollama server
ollama pull qwen3:8b  # Download the model (about 5GB)
```

#### Choosing a Model

Mycel's core loop (chat → tool calls → code generation → execution) needs a
model with **native tool-calling** and **strong code generation**. The qwen3
family is the default and recommended choice:

| Hardware | Model | Download | Notes |
|----------|-------|----------|-------|
| <8GB RAM, no GPU | `qwen3:4b` | ~2.5GB | Fast, capable; weaker code gen |
| 8–16GB RAM | **`qwen3:8b`** (default) | ~5GB | Best balance; verified in E2E tests |
| GPU 8GB+ VRAM | `qwen3:14b` | ~9GB | Much better reasoning |
| GPU 16GB+ VRAM | `qwen3:32b` | ~20GB | Near-cloud quality |

For code-heavy use, `deepseek-coder-v2:16b-lite` is the strongest local code
model, but weaker at conversation and tool-calling than qwen3.

Safety note: command safety is enforced by Mycel's **policy evaluator** (which
gates dangerous commands for confirmation), not by the model refusing requests.

### 3. Set Up Cloud AI via OpenRouter (Optional)

For cloud AI features, get an API key from [openrouter.ai](https://openrouter.ai/keys):

```bash
export OPENROUTER_API_KEY="sk-or-your-key-here"
```

### 4. Set Up MCP Servers (Recommended)

MCP servers give the AI real system tools (package management, system info,
service control). This creates a Python virtualenv for each server:

```bash
./scripts/setup-mcp-servers.sh
```

Without this, the AI still works but only has the built-in `shell_execute` tool.

### 5. Run It

**Option A — Interactive CLI** (simplest):

```bash
./scripts/dev-run.sh
```

This checks Ollama, pulls the model if needed, sets up MCP servers, builds the
runtime, and drops you into a `mycel>` prompt.

**Option B — Daemon + CLI client** (how the system runs in production):

```bash
# Terminal 1: run the daemon
./scripts/dev-run.sh --daemon
# Watch for a line like:
#   IPC auth token: 4bc2439b-2380-4ec5-8d17-3ff6d3047a06

# Terminal 2: connect the CLI
export MYCEL_AUTH_TOKEN="<token from daemon log>"
python3 tools/mycel-cli.py chat
```

**Option C — manual** (full control):

```bash
cd mycel-runtime
cargo build --release
./target/release/mycel-runtime --dev --verbose   # interactive
# or
./target/release/mycel-runtime --dev --daemon    # background + IPC socket
```

### 6. Try It Out

At the `mycel>` prompt (or in the CLI client):

```
mycel> what OS am I running?
You are running Ubuntu 26.04 LTS with the 7.0.0-29-generic kernel.

mycel> count the files in /etc
234

mycel> remember the codeword FUNGAL123
OK.

mycel> what's the codeword?
The codeword you gave me is FUNGAL123.

mycel> delete every file in /home
requires confirmation: Dangerous command detected: 'rm -rf'. Proceed?
```

Notes on what you'll see:
- The first request after boot may take ~20–40s while the model loads; a
  background warmup minimizes this. Subsequent requests are typically 2–8s on
  CPU, faster on GPU.
- Dangerous commands are gated by the policy evaluator and ask for
  confirmation — the AI doesn't refuse, the OS protects you.
- The AI remembers conversation context within a session.

## Understanding the Architecture

### The Mycel Runtime

The heart of Mycel OS is the **Mycel Runtime** (`mycel-runtime`), a Rust daemon that:
- Runs a local LLM for fast, private responses
- Routes complex queries to cloud AI when needed
- Generates and executes code in a sandbox
- Creates dynamic UI surfaces

### Local vs Cloud AI

Mycel OS uses a hybrid approach:

| Task | Where it runs | Why |
|------|--------------|-----|
| Simple questions | Local | Fast, private |
| Code generation | Local first | Speed, can escalate |
| Complex analysis | Cloud | Better reasoning |
| Creative writing | Cloud | Higher quality |
| System commands | Local | Low latency |

You control this with configuration and can run fully local (no cloud) or fully cloud (no local model).

### The Fluid UI Concept

Unlike traditional window managers, Mycel OS generates UI on-demand:

```
mycel> I need to compare these three documents side by side

[Creates a three-column view with synchronized scrolling and diff highlighting]

mycel> Actually, highlight the differences between versions 2 and 3

[Modifies the view to show diffs, fades out version 1]

mycel> Save this comparison as a PDF

[Generates PDF with the current view]
```

This is still in development - the current release focuses on the CLI and backend.

## Configuration

Edit `config/config.toml` (dev) or `/etc/mycel/config.toml` (installed):

```toml
# Use a smaller/faster model
local_model = "qwen3:4b"

# Or a larger/smarter model
local_model = "qwen3:32b"

# Cloud routing via OpenRouter (leave empty for local-only)
cloud_api_key = ""

# Or disable the local model entirely (cloud only):
# Run with: mycel-runtime --no-local-llm
```

Environment variable overrides: `MYCEL_LOCAL_MODEL`, `OLLAMA_URL`,
`OPENROUTER_API_KEY`, `MYCEL_PREFER_CLOUD`.

## Building a Full OS Image

To create a bootable Mycel OS image:

```bash
sudo ./scripts/build-image.sh
```

This creates `build/mycel-os.img` which can be:
- Run in QEMU: `./scripts/run-vm.sh`
- Written to USB: `sudo dd if=build/mycel-os.img of=/dev/sdX bs=4M`

## Project Structure

```
mycel-os/
├── mycel-runtime/       # Core Rust daemon
│   └── src/
│       ├── main.rs      # Entry point
│       ├── ai/          # LLM interface (local + cloud)
│       ├── context/     # Session and user context
│       ├── executor/    # Sandboxed code execution
│       └── ui/          # Surface generation
├── config/              # Configuration files
├── scripts/             # Build and run scripts
└── tools/               # CLI and utilities
```

## Next Steps

1. **Experiment with the CLI** - Try different requests to understand Mycel's capabilities
2. **Customize the config** - Adjust model selection and routing preferences
3. **Extend the runtime** - Add new intent handlers or UI surface types
4. **Build the compositor** - Help develop the visual layer (contributions welcome!)

## Troubleshooting

### Ollama won't start
```bash
# Check if it's already running
pgrep ollama

# Start it manually and watch the log
ollama serve
```

### First request is very slow (~30-60s)
That's the one-time model load into memory. The runtime spawns a background
warmup at boot to hide this; subsequent requests are 2-8s on CPU. If it's
slow on *every* request, check that Ollama isn't unloading the model
(`keep_alive` is set to 30m by the warmup).

### MCP server 'void-tools' failed to start
Its Python dependencies live in a per-server virtualenv. Run:
```bash
./scripts/setup-mcp-servers.sh
```
The runtime log will show the underlying stderr (e.g. a missing `mcp` module)
if it still fails. The system works without it - the AI just loses the
Void-specific tools and falls back to the built-in `shell_execute`.

### "command not found: cargo" or build errors
Install Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`.
The runtime uses rustls (pure-Rust TLS), so no OpenSSL/pkg-config system
dependencies are needed.

### Out of GPU memory
Use a smaller model:
```bash
ollama pull qwen3:4b  # 2.5GB instead of 5GB
```
Update config:
```toml
local_model = "qwen3:4b"
```

### Cloud API errors
Check your OpenRouter API key:
```bash
curl https://openrouter.ai/api/v1/chat/completions \
  -H "Authorization: Bearer $OPENROUTER_API_KEY" \
  -H "content-type: application/json" \
  -d '{"model":"anthropic/claude-3.5-sonnet","messages":[{"role":"user","content":"Hi"}]}'
```

## Contributing

See [CONTRIBUTING.md](docs/CONTRIBUTING.md) for guidelines. Key areas needing help:
- Wayland compositor implementation
- Additional UI surface types
- Improved intent parsing
- Security hardening
- Documentation

---

Questions? Open an issue or reach out to the community.
