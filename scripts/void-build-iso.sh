#!/bin/bash
# Build Mycel OS x86_64-musl ISO inside Void Linux container
set -eo pipefail

PROFILE="${1:-${PROFILE:-smoke}}"
OUTPUT_DIR="${OUTPUT_DIR:-/output}"
WORKSPACE="${WORKSPACE:-/workspace}"
ARCH="x86_64-musl"
REPO="${VOID_REPO:-https://repo-default.voidlinux.org/current/musl}"
DATE="$(date +%Y%m%d)"

echo ""
echo "    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗     "
echo "    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║     "
echo "    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║     "
echo "    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║     "
echo "    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗"
echo "    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝"
echo ""
echo "Building Mycel OS ISO (profile=${PROFILE}, arch=${ARCH})..."
echo ""

mkdir -p "$OUTPUT_DIR"

case "$PROFILE" in
    smoke|quick|minimal)
        PROFILE="smoke"
        PACKAGES="base-system linux grub-x86_64-efi grub dhcpcd iproute2 iputils curl ca-certificates bubblewrap python3 wireguard-tools"
        SERVICES="agetty-ttyS0 dhcpcd"
        ;;
    desktop)
        PACKAGES="base-system linux grub-x86_64-efi grub dhcpcd iproute2 iputils curl ca-certificates bubblewrap python3 wireguard-tools seatd eudev mesa-dri libdrm libxkbcommon libinput pixman wayland dejavu-fonts-ttf"
        SERVICES="agetty-ttyS0 dhcpcd udevd seatd"
        ;;
    full)
        PACKAGES="base-system linux linux-firmware grub-x86_64-efi grub NetworkManager iwd bluez pipewire wireplumber elogind dbus polkit xorg-minimal xorg-fonts mesa-dri sway foot bemenu mako grim slurp wl-clipboard firefox curl wget git vim neovim htop tmux zsh python3 bubblewrap wireguard-tools"
        SERVICES="agetty-ttyS0 dbus NetworkManager elogind polkit"
        ;;
    *)
        echo "ERROR: Unknown profile '$PROFILE' (expected: smoke | desktop | full)" >&2
        exit 1
        ;;
esac

echo "[1/4] Verifying static musl mycel-runtime binary..."
RUNTIME_BIN="$WORKSPACE/mycel-runtime/target/x86_64-unknown-linux-musl/release/mycel-runtime"
CLI_SCRIPT="$WORKSPACE/tools/mycel-cli.py"

if [ ! -f "$RUNTIME_BIN" ]; then
    echo "ERROR: Static musl binary not found at $RUNTIME_BIN" >&2
    echo "Build it first with: cargo build --release --target x86_64-unknown-linux-musl" >&2
    exit 1
fi
if [ ! -f "$CLI_SCRIPT" ]; then
    echo "ERROR: CLI script not found at $CLI_SCRIPT" >&2
    exit 1
fi

echo "Binary info: $(file "$RUNTIME_BIN")"

echo "[2/4] Preparing rootfs overlay..."
INCLUDEDIR="/tmp/mycel-includedir"
rm -rf "$INCLUDEDIR"
mkdir -p \
    "$INCLUDEDIR/usr/local/bin" \
    "$INCLUDEDIR/usr/local/lib/mycel" \
    "$INCLUDEDIR/etc/mycel" \
    "$INCLUDEDIR/etc/sv/mycel-runtime" \
    "$INCLUDEDIR/etc/runit/runsvdir/default"

# Real static musl mycel-runtime binary
install -m 0755 "$RUNTIME_BIN" "$INCLUDEDIR/usr/local/bin/mycel-runtime"

if [ "$PROFILE" = "desktop" ] || [ "$PROFILE" = "full" ]; then
    for bin in mycel-compositor mycel-shell; do
        BIN_PATH="$WORKSPACE/$bin/target/x86_64-unknown-linux-musl/release/$bin"
        if [ ! -f "$BIN_PATH" ]; then
            BIN_PATH="$WORKSPACE/$bin/target/release/$bin"
        fi
        if [ ! -f "$BIN_PATH" ]; then
            echo "ERROR: $bin binary not found (checked target/x86_64-unknown-linux-musl/release/$bin and target/release/$bin)" >&2
            exit 1
        fi
        install -m 0755 "$BIN_PATH" "$INCLUDEDIR/usr/local/bin/$bin"
    done

    cat > "$INCLUDEDIR/usr/local/bin/mycel-session" << 'EOF'
