#!/bin/bash
# Build Mycel OS ISO inside Void Linux container
set -e

echo ""
echo "    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗     "
echo "    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║     "
echo "    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║     "
echo "    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║     "
echo "    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗"
echo "    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝"
echo ""
echo "Building Mycel OS ISO..."
echo ""

BUILD_DIR="/build"
OUTPUT_DIR="/output"
WORKSPACE="/workspace"

# Packages to include in the ISO
PACKAGES="
base-system
linux
grub-x86_64-efi
grub-i386-pc
NetworkManager
iwd
bluez
pipewire
wireplumber
elogind
dbus
polkit
xorg-minimal
xorg-fonts
mesa-dri
intel-video-accel
xf86-video-intel
xf86-video-amdgpu
xf86-video-nouveau
sway
foot
bemenu
mako
grim
slurp
wl-clipboard
firefox
curl
wget
git
vim
neovim
htop
tmux
zsh
python3
rustup
"

# Services to enable
SERVICES="
dbus
NetworkManager
elogind
polkit
"

echo "[1/5] Setting up build environment..."
cd "$BUILD_DIR"

# Check if void-mklive exists
if [ ! -d "/usr/share/void-mklive" ]; then
    echo "Cloning void-mklive..."
    git clone https://github.com/void-linux/void-mklive.git
    cd void-mklive
else
    cd /usr/share/void-mklive || cd void-mklive 2>/dev/null || {
        git clone https://github.com/void-linux/void-mklive.git
        cd void-mklive
    }
fi

echo "[2/5] Creating package list..."
echo "$PACKAGES" | tr ' ' '\n' | grep -v '^$' > /tmp/packages.txt

echo "[3/5] Building ISO (this takes 10-20 minutes)..."

# Build the ISO
# -a x86_64: 64-bit
# -r repo: use default repo
# -p packages: include these packages
# -S services: enable these services
# -o output: output file
# -I includedir: include additional files

./mklive.sh \
    -a x86_64 \
    -p "$(cat /tmp/packages.txt | tr '\n' ' ')" \
    -S "$SERVICES" \
    -o "$OUTPUT_DIR/mycel-os-$(date +%Y%m%d).iso" \
    -T "Mycel OS" \
    -C "Mycel OS - The intelligent network beneath everything" \
    2>&1 | tee "$OUTPUT_DIR/build.log"

echo "[4/5] Copying mycel-runtime..."
# If we have a pre-built runtime, we'd add it here
if [ -f "$WORKSPACE/mycel-runtime/target/release/mycel-runtime" ]; then
    echo "Found mycel-runtime binary"
    # Would need to remaster ISO to include it
fi

echo "[5/5] Build complete!"
echo ""
ls -lh "$OUTPUT_DIR"/*.iso 2>/dev/null || echo "ISO not found - check build.log"
echo ""
echo "ISO saved to: $OUTPUT_DIR/"
