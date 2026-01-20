#!/bin/bash
# Build Mycel OS ISO from Codespace
# This script orchestrates the Docker-based build

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
OUTPUT_DIR="$PROJECT_DIR/output"

echo ""
echo "================================"
echo "  Mycel OS ISO Builder"
echo "================================"
echo ""

# Create output directory
mkdir -p "$OUTPUT_DIR"

# Check Docker
if ! command -v docker &> /dev/null; then
    echo "ERROR: Docker not available"
    echo "Make sure Docker-in-Docker feature is enabled"
    exit 1
fi

# Option 1: Quick build with pre-built Void image
quick_build() {
    echo "Using quick build method..."
    echo ""
    
    docker run --rm \
        --privileged \
        -v "$PROJECT_DIR:/workspace:ro" \
        -v "$OUTPUT_DIR:/output" \
        ghcr.io/void-linux/void-glibc-full:latest \
        bash -c '
            set -e
            echo "Installing build tools..."
            xbps-install -Syu -y
            xbps-install -y git xorriso squashfs-tools dosfstools e2fsprogs mtools grub-x86_64-efi grub-i386-pc liblz4
            
            echo "Cloning void-mklive..."
            cd /tmp
            git clone --depth 1 https://github.com/void-linux/void-mklive.git
            cd void-mklive
            
            echo "Building minimal ISO..."
            ./mklive.sh \
                -a x86_64 \
                -p "base-system linux grub-x86_64-efi NetworkManager vim curl" \
                -S "dbus NetworkManager" \
                -o "/output/mycel-os-minimal-$(date +%Y%m%d).iso" \
                -T "Mycel OS" \
                2>&1 | tee /output/build.log
            
            echo "Build complete!"
            ls -lh /output/*.iso
        '
}

# Option 2: Full build with custom Dockerfile
full_build() {
    echo "Building custom Void Linux builder image..."
    docker build -t mycel-iso-builder -f "$PROJECT_DIR/docker/Dockerfile.void-builder" "$PROJECT_DIR"
    
    echo "Running ISO build..."
    docker run --rm \
        --privileged \
        -v "$PROJECT_DIR:/workspace:ro" \
        -v "$OUTPUT_DIR:/output" \
        mycel-iso-builder
}

# Parse arguments
case "${1:-quick}" in
    quick|minimal)
        quick_build
        ;;
    full)
        full_build
        ;;
    *)
        echo "Usage: $0 [quick|full]"
        echo "  quick - Minimal ISO with base packages (~5-10 min)"
        echo "  full  - Full ISO with all packages (~15-30 min)"
        exit 1
        ;;
esac

echo ""
echo "================================"
echo "  Build Complete!"
echo "================================"
echo ""
echo "Output files:"
ls -lh "$OUTPUT_DIR"/*.iso 2>/dev/null || echo "No ISO files found"
echo ""
echo "To test the ISO:"
echo "  ./scripts/test-iso.sh"
echo ""
