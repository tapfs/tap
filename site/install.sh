#!/bin/sh
set -e

REPO="tapfs/tap"
INSTALL_DIR="${TAPFS_INSTALL_DIR:-$HOME/.tapfs/bin}"

# Detect OS
OS="$(uname -s)"
case "$OS" in
    Linux)  OS="linux" ;;
    Darwin) OS="darwin" ;;
    *) echo "Unsupported OS: $OS"; exit 1 ;;
esac

# Detect architecture
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|amd64) ARCH="x64" ;;
    aarch64|arm64) ARCH="arm64" ;;
    *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

BINARY="tap-${OS}-${ARCH}"
URL="https://github.com/${REPO}/releases/latest/download/${BINARY}"

echo "Installing tapfs (${OS}/${ARCH})..."

# Download
mkdir -p "$INSTALL_DIR"
if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$URL" -o "${INSTALL_DIR}/tap"
elif command -v wget >/dev/null 2>&1; then
    wget -q "$URL" -O "${INSTALL_DIR}/tap"
else
    echo "Error: curl or wget required"; exit 1
fi

chmod +x "${INSTALL_DIR}/tap"

echo ""
echo "tapfs installed to ${INSTALL_DIR}/tap"

# Check if already in PATH
if command -v tap >/dev/null 2>&1; then
    echo ""
    tap connectors 2>/dev/null || true
else
    echo ""
    echo "Add to your shell profile:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo ""
    echo "Then run:"
    echo "  tap connectors                    # list available connectors"
    echo "  tap mount github                  # mount GitHub API"
    echo "  tap setup claude --append         # connect to Claude Code"
fi
