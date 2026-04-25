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
mkdir -p "$INSTALL_DIR"

# Try GitHub Release download first
DOWNLOAD_OK=false
if command -v curl >/dev/null 2>&1; then
    if curl -fsSL "$URL" -o "${INSTALL_DIR}/tap" 2>/dev/null; then
        DOWNLOAD_OK=true
    fi
elif command -v wget >/dev/null 2>&1; then
    if wget -q "$URL" -O "${INSTALL_DIR}/tap" 2>/dev/null; then
        DOWNLOAD_OK=true
    fi
fi

if [ "$DOWNLOAD_OK" = true ]; then
    chmod +x "${INSTALL_DIR}/tap"
    echo "tapfs installed to ${INSTALL_DIR}/tap"
else
    # Fallback: build from source
    echo "Binary not available for ${OS}/${ARCH}. Building from source..."
    if ! command -v cargo >/dev/null 2>&1; then
        echo "Error: cargo not found. Install Rust first: https://rustup.rs"
        exit 1
    fi
    TMPDIR=$(mktemp -d)
    git clone --depth 1 "https://github.com/${REPO}.git" "$TMPDIR/tap" 2>/dev/null || \
        git clone --depth 1 "git@github.com:${REPO}.git" "$TMPDIR/tap"
    cd "$TMPDIR/tap"
    if [ "$OS" = "darwin" ]; then
        cargo build --release --no-default-features --features nfs
    else
        cargo build --release
    fi
    cp target/release/tap "${INSTALL_DIR}/tap"
    rm -rf "$TMPDIR"
    echo "tapfs built and installed to ${INSTALL_DIR}/tap"
fi

echo ""

# Check if already in PATH
if echo "$PATH" | grep -q "$INSTALL_DIR"; then
    "${INSTALL_DIR}/tap" connectors 2>/dev/null || true
else
    echo "Add to your shell profile:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo ""
    echo "Then run:"
    echo "  tap connectors                    # list available connectors"
    echo "  tap mount github                  # mount GitHub API"
    echo "  tap setup claude --append         # connect to Claude Code"
fi
