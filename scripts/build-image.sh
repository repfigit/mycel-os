#!/bin/bash
# Build a bootable Clay OS image
# This creates a minimal Linux system with Clay Runtime

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUILD_DIR="$PROJECT_DIR/build"
ROOTFS_DIR="$BUILD_DIR/rootfs"
IMAGE_SIZE="4G"
IMAGE_FILE="$BUILD_DIR/clay-os.img"

echo "=== Building Clay OS Image ==="
echo ""

# Check for required tools
for tool in debootstrap qemu-img parted mkfs.ext4; do
    if ! command -v $tool &> /dev/null; then
        echo "ERROR: $tool is required but not installed"
        exit 1
    fi
done

# Check for root (needed for debootstrap and mounting)
if [ "$EUID" -ne 0 ]; then
    echo "This script must be run as root (for debootstrap and mounting)"
    echo "Run: sudo $0"
    exit 1
fi

# Create build directory
mkdir -p "$BUILD_DIR"
rm -rf "$ROOTFS_DIR"
mkdir -p "$ROOTFS_DIR"

echo "Step 1: Building Clay Runtime..."
cd "$PROJECT_DIR/clay-runtime"
cargo build --release
CLAY_BINARY="$PROJECT_DIR/clay-runtime/target/release/clay-runtime"

echo "Step 2: Creating disk image..."
qemu-img create -f raw "$IMAGE_FILE" $IMAGE_SIZE

echo "Step 3: Partitioning..."
parted -s "$IMAGE_FILE" mklabel gpt
parted -s "$IMAGE_FILE" mkpart primary ext4 1MiB 100%
parted -s "$IMAGE_FILE" set 1 boot on

echo "Step 4: Setting up loop device..."
LOOP_DEV=$(losetup -f --show -P "$IMAGE_FILE")
PART_DEV="${LOOP_DEV}p1"

echo "Step 5: Creating filesystem..."
mkfs.ext4 -L "CLAYOS" "$PART_DEV"

echo "Step 6: Mounting..."
mount "$PART_DEV" "$ROOTFS_DIR"

echo "Step 7: Installing base system (this takes a while)..."
# Using Debian as base for broader compatibility
debootstrap --variant=minbase --include=systemd,systemd-sysv,linux-image-amd64,grub-pc \
    bookworm "$ROOTFS_DIR" http://deb.debian.org/debian

echo "Step 8: Installing additional packages..."
chroot "$ROOTFS_DIR" apt-get update
chroot "$ROOTFS_DIR" apt-get install -y --no-install-recommends \
    python3 \
    python3-pip \
    nodejs \
    firejail \
    curl \
    ca-certificates \
    locales \
    sudo

echo "Step 9: Installing Ollama..."
curl -fsSL https://ollama.com/install.sh | chroot "$ROOTFS_DIR" sh

echo "Step 10: Copying Clay Runtime..."
mkdir -p "$ROOTFS_DIR/usr/local/bin"
cp "$CLAY_BINARY" "$ROOTFS_DIR/usr/local/bin/"
chmod +x "$ROOTFS_DIR/usr/local/bin/clay-runtime"

mkdir -p "$ROOTFS_DIR/etc/clay"
cp "$PROJECT_DIR/config/config.toml" "$ROOTFS_DIR/etc/clay/"

echo "Step 11: Creating systemd service..."
cat > "$ROOTFS_DIR/etc/systemd/system/clay-runtime.service" << 'EOF'
[Unit]
Description=Clay OS Runtime
After=network.target ollama.service

[Service]
Type=simple
ExecStart=/usr/local/bin/clay-runtime --config /etc/clay/config.toml
Restart=always
RestartSec=5
Environment=ANTHROPIC_API_KEY=

[Install]
WantedBy=multi-user.target
EOF

chroot "$ROOTFS_DIR" systemctl enable clay-runtime

echo "Step 12: Setting up user..."
chroot "$ROOTFS_DIR" useradd -m -s /bin/bash clay
echo "clay:clay" | chroot "$ROOTFS_DIR" chpasswd
chroot "$ROOTFS_DIR" usermod -aG sudo clay

echo "Step 13: Configuring boot..."
# Install GRUB
mount --bind /dev "$ROOTFS_DIR/dev"
mount --bind /proc "$ROOTFS_DIR/proc"
mount --bind /sys "$ROOTFS_DIR/sys"

chroot "$ROOTFS_DIR" grub-install --target=i386-pc "$LOOP_DEV"
chroot "$ROOTFS_DIR" update-grub

umount "$ROOTFS_DIR/sys"
umount "$ROOTFS_DIR/proc"
umount "$ROOTFS_DIR/dev"

echo "Step 14: Setting hostname..."
echo "clay-os" > "$ROOTFS_DIR/etc/hostname"

echo "Step 15: Creating welcome message..."
cat > "$ROOTFS_DIR/etc/motd" << 'EOF'

   ____  _                ___  ____  
  / ___|| | __ _ _   _   / _ \/ ___| 
 | |    | |/ _` | | | | | | | \___ \ 
 | |___ | | (_| | |_| | | |_| |___) |
  \____||_|\__,_|\__, |  \___/|____/ 
                 |___/               

Welcome to Clay OS - Computing reimagined.

The Clay Runtime is running. Connect via:
  - CLI: clay-cli
  - Socket: /tmp/clay.sock

Type "clay help" to get started.

EOF

echo "Step 16: Cleanup..."
umount "$ROOTFS_DIR"
losetup -d "$LOOP_DEV"

echo ""
echo "=== Build Complete ==="
echo "Image: $IMAGE_FILE"
echo ""
echo "To run in QEMU:"
echo "  qemu-system-x86_64 -enable-kvm -m 4G -hda $IMAGE_FILE"
echo ""
echo "To write to USB (replace /dev/sdX):"
echo "  sudo dd if=$IMAGE_FILE of=/dev/sdX bs=4M status=progress"
