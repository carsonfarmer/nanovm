#!/usr/bin/env bash
# Build the beta provider and runnable example without publishing anything.
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
bash "$root/components/keyvalue/build.sh"
: "${WASI_SDK_PATH:?Set WASI_SDK_PATH to an unpacked WASI SDK 34}"
[[ $(wac --version) == 'wac-cli 0.12.0' ]] || { echo 'Install wac-cli 0.12.0 with --locked' >&2; exit 1; }
export CC_wasm32_wasip2="$WASI_SDK_PATH/bin/clang"
export AR_wasm32_wasip2="$WASI_SDK_PATH/bin/llvm-ar"
export CFLAGS_wasm32_wasip2="--sysroot=$WASI_SDK_PATH/share/wasi-sysroot"
export LIBSQLITE3_FLAGS='-DSQLITE_OS_OTHER=1 -DSQLITE_TEMP_STORE=3'
export CARGO_TARGET_DIR="$root/target/sqlite"
cargo build --locked --manifest-path "$root/components/sqlite/Cargo.toml" \
    --release --target wasm32-wasip2
# Copy the normal provider before any test-hook build can overwrite Cargo's output.
cp "$CARGO_TARGET_DIR/wasm32-wasip2/release/sqlite_provider.wasm" "$CARGO_TARGET_DIR/provider.wasm"
wac plug "$CARGO_TARGET_DIR/wasm32-wasip2/release/sqlite_example.wasm" \
    --plug "$CARGO_TARGET_DIR/provider.wasm" --plug "$root/target/keyvalue/provider.wasm" -o "$CARGO_TARGET_DIR/example.wasm"
echo "Built target/sqlite/provider.wasm and target/sqlite/example.wasm"
