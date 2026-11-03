#!/bin/sh
# Builds the chat app and nanovm, then runs the app on loopback:
# open http://127.0.0.1:8080/lobby/. Extra arguments go to `nanovm run`.
set -e
cd "$(dirname "$0")/../.."
cargo build --locked --release --target wasm32-wasip2 --manifest-path guests/Cargo.toml -p chat
exec cargo run --locked --release -p nanovm -- run guests/target/wasm32-wasip2/release/chat.wasm \
    --manifest examples/chat/nanovm.json --data target/chat-data "$@"
