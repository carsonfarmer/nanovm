#!/bin/sh
# Builds the demo/test guest components (wasm32-wasip2 + wasip3 bindings).
set -e
cd "$(dirname "$0")/../guests"
cargo build --release --target wasm32-wasip2
ls -l target/wasm32-wasip2/release/*.wasm
