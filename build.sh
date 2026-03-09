#!/bin/bash
set -e
cargo build --release
codesign --force --sign "Apple Development: faivrem@gmail.com (23L76N6978)" --identifier com.micktaiwan.kova --entitlements Kova.entitlements ~/.cargo/target/release/kova
echo "Build + codesign done"
