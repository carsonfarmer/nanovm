#!/usr/bin/env bash
# WASI SDK 34; keeps experiment artifacts out of the normal guest workspace.
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
: "${WASI_SDK_PATH:?Set WASI_SDK_PATH to an unpacked WASI SDK 34}"
export CC_wasm32_wasip2="$WASI_SDK_PATH/bin/clang"
export AR_wasm32_wasip2="$WASI_SDK_PATH/bin/llvm-ar"
export CFLAGS_wasm32_wasip2="--sysroot=$WASI_SDK_PATH/share/wasi-sysroot"
export CARGO_TARGET_DIR="$root/target/sqlite-filesystem"
export LIBSQLITE3_FLAGS='-DSQLITE_OS_OTHER=1 -DSQLITE_TEMP_STORE=3'
for variant in stock p3; do
    features=()
    if [[ $variant == stock ]]; then features=(--features stock); fi
    cargo build --locked --manifest-path "$root/experiments/sqlite-filesystem/Cargo.toml" \
        --release --target wasm32-wasip2 "${features[@]}"
    cp "$CARGO_TARGET_DIR/wasm32-wasip2/release/sqlite_filesystem.wasm" "$CARGO_TARGET_DIR/$variant.wasm"
done
