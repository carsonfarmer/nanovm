#!/bin/sh
# Builds a nanovm component from a JavaScript app:  js/build.sh app.js [out.wasm]
# The runtime is compiled with the app in it, then pre-initialized: the
# component starts with the engine up and the app's module evaluated.
set -e
src=$(realpath "$1")
out=$(realpath -m "${2:-${1%.js}.wasm}")
cd "$(dirname "$0")"
NANOVM_JS="$src" cargo build --locked --release --target wasm32-wasip2
cargo run --locked -q --manifest-path ../Cargo.toml -p nanovm --example js-wizer -- \
    target/wasm32-wasip2/release/nanovm_js.wasm "$out"
ls -l "$out"
