#!/bin/bash
# Mycel OS Development Container Entrypoint (Void Linux)

set -e

echo ""
echo "    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗     "
echo "    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║     "
echo "    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║     "
echo "    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║     "
echo "    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗"
echo "    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝"
echo ""
echo "    The intelligent network beneath everything."
echo "    Development Environment (Void Linux musl)"
echo ""

# Start SSH daemon
echo "[*] Starting SSH server..."
/usr/sbin/sshd

# Get container IP
CONTAINER_IP=$(hostname -I 2>/dev/null | awk '{print $1}' || echo "localhost")
echo "[✓] SSH available: ssh mycel@${CONTAINER_IP} -p 22"
echo "    Password: mycel"

# Start Ollama
echo "[*] Starting Ollama..."
ollama serve &
OLLAMA_PID=$!

# Wait for Ollama
for i in {1..30}; do
    if curl -s http://localhost:11434/api/tags > /dev/null 2>&1; then
        echo "[✓] Ollama is ready"
        break
    fi
    sleep 1
done

# Check for models
if ! ollama list 2>/dev/null | grep -q "phi3\|mistral\|llama"; then
    echo ""
    echo "[!] No LLM models found. Pull one with:"
    echo "    ollama pull phi3:medium    # 8GB, recommended"
    echo "    ollama pull phi3:mini      # 2GB, faster"
    echo "    ollama pull mistral:7b     # Alternative"
fi

echo ""
echo "=== Environment Ready ==="
echo ""
echo "Build Mycel Runtime:"
echo "  cd /workspace/mycel-os/mycel-runtime"
echo "  cargo build --release"
echo ""
echo "Build Mycel OS ISO:"
echo "  ./scripts/build-mycel-iso.sh"
echo ""
echo "Test in VM:"
echo "  ./scripts/test-vm.sh"
echo ""

# Run command or shell
if [ "$1" = "bash" ] || [ -z "$1" ]; then
    exec /bin/bash
else
    exec "$@"
fi
