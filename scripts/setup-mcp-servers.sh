#!/usr/bin/env bash
# Set up Python virtualenvs for Mycel OS MCP servers.
#
# PEP 668 systems (modern Debian/Ubuntu) refuse pip installs into the system
# Python, so each Python MCP server gets its own venv next to its code.
# The runtime auto-discovers .venv/bin/python3 at startup.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVERS_DIR="$REPO_ROOT/mycel-runtime/mcp-servers"

setup_server() {
    local name="$1"
    local dir="$SERVERS_DIR/$name"
    if [ ! -d "$dir" ]; then
        return
    fi
    # Only set up servers that have a Python entry point
    if [ ! -f "$dir/pyproject.toml" ] && [ ! -f "$dir/requirements.txt" ]; then
        return
    fi

    echo "==> Setting up $name"
    python3 -m venv "$dir/.venv"
    if [ -f "$dir/requirements.txt" ]; then
        "$dir/.venv/bin/pip" install -q -r "$dir/requirements.txt"
    else
        # Install the package (respects pyproject.toml dependency pins)
        "$dir/.venv/bin/pip" install -q -e "$dir" 2>/dev/null \
            || "$dir/.venv/bin/pip" install -q "mcp>=1.0.0,<2.0.0"
    fi
    echo "    done: $dir/.venv"
}

main() {
    if ! command -v python3 >/dev/null; then
        echo "error: python3 not found" >&2
        exit 1
    fi
    for dir in "$SERVERS_DIR"/*/; do
        setup_server "$(basename "$dir")"
    done
    echo "MCP server setup complete."
}

main "$@"
