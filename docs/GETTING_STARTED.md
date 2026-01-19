# Getting Started with Clay OS

This guide walks you through setting up and running Clay OS in development mode.

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
git clone https://github.com/yourusername/clay-os
cd clay-os
```

### 2. Install Ollama (Local LLM)

```bash
curl -fsSL https://ollama.com/install.sh | sh
ollama serve &  # Start Ollama server
ollama pull phi3:medium  # Download the model (about 8GB)
```

### 3. Set Up Anthropic API (Optional but Recommended)

For cloud AI features, get an API key from [console.anthropic.com](https://console.anthropic.com):

```bash
export ANTHROPIC_API_KEY="sk-ant-your-key-here"
```

### 4. Run in Development Mode

```bash
./scripts/dev-run.sh
```

This will:
- Build the Clay Runtime
- Start Ollama if not running
- Launch an interactive CLI

### 5. Try It Out

```
clay> Hello, what can you do?

I'm Clay, the AI at the heart of Clay OS. I can help you with:

- Answering questions and explaining concepts
- Writing and executing code on-the-fly
- Creating custom interfaces for your tasks
- Analyzing files and data
- Automating workflows

What would you like to work on?

clay> Create a Python script that finds duplicate files in a directory

--- Generated Code ---
#!/usr/bin/env python3
import os
import hashlib
from collections import defaultdict

def get_file_hash(filepath):
    hasher = hashlib.md5()
    with open(filepath, 'rb') as f:
        for chunk in iter(lambda: f.read(4096), b''):
            hasher.update(chunk)
    return hasher.hexdigest()

def find_duplicates(directory):
    hash_map = defaultdict(list)
    for root, dirs, files in os.walk(directory):
        for filename in files:
            filepath = os.path.join(root, filename)
            file_hash = get_file_hash(filepath)
            hash_map[file_hash].append(filepath)
    
    duplicates = {h: paths for h, paths in hash_map.items() if len(paths) > 1}
    return duplicates

# Example usage
dupes = find_duplicates('.')
for hash_val, files in dupes.items():
    print(f"Duplicates (hash: {hash_val[:8]}...):")
    for f in files:
        print(f"  {f}")

--- Output ---
[Script executed successfully - no duplicates found in current directory]
```

## Understanding the Architecture

### The Clay Runtime

The heart of Clay OS is the **Clay Runtime** (`clay-runtime`), a Rust daemon that:
- Runs a local LLM for fast, private responses
- Routes complex queries to cloud AI when needed
- Generates and executes code in a sandbox
- Creates dynamic UI surfaces

### Local vs Cloud AI

Clay OS uses a hybrid approach:

| Task | Where it runs | Why |
|------|--------------|-----|
| Simple questions | Local | Fast, private |
| Code generation | Local first | Speed, can escalate |
| Complex analysis | Cloud | Better reasoning |
| Creative writing | Cloud | Higher quality |
| System commands | Local | Low latency |

You control this with configuration and can run fully local (no cloud) or fully cloud (no local model).

### The Fluid UI Concept

Unlike traditional window managers, Clay OS generates UI on-demand:

```
clay> I need to compare these three documents side by side

[Creates a three-column view with synchronized scrolling and diff highlighting]

clay> Actually, highlight the differences between versions 2 and 3

[Modifies the view to show diffs, fades out version 1]

clay> Save this comparison as a PDF

[Generates PDF with the current view]
```

This is still in development - the current release focuses on the CLI and backend.

## Configuration

Edit `config/config.toml` or `/etc/clay/config.toml`:

```toml
# Use a smaller/faster model
local_model = "mistral:7b"

# Or a larger/smarter model
local_model = "llama3.1:70b"

# Disable cloud entirely
anthropic_api_key = ""

# Or disable local model (cloud only)
# Run with: clay-runtime --no-local-llm
```

## Building a Full OS Image

To create a bootable Clay OS image:

```bash
sudo ./scripts/build-image.sh
```

This creates `build/clay-os.img` which can be:
- Run in QEMU: `./scripts/run-vm.sh`
- Written to USB: `sudo dd if=build/clay-os.img of=/dev/sdX bs=4M`

## Project Structure

```
clay-os/
├── clay-runtime/        # Core Rust daemon
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

1. **Experiment with the CLI** - Try different requests to understand Clay's capabilities
2. **Customize the config** - Adjust model selection and routing preferences
3. **Extend the runtime** - Add new intent handlers or UI surface types
4. **Build the compositor** - Help develop the visual layer (contributions welcome!)

## Troubleshooting

### Ollama won't start
```bash
# Check if it's already running
pgrep ollama

# Check logs
journalctl -u ollama -f

# Restart
systemctl restart ollama
```

### Out of GPU memory
Use a smaller model:
```bash
ollama pull phi3:mini  # 2GB instead of 8GB
```
Update config:
```toml
local_model = "phi3:mini"
```

### Cloud API errors
Check your API key:
```bash
curl https://api.anthropic.com/v1/messages \
  -H "x-api-key: $ANTHROPIC_API_KEY" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"claude-sonnet-4-20250514","max_tokens":100,"messages":[{"role":"user","content":"Hi"}]}'
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
