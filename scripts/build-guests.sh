#!/usr/bin/env bash
# Builds the demo/test guest components (wasm32-wasip2 + wasip3 bindings).
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
bash "$root/components/keyvalue/build.sh"
CARGO_TARGET_DIR="$root/target/guests" cargo build --locked --release --target wasm32-wasip2 --manifest-path "$root/guests/Cargo.toml"
mkdir -p "$root/guests/target/wasm32-wasip2/release"
for guest in counter hostile; do
    wasm="$root/guests/target/wasm32-wasip2/release/$guest.wasm"
    wac plug "$root/target/guests/wasm32-wasip2/release/$guest.wasm" --plug "$root/target/keyvalue/provider.wasm" -o "$wasm.composed"
    mv "$wasm.composed" "$wasm"
done
