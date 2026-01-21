#!/bin/bash
# Local development script - runs mycel with optimal LLM performance
#
# Usage:
#   ./scripts/local-dev.sh          # Uses host's Ollama (recommended)
#   ./scripts/local-dev.sh --gpu    # Run Ollama in container with GPU
#   ./scripts/local-dev.sh --cloud  # Use Claude API only

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

cd "$PROJECT_DIR"

MODE="${1:-host}"

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${GREEN}"
echo "    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗     "
echo "    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║     "
echo "    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║     "
echo "    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║     "
echo "    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗"
echo "    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝"
echo -e "${NC}"
echo "Local Development Environment"
echo ""

case "$MODE" in
    --host|host)
        echo -e "${YELLOW}Mode: Using host's Ollama${NC}"
        echo "Make sure Ollama is running on your host: ollama serve"
        echo ""

        # Check if host Ollama is accessible
        if curl -s http://localhost:11434/api/tags >/dev/null 2>&1; then
            echo "✅ Host Ollama detected"
        else
            echo "⚠️  Host Ollama not running. Start it with: ollama serve"
            echo "   Or use --cloud mode: ./scripts/local-dev.sh --cloud"
            exit 1
        fi

        docker compose -f docker/docker-compose.yml run --rm \
            --network host \
            -e OLLAMA_URL=http://localhost:11434 \
            mycel-dev
        ;;

    --gpu|gpu)
        echo -e "${YELLOW}Mode: Container with GPU${NC}"
        echo "Requires NVIDIA Container Toolkit"
        echo ""

        docker compose -f docker/docker-compose.yml up -d mycel-dev
        docker compose -f docker/docker-compose.yml exec mycel-dev bash
        ;;

    --cloud|cloud)
        echo -e "${YELLOW}Mode: Cloud API only${NC}"

        if [ -z "$ANTHROPIC_API_KEY" ]; then
            echo "⚠️  ANTHROPIC_API_KEY not set"
            echo "   export ANTHROPIC_API_KEY='sk-ant-...'"
            exit 1
        fi

        docker compose -f docker/docker-compose.yml run --rm \
            -e ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" \
            mycel-dev
        ;;

    *)
        echo "Usage: $0 [--host|--gpu|--cloud]"
        echo ""
        echo "  --host   Use host's Ollama (default, recommended)"
        echo "  --gpu    Run Ollama in container with GPU passthrough"
        echo "  --cloud  Use Claude API only (requires ANTHROPIC_API_KEY)"
        exit 1
        ;;
esac
