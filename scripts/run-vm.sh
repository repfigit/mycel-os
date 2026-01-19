#!/bin/bash
# Run Clay OS in QEMU
# Useful for testing the full OS image

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUILD_DIR="$PROJECT_DIR/build"
IMAGE_FILE="$BUILD_DIR/clay-os.img"

# Check if image exists
if [ ! -f "$IMAGE_FILE" ]; then
    echo "ERROR: OS image not found at $IMAGE_FILE"
    echo "Run ./scripts/build-image.sh first"
    exit 1
fi

# Default settings
RAM="4G"
CPUS="2"
ENABLE_KVM=""

# Check for KVM support
if [ -e /dev/kvm ]; then
    ENABLE_KVM="-enable-kvm"
    echo "KVM acceleration enabled"
fi

echo "Starting Clay OS in QEMU..."
echo "  RAM: $RAM"
echo "  CPUs: $CPUS"
echo ""
echo "Press Ctrl+A, X to exit QEMU"
echo ""

qemu-system-x86_64 \
    $ENABLE_KVM \
    -m "$RAM" \
    -smp "$CPUS" \
    -hda "$IMAGE_FILE" \
    -netdev user,id=net0,hostfwd=tcp::2222-:22 \
    -device virtio-net-pci,netdev=net0 \
    -display gtk \
    -serial mon:stdio