#!/bin/sh
set -e
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 0700 "$XDG_RUNTIME_DIR"
export MYCEL_SOCKET="${MYCEL_SOCKET:-/run/mycel/runtime.sock}"
export LIBSEAT_BACKEND="${LIBSEAT_BACKEND:-seatd}"
exec /usr/local/bin/mycel-compositor --shell /usr/local/bin/mycel-shell "$@"
EOF
    chmod 0755 "$INCLUDEDIR/usr/local/bin/mycel-session"
fi

# Real Python CLI + thin wrapper
install -m 0755 "$CLI_SCRIPT" "$INCLUDEDIR/usr/local/lib/mycel/mycel-cli.py"
cat > "$INCLUDEDIR/usr/local/bin/mycel" << 'EOF'
#!/bin/sh
export MYCEL_SOCKET="${MYCEL_SOCKET:-/run/mycel/runtime.sock}"
exec python3 /usr/local/lib/mycel/mycel-cli.py "$@"
EOF
chmod 0755 "$INCLUDEDIR/usr/local/bin/mycel"

# Real MycelConfig TOML (top-level keys)
cat > "$INCLUDEDIR/etc/mycel/config.toml" << 'EOF'
ollama_url = "http://10.0.2.2:11434"
local_model = "qwen3:8b"
ipc_socket_path = "/run/mycel/runtime.sock"
context_path = "/var/lib/mycel"
code_path = "/var/cache/mycel/code"

[mcp]
enabled = false

[sandbox]
mode = "bwrap"
bwrap_path = "bwrap"
allow_network = false

[mesh]
enabled = false
EOF
chmod 0644 "$INCLUDEDIR/etc/mycel/config.toml"

# Runit service for mycel-runtime
cat > "$INCLUDEDIR/etc/sv/mycel-runtime/run" << 'EOF'
#!/bin/sh
exec 2>&1
mkdir -p /run/mycel /var/lib/mycel /var/cache/mycel/code
exec /usr/local/bin/mycel-runtime --daemon --config /etc/mycel/config.toml
EOF
chmod 0755 "$INCLUDEDIR/etc/sv/mycel-runtime/run"
ln -sf /etc/sv/mycel-runtime "$INCLUDEDIR/etc/runit/runsvdir/default/mycel-runtime"

# Clean branding
echo "mycel" > "$INCLUDEDIR/etc/hostname"
cat > "$INCLUDEDIR/etc/os-release" << 'EOF'
NAME="Mycel OS"
ID=mycel
ID_LIKE=void
VERSION="0.1.0"
VERSION_ID="0.1.0"
PRETTY_NAME="Mycel OS 0.1.0 (Void musl)"
HOME_URL="https://mycel-os.org"
EOF
if [ "$PROFILE" = "desktop" ]; then
    cat > "$INCLUDEDIR/etc/motd" << 'EOF'

    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗
    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║
    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║
    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║
    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗
    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝

    Welcome to Mycel OS (x86_64-musl)
    Run 'mycel-session' to launch the desktop, 'mycel --help', or 'sv status mycel-runtime'.

EOF
else
    cat > "$INCLUDEDIR/etc/motd" << 'EOF'

    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗
    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║
    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║
    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║
    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗
    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝

    Welcome to Mycel OS (x86_64-musl)
    Run 'mycel --help' or 'sv status mycel-runtime' to get started.

EOF
fi

echo "[3/4] Setting up void-mklive..."
MKLIVE_DIR="/opt/void-mklive"
if [ ! -d "$MKLIVE_DIR" ]; then
    MKLIVE_DIR="/build/void-mklive"
    if [ ! -d "$MKLIVE_DIR" ]; then
        git clone --depth 1 https://github.com/void-linux/void-mklive.git "$MKLIVE_DIR"
    fi
fi
cd "$MKLIVE_DIR"

echo "[4/4] Building ISO with mklive.sh..."
ISO_PATH="$OUTPUT_DIR/mycel-os-${PROFILE}-${ARCH}-${DATE}.iso"
rm -f "$ISO_PATH"

./mklive.sh \
    -a "$ARCH" \
    -r "$REPO" \
    -p "$PACKAGES" \
    -S "$SERVICES" \
    -I "$INCLUDEDIR" \
    -o "$ISO_PATH" \
    -T "Mycel OS" \
    -C "console=ttyS0,115200n8 console=tty0" \
    2>&1 | tee "$OUTPUT_DIR/build.log"

echo ""
echo "Build complete!"
ls -lh "$ISO_PATH"
