#!/usr/bin/env bash
# Builds the queue provider and its test app (composed) into target/components/queue.
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
export CARGO_TARGET_DIR="$root/target/components"
out="$CARGO_TARGET_DIR/queue"
cargo build --locked --release --target wasm32-wasip2 --manifest-path "$root/components/queue/Cargo.toml"
mkdir -p "$out"
cp "$CARGO_TARGET_DIR/wasm32-wasip2/release/queue_provider.wasm" "$out/provider.wasm"
wac plug "$CARGO_TARGET_DIR/wasm32-wasip2/release/queue_tests.wasm" --plug "$out/provider.wasm" -o "$out/tests.wasm"
