#!/bin/bash
# Mycel OS Development Environment Setup
# Sets up a Void Linux-based development environment

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo ""
echo "    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗     "
echo "    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║     "
echo "    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║     "
echo "    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║     "
echo "    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗"
echo "    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝"
echo ""
echo "    The intelligent network beneath everything."
echo "    Development Environment Setup"
echo ""

# Check Docker
if ! command -v docker &> /dev/null; then
    echo -e "${RED}Error: Docker is not installed.${NC}"
    echo ""
    echo "Install Docker first:"
    echo "  https://docs.docker.com/get-docker/"
    exit 1
fi

echo -e "${GREEN}✓ Docker found${NC}"

# Get script directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

# Parse arguments
BUILD_ISO=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --build-iso)
            BUILD_ISO=true
            shift
            ;;
        --help|-h)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --build-iso    Build Mycel OS ISO after starting environment"
            echo "  --help         Show this help"
            exit 0
            ;;
        *)
            echo -e "${RED}Unknown option: $1${NC}"
            exit 1
            ;;
    esac
done

# Check for Anthropic API key
if [ -z "$ANTHROPIC_API_KEY" ]; then
    echo -e "${YELLOW}Note: ANTHROPIC_API_KEY not set.${NC}"
    echo "      Cloud AI features will be disabled."
    echo "      Set with: export ANTHROPIC_API_KEY='sk-ant-...'"
    echo ""
fi

# Build and start
echo -e "${BLUE}Building Void Linux development environment...${NC}"
echo ""

cd docker
docker compose build mycel-dev
docker compose up -d mycel-dev

echo ""
echo -e "${YELLOW}Waiting for container...${NC}"
sleep 3

echo ""
echo -e "${GREEN}=== Development Environment Ready ===${NC}"
echo ""
echo -e "${BLUE}Connect via SSH:${NC}"
echo "  ssh mycel@localhost -p 2222"
echo "  Password: mycel"
echo ""
echo -e "${BLUE}Or attach directly:${NC}"
echo "  docker exec -it mycel-dev bash"
echo ""
echo -e "${BLUE}Build Mycel Runtime:${NC}"
echo "  cd /workspace/mycel-os/mycel-runtime"
echo "  cargo build --release"
echo ""
echo -e "${BLUE}Build Mycel OS ISO:${NC}"
echo "  ./scripts/build-mycel-iso.sh"
echo ""
echo -e "${BLUE}Stop environment:${NC}"
echo "  docker compose -f docker/docker-compose.yml down"
echo ""

# Optionally build ISO
if [ "$BUILD_ISO" = true ]; then
    echo -e "${BLUE}Building Mycel OS ISO...${NC}"
    docker exec mycel-dev /workspace/mycel-os/scripts/build-mycel-iso.sh
fi
