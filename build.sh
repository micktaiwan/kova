#!/bin/bash
set -e
cargo build --release

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY="$HOME/.cargo/target/release/kova"
BUNDLE="/Applications/Kova.app"
BUNDLE_BIN="$BUNDLE/Contents/MacOS/kova"

# Update bundle contents (binary + Info.plist for version)
if [ -d "$BUNDLE" ]; then
    rm -f "$BUNDLE_BIN"
    cp "$BINARY" "$BUNDLE_BIN"
    cp "$SCRIPT_DIR/Info.plist" "$BUNDLE/Contents/Info.plist"
fi

# Codesign (optional — requires a local Apple Development certificate)
# Sign the entire .app bundle so macOS preserves TCC permissions across rebuilds.
if [ -d "$BUNDLE" ]; then
    if codesign --force --sign "Apple Development" --identifier com.micktaiwan.kova --entitlements "$SCRIPT_DIR/Kova.entitlements" "$BUNDLE" 2>/dev/null; then
        echo "Build + codesign (bundle) done"
    else
        echo "Build done (codesign skipped — no certificate found)"
    fi
elif codesign --force --sign "Apple Development" --identifier com.micktaiwan.kova --entitlements "$SCRIPT_DIR/Kova.entitlements" "$BINARY" 2>/dev/null; then
    echo "Build + codesign (binary) done"
else
    echo "Build done (codesign skipped — no certificate found)"
fi
