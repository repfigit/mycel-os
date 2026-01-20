#!/bin/bash
# Test Mycel OS ISO in QEMU
# Supports both headless (serial) and VNC modes

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
OUTPUT_DIR="$PROJECT_DIR/output"

# Find the most recent ISO
ISO_FILE="${1:-$(ls -t "$OUTPUT_DIR"/*.iso 2>/dev/null | head -1)}"

if [ -z "$ISO_FILE" ] || [ ! -f "$ISO_FILE" ]; then
    echo "ERROR: No ISO file found"
    echo ""
    echo "Build one first with: ./scripts/build-iso.sh"
    echo ""
    echo "Or specify ISO path: $0 /path/to/mycel-os.iso"
    exit 1
fi

echo ""
echo "================================"
echo "  Mycel OS QEMU Tester"
echo "================================"
echo ""
echo "ISO: $ISO_FILE"
echo ""

# Check QEMU
if ! command -v qemu-system-x86_64 &> /dev/null; then
    echo "ERROR: QEMU not installed"
    echo "Run: sudo apt-get install qemu-system-x86"
    exit 1
fi

# Create disk image for persistence (optional)
DISK_FILE="$OUTPUT_DIR/mycel-disk.qcow2"
if [ ! -f "$DISK_FILE" ]; then
    echo "Creating virtual disk (20GB)..."
    qemu-img create -f qcow2 "$DISK_FILE" 20G
fi

# QEMU options
QEMU_OPTS=(
    -m 4G
    -smp 2
    -cpu host
    -cdrom "$ISO_FILE"
    -drive file="$DISK_FILE",format=qcow2,if=virtio
    -boot d
    -netdev user,id=net0,hostfwd=tcp::2222-:22,hostfwd=tcp::11434-:11434
    -device virtio-net-pci,netdev=net0
)

# Mode selection
MODE="${2:-serial}"

case "$MODE" in
    serial|headless)
        echo "Starting in SERIAL mode (headless)..."
        echo "Console output will appear below."
        echo "Press Ctrl+A, X to exit QEMU"
        echo ""
        echo "----------------------------------------"
        
        qemu-system-x86_64 \
            "${QEMU_OPTS[@]}" \
            -nographic \
            -serial mon:stdio
        ;;
        
    vnc)
        echo "Starting in VNC mode..."
        echo ""
        echo "Connect via VNC:"
        echo "  - VNC port: 5900 (forwarded by Codespace)"
        echo "  - Or use noVNC in browser if available"
        echo ""
        echo "Press Ctrl+C to stop QEMU"
        echo ""
        
        qemu-system-x86_64 \
            "${QEMU_OPTS[@]}" \
            -vnc :0 \
            -monitor stdio
        ;;
        
    spice)
        echo "Starting in SPICE mode..."
        echo "Connect to port 5930"
        echo ""
        
        qemu-system-x86_64 \
            "${QEMU_OPTS[@]}" \
            -spice port=5930,disable-ticketing=on \
            -device virtio-serial-pci \
            -chardev spicevmc,id=vdagent,debug=0,name=vdagent \
            -device virtserialport,chardev=vdagent,name=com.redhat.spice.0 \
            -monitor stdio
        ;;
        
    *)
        echo "Usage: $0 [iso_file] [mode]"
        echo ""
        echo "Modes:"
        echo "  serial  - Headless with serial console (default)"
        echo "  vnc     - VNC display on port 5900"
        echo "  spice   - SPICE display on port 5930"
        echo ""
        echo "Examples:"
        echo "  $0                           # Latest ISO, serial mode"
        echo "  $0 output/mycel.iso vnc      # Specific ISO, VNC mode"
        exit 1
        ;;
esac
