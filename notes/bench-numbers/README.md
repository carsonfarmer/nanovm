# Current v1 benchmark protocol

The Python driver measures HTTP through a supervisor and jailed app worker,
and separately runs the build's in-process benchmark. It accepts one build or
alternating builds, all using the current v1 API/config. No format adapters or
historical config dialects are supported (AGENTS.md rule 1).

The integrated contract includes `origin/trim/edge` at `2efdd1a`: request rates
belong at the edge, not in nanovm. Node/proxy configs inherit the connection-only
`clients` defaults (`connections_per_client`, `connections`, `trusted`); the
driver writes no client-rate keys and sends no forwarded-client accounting
headers. There is no rate-limit/rejection benchmark or exemption for failed HTTP
responses. No independent connection-limit benchmark existed here, so none is
added; connection limits remain enabled normally. Fixed offered-rate latency
(`--rates`, oha `-q`) is still a load-generator workload, not a server rate limit.
Supported `--only` phases are `inproc,node_cold,app_cold,object_cold,syncs,direct,fixed_rate,proxy,density`;
unknown phase names are refused before any workload starts.

## Correctness gates

- Ordinary measured runs **and warmups** must have only 200/204 responses, no
  transport errors, a successful client exit, and the full count for `-n` runs.
  A failure aborts the run, records invalid evidence, and prevents summarization.
  Application-cold failures invalidate that sample set. Node-cold startup polling
  deliberately reports time until first 200 and retains intermediate response codes.
- Density stops the ordinary stack, publishes a dedicated counter manifest, and
  starts a fresh stack plus the existing `dev-collector` example. Both node and
  app idle retention are one hour, with a half-hour measurement deadline. After
  warming one seed object, each of three batches must return 2,000 successful
  responses and export the exact live/store count: 1, 2,001, 4,001, 6,001. Export
  snapshots must postdate the batch and name the same target worker PID. RSS is
  read for **that PID**, not the sum of supervisor children. The slope divides
  its RSS delta by the **observed live-count delta**. No retained-object count
  is inferred from directory entries, attempted requests or a timeout assumption.
- Work/output roots must not exist and must not overlap. Every build-round has
  its own work directory. All paths passed to subprocesses are absolute; the
  driver's work data is never confused with a subprocess's checkout-relative data.
  In-process exit failure, missing/invalid JSON, ordinary failed counts, and the
  write benchmark's printed `FAILED` marker invalidate the run.
- Raw measurements carry full commit and build/workload identity. Identity hashes
  binaries, enabled guest fixtures/manifests, oha, driver and timing/rate settings.
  The summarizer refuses invalid rows, duplicate build/round/measure tuples,
  differing identities under one build name, or different offered rates for one
  measure, including across compared builds. CSV retains identity and offered rate.

Python's standard library already supplies subprocess, JSON, TOML parsing for
tests and unittest mocks; no extra Python dependency or custom HTTP load client
was needed. Density reuses the project's collector and admin API instead of
implementing another telemetry decoder or introducing a runtime API.

## Validation of the harness (not benchmark results)

```sh
PYTHONDONTWRITEBYTECODE=1 python3 notes/bench-numbers/test_harness.py -v
git diff --check
```

The initial ten regression tests failed against the unchanged driver: five
assertion failures and eight errors (including subtests). They reproduced silent
HTTP error acceptance, ignored in-process exit/missing JSON, missing density/live
verification and path handling, config dialect requirements, and pooled duplicate,
commit/binary/rate/invalid rows. The current suite has 18 passing tests, including
a real disposable stub subprocess running from another cwd, synthetic collector
snapshots, dedicated density orchestration and failed-creation cleanup. A CLI
help probe also caught and fixed argparse's unescaped percent-sign failure.
For the edge contract update, changed config/summary/phase probes first failed
(two assertions and one missing-function error), then passed after removing
client-rate config and rejection exemptions and validating the current phase set.
HTTP 429 is invalid, just like any response outside 200/204.
Mutating the current config to emit client-rate keys, exempting 429 in summaries,
and bypassing phase validation each failed its targeted test. All three changes
were restored from the staged source; the unstaged diff was empty afterward and
all 18 tests passed again.

Four initial source mutation probes were rejected: unconditional HTTP validity (three
tests fail), removal of live/store-count comparison (density test fails), removal
of absolute in-process work resolution (stub subprocess test fails), and removal
of pooling identity/rate/duplicate checks (four assertions fail). Each mutation
was restored from the staged source using `git checkout -- <file>`; the unstaged
diff was empty afterward and the restored suite passed. No real benchmark,
compilation, server startup or performance claim belongs to this validation.
Runtime source-line delta: **0**.

## Fresh build, then idle-machine measurements

Run from the **integrated candidate checkout**, after applying this harness commit.
The registry and in-process binary embed that checkout's paths at compile time:
do not borrow binaries from another worktree. Do not share a host target directory.
The commands below are a runbook, not commands executed during harness validation.

Tooling: Linux with `/proc`, root for jailed-worker uids and syscall tracing,
Python 3.11+, Rust stable, wasm32-wasip2, wac-cli 0.12.0, WASI SDK 34
(`WASI_SDK_PATH`), oha (1.16.0 JSON contract), strace, ps, and a normal C build
toolchain. Rust/Wasm/JS/SQLite fixture builds must finish before measurement.

