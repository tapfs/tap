#!/usr/bin/env bash
#
# build.sh -- Build the TapFS macOS File Provider extension.
#
# This script:
#   1. Builds the Rust cdylib (libtapfs.dylib) in release mode.
#   2. Compiles the Swift File Provider extension, linking against libtapfs.
#   3. Compiles a minimal host-app binary (required by macOS to load the appex).
#   4. Assembles the .app / .appex bundle structure.
#
# Usage:
#   cd tapfs/macos && ./build.sh          # release build
#   cd tapfs/macos && DEBUG=1 ./build.sh  # debug build

set -euo pipefail

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
MACOS_DIR="$SCRIPT_DIR"
SWIFT_SRC="$MACOS_DIR/TapFS/TapFSProvider"
HOST_PLIST="$MACOS_DIR/TapFS/Info.plist"
EXT_PLIST="$SWIFT_SRC/Info.plist"
BRIDGE_HEADER="$SWIFT_SRC/TapFSBridge.h"

BUILD_DIR="$MACOS_DIR/build"
BUNDLE_DIR="$BUILD_DIR/TapFS.app"
mkdir -p "$BUILD_DIR"

if [[ -n "${DEBUG:-}" ]]; then
    CARGO_PROFILE="debug"
    SWIFT_OPT=""
else
    CARGO_PROFILE="release"
    SWIFT_OPT="-O"
fi

RUST_LIB_DIR="$PROJECT_ROOT/target/$CARGO_PROFILE"
DYLIB_NAME="libtapfs.dylib"

# Detect host architecture for universal-binary friendliness.
ARCH="$(uname -m)"  # arm64 or x86_64

# ---------------------------------------------------------------------------
# Step 1: Build Rust cdylib
# ---------------------------------------------------------------------------
echo "==> Building Rust cdylib ($CARGO_PROFILE) ..."

CARGO_FLAGS=()
if [[ "$CARGO_PROFILE" == "release" ]]; then
    CARGO_FLAGS+=(--release)
fi

(cd "$PROJECT_ROOT" && cargo build "${CARGO_FLAGS[@]}")

if [[ ! -f "$RUST_LIB_DIR/$DYLIB_NAME" ]]; then
    echo "ERROR: $RUST_LIB_DIR/$DYLIB_NAME not found after cargo build" >&2
    exit 1
fi

echo "    Rust library: $RUST_LIB_DIR/$DYLIB_NAME"

# ---------------------------------------------------------------------------
# Step 2: Compile Swift File Provider extension
# ---------------------------------------------------------------------------
echo "==> Compiling Swift File Provider extension ..."

SWIFT_FILES=(
    "$SWIFT_SRC/FileProviderExtension.swift"
    "$SWIFT_SRC/FileProviderEnumerator.swift"
    "$SWIFT_SRC/FileProviderItem.swift"
)

EXT_BINARY="$BUILD_DIR/TapFSProvider"

swiftc \
    -target "${ARCH}-apple-macosx13.0" \
    $SWIFT_OPT \
    -module-name TapFSProvider \
    -application-extension \
    -import-objc-header "$BRIDGE_HEADER" \
    -I "$RUST_LIB_DIR" \
    -L "$RUST_LIB_DIR" \
    -ltapfs \
    -framework FileProvider \
    -framework Foundation \
    -Xlinker -bundle \
    "${SWIFT_FILES[@]}" \
    -o "$EXT_BINARY"

echo "    Extension binary: $EXT_BINARY"

# ---------------------------------------------------------------------------
# Step 3: Compile minimal host-app binary
# ---------------------------------------------------------------------------
echo "==> Compiling host app ..."

HOST_SWIFT="$BUILD_DIR/_TapFSHostApp.swift"
cat > "$HOST_SWIFT" << 'SWIFT_EOF'
import Foundation
import FileProvider

@main
struct TapFSApp {
    static func main() {
        // Register the File Provider domain so macOS knows about our extension.
        let domain = NSFileProviderDomain(
            identifier: NSFileProviderDomainIdentifier(rawValue: "com.tapfs.provider"),
            displayName: "TapFS"
        )
        NSFileProviderManager.add(domain) { error in
            if let error = error {
                fputs("Failed to register File Provider domain: \(error)\n", stderr)
            } else {
                fputs("TapFS File Provider domain registered.\n", stderr)
            }
        }
        // Keep the process alive briefly so the async add completes.
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 2))
    }
}
SWIFT_EOF

HOST_BINARY="$BUILD_DIR/TapFS"

swiftc \
    -target "${ARCH}-apple-macosx13.0" \
    $SWIFT_OPT \
    -parse-as-library \
    -module-name TapFS \
    -framework FileProvider \
    -framework Foundation \
    "$HOST_SWIFT" \
    -o "$HOST_BINARY"

echo "    Host binary: $HOST_BINARY"

# ---------------------------------------------------------------------------
# Step 4: Assemble the .app bundle
# ---------------------------------------------------------------------------
echo "==> Assembling TapFS.app bundle ..."

rm -rf "$BUNDLE_DIR"

# Host app structure
mkdir -p "$BUNDLE_DIR/Contents/MacOS"
mkdir -p "$BUNDLE_DIR/Contents/Frameworks"

cp "$HOST_BINARY" "$BUNDLE_DIR/Contents/MacOS/TapFS"
cp "$HOST_PLIST"  "$BUNDLE_DIR/Contents/Info.plist"

# Copy the Rust dylib into Frameworks and fix the install name.
cp "$RUST_LIB_DIR/$DYLIB_NAME" "$BUNDLE_DIR/Contents/Frameworks/$DYLIB_NAME"
install_name_tool -id "@rpath/$DYLIB_NAME" "$BUNDLE_DIR/Contents/Frameworks/$DYLIB_NAME"

# Fix rpath on the host binary.
install_name_tool -add_rpath "@executable_path/../Frameworks" \
    "$BUNDLE_DIR/Contents/MacOS/TapFS" 2>/dev/null || true
install_name_tool -change "$RUST_LIB_DIR/$DYLIB_NAME" "@rpath/$DYLIB_NAME" \
    "$BUNDLE_DIR/Contents/MacOS/TapFS" 2>/dev/null || true

# Extension (appex) structure
APPEX_DIR="$BUNDLE_DIR/Contents/PlugIns/TapFSProvider.appex"
mkdir -p "$APPEX_DIR/Contents/MacOS"

cp "$EXT_BINARY" "$APPEX_DIR/Contents/MacOS/TapFSProvider"
cp "$EXT_PLIST"  "$APPEX_DIR/Contents/Info.plist"

# Fix rpath on the extension binary so it can find libtapfs in the host's Frameworks.
install_name_tool -add_rpath "@executable_path/../../../../Frameworks" \
    "$APPEX_DIR/Contents/MacOS/TapFSProvider" 2>/dev/null || true
# Fix references to libtapfs -- cargo may place it in deps/ or directly in the profile dir.
for lib_ref in $(otool -L "$APPEX_DIR/Contents/MacOS/TapFSProvider" 2>/dev/null | grep tapfs | awk '{print $1}'); do
    install_name_tool -change "$lib_ref" "@rpath/$DYLIB_NAME" \
        "$APPEX_DIR/Contents/MacOS/TapFSProvider" 2>/dev/null || true
done

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------
echo ""
echo "Build complete!"
echo "  $BUNDLE_DIR"
echo ""
echo "Bundle layout:"
find "$BUNDLE_DIR" -type f | sort | sed "s|$BUILD_DIR/||"
echo ""
echo "To register the File Provider domain, run:"
echo "  $BUNDLE_DIR/Contents/MacOS/TapFS"
