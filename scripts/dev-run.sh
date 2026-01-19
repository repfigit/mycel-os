#!/bin/bash
# Development script for Clay OS
# Runs the Clay Runtime in development mode

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

cd "$PROJECT_DIR"

echo "=== Clay OS Development Environment ==="
echo ""

# Check for Ollama
if ! command -v ollama &> /dev/null; then
    echo "WARNING: Ollama not found. Install it for local LLM support:"
    echo "  curl -fsSL https://ollama.com/install.sh | sh"
    echo ""
fi

# Check if Ollama is running
if command -v ollama &> /dev/null; then
    if ! curl -s http://localhost:11434/api/tags &> /dev/null; then
        echo "Starting Ollama..."
        ollama serve &
        sleep 2
    fi
    
    # Check if model is available
    if ! ollama list | grep -q "phi3:medium"; then
        echo "Downloading phi3:medium model (this may take a while)..."
        ollama pull phi3:medium
    fi
fi

# Check for Anthropic API key
if [ -z "$ANTHROPIC_API_KEY" ]; then
    echo "NOTE: ANTHROPIC_API_KEY not set. Cloud features will be disabled."
    echo "  Set it with: export ANTHROPIC_API_KEY='your-key'"
    echo ""
fi

# Build in debug mode
echo "Building Clay Runtime..."
cd clay-runtime
cargo build

echo ""
echo "Starting Clay Runtime in development mode..."
echo "Type your requests at the prompt. Type 'quit' to exit."
echo ""

# Run with dev flag
cargo run -- --dev --verbose
