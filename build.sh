#!/bin/bash
set -e
cargo build --release

# Codesign (optional — requires a local Apple Development certificate)
if codesign --force --sign "Apple Development" --identifier com.micktaiwan.kova --entitlements Kova.entitlements ~/.cargo/target/release/kova 2>/dev/null; then
    echo "Build + codesign done"
else
    echo "Build done (codesign skipped — no certificate found)"
fi
