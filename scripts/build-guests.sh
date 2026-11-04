#!/bin/sh
# Builds the demo/test guest components (wasm32-wasip2 + wasip3 bindings),
# and the JavaScript ones (js/build.sh: QuickJS needs WASI_SDK_PATH, a WASI SDK 34).
set -e
cd "$(dirname "$0")/.."
(cd guests && cargo build --release --target wasm32-wasip2)
ls -l guests/target/wasm32-wasip2/release/*.wasm
js/build.sh js/tests/app.js js/target/test-app.wasm
js/build.sh js/examples/counter.js js/target/counter.wasm
