# Mycel OS Makefile
# Common development tasks

.PHONY: all build test lint check dev release clean iso test-iso help

# Default target
all: build

# Build the runtime (debug)
build:
	cd mycel-runtime && cargo build

# Build the runtime (release)
release:
	cd mycel-runtime && cargo build --release

# Run all tests
test:
	cd mycel-runtime && cargo test

# Run linting (clippy + format check)
lint:
	cd mycel-runtime && cargo fmt --all -- --check
	cd mycel-runtime && cargo clippy --all-targets --all-features -- -D warnings

# Quick check (no codegen)
check:
	cd mycel-runtime && cargo check

# Run in development mode
dev:
	cd mycel-runtime && cargo run -- --dev --verbose

# Run with watch (auto-reload on changes)
watch:
	cd mycel-runtime && cargo watch -x 'run -- --dev --verbose'

# Security audit
audit:
	cd mycel-runtime && cargo audit

# Format code
fmt:
	cd mycel-runtime && cargo fmt --all

# Clean build artifacts
clean:
	cd mycel-runtime && cargo clean
	rm -rf output/*.iso

# Build bootable ISO (quick mode)
iso:
	./scripts/build-iso.sh quick

# Build bootable ISO (full mode)
iso-full:
	./scripts/build-iso.sh full

# Test ISO in QEMU
test-iso:
	./scripts/test-iso.sh

# Install development dependencies
deps:
	cargo install cargo-watch cargo-audit

# Help
help:
	@echo "Mycel OS Development Commands"
	@echo ""
	@echo "  make build      - Build runtime (debug)"
	@echo "  make release    - Build runtime (release)"
	@echo "  make test       - Run all tests"
	@echo "  make lint       - Run clippy and format check"
	@echo "  make check      - Quick syntax check"
	@echo "  make dev        - Run in development mode"
	@echo "  make watch      - Run with auto-reload"
	@echo "  make audit      - Security audit"
	@echo "  make fmt        - Format code"
	@echo "  make clean      - Clean build artifacts"
	@echo "  make iso        - Build quick ISO"
	@echo "  make iso-full   - Build full ISO"
	@echo "  make test-iso   - Test ISO in QEMU"
	@echo "  make deps       - Install dev dependencies"
	@echo "  make help       - Show this help"
