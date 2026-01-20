# Mycel OS - ISO Build Instructions

Complete guide for building and testing Mycel OS in GitHub Codespaces.

---

## Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    GitHub Codespace                          │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐ │
│  │  1. Develop Runtime                                     │ │
│  │     cargo build                                         │ │
│  │     cargo test                                          │ │
│  └────────────────────────────────────────────────────────┘ │
│                           │                                  │
│                           ▼                                  │
│  ┌────────────────────────────────────────────────────────┐ │
│  │  2. Build ISO (Docker)                                  │ │
│  │     ┌──────────────────────────────────┐               │ │
│  │     │  Void Linux Container            │               │ │
│  │     │  - void-mklive                   │               │ │
│  │     │  - Creates bootable ISO          │               │ │
│  │     └──────────────────────────────────┘               │ │
│  └────────────────────────────────────────────────────────┘ │
│                           │                                  │
│                           ▼                                  │
│  ┌────────────────────────────────────────────────────────┐ │
│  │  3. Test ISO (QEMU)                                     │ │
│  │     ┌──────────────────────────────────┐               │ │
│  │     │  Virtual Machine                 │               │ │
│  │     │  - Boots mycel-os.iso            │               │ │
│  │     │  - Serial console or VNC         │               │ │
│  │     └──────────────────────────────────┘               │ │
│  └────────────────────────────────────────────────────────┘ │
│                                                              │
└─────────────────────────────────────────────────────────────┘
```

---

## Quick Start

### Step 1: Create Codespace

1. Go to your GitHub repo
2. Click **Code** → **Codespaces** → **Create codespace on main**
3. Select **8-core** machine (recommended for builds)
4. Wait for setup (~3 minutes)

### Step 2: Build the Runtime

```bash
cd mycel-runtime
cargo build --release
```

### Step 3: Build the ISO

```bash
# Quick build (~5-10 minutes)
./scripts/build-iso.sh quick

# Full build with all packages (~15-30 minutes)
./scripts/build-iso.sh full
```

### Step 4: Test the ISO

```bash
# Serial console (headless)
./scripts/test-iso.sh

# Or with VNC (if you need GUI)
./scripts/test-iso.sh output/mycel-os-*.iso vnc
```

---

## Detailed Instructions

### Prerequisites Check

After Codespace starts, verify tools are available:

```bash
# Check Docker
docker --version

# Check QEMU
qemu-system-x86_64 --version

# Check Rust
cargo --version
```

### Building the Runtime

The runtime must compile before including in ISO:

```bash
cd mycel-runtime

# Debug build (fast, for development)
cargo build

# Release build (optimized, for ISO)
cargo build --release

# Run tests
cargo test

# Check for issues
cargo clippy
```

### Building the ISO

#### Option A: Minimal ISO (Fastest)

Creates a basic Void Linux ISO with essential packages:

```bash
./scripts/build-iso.sh quick
```

**Includes:**
- Base Void Linux system
- Linux kernel
- NetworkManager
- Basic utilities (vim, curl)

**Time:** ~5-10 minutes

#### Option B: Full ISO

Creates complete Mycel OS with all packages:

```bash
./scripts/build-iso.sh full
```

**Includes:**
- Everything in minimal
- Sway (Wayland compositor)
- Firefox
- Development tools
- Ollama (AI)
- mycel-runtime

**Time:** ~15-30 minutes

### Testing the ISO

#### Serial Console Mode (Default)

Best for Codespaces - no GUI needed:

```bash
./scripts/test-iso.sh
```

**Controls:**
- `Ctrl+A, X` - Exit QEMU
- `Ctrl+A, C` - QEMU monitor
- `Ctrl+A, H` - Help

**Login:**
- Username: `root`
- Password: `voidlinux` (or blank)

#### VNC Mode

If you need graphical interface:

```bash
./scripts/test-iso.sh output/mycel-os-*.iso vnc
```

Then connect via:
1. Find the forwarded port in Codespaces "Ports" tab
2. Connect with VNC client to that port
3. Or use noVNC web interface if available

### Port Forwarding

The QEMU VM forwards these ports:

| VM Port | Host Port | Service |
|---------|-----------|---------|
| 22 | 2222 | SSH |
| 11434 | 11434 | Ollama |

SSH into the running VM:
```bash
ssh -p 2222 root@localhost
```

---

## Customizing the ISO

### Adding Packages

Edit `scripts/void-build-iso.sh`:

```bash
PACKAGES="
base-system
linux
# Add your packages here
neofetch
btop
"
```

### Adding Custom Files

To include files in the ISO, modify the build script to use `mklive.sh -I` option:

```bash
./mklive.sh \
    -I /path/to/include/dir \
    ...
```

### Including mycel-runtime

After building the runtime:

```bash
# Build runtime
cd mycel-runtime
cargo build --release

# Copy to include directory
mkdir -p ../iso-include/usr/local/bin
cp target/release/mycel-runtime ../iso-include/usr/local/bin/

# Build ISO with runtime included
./scripts/build-iso.sh full
```

---

## Troubleshooting

### Docker not available

```bash
# Check if Docker daemon is running
docker ps

# If not, the Docker-in-Docker feature may have failed
# Try rebuilding the Codespace
```

### Build fails with "No space left"

```bash
# Clean up Docker
docker system prune -a

# Check disk space
df -h
```

### QEMU won't start

```bash
# Check if KVM is available (may not be in Codespaces)
ls -la /dev/kvm

# If not, QEMU will use software emulation (slower)
# Remove -cpu host from the script
```

### ISO won't boot

```bash
# Check build log
cat output/build.log

# Verify ISO is valid
file output/mycel-os-*.iso
```

### VNC connection refused

```bash
# Check if QEMU is running
ps aux | grep qemu

# Check port is listening
netstat -tlnp | grep 5900

# Make sure port 5900 is forwarded in Codespaces
```

---

## Build Artifacts

After successful build:

```
output/
├── mycel-os-20240119.iso    # Bootable ISO image
├── mycel-disk.qcow2         # Persistent disk for VM
└── build.log                # Build output log
```

---

## CI/CD Integration

To build ISOs automatically:

```yaml
# .github/workflows/build-iso.yml
name: Build ISO

on:
  push:
    tags: ['v*']

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      
      - name: Build ISO
        run: ./scripts/build-iso.sh quick
        
      - name: Upload ISO
        uses: actions/upload-artifact@v4
        with:
          name: mycel-os-iso
          path: output/*.iso
```

---

## Next Steps

1. **Customize packages** - Add software you need
2. **Add mycel-runtime** - Include the AI runtime in ISO
3. **Configure services** - Set up auto-start for mycel-runtime
4. **Brand the ISO** - Customize boot screen, wallpaper
5. **Test thoroughly** - Boot on real hardware if possible

---

## Commands Reference

| Command | Description |
|---------|-------------|
| `./scripts/build-iso.sh quick` | Build minimal ISO |
| `./scripts/build-iso.sh full` | Build full ISO |
| `./scripts/test-iso.sh` | Test ISO (serial) |
| `./scripts/test-iso.sh ISO vnc` | Test ISO (VNC) |
| `docker system prune -a` | Clean Docker cache |
| `ls -lh output/` | List built ISOs |
