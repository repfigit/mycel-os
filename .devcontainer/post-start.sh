#!/bin/bash
# Post-start script for Mycel OS Codespace
# Runs every time the Codespace starts (including rebuilds)

set -e

echo "Starting Mycel OS services..."

# ============================================
# Start Ollama
# ============================================
echo "[*] Starting Ollama..."

# Try systemd user service first
if systemctl --user start ollama 2>/dev/null; then
    echo "[✓] Ollama started via systemd"
else
    # Fallback: start directly in background
    echo "[*] Starting Ollama directly..."
    nohup ollama serve > /tmp/ollama.log 2>&1 &
    echo "[✓] Ollama started (logs: /tmp/ollama.log)"
fi

# Wait for Ollama to be ready
echo "[*] Waiting for Ollama to be ready..."
for i in {1..30}; do
    if curl -s http://localhost:11434/api/tags > /dev/null 2>&1; then
        echo "[✓] Ollama is ready"
        break
    fi
    sleep 1
done

# ============================================
# Check for models
# ============================================
if ! ollama list 2>/dev/null | grep -q "phi3\|mistral\|llama"; then
    echo ""
    echo "[!] No LLM models found. Pull one with:"
    echo "    ollama pull phi3:mini      # 2GB, fast"
    echo "    ollama pull phi3:medium    # 8GB, better quality"
    echo ""
fi

# ============================================
# Show status
# ============================================
echo ""
echo "============================================"
echo "  Mycel OS Codespace Ready"
echo "============================================"
echo ""
echo "Ollama: $(curl -s http://localhost:11434/api/tags | jq -r '.models | length') models loaded"
echo ""
echo "Commands:"
echo "  mb    - Build (cargo build)"
echo "  mr    - Run dev mode"
echo "  mt    - Run tests"
echo "  mc    - Check (fast compile check)"
echo ""
