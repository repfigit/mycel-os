#!/usr/bin/env bash
# Mycel OS - Development runner
# Builds the runtime, ensures Ollama + model are ready, sets up MCP servers,
# and launches an interactive dev session.
#
# Usage:
#   ./scripts/dev-run.sh            # interactive CLI (mycel> prompt)
#   ./scripts/dev-run.sh --daemon   # background daemon + IPC socket
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
MODEL="${MYCEL_LOCAL_MODEL:-qwen3:8b}"

cd "$PROJECT_DIR"

echo "    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗     "
echo "    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║     "
echo "    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║     "
echo "    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║     "
echo "    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗"
echo "    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝"
echo ""
echo "    The intelligent network beneath everything."
echo ""

# --- 1. Ollama ---
if ! command -v ollama &> /dev/null; then
    echo "ERROR: Ollama not installed. Install with:"
    echo "  curl -fsSL https://ollama.com/install.sh | sh"
    exit 1
fi

if ! curl -s --max-time 3 http://localhost:11434/api/tags &> /dev/null; then
    echo "Starting Ollama..."
    nohup ollama serve > /tmp/ollama.log 2>&1 &
    sleep 3
fi

if ! ollama list 2>/dev/null | grep -q "^${MODEL%%:*}"; then
    echo "Downloading model: $MODEL (this can take a while)..."
    ollama pull "$MODEL"
fi
echo "✓ Ollama running with $MODEL"

# --- 2. MCP servers (optional, non-fatal) ---
if [ -f "$PROJECT_DIR/scripts/setup-mcp-servers.sh" ]; then
    if [ ! -d "$PROJECT_DIR/mycel-runtime/mcp-servers/void-tools/.venv" ]; then
        echo "Setting up MCP server environments..."
        "$PROJECT_DIR/scripts/setup-mcp-servers.sh" || echo "  (MCP setup failed - continuing without system tools)"
    fi
    echo "✓ MCP servers configured"
fi

# --- 3. Cloud AI (optional) ---
if [ -z "${OPENROUTER_API_KEY:-}" ]; then
    echo "NOTE: OPENROUTER_API_KEY not set - running local-only."
fi

# --- 4. Build ---
echo "Building Mycel Runtime..."
cd "$PROJECT_DIR/mycel-runtime"
cargo build --release

echo ""
if [ "${1:-}" = "--daemon" ]; then
    echo "Starting Mycel Runtime as daemon..."
    echo "Socket: /tmp/mycel-dev.sock"
    echo "Watch the log for the IPC auth token, then connect with:"
    echo "  MYCEL_AUTH_TOKEN=<token> python3 tools/mycel-cli.py chat"
    echo ""
    exec ./target/release/mycel-runtime --dev --daemon --verbose
else
    echo "Starting Mycel Runtime (interactive)..."
    echo "Type 'quit' to exit."
    echo ""
    exec ./target/release/mycel-runtime --dev --verbose
fi
