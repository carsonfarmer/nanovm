# Benchmark numbers: A, B, C (and D)

**Status (2026-11-14): harness ready, nothing measured yet.** Paused on the
coordinator's request while other agents compile on this machine (numbers
taken now would be noise). This file holds the setup and the plan; results
and tables replace this section once the run is done.

## Builds

| | Commit | What it is | Config dialect |
|---|---|---|---|
| A | `5eeb17f` | main before round 3 (merge of PR #1) | proxy `node = …`; no `cgroup`/`[clients]` keys (cgroup off by default) |
| B | `0f9ee0f` | main now, round 3 (merge of PR #2) | proxy `nodes = […]`; cgroup off by default |
| C | `7988cae` | `origin/beta/integration`, PR #3 | `cgroup = "off"` required here; `[clients]`; proxy `hosts`/`signers` |
| D | (to come) | the trimmed beta/integration | as C |

Worktrees: `/home/user/wt/bench` (branch `bench/numbers`, from `origin/main`),
detached `/home/user/wt/bench-a`, `bench-b`, `bench-c`.

## Machine

Intel Xeon Processor @ 2.10GHz, nproc 4, 15 GB RAM, no swap, Linux
6.18.44-fc-v470 (Firecracker microVM). A shared VM: other agents' work can
land on the same cores. `vm.max_map_count=1048576`, `ulimit -n` 20000 (hard).
cgroup v2 here has no controllers, so C (and D) must run with `cgroup = "off"`.

## What is built

- A's release binaries (`--features dev`): `/home/user/wt/bench/tmp/bin-a/`
  (`nanovm`, `dev-registry`, `bench`), 5 min on 4 cores. Shared target dir
  `/home/user/wt/bench/tmp/target` (1.7 GB).
- Not yet built: B, C, D release binaries; the guests; the JS app.

## Remaining setup (run when the machine is quiet)

```sh
export CARGO_TARGET_DIR=/home/user/wt/bench/tmp/target PATH=/home/user/wt/tools/bin:$PATH
# Release binaries, one build at a time (the output paths are shared; copy each out):
for x in b c; do
  (cd /home/user/wt/bench-$x && env -u RUST_BACKTRACE cargo build --locked --release -p nanovm \
     --features dev --bins --example dev-registry --example bench --example js-wizer)
  mkdir -p /home/user/wt/bench/tmp/bin-$x
  cp $CARGO_TARGET_DIR/release/{nanovm,examples/dev-registry,examples/bench,examples/js-wizer} /home/user/wt/bench/tmp/bin-$x/
done
# Guests: counter, hostile and the KV provider are identical in A, B and C, so build once and copy:
(cd /home/user/wt/bench-c && unset CARGO_TARGET_DIR && bash scripts/build-guests.sh)
for x in a b; do mkdir -p /home/user/wt/bench-$x/guests/target/wasm32-wasip2/release
  cp /home/user/wt/bench-c/guests/target/wasm32-wasip2/release/{counter,hostile}.wasm \
     /home/user/wt/bench-$x/guests/target/wasm32-wasip2/release/; done
# JS app (B, C, D; A has no JS runtime): js/examples/counter.js with each build's own runtime.
# js/build.sh runs js-wizer through `cargo run` (a debug build of the node); to save disk, run the
# release js-wizer copied above instead:
export WASI_SDK_PATH=/home/user/wt/tools/wasi-sdk-34.0-x86_64-linux
(cd /home/user/wt/bench-$x/js && unset CARGO_TARGET_DIR && WASI_SDK="$WASI_SDK_PATH" \
   NANOVM_JS=$PWD/examples/counter.js cargo build --locked -q --release --target wasm32-wasip2 &&
 /home/user/wt/bench/tmp/bin-$x/js-wizer target/wasm32-wasip2/release/nanovm_js.wasm /tmp/js.wasm &&
 wac plug /tmp/js.wasm --plug /home/user/wt/bench-c/target/keyvalue/provider.wasm \
   -o /home/user/wt/bench/tmp/bin-$x/jscounter.wasm)
```

Check each binary is its own build (the shared target dir rebuilds per worktree
path, but verify): `sha256sum tmp/bin-*/nanovm` must differ, and only C's has
the rate limiter (`strings tmp/bin-c/nanovm | grep -c rate-limited`).

## Running

```sh
sysctl -w vm.max_map_count=1048576
cd /home/user/wt/bench
# 1. Pilot: one round of the saturation sweep, to fix the fixed-rate targets.
python3 notes/bench-numbers/drive.py --work tmp/pilot --out tmp/pilot --rounds 1 --only direct \
  --build a:/home/user/wt/bench-a:/home/user/wt/bench/tmp/bin-a:r2 \
  --build b:/home/user/wt/bench-b:/home/user/wt/bench/tmp/bin-b:r3 \
  --build c:/home/user/wt/bench-c:/home/user/wt/bench/tmp/bin-c:beta
# rates.json: {case: {"50%": r, "80%": r}} at 50% and 80% of the LOWEST build's max for that case,
# so every build sees the same offered load, below its saturation.
# 2. Three alternating rounds of everything (A B C D A B C D A B C D):
python3 notes/bench-numbers/drive.py --work tmp/run --out notes/bench-numbers --rounds 3 \
  --rates notes/bench-numbers/rates.json --build … (as above, plus D)
python3 notes/bench-numbers/summarize.py notes/bench-numbers/raw.jsonl > notes/bench-numbers/tables.md
```

Expected wall time: about 15 min per build per round, so about 3 h for four
builds and three rounds. Before each build-round the driver waits for the
1-minute load average to drop below 0.5 and records the load it started at.

## Definitions and method

All HTTP numbers are from `oha` 1.16.0 (`/home/user/wt/tools/oha`) on the same
4 cores as the node; oha's own CPU is recorded per run (`client_cpu_s`). The
node runs as in production — `nanovm serve`, a supervisor running no guest
code, one jailed worker per app (own uid, Landlock, seccomp) — except: dev
mode (`--features dev`, `dev = true`: unsigned apps from a loopback registry,
`cargo run --example dev-registry`), cgroups off (A, B by default; C and D
`cgroup = "off"`), and no OTLP Collector (the default endpoint
`127.0.0.1:4318` has nothing listening, so exports fail fast and are dropped,
the same on every build). Node config: `max_live_objects = 8000`,
`memory_reservation_mb = 64`, `epoch_tick_ms = 5`. Apps: the `counter` guest
(its own `nanovm.json`: 16 MiB, 50 ms CPU, `idle_timeout_ms` 60 s,
`max_queue` default 64) and, on B, C and D, `js/examples/counter.js`.

- **node cold**: from `exec` of `nanovm serve` to the first 200 for
  `GET /<new object>/` on the counter app, polled every 1 ms (connection
  refused until it listens; the supervisor then holds the request while it
  fetches the app, starts and jails the worker, which compiles or loads the
  component). Two variants: empty `data_dir` (registry fetch and compile) and
  the `data_dir` a previous start left (blobs and `.cwasm` cached; the
  manifest is still fetched). 30 samples per variant per round.
- **app cold**: the node running, the counter app's worker killed (SIGKILL,
  reaped, 200 ms later), then one `GET` timed: the supervisor fetches the
  manifest, starts and jails a fresh worker, which loads the component.
  Variants: compile cache warm, and with `data_dir/cache/<app>/*.cwasm`
  deleted first (the worker compiles). 30 samples each per round.
- **object cold**: the first request to a new object of a running app:
  `GET /<16 random letters>/`, 2,000 serially (c=1), and 6,000 at c=64.
- **warm**: `GET /o000/` to a live object, 5,000 serially.
- **throughput**: `oha -z 6s -c {1,16,64,256}` after a 2 s warm-up at the same
  concurrency; requests/s counts 200s only. Cases: counter `GET /` (≈130 B
  reply) and `POST /incr` (rewrites `/data/count` and commits), each spread
  over 1,000 objects (`/o[0-9]{3}/`, random) and on one hot object; `GET
  /kv/small` (512 B value) and `GET /kv/big` (64 KiB value) over the same
  1,000 objects (one KV read each, so the size is the only difference); the
  JS counter `GET` and `POST` over 1,000 objects.
- **latency**: `oha -q <rate> --latency-correction -c 64 -z 15s` at the same
  absolute rates for every build (50% and 80% of the lowest build's maximum),
  so latency is measured below saturation and without coordinated omission.
- **CPU per request**: utime + stime from `/proc/<pid>/stat` of the
  supervisor, each worker (its children) and the proxy, before and after the
  measured run, divided by the responses. Clock ticks are 10 ms; runs are long
  enough (tens of thousands of requests) for that to be <1%.
- **memory per live object and density**: worker RSS (`VmRSS`) after 0,
  2,000, 4,000 and 6,000 new objects (GET, so an instance and its empty
  volume each); KB per object is the slope; density is objects per GiB at
  that slope (worker base RSS on top).
- **commits**: sync syscalls, not device flushes: `strace -f -c` on every
  thread of the worker for 500 serial `POST /incr`, 500 `GET /` and 500
  `PUT /kv/k`, counting `fsync`, `fdatasync`, `sync_file_range`, `syncfs`,
  `msync` and `rename*` per request.
- **path**: direct to the node (supervisor → jailed worker) for everything;
  through the proxy (proxy → supervisor → worker, DNS from a local
  DNS-over-HTTPS stub in the driver, TTL 1 day) for `GET`, `POST /incr`, the
  64 KiB read and JS at c = 1, 64, 256; in-process (each build's own
  `bench --json --cold 9000`, no sockets or jail, and `--writes 3` on B, C,
  D) as the floor.
- **C's client limits.** Throughput and latency on C and D run with
  `[clients] rate = 0, app_rate = 0`. Loopback is in `trusted` by default,
  and a trusted peer's own requests are never limited, so a local `oha` is
  not limited even at the defaults. A separate pass at the defaults measures
  `GET` and `POST /incr` (c=64, 10 s) from loopback, from one client behind a
  trusted forwarder (`X-Forwarded-For: 198.51.100.7`, as Caddy or the proxy
  would send it), and 16 such clients at once (the app's 1,000/s).

## Files

- `notes/bench-numbers/drive.py`: the driver (above). Untested end to end:
  written while the machine was reserved for builds; expect a pilot to need
  small fixes (notably `strace` on a jailed worker and oha's JSON fields).
- `notes/bench-numbers/summarize.py`: tables (median and range over rounds,
  builds side by side) and `summary.csv` from `raw.jsonl`.
