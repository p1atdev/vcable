#!/bin/sh
set -eu

workspace_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
derived_data="$workspace_root/.build/xcode"
project="$workspace_root/native/VCableDriver/VCableDriver.xcodeproj"

cd "$workspace_root"

cargo fmt --all -- --check
CARGO_BUILD_RUSTC_WRAPPER= cargo check --workspace --all-targets
CARGO_BUILD_RUSTC_WRAPPER= cargo test --workspace
CARGO_BUILD_RUSTC_WRAPPER= cargo clippy --workspace --all-targets -- -D warnings

xcodebuildmcp macos build \
    --project-path "$project" \
    --scheme VCableDriverTests \
    --configuration Debug \
    --derived-data-path "$derived_data" \
    --arch arm64

driver="$derived_data/Build/Products/Debug/VCableDriver.driver"
test_binary="$derived_data/Build/Products/Debug/VCableDriverTests"

/usr/bin/codesign --verify --deep --strict --verbose=2 "$driver"
/usr/bin/plutil -lint "$driver/Contents/Info.plist"
"$test_binary" "$driver/Contents/MacOS/VCableDriver"

