#!/usr/bin/env bash
# Builds the auth provider and its test app (composed) into target/components/auth.
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
export CARGO_TARGET_DIR="$root/target/components"
out="$CARGO_TARGET_DIR/auth"
cargo build --locked --release --target wasm32-wasip2 --manifest-path "$root/components/auth/Cargo.toml"
mkdir -p "$out"
cp "$CARGO_TARGET_DIR/wasm32-wasip2/release/auth_provider.wasm" "$out/provider.wasm"
wac plug "$CARGO_TARGET_DIR/wasm32-wasip2/release/auth_tests.wasm" --plug "$out/provider.wasm" -o "$out/tests.wasm"
