#!/usr/bin/env bash
# Builds the docs provider and its test app (composed) into target/components/docs.
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
export CARGO_TARGET_DIR="$root/target/components"
out="$CARGO_TARGET_DIR/docs"
cargo build --locked --release --target wasm32-wasip2 --manifest-path "$root/components/docs/Cargo.toml"
mkdir -p "$out"
cp "$CARGO_TARGET_DIR/wasm32-wasip2/release/docs_provider.wasm" "$out/provider.wasm"
wac plug "$CARGO_TARGET_DIR/wasm32-wasip2/release/docs_tests.wasm" --plug "$out/provider.wasm" -o "$out/tests.wasm"
