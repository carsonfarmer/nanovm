#!/bin/sh
# Builds a JavaScript app's component, pre-initialized but not yet composed:
#   js/build.sh app.js out.wasm [--locked]
# Use tools/build-app.sh app.js, which also composes the key-value provider.
# The runtime is compiled with the app in it, then pre-initialized: the
# component starts with the engine up and the app's module evaluated.
# QuickJS is C: it needs WASI SDK 34 (set WASI_SDK_PATH to it).
set -e
: "${WASI_SDK_PATH:?Set WASI_SDK_PATH to an unpacked WASI SDK 34}"
src=$(realpath "$1")
out=$(realpath -m "$2")
cd "$(dirname "$0")"
WASI_SDK="$WASI_SDK_PATH" NANOVM_JS="$src" cargo build $3 -q --release --target wasm32-wasip2
cargo run $3 -q --manifest-path ../Cargo.toml -p nanovm --example js-wizer -- \
    target/wasm32-wasip2/release/nanovm_js.wasm "$out"
