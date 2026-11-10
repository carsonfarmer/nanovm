#!/usr/bin/env bash
# Builds everything the pre-initialization spike (K-100) measures, into $OUT
# (default target/preinit): the pre-initializer, the experiment guest's
# variants with and without pre-initialization, and the SQLite example with
# the patched provider with and without it. Needs WASI SDK 34
# (WASI_SDK_PATH), wac 0.12, and components/sqlite/build.sh run once.
# Then: see README.md, "Reproduce".
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
here="$root/experiments/preinit"
out=${OUT:-$root/target/preinit}
mkdir -p "$out"
: "${WASI_SDK_PATH:?Set WASI_SDK_PATH to an unpacked WASI SDK 34}"

CARGO_TARGET_DIR="$out/preinit-target" cargo build -q --locked --release --manifest-path "$here/tool/Cargo.toml"
preinit="$out/preinit-target/release/preinit"

for variant in plain heavy touch-data; do
    features=()
    [[ $variant != plain ]] && features=(--features "$variant")
    CARGO_TARGET_DIR="$out/preinit-guest" cargo build -q --locked --release --target wasm32-wasip2 \
        --manifest-path "$here/guest/Cargo.toml" "${features[@]}"
    cp "$out/preinit-guest/wasm32-wasip2/release/preinit_guest.wasm" "$out/$variant.wasm"
done
# std's HashMap asks for wasi:random/insecure-seed: --seed answers a constant.
"$preinit" "$out/plain.wasm" "$out/plain-pre.wasm" --seed
"$preinit" "$out/heavy.wasm" "$out/heavy-pre.wasm" --seed
if "$preinit" "$out/touch-data.wasm" "$out/touch-data-pre.wasm" --seed 2>"$out/touch-data.err"; then
    echo "pre-initialization accepted an initializer that reads /data" >&2
    exit 1
fi
grep -o 'unknown import: `[^`]*`' "$out/touch-data.err"

# The SQLite provider with a wizer-initialize export, built from a patched copy
# laid out as in the repository (its path dependency on guests/util).
rm -rf "$out/repo" && mkdir -p "$out/repo/components"
cp -r "$root/components/sqlite" "$out/repo/components/"
ln -s "$root/guests" "$out/repo/guests"
patch -s -d "$out/repo/components/sqlite" -p1 <"$here/sqlite-init.patch"
export CC_wasm32_wasip2="$WASI_SDK_PATH/bin/clang" AR_wasm32_wasip2="$WASI_SDK_PATH/bin/llvm-ar"
export CFLAGS_wasm32_wasip2="--sysroot=$WASI_SDK_PATH/share/wasi-sysroot"
export LIBSQLITE3_FLAGS='-DSQLITE_OS_OTHER=1 -DSQLITE_TEMP_STORE=3'
CARGO_TARGET_DIR="$out/sqlite-target" cargo build -q --locked --release --target wasm32-wasip2 \
    --manifest-path "$out/repo/components/sqlite/Cargo.toml" -p sqlite-provider
cp "$out/sqlite-target/wasm32-wasip2/release/sqlite_provider.wasm" "$out/provider-init.wasm"
"$preinit" "$out/provider-init.wasm" "$out/provider-pre.wasm"
example="$root/target/sqlite/wasm32-wasip2/release/sqlite_example.wasm"
[[ -f $example ]] || { echo "run components/sqlite/build.sh first" >&2; exit 1; }
for p in init pre; do
    wac plug "$example" --plug "$out/provider-$p.wasm" --plug "$root/target/keyvalue/provider.wasm" \
        -o "$out/sqlite-$( [[ $p == init ]] && echo orig || echo pre ).wasm"
done
ls -l "$out"/*.wasm
