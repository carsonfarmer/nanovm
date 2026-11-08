#!/usr/bin/env bash
# Builds a nanovm app into the one component that `nanovm run` serves and
# `nanovm publish` pushes:
#
#   tools/build-app.sh [DIR | APP.js] [-o OUT.wasm] [--locked] [--run [nanovm run flags...]]
#
# DIR (default: the current directory) is a Rust crate on nanovm-sdk, built
# for wasm32-wasip2 in release; APP.js is a JavaScript app (js/README.md).
# If the component imports `spin:key-value`, this repository's key-value
# provider is composed into it with `wac plug`: nodes link only the
# filesystem, HTTP and config, so the store travels inside the app. OUT
# defaults to <name>.wasm in the current directory. With --run, the app is
# then served with `nanovm run OUT [flags]` (`$NANOVM`, else nanovm on PATH).
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
src=. out= locked=() run=()
while (($#)); do
    case $1 in
        -o) out=$2; shift 2 ;;
        --locked) locked=(--locked); shift ;;
        --run) shift; run=("${NANOVM:-nanovm}" run); break ;;
        -h | --help) sed -n '2,13p' "$0"; exit ;;
        *) src=$1; shift ;;
    esac
done
need() { command -v "$1" >/dev/null || { echo "build-app: $1 not found; $2" >&2; exit 1; }; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [[ $src == *.js ]]; then
    wasm=$tmp/$(basename "${src%.js}").wasm
    bash "$root/js/build.sh" "$src" "$wasm" "${locked[@]}"
else
    need cargo "install Rust: https://rustup.rs"
    # The cdylib's artifact is the last .wasm cargo reports.
    wasm=$(cargo build "${locked[@]}" --release --target wasm32-wasip2 --manifest-path "$src/Cargo.toml" \
        --message-format=json-render-diagnostics | { grep -o '"filenames":\["[^"]*\.wasm"' || true; } | tail -1 | sed 's/.*\["//; s/"$//')
    [[ -n $wasm ]] || { echo "build-app: $src built no .wasm; is it a [lib] crate-type = [\"cdylib\"]?" >&2; exit 1; }
fi
out=${out:-$(basename "$wasm")}

if LC_ALL=C grep -aq 'spin:key-value/key-value@' "$wasm"; then
    need wac "it composes the key-value provider: cargo install --locked wac-cli@0.12.0"
    CARGO_TARGET_DIR="$root/target/keyvalue" cargo build -q --locked --release --target wasm32-wasip2 \
        --manifest-path "$root/components/keyvalue/Cargo.toml" -p spin-kv-provider
    provider=$root/target/keyvalue/wasm32-wasip2/release/spin_kv_provider.wasm
    # A component that already has a provider has nothing left to plug.
    if ! err=$(wac plug "$wasm" --plug "$provider" -o "$tmp/out.wasm" 2>&1); then
        [[ $err == *"no matching imports"* ]] || { echo "$err" >&2; exit 1; }
        cp "$wasm" "$tmp/out.wasm"
    fi
    wasm=$tmp/out.wasm
fi
mkdir -p "$(dirname "$out")"
cp "$wasm" "$out.part" && mv "$out.part" "$out"
echo "build-app: $out ($(wc -c <"$out") bytes)" >&2
((${#run[@]} == 0)) || exec "${run[@]}" "$out" "$@"
