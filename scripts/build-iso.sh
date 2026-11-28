#!/bin/bash
# Build Mycel OS x86_64-musl ISO
# 1. Builds the Void musl Docker builder image
# 2. Pre-builds static musl mycel-runtime binary
# 3. Builds the bootable ISO via void-mklive inside Docker
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
OUTPUT_DIR="$PROJECT_DIR/output"
PROFILE="${1:-smoke}"

case "$PROFILE" in
    smoke|quick|minimal)
        PROFILE="smoke"
        ;;
    desktop|full)
        ;;
    *)
        echo "Usage: $0 [smoke|desktop|full]"
        echo "  smoke   - Headless x86_64-musl smoke ISO (default)"
        echo "  desktop - Mycel compositor + shell desktop x86_64-musl ISO"
        echo "  full    - Full desktop x86_64-musl ISO"
        exit 1
        ;;
esac

echo ""
echo "================================"
echo "  Mycel OS ISO Builder ($PROFILE)"
echo "================================"
echo ""

mkdir -p "$OUTPUT_DIR"

if ! command -v docker &> /dev/null; then
    echo "ERROR: Docker not available" >&2
    exit 1
fi

echo "[1/3] Building Void musl builder image (mycel-iso-builder)..."
docker build -t mycel-iso-builder -f "$PROJECT_DIR/docker/Dockerfile.void-builder" "$PROJECT_DIR/scripts"

echo "[2/3] Building static musl mycel-runtime binary..."
RUNTIME_BIN="$PROJECT_DIR/mycel-runtime/target/x86_64-unknown-linux-musl/release/mycel-runtime"
CARGO_CACHE_DIR="${CARGO_CACHE_DIR:-$HOME/.cache/mycel-musl-cargo}"
mkdir -p "$CARGO_CACHE_DIR"

if command -v x86_64-linux-musl-gcc &> /dev/null; then
    (
        cd "$PROJECT_DIR/mycel-runtime"
        RUSTFLAGS="-C target-feature=+crt-static" \
            cargo build --release --target x86_64-unknown-linux-musl
    )
else
    # Build inside the Void musl container (where gcc is native musl)
    docker run --rm \
        -u "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e CARGO_HOME=/tmp/.cargo \
        -e RUSTFLAGS="-C target-feature=+crt-static" \
        -v "$CARGO_CACHE_DIR:/tmp/.cargo" \
        -v "$PROJECT_DIR:/workspace" \
        -w /workspace/mycel-runtime \
        mycel-iso-builder \
        cargo build --release --target x86_64-unknown-linux-musl
fi

if [ ! -f "$RUNTIME_BIN" ]; then
    echo "ERROR: Expected static binary not found at $RUNTIME_BIN" >&2
    exit 1
fi

echo "Verifying static linkage of $RUNTIME_BIN:"
file "$RUNTIME_BIN"
if command -v ldd &> /dev/null; then
    if ldd "$RUNTIME_BIN" 2>&1 | grep -q "libc\.so\.6"; then
        echo "ERROR: Binary is dynamically linked against glibc!" >&2
        exit 1
    fi
fi

if [ "$PROFILE" = "desktop" ] || [ "$PROFILE" = "full" ]; then
    echo "[2b/3] Building mycel-shell and mycel-compositor inside mycel-iso-builder..."
    docker run --rm \
        -u "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e CARGO_HOME=/tmp/.cargo \
        -v "$CARGO_CACHE_DIR:/tmp/.cargo" \
        -v "$PROJECT_DIR:/workspace" \
        -w /workspace/mycel-shell \
        mycel-iso-builder \
        cargo build --release

    COMPOSITOR_CARGO_ARGS=(build --release)
    if [ "$PROFILE" = "desktop" ]; then
        COMPOSITOR_CARGO_ARGS+=(--features drm)
    fi
    docker run --rm \
        -u "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e CARGO_HOME=/tmp/.cargo \
        -v "$CARGO_CACHE_DIR:/tmp/.cargo" \
        -v "$PROJECT_DIR:/workspace" \
        -w /workspace/mycel-compositor \
        mycel-iso-builder \
        cargo "${COMPOSITOR_CARGO_ARGS[@]}"

    for bin in mycel-shell mycel-compositor; do
        if [ ! -f "$PROJECT_DIR/$bin/target/x86_64-unknown-linux-musl/release/$bin" ] && \
           [ ! -f "$PROJECT_DIR/$bin/target/release/$bin" ]; then
            echo "ERROR: Expected $bin binary not found in $PROJECT_DIR/$bin/target/" >&2
            exit 1
        fi
    done
