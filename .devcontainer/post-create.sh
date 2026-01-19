#!/bin/bash
# Post-create script for Mycel OS Codespace
# Runs once when the Codespace is first created

set -e

echo ""
echo "    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗     "
echo "    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║     "
echo "    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║     "
echo "    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║     "
echo "    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗"
echo "    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝"
echo ""
echo "Setting up Mycel OS development environment..."
echo ""

# ============================================
# Install system dependencies
# ============================================
echo "[1/6] Installing system dependencies..."
sudo apt-get update
sudo apt-get install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    jq \
    netcat-openbsd \
    firejail \
    bubblewrap \
    qemu-system-x86 \
    curl \
    wget

# ============================================
# Install Ollama
# ============================================
echo "[2/6] Installing Ollama..."
curl -fsSL https://ollama.com/install.sh | sh

# Create Ollama systemd service for the user
mkdir -p ~/.config/systemd/user/
cat > ~/.config/systemd/user/ollama.service << 'EOF'
[Unit]
Description=Ollama Service
After=network-online.target

[Service]
ExecStart=/usr/local/bin/ollama serve
Restart=always
RestartSec=3
Environment="OLLAMA_HOST=127.0.0.1:11434"

[Install]
WantedBy=default.target
EOF

# Enable lingering for user services
sudo loginctl enable-linger vscode 2>/dev/null || true

# ============================================
# Install Rust tools
# ============================================
echo "[3/6] Installing Rust tools..."
rustup component add clippy rustfmt
cargo install cargo-watch cargo-edit 2>/dev/null || true

# ============================================
# Setup project
# ============================================
echo "[4/6] Setting up project..."

# Create directories
mkdir -p ~/.local/share/mycel
mkdir -p ~/.cache/mycel
mkdir -p /tmp/mycel

# Make scripts executable
chmod +x scripts/*.sh 2>/dev/null || true
chmod +x tools/*.py 2>/dev/null || true

# ============================================
# Initial build check
# ============================================
echo "[5/6] Checking Rust project..."
cd mycel-runtime
cargo check 2>&1 || echo "Note: Initial cargo check had issues - this is expected, see TODO.md"
cd ..

# ============================================
# Create convenience aliases
# ============================================
echo "[6/6] Setting up shell aliases..."

cat >> ~/.zshrc << 'EOF'

# Mycel OS aliases
alias mb="cd /workspaces/*/mycel-runtime && cargo build"
alias mr="cd /workspaces/*/mycel-runtime && cargo run -- --dev --verbose"
alias mt="cd /workspaces/*/mycel-runtime && cargo test"
alias mc="cd /workspaces/*/mycel-runtime && cargo check"
alias mw="cd /workspaces/*/mycel-runtime && cargo watch -x check"
alias ollama-start="systemctl --user start ollama"
alias ollama-stop="systemctl --user stop ollama"
alias ollama-status="systemctl --user status ollama"
alias ollama-logs="journalctl --user -u ollama -f"

# Quick pull model
ollama-pull() {
    ollama pull "${1:-phi3:mini}"
}

# Mycel CLI shortcut
mycel() {
    python3 /workspaces/*/tools/mycel-cli.py "$@"
}

EOF

# Also add to bashrc for bash users
cat >> ~/.bashrc << 'EOF'

# Mycel OS aliases
alias mb="cd /workspaces/*/mycel-runtime && cargo build"
alias mr="cd /workspaces/*/mycel-runtime && cargo run -- --dev --verbose"
alias mt="cd /workspaces/*/mycel-runtime && cargo test"
alias mc="cd /workspaces/*/mycel-runtime && cargo check"

EOF

echo ""
echo "============================================"
echo "  Mycel OS Codespace Setup Complete!"
echo "============================================"
echo ""
echo "Quick start:"
echo "  1. Start Ollama:     ollama-start"
echo "  2. Pull a model:     ollama-pull phi3:mini"
echo "  3. Build project:    mb  (or: cd mycel-runtime && cargo build)"
echo "  4. Run dev mode:     mr  (or: cargo run -- --dev)"
echo ""
echo "Read CLAUDE.md and TODO.md to get started!"
echo ""
