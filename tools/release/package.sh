#!/bin/bash
# Packages a built nanovm binary as a release archive with its checksum:
#   tools/release/package.sh <binary> <version tag> <target triple> <out dir>
# writes <out>/nanovm-<tag>-<target>.tar.gz and .tar.gz.sha256. The archive
# holds the binary, README, LICENSE files if any, and deploy/ (units, image,
# configs). Used by .github/workflows/release.yml; runs the same locally.
set -euo pipefail
[ $# -eq 4 ] || { echo "usage: $0 <binary> <tag> <target> <out dir>" >&2; exit 2; }
bin=$1 tag=$2 target=$3 out=$4
repo=$(cd "$(dirname "$0")/../.." && pwd)
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "not a version tag: $tag" >&2; exit 2; }
usage=$("$bin" 2>&1 || true)
[[ $usage == *"usage: nanovm serve"* ]] || { echo "$bin is not a nanovm binary" >&2; exit 1; }
name=nanovm-$tag-$target
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name" "$out"
install -m755 "$bin" "$stage/$name/nanovm"
cp "$repo/README.md" "$stage/$name/"
for f in "$repo"/LICENSE*; do if [ -e "$f" ]; then cp "$f" "$stage/$name/"; fi; done
if [ -d "$repo/deploy" ]; then cp -r "$repo/deploy" "$stage/$name/"; fi
# Reproducible archive: fixed order, owner and mtime (the tagged commit's).
mtime=$(git -C "$repo" log -1 --format=%ct 2>/dev/null || echo 0)
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$mtime" -C "$stage" -cf - "$name" \
    | gzip -n >"$out/$name.tar.gz"
(cd "$out" && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
echo "$out/$name.tar.gz"
