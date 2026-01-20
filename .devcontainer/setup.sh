#!/bin/bash
# Codespace setup script for Mycel OS development
set -e

echo "================================"
echo "  Mycel OS Codespace Setup"
echo "================================"

# Install system packages
echo "[1/4] Installing system packages..."
sudo apt-get update -qq
sudo apt-get install -y -qq \
    qemu-system-x86 \
    qemu-utils \
    ovmf \
    curl \
    jq \
    netcat-openbsd \
    xorriso \
    squashfs-tools

# Install Rust components
echo "[2/4] Installing Rust components..."
rustup component add clippy rustfmt

# Install Ollama
echo "[3/4] Installing Ollama..."
if ! command -v ollama &> /dev/null; then
    curl -fsSL https://ollama.com/install.sh | sh
fi

# Make scripts executable
echo "[4/4] Setting up project..."
chmod +x scripts/*.sh 2>/dev/null || true
chmod +x tools/*.py 2>/dev/null || true

# Create output directory
mkdir -p output

echo ""
echo "================================"
echo "  Setup Complete!"
echo "================================"
echo ""
echo "Quick Start:"
echo "  1. Build runtime:  cd mycel-runtime && cargo build"
echo "  2. Build ISO:      ./scripts/build-iso.sh"
echo "  3. Test ISO:       ./scripts/test-iso.sh"
echo ""
echo "See .devcontainer/INSTRUCTIONS.md for details"
echo ""
