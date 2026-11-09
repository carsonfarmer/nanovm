#!/usr/bin/env bash
# Builds a nanovm app into the one component that `nanovm run` serves and
# `nanovm publish` pushes:
#
#   tools/build-app.sh [DIR | APP.js] [-o OUT.wasm] [--locked] [--run [nanovm run flags...]]
#
# DIR (default: the current directory) is a Rust crate on nanovm-sdk, built
# for wasm32-wasip2 in release; APP.js is a JavaScript app (js/README.md).
# If the component imports `spin:key-value` or `spin:sqlite`, this
# repository's providers are composed into it with `wac plug`: nodes link
# only the filesystem, HTTP and config, so the stores travel inside the app
# (SQLite's needs WASI_SDK_PATH, a WASI SDK 34, to build). OUT
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
        -h | --help) sed -n '2,14p' "$0"; exit ;;
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

plugs=()
if LC_ALL=C grep -aq 'spin:sqlite/sqlite@' "$wasm"; then
    : "${WASI_SDK_PATH:?build-app: SQLite is C: set WASI_SDK_PATH to an unpacked WASI SDK 34 (components/sqlite/README.md)}"
    # The flags components/sqlite/build.sh builds it with.
    CC_wasm32_wasip2="$WASI_SDK_PATH/bin/clang" AR_wasm32_wasip2="$WASI_SDK_PATH/bin/llvm-ar" \
    CFLAGS_wasm32_wasip2="--sysroot=$WASI_SDK_PATH/share/wasi-sysroot" \
    LIBSQLITE3_FLAGS='-DSQLITE_OS_OTHER=1 -DSQLITE_TEMP_STORE=3' CARGO_TARGET_DIR="$root/target/sqlite" \
        cargo build -q --locked --release --target wasm32-wasip2 \
        --manifest-path "$root/components/sqlite/Cargo.toml" -p sqlite-provider
    plugs+=(--plug "$root/target/sqlite/wasm32-wasip2/release/sqlite_provider.wasm")
fi
if LC_ALL=C grep -aq 'spin:key-value/key-value@' "$wasm"; then
    CARGO_TARGET_DIR="$root/target/keyvalue" cargo build -q --locked --release --target wasm32-wasip2 \
        --manifest-path "$root/components/keyvalue/Cargo.toml" -p spin-kv-provider
    plugs+=(--plug "$root/target/keyvalue/wasm32-wasip2/release/spin_kv_provider.wasm")
fi
if ((${#plugs[@]})); then
    need wac "it composes the key-value and SQLite providers: cargo install --locked wac-cli@0.12.0"
    # A component that already has its providers has nothing left to plug.
    if ! err=$(wac plug "$wasm" "${plugs[@]}" -o "$tmp/out.wasm" 2>&1); then
        [[ $err == *"no matching imports"* ]] || { echo "$err" >&2; exit 1; }
        cp "$wasm" "$tmp/out.wasm"
    fi
    wasm=$tmp/out.wasm
fi
mkdir -p "$(dirname "$out")"
cp "$wasm" "$out.part" && mv "$out.part" "$out"
echo "build-app: $out ($(wc -c <"$out") bytes)" >&2
((${#run[@]} == 0)) || exec "${run[@]}" "$out" "$@"
