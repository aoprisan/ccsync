#!/usr/bin/env bash
# Build ccsync in release mode and copy the binary onto your PATH.
# Equivalent to `cargo build --release && ./target/release/ccsync install`.
set -euo pipefail

# Run from the repo root regardless of where this script is invoked from.
cd "$(dirname "$0")"

echo "Building release binary..."
cargo build --release

echo "Installing onto PATH..."
./target/release/ccsync install

echo "Cleaning build artifacts..."
cargo clean
