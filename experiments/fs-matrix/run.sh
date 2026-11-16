#!/bin/bash
# Filesystem matrix for durable writes (E-63): fsync latency (fsync.py) and a
# dev node's `POST /incr` throughput, one object serially and 100 objects at
# 64 concurrent, with data_dir on each filesystem. Measurement only. Needs root,
# losetup, mkfs.xfs/mkfs.btrfs if those rows are wanted, oha, and a dev build:
#   cargo build -p nanovm --features dev --bin nanovm --example dev-registry
#   sudo experiments/fs-matrix/run.sh OUT_DIR [ROUNDS]
# Writes OUT_DIR/results.jsonl, one line per (filesystem, round).
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
BIN=${BIN:-$REPO/target/debug}
OHA=${OHA:-oha}
OUT=$1 ROUNDS=${2:-3}
PORT=${PORT:-28280} REG=${REG:-25280}
mkdir -p "$OUT"; OUT=$(cd "$OUT" && pwd)
# Workers run as other uids and must traverse to data_dir: not under a 0700 dir.
W=$(mktemp -d /var/tmp/nanovm-fs-matrix.XXXXXX) && chmod 755 "$W" && mkdir -p "$W/mnt"
ulimit -n 20000
loops=() pids=()
cleanup() {
    kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null
    for m in "$W"/mnt/*; do mountpoint -q "$m" && umount "$m"; done
    for l in "${loops[@]}"; do losetup -d "$l"; done
    rm -rf "$W"
}
trap cleanup EXIT

# name -> directory. ext4 is this disk (as mounted); the loop devices sit in
# files on it, so ext4-loop is their control.
declare -A DIRS
DIRS[ext4]=$W/native; mkdir -p "$W/native"
mkdir -p "$W/mnt/tmpfs" && mount -t tmpfs -o size=1g,mode=755 tmpfs "$W/mnt/tmpfs" && DIRS[tmpfs]=$W/mnt/tmpfs
loopfs() { # NAME MKFS-COMMAND MOUNT-OPTIONS
    local img=$W/$1.img dev
    truncate -s 2G "$img"
    dev=$(losetup -f --show "$img") && loops+=("$dev")
    $2 "$dev" >/dev/null 2>&1 || { echo "skip $1: $2 failed"; return; }
    mkdir -p "$W/mnt/$1" && mount -o "$3" "$dev" "$W/mnt/$1" && chmod 755 "$W/mnt/$1" && DIRS[$1]=$W/mnt/$1
}
loopfs ext4-loop "mkfs.ext4 -q -F" defaults
loopfs ext4-loop-discard "mkfs.ext4 -q -F" discard
command -v mkfs.xfs >/dev/null && loopfs xfs-loop "mkfs.xfs -q -f" defaults
command -v mkfs.btrfs >/dev/null && loopfs btrfs-loop "mkfs.btrfs -q -f" defaults
names=("${!DIRS[@]}")
mapfile -t names < <(printf '%s\n' "${names[@]}" | sort)
echo "filesystems: ${names[*]}"
findmnt -no SOURCE,FSTYPE,OPTIONS --target "$W/native" >"$OUT/mounts.txt"
for n in "${names[@]}"; do echo "$n $(findmnt -no FSTYPE,OPTIONS --target "${DIRS[$n]}")" >>"$OUT/mounts.txt"; done

"$BIN/examples/dev-registry" "127.0.0.1:$REG" >"$W/apps" 2>/dev/null & pids+=($!)
for _ in $(seq 100); do [ "$(grep -c ' 127' "$W/apps")" = 2 ] && break; sleep 0.1; done
APP=$(awk '$1 == "counter" {print $2}' "$W/apps")
H=(-H "nanovm-app: $APP" -H 'nanovm-trust: dev dev')

oha_run() { # C N URL -> "rps p50_ms p99_ms ok other-statuses"
    "$OHA" --no-tui --output-format json -m POST -c "$1" -n "$2" "${H[@]}" --rand-regex-url "$3" |
        python3 -c 'import json,sys; o=json.load(sys.stdin); l=o["latencyPercentiles"]
print(round(o["summary"]["requestsPerSec"]), round(l["p50"]*1e3, 3), round(l["p99"]*1e3, 3), o["statusCodeDistribution"].get("200", 0),
      ",".join(f"{k}:{v}" for k, v in sorted(o["statusCodeDistribution"].items()) if k != "200") or "-")'
}

measure() { # NAME ROUND
    local d=${DIRS[$1]}/node-$2
    rm -rf "$d"; mkdir -p "$d"
    local f; f=$(python3 -I "$HERE/fsync.py" "${DIRS[$1]}" "${FSYNC_N:-500}")
    printf 'listen = "127.0.0.1:%s"\ndata_dir = "%s"\ndev = true\nmax_live_objects = 200\ntelemetry_endpoint = "http://127.0.0.1:9"\n' \
        "$PORT" "$d" >"$W/node.toml"
    env -u RUST_BACKTRACE "$BIN/nanovm" serve --config "$W/node.toml" >"$W/node.log" 2>&1 & local pid=$!
    for _ in $(seq 100); do curl -sf "http://127.0.0.1:$PORT/_nanovm/ready" >/dev/null && break; sleep 0.1; done
    oha_run 16 400 "http://127.0.0.1:$PORT/o[0-9]{2}/incr" >/dev/null    # compile, start, warm
    read -r s_rps s_p50 s_p99 s_ok s_other <<<"$(oha_run 1 "${SERIAL_N:-500}" "http://127.0.0.1:$PORT/one/incr")"
    read -r c_rps c_p50 c_p99 c_ok c_other <<<"$(oha_run 64 "${CONC_N:-5000}" "http://127.0.0.1:$PORT/o[0-9]{2}/incr")"
    kill $pid; wait $pid 2>/dev/null
    rm -rf "$d"
    printf '{"fs":"%s","round":%s,"fsync":%s,"incr_serial":{"n":%s,"rps":%s,"p50_ms":%s,"p99_ms":%s,"ok":%s,"other":"%s"},"incr_c64":{"n":%s,"rps":%s,"p50_ms":%s,"p99_ms":%s,"ok":%s,"other":"%s"},"load1":%s}\n' \
        "$1" "$2" "$f" "${SERIAL_N:-500}" "$s_rps" "$s_p50" "$s_p99" "$s_ok" "$s_other" "${CONC_N:-5000}" "$c_rps" "$c_p50" "$c_p99" "$c_ok" "$c_other" \
        "$(cut -d' ' -f1 /proc/loadavg)" |
        tee -a "$OUT/results.jsonl"
}

for r in $(seq "$ROUNDS"); do
    order=("${names[@]}")
    [ $((r % 2)) -eq 0 ] && mapfile -t order < <(printf '%s\n' "${names[@]}" | tac)
    for n in "${order[@]}"; do measure "$n" "$r"; done
done
python3 -I "$HERE/summarize.py" "$OUT/results.jsonl" | tee "$OUT/summary.md"