```sh
set -eu
root=$(pwd -P)
stamp=$(date -u +%Y%m%dT%H%M%SZ)
runbase="$root/.amp/in/orb-backup/bench-$stamp"
mkdir -p "$runbase/bin"
unset RUST_BACKTRACE NANOVM_BENCH_CAPTURE NANOVM_BENCH_DEV
rustup target add wasm32-wasip2
# Install missing tooling before continuing; never install/build during timings:
# cargo install --locked wac-cli --version 0.12.0
# cargo install --locked oha --version 1.16.0
test -x "$WASI_SDK_PATH/bin/clang"
bash scripts/build-guests.sh
bash components/sqlite/build.sh
tools/build-app.sh js/examples/counter.js --locked -o "$runbase/bin/jscounter.wasm"
CARGO_TARGET_DIR="$root/target/bench-release-$stamp" cargo build --locked --release \
  -p nanovm --features dev --bin nanovm --example bench \
  --example dev-registry --example dev-collector
cp "$root/target/bench-release-$stamp/release/nanovm" "$runbase/bin/"
cp "$root/target/bench-release-$stamp/release/examples/bench" \
   "$root/target/bench-release-$stamp/release/examples/dev-registry" \
   "$root/target/bench-release-$stamp/release/examples/dev-collector" "$runbase/bin/"
sudo sysctl -w vm.max_map_count=1048576
ulimit -n 100000
oha=$(command -v oha)
```

Now stop **all** other builds, tests, demos, load generators, servers and agents
on the VM. Inspect `amp orb service list`, `ps -eo pid,pcpu,comm,args --sort=-pcpu`
and `ss -ltnp`; stop only workloads you own, using their service stop commands.
Required ports: 5000, 8053, 8080, 8443, 8444; 4318 must be unused except during
the dedicated density phase. The driver refuses occupied ports; it never kills
unrelated processes. It waits for load1 <= 0.5 before each build-round and refuses
to run after a 15-minute idle-wait timeout. This is a guard, not proof of an idle
or dedicated physical host: inspect CPU activity too.

First collect a separate throughput pilot, then derive offered rates from its
clean concurrency-64 medians. Keep pilot and full outputs separate; never pool them.

```sh
sudo env -u RUST_BACKTRACE -u NANOVM_BENCH_CAPTURE -u NANOVM_BENCH_DEV \
  python3 notes/bench-numbers/drive.py --build "current:$root:$runbase/bin" \
  --work "$runbase/pilot-work" --out "$runbase/pilot-out" --oha "$oha" \
  --rounds 3 --only direct
sudo python3 notes/bench-numbers/summarize.py "$runbase/pilot-out/raw.jsonl" > "$runbase/pilot-tables.md"
sudo python3 - "$runbase/pilot-out/raw.jsonl" "$runbase/rates.json" <<'PY'
import collections, json, statistics, sys
rows = [json.loads(s) for s in open(sys.argv[1])]
samples = collections.defaultdict(list)
for r in rows:
    if r.get('phase') == 'direct' and r.get('conc') == 64:
        assert r['valid']
        samples[r['case']].append(r['rps'])
assert samples and all(len(v) == 3 for v in samples.values())
rates = {k: {f'{p}%': max(1, int(statistics.median(v) * p / 100))
             for p in (50, 80, 95)} for k, v in samples.items()}
with open(sys.argv[2], 'w') as f:
    json.dump(rates, f, indent=2)
PY
sudo env -u RUST_BACKTRACE -u NANOVM_BENCH_CAPTURE -u NANOVM_BENCH_DEV \
  python3 notes/bench-numbers/drive.py --build "current:$root:$runbase/bin" \
  --work "$runbase/full-work" --out "$runbase/full-out" --oha "$oha" \
  --rounds 3 --rates "$runbase/rates.json"
sudo python3 notes/bench-numbers/summarize.py "$runbase/full-out/raw.jsonl" > "$runbase/full-tables.md"
```

For A/B comparisons, add another `--build name:tree:bin` with a separately built
current-v1 candidate and use one fixed rates file for both. Rounds alternate builds.
A single build describes the integrated candidate, not improvement versus a control.

Expect roughly **one to two hours** for the three-round full matrix with JS and
three offered rates, plus a 10–20 minute pilot and build time (cold release builds
can take tens of minutes). These are estimates, not freshly measured durations.

## Boundaries and remaining verification

The actual integrated release, its CLI/collector exchange, jailed-worker RSS,
oha output and strace permissions still require the idle-VM run above; the unit
tests do not establish their performance or end-to-end correctness. Do not report
the invalidated aggregate-child/attempt-count density numbers as live-object density.
RSS slope is incremental target-process residency (KiB), not a memory hard cap,
total-node density or proof that every object can grow to its memory quota. The
density collector adds work only to that phase; ordinary HTTP runs intentionally
have no active telemetry sink and are not full-export performance evidence.
Sync counts are syscalls, not device flushes. In-process write output is retained
as text, not pooled with HTTP measurements. SQLite is optional in that executable;
build its fixture above and check that its POST /set rows actually appear. CPU
accounting uses surviving worker `/proc` counters and can miss CPU from workers
that exit inside a sample; fresh performance evidence must inspect restart logs.
Offered rates are pilot fractions, not a guaranteed safe production request rate.
