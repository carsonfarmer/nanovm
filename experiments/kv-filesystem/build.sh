#!/usr/bin/env bash
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
[[ $(wac --version) == 'wac-cli 0.12.0' ]] || { echo 'Install wac-cli 0.12.0 with --locked' >&2; exit 1; }
export CARGO_TARGET_DIR="$root/target/kv-filesystem"
cargo build --locked --release --target wasm32-wasip2 --manifest-path "$root/experiments/kv-filesystem/Cargo.toml"
wac plug "$CARGO_TARGET_DIR/wasm32-wasip2/release/kv_filesystem_client.wasm" \
    --plug "$CARGO_TARGET_DIR/wasm32-wasip2/release/kv_filesystem_probe.wasm" \
    -o "$CARGO_TARGET_DIR/composed.wasm"
wac plug "$CARGO_TARGET_DIR/wasm32-wasip2/release/spin_kv_client.wasm" \
    --plug "$CARGO_TARGET_DIR/wasm32-wasip2/release/spin_kv_provider.wasm" \
    -o "$CARGO_TARGET_DIR/spin-composed.wasm"
