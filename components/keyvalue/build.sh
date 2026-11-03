#!/usr/bin/env bash
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
export CARGO_TARGET_DIR="$root/target/keyvalue"
cargo build --locked --release --target wasm32-wasip2 --manifest-path "$root/components/keyvalue/Cargo.toml"
cp "$CARGO_TARGET_DIR/wasm32-wasip2/release/spin_kv_provider.wasm" "$CARGO_TARGET_DIR/provider.wasm"
wac plug "$CARGO_TARGET_DIR/wasm32-wasip2/release/spin_kv_client.wasm" \
    --plug "$CARGO_TARGET_DIR/provider.wasm" -o "$CARGO_TARGET_DIR/tests.wasm"
if [[ ${1:-} == --test-hooks ]]; then
    cargo build --locked --release --target wasm32-wasip2 --manifest-path "$root/components/keyvalue/Cargo.toml" -p spin-kv-provider --features test-hooks
    wac plug "$CARGO_TARGET_DIR/wasm32-wasip2/release/spin_kv_client.wasm" \
        --plug "$CARGO_TARGET_DIR/wasm32-wasip2/release/spin_kv_provider.wasm" -o "$CARGO_TARGET_DIR/tests-faults.wasm"
fi
