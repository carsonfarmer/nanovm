#!/bin/bash
# Disk-full drill (runbooks.md, "Disk full"): a dev node with data_dir on a
# 32 MiB tmpfs, filled by a filler file, then freed. Needs root and a dev build:
#   cargo build -p nanovm --features dev --bin nanovm --example dev-registry
#   sudo docs/ops/drills/disk-full.sh target/debug
set -u
BIN=$1; PORT=28380; REG=25380
W=$(mktemp -d /var/tmp/nanovm-diskfull.XXXXXX); chmod 755 $W
mkdir $W/data && mount -t tmpfs -o size=32m,mode=755 tmpfs $W/data
pids=()
# Workers die with the supervisor, but not at once: retry the unmount.
trap 'kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null; for _ in $(seq 50); do umount $W/data 2>/dev/null && break; sleep 0.1; done; rm -rf $W' EXIT
$BIN/examples/dev-registry 127.0.0.1:$REG > $W/apps 2>/dev/null & pids+=($!)
sleep 2
for a in counter hostile; do printf 'nanovm-app: %s\nnanovm-trust: dev dev\n' "$(awk -v a=$a '$1==a{print $2}' $W/apps)" > $W/$a.h; done
printf 'listen = "127.0.0.1:%s"\ndata_dir = "%s/data"\ndev = true\ntelemetry_endpoint = "http://127.0.0.1:9"\n' $PORT $W > $W/n.toml
env -u RUST_BACKTRACE $BIN/nanovm serve --config $W/n.toml > $W/n.log 2>&1 & pids+=($!)
sleep 1
U=http://127.0.0.1:$PORT
r() { curl -s -w ' [%{http_code}]' "$@"; echo; }
echo "== before"; for _ in 1 2 3; do r -XPOST -H @$W/counter.h $U/a/incr; done
r -H @$W/hostile.h "$U/h/commit?v=before"
df -h $W/data | tail -1
fallocate -l 64M $W/data/filler 2>/dev/null || dd if=/dev/zero of=$W/data/filler bs=1M 2>/dev/null
df -h $W/data | tail -1
echo "== full: counter incr, hostile write+commit, new object, new app"
r -XPOST -H @$W/counter.h $U/a/incr
r -XPOST -H @$W/counter.h $U/a/incr
r -H @$W/hostile.h "$U/h/commit?v=during"
r -XPOST -H @$W/counter.h $U/newobj/incr
r -H @$W/counter.h $U/a/
echo "journal files (each object keeps its log): $(find $W/data/objects -path '*~journal*' -type f | sed "s|$W/data/||" | head -5 | tr '\n' ' ')"
grep -v 'OTLP\|opentelemetry\|Batch' $W/n.log | tail -5
rm $W/data/filler
echo "== freed"
r -H @$W/counter.h $U/a/
r -XPOST -H @$W/counter.h $U/a/incr
r -H @$W/hostile.h "$U/h/commit"
r -XPOST -H @$W/counter.h $U/newobj/incr
echo "journal files: $(find $W/data/objects -path '*~journal*' -type f | wc -l)"
ls -la $W/data/objects/*/ | head -20
