#!/bin/bash
# Build Mycel OS ISO
# Based on void-mklive

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUILD_DIR="${PROJECT_DIR}/build"
OUTPUT_DIR="${PROJECT_DIR}/output"

# Configuration
ARCH="${ARCH:-x86_64}"
LIBC="${LIBC:-musl}"
VERSION="${VERSION:-0.1.0}"
DATE=$(date +%Y%m%d)

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo ""
echo "    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗     "
echo "    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║     "
echo "    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║     "
echo "    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║     "
echo "    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗"
echo "    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝"
echo ""
echo -e "${GREEN}   Building Mycel OS ${VERSION}${NC}"
echo "   Architecture: ${ARCH}-${LIBC}"
echo "   Date: ${DATE}"
echo ""

# Check if running as root (required for mklive)
if [ "$EUID" -ne 0 ]; then
    echo -e "${YELLOW}Note: ISO building requires root. Will use sudo.${NC}"
    SUDO="sudo"
else
    SUDO=""
fi

# Create directories
mkdir -p "${BUILD_DIR}" "${OUTPUT_DIR}"

# Clone void-mklive if not present
if [ ! -d "${BUILD_DIR}/void-mklive" ]; then
    echo -e "${YELLOW}[*] Cloning void-mklive...${NC}"
    git clone --depth 1 https://github.com/void-linux/void-mklive "${BUILD_DIR}/void-mklive"
fi

cd "${BUILD_DIR}/void-mklive"

# Create Mycel OS package list
cat > "${BUILD_DIR}/mycel-packages.txt" << 'EOF'
# Base system
base-system
linux
linux-firmware
dracut

# Networking
dhcpcd
iproute2
iputils
openssh
curl
wget
ca-certificates
wireguard-tools

# Development (for now - can be removed in minimal images)
base-devel
git
rust
cargo

# Display
mesa
mesa-dri
libdrm
libinput
xorg-server-xwayland

# Audio
pipewire
wireplumber

# Utilities
vim
htop
tmux
jq
file
xz
zstd

# Sandboxing
firejail
bubblewrap

# File systems
e2fsprogs
dosfstools
ntfs-3g

# Boot
grub-x86_64-efi
efibootmgr
EOF

# Create Mycel OS customization script
cat > "${BUILD_DIR}/mycel-customize.sh" << 'CUSTOMIZE_EOF'
#!/bin/bash
# Mycel OS customization script
# Runs inside the live environment during build

set -e

# Create mycel user
useradd -m -s /bin/bash -G wheel,audio,video,input mycel
echo 'mycel:mycel' | chpasswd

# Enable passwordless sudo for wheel
echo '%wheel ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/wheel

# Set hostname
echo "mycel" > /etc/hostname

# Create Mycel OS branding
cat > /etc/os-release << 'OSRELEASE'
NAME="Mycel OS"
ID=mycel
ID_LIKE=void
VERSION="0.1.0"
VERSION_ID="0.1.0"
PRETTY_NAME="Mycel OS 0.1.0"
HOME_URL="https://mycel-os.org"
BUG_REPORT_URL="https://github.com/mycel-os/mycel/issues"
OSRELEASE

# Create motd
cat > /etc/motd << 'MOTD'

    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗     
    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║     
    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║     
    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║     
    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗
    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝

    Welcome to Mycel OS
    The intelligent network beneath everything.

    Get started:
      mycel "help me get started"

MOTD

# Install Ollama
curl -fsSL https://ollama.com/install.sh | sh

# Create Mycel Runtime placeholder
mkdir -p /usr/lib/mycel
cat > /usr/bin/mycel << 'MYCELCLI'
#!/bin/bash
echo "Mycel Runtime not yet installed."
echo "This is a placeholder for the Mycel OS CLI."
echo ""
echo "To build Mycel Runtime:"
echo "  cd /home/mycel/mycel-os"
echo "  cargo build --release"
echo "  sudo cp target/release/mycel-runtime /usr/bin/mycel"
MYCELCLI
chmod +x /usr/bin/mycel

# Create Mycel configuration directory
mkdir -p /etc/mycel
cat > /etc/mycel/config.toml << 'CONFIG'
[system]
hostname = "mycel"
node_name = "default"

[ai]
local_enabled = true
local_model = "phi3:medium"
local_url = "http://localhost:11434"

cloud_enabled = false
# cloud_api_key = ""

[collective]
enabled = false
# near_account = ""
# bittensor_coldkey = ""

[mesh]
enabled = false
# recovery_phrase stored separately, encrypted
CONFIG

# Enable services for runit
ln -sf /etc/sv/sshd /var/service/
ln -sf /etc/sv/dhcpcd /var/service/

# Create ollama runit service
mkdir -p /etc/sv/ollama
cat > /etc/sv/ollama/run << 'OLLAMARUN'
#!/bin/sh
exec chpst -u mycel /usr/local/bin/ollama serve 2>&1
OLLAMARUN
chmod +x /etc/sv/ollama/run
ln -sf /etc/sv/ollama /var/service/

echo "Mycel OS customization complete."
CUSTOMIZE_EOF

chmod +x "${BUILD_DIR}/mycel-customize.sh"

# Build the ISO
echo -e "${YELLOW}[*] Building Mycel OS ISO...${NC}"
echo "    This may take 10-30 minutes depending on your system."

$SUDO ./mklive.sh \
    -a "${ARCH}-${LIBC}" \
    -r "https://repo-default.voidlinux.org/current/${LIBC}" \
    -p "$(cat ${BUILD_DIR}/mycel-packages.txt | grep -v '^#' | tr '\n' ' ')" \
    -C "${BUILD_DIR}/mycel-customize.sh" \
    -o "${OUTPUT_DIR}/mycel-os-${VERSION}-${ARCH}-${LIBC}-${DATE}.iso"

# Create symlink to latest
ln -sf "mycel-os-${VERSION}-${ARCH}-${LIBC}-${DATE}.iso" "${OUTPUT_DIR}/mycel-os-latest.iso"

if [ -f "${OUTPUT_DIR}/mycel-os-${VERSION}-${ARCH}-${LIBC}-${DATE}.iso" ]; then
    echo ""
    echo -e "${GREEN}[✓] ISO built successfully!${NC}"
    echo ""
    echo "    Output: ${OUTPUT_DIR}/mycel-os-${VERSION}-${ARCH}-${LIBC}-${DATE}.iso"
    echo ""
    echo "    Test with:"
    echo "    qemu-system-x86_64 -enable-kvm -m 4G -cdrom ${OUTPUT_DIR}/mycel-os-latest.iso"
else
    echo -e "${RED}[✗] ISO build failed${NC}"
    exit 1
fi