fi

echo "[2c/3] Ensuring native Void musl llama-server binary..."
LLAMA_CACHE_DIR="$PROJECT_DIR/build/llama-cpp"
LLAMA_SRC_DIR="$LLAMA_CACHE_DIR/src"
LLAMA_BIN="$LLAMA_CACHE_DIR/llama-server"
mkdir -p "$LLAMA_CACHE_DIR"

if [ ! -x "$LLAMA_BIN" ] || [ "${REBUILD_LLAMA_CPP:-0}" = "1" ]; then
    docker run --rm \
        -u "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e LLAMA_CPP_REF="${LLAMA_CPP_REF:-}" \
        -v "$PROJECT_DIR:/workspace" \
        mycel-iso-builder \
        bash -c '
            set -eo pipefail
            SRC="/workspace/build/llama-cpp/src"
            BUILD="/workspace/build/llama-cpp/build"
            OUT="/workspace/build/llama-cpp/llama-server"
            if [ ! -d "$SRC/.git" ]; then
                rm -rf "$SRC"
                if [ -n "$LLAMA_CPP_REF" ]; then
                    git clone --depth 1 --branch "$LLAMA_CPP_REF" https://github.com/ggml-org/llama.cpp.git "$SRC"
                else
                    git clone --depth 1 https://github.com/ggml-org/llama.cpp.git "$SRC"
                fi
            fi
            cmake -S "$SRC" -B "$BUILD" \
                -DCMAKE_BUILD_TYPE=Release \
                -DBUILD_SHARED_LIBS=OFF \
                -DGGML_NATIVE=OFF \
                -DLLAMA_CURL=OFF \
                -DLLAMA_BUILD_TESTS=OFF \
                -DLLAMA_BUILD_EXAMPLES=OFF \
                -DGGML_OPENMP=OFF \
                -DCMAKE_EXE_LINKER_FLAGS="-static"
            cmake --build "$BUILD" --target llama-server -j"$(nproc)"
            cp "$BUILD/bin/llama-server" "$OUT"
            chmod 0755 "$OUT"
        '
else
    echo "Using cached llama-server at $LLAMA_BIN (set REBUILD_LLAMA_CPP=1 to rebuild)"
fi

if [ ! -x "$LLAMA_BIN" ]; then
    echo "ERROR: Expected llama-server binary not found at $LLAMA_BIN" >&2
    exit 1
fi

echo "Verifying static Void musl linkage of $LLAMA_BIN:"
file "$LLAMA_BIN"
if command -v ldd &> /dev/null; then
    if ldd "$LLAMA_BIN" 2>&1 | grep -q "libc\.so\.6"; then
        echo "ERROR: llama-server is dynamically linked against glibc!" >&2
        exit 1
    fi
fi

echo "[3/3] Running void-mklive ISO build (profile=$PROFILE)..."
DOCKER_ISO_ARGS=(
    --rm
    --privileged
    -v "$PROJECT_DIR:/workspace:ro"
    -v "$OUTPUT_DIR:/output"
    -e PROFILE="$PROFILE"
)
if [ -n "${MYCEL_GGUF_MODEL:-}" ] && [ -r "$MYCEL_GGUF_MODEL" ]; then
    HOST_GGUF_ABS="$(cd "$(dirname "$MYCEL_GGUF_MODEL")" && pwd)/$(basename "$MYCEL_GGUF_MODEL")"
    GGUF_NAME="${MYCEL_GGUF_NAME:-$(basename "$MYCEL_GGUF_MODEL")}"
    DOCKER_ISO_ARGS+=(
        -v "$HOST_GGUF_ABS:/tmp/mycel-model.gguf:ro"
        -e MYCEL_GGUF_MODEL="/tmp/mycel-model.gguf"
        -e MYCEL_GGUF_NAME="$GGUF_NAME"
    )
fi

docker run "${DOCKER_ISO_ARGS[@]}" \
    mycel-iso-builder \
    /build/void-build-iso.sh "$PROFILE"

echo ""
echo "================================"
echo "  Build Complete!"
echo "================================"
echo ""
ls -lh "$OUTPUT_DIR"/*.iso
echo ""
echo "To test the ISO:"
echo "  ./scripts/test-iso.sh"
