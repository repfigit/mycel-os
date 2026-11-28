#!/bin/bash
# Wrapper for the unified Void x86_64-musl ISO build pipeline
set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "$SCRIPT_DIR/build-iso.sh" "${1:-smoke}"
