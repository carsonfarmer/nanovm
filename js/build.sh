#!/bin/sh
# Builds a nanovm component from a JavaScript app:  js/build.sh app.js [out.wasm]
set -e
src=$(realpath "$1")
out=$(realpath -m "${2:-${1%.js}.wasm}")
cd "$(dirname "$0")"
NANOVM_JS="$src" cargo build --locked --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/nanovm_js.wasm "$out"
ls -l "$out"
