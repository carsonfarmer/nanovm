# Quality notes (quality agent, round 3)

Test and verification work from IDEAS.md sections D and G. Each item is on its own branch from
`cf/main-next`; this file collects the notes. Commands run from the repository root.

## Branches and findings

| Item | Branch | Runtime lines | Found |
| --- | --- | --- | --- |
| D-52 deploy-time validation | `ideas/deploy-validation` | 0 | N-500 (only the import allowlist keeps WASIp2 files out of quota and journal) |
| D-50 seccomp drift check | `ideas/seccomp-drift` | 0 | no drift |
| G-72 supply-chain checks | `ideas/supply-chain` | 0 | N-501 (dev-only RustSec advisory via `tempdir`) |
| D-45 fuzzing | `ideas/fuzz` | 0 | N-507 (fixed on `ideas/fix-trust-comma`), N-506 (open, low) |
| D-46 model-based quota test | `ideas/quota-model` | 0 | N-502 |
| N-502..N-504 recovery class | `ideas/recover-class` (98f25be on `cf/main-next`, + 80b9fe6) | +33 | the fix |
| D-58 hostile guest, second edition | `ideas/hostile2-final` | 0 | N-503, N-504, N-505 (open) |
| D-51 soak | `ideas/soak` | 0 | flat RSS/fds; a slowing mapping creep to watch |
| D-55/D-56 coverage, mutants | `ideas/mutants` | 0 | W^X and cache-bound gaps, now tested |

## D-52: deploy-time validation (`ideas/deploy-validation`)

**What it proves.** A component that imports something nanovm refuses is rejected when it is loaded,
before any object exists, and the error names the import:

| Fixture (`crates/nanovm/tests/refused/examples`) | Imports | Error names |
| --- | --- | --- |
| `p2_sockets` | `wasi:sockets/tcp-create-socket@0.2.12` (via `wasip2`) | ``import `wasi:sockets/…` is not permitted`` |
| `p3_sockets` | `wasi:sockets/types@0.3.0` | ``import `wasi:sockets/types@0.3.0` is not permitted`` |
| `p2_filesystem` | `wasi:filesystem/types@0.2.12` (from `std::fs`) | ``import `wasi:filesystem/types@0.2.12` …`` |
| `custom_import` | `acme:backdoor/shell` | ``import `acme:backdoor/shell` …`` |
| `command` | allowed imports only, exports `wasi:cli/run` | `wasi:http/handler` (missing export) |

The library in the same crate is the control: the same service with only allowed imports loads and
answers 200. Through a registry and a jailed worker, a request to the `p3_sockets` app is answered
502 `worker-down` with the import named in the body, and so is the retry.

**Why it matters (finding N-500).** `app::linker` adds all of WASIp2 (`p2::add_to_linker_async`),
and the instance's `WasiCtx` preopens `/data` and `/tmp` read-write. `p2_sockets` and
`p2_filesystem` both *link* against that linker (the test asserts `instantiate_pre` succeeds). For
sockets, wasmtime's default address check still denies every address, so there are two layers. For
files there is one: without `ALLOWED_IMPORTS`, `std::fs` could write `/data` outside the quota and
outside the journal (no rollback). Mutation check: with the allowlist test turned off
(`if false && …` in `check_imports`), both new tests fail (`p2_sockets was loaded`).

**Std pulls in more than you ask for.** The first try used `std::net::TcpStream` for the WASIp2
socket fixture; wasi-libc's socket code also imports `wasi:filesystem/types@0.2`, which came first,
so the error named the filesystem. The fixture now calls `wasip2` bindings directly. `check_imports`
reports only the first refused import; that is enough to reject, but an app author fixing them one
at a time would have to redeploy once per import.

Run: `cargo test --locked -p nanovm --test integration refuse` (builds the fixtures into
`target/test-guest` on first use; ~25 s cold).

## D-50: seccomp drift check (`ideas/seccomp-drift`)

**What it is.** `scripts/seccomp-drift.py` builds the integration tests, runs the 17 that start
jailed workers under `strace -f`, and keeps only the syscalls that worker threads made after the
filter applied to them: it rebuilds the process tree from `clone`/`clone3`/`vfork` results, finds
`execve(".../nanovm", [..., "worker"])`, and starts counting a thread at its own `seccomp(...) = 0`
or at its birth from a jailed thread. It parses the allowlist out of `jail.rs` (`seccomp_filter`,
honouring the `x86_64`-only block and noting which syscalls have argument rules) and reports:

- **drift**: jailed calls the filter refused (`ENOSYS`), split into "not allowlisted" and
  "arguments outside its rule" (e.g. an `fcntl` command), with examples. `clone3` is the one
  expected denial (glibc falls back to `clone`, as `jail.rs` says).
- allowlisted syscalls never seen (to review, not an error);
- two consistency checks that make the run fail (exit 2): a refused syscall that *succeeded* in a
  jailed thread, or a worker thread running unjailed after its process entered the jail (the filter
  is applied per thread, without TSYNC, so a thread started before `Jail::enter` would escape it).

`.github/workflows/seccomp-drift.yml` runs it nightly and on demand with `--fail-on-drift`, writes
the report to the job summary, emits one `::warning` per drifting syscall and uploads the JSON.
`scripts/test_seccomp_drift.py` tests the parser on a synthetic trace (harness and non-worker
processes ignored, pre-jail calls ignored, resumed calls, argument denials, a pre-jail thread).

**Result at `cf/main-next` (0ac2849), x86_64, kernel 6.18.** 17 tests passed under strace in 52 s
(2 m 11 s with the build). 32 worker processes, 338 threads, 324,508 jailed syscalls, 56 distinct.
**No drift**; `clone3` refused 32 times (once per worker). Every worker thread was jailed.
Allowlisted but not seen (22): `access brk clock_getres clock_gettime epoll_pwait exit_group
faccessat2 getcwd membarrier mremap newfstatat preadv prlimit64 pwritev readlinkat readv renameat2
rmdir sendto set_tid_address tgkill utimensat`. Most are deliberate (`wasi:filesystem` calls the
tests don't make, `getcwd` for a relative `data_dir`, `membarrier` on aarch64, `tgkill` for abort,
vDSO clocks); none is a reason to change the filter without a closer look.

**Does it catch drift?** Mutation: deleting `SYS_sched_yield` from the allowlist and re-running two
tests reports `sched_yield: 564 times, not allowlisted` and exits 1. Both tests still *passed*:
tokio treats the refused yield as harmless, which is exactly the silent kind of drift a dependency
upgrade could introduce.

Run: `python3 scripts/seccomp-drift.py [--json out.json] [--fail-on-drift] [test filters…]`;
`python3 -m unittest discover -s scripts -p test_seccomp_drift.py`. Needs `strace` and the guests
built (`scripts/build-guests.sh`). The trace is ~125 MB, in a temporary file deleted afterwards
(`--keep FILE` keeps it).

## G-72: supply-chain checks (`ideas/supply-chain`)

**What it adds.** `deny.toml` (cargo-deny 0.20.2: advisories, licenses, bans, sources),
`.cargo/audit.toml` (cargo-audit 0.22.2) and `.github/workflows/supply-chain.yml`, which runs both
on pushes and pull requests that touch a manifest, lockfile or either config, daily (new advisories
appear without any change here) and on demand. The node's workspace gets every cargo-deny check;
each other lockfile (guests, KV and SQLite components, the post-return fixture, the SQLite
experiment) gets advisories and sources, since their own crates are unlicensed path crates.

**Policy.** Linux targets only (x86_64, aarch64). Licenses allowed: the permissive set the graph
actually uses (MIT, Apache-2.0 with or without LLVM-exception, BSD-2/3, ISC, Zlib, Unicode-3.0,
CDLA-Permissive-2.0 for webpki roots, BSL-1.0, CC0, MIT-0, Unlicense); no copyleft is needed (`r-efi`
offers LGPL only as one alternative of three). Only crates.io as a source; no git dependencies;
wildcards denied; duplicate versions warn (13 crates, all inside wasmtime/sigstore/rand
ecosystems: not worth fighting).

**What they flagged at `cf/main-next` (0ac2849), and what was done.**

| Finding | Where from | Action |
| --- | --- | --- |
| RUSTSEC-2023-0018 (vulnerability): `remove_dir_all` 0.5.3 TOCTOU | `tempdir` 0.3.7 ← `container-registry` 0.3.1 `test-support` (dev-dependency, the tests' loopback registry) | Ignored with reason in both configs. Not in the node binary; `container-registry` 0.3.1 is the latest and requires `tempdir` for `test-support`, and `tempdir` pins `remove_dir_all` ^0.5, so no upgrade exists. The tests delete only their own temporary storage. (N-501) |
| RUSTSEC-2018-0017 (unmaintained): `tempdir` | same path | Same. |
| `nanovm` is unlicensed | the workspace crate has no `license` | `publish = false` in `crates/nanovm/Cargo.toml` (true: it isn't published), so cargo-deny's private-crate rule skips it. No license was invented. |
| `experiments/fs-backends/Cargo.lock` is stale | `--locked` refuses it | Skipped in CI, noted in N-501. |

No dependency was upgraded or changed; `Cargo.lock` is untouched. After the changes:
`cargo deny --locked check` → `advisories ok, bans ok, licenses ok, sources ok`; `cargo audit`
→ exit 0 on all six lockfiles (416 crates in the node's). Without `.cargo/audit.toml`, `cargo audit`
reports both advisories and exits 1, so the exceptions are what keeps it green.

Run: `cargo install --locked cargo-deny --version 0.20.2 && cargo install --locked cargo-audit
--version 0.22.2`, then `cargo deny --locked check` and `cargo audit`.

## D-45: fuzzing (`ideas/fuzz`)

**What it is.** A `cargo-fuzz` crate in `fuzz/` (its own workspace, so the node's never builds
libFuzzer) with six targets, each one line calling an entry point in `fuzz/entry.rs`. That file is
compiled into nanovm only with its new `fuzzing` feature (`lib.rs`: `#[cfg(feature = "fuzzing")]
#[path = "../../../fuzz/entry.rs"] pub mod fuzz;`, below the test module, so not in the runtime
count). Visibility changes are `pub(crate)` only: `ws::{parse, messages, Event::encode}`,
`proxy::{parse, unquote, Route}`, `journal::{Undo, encode, decode}`, `telemetry::{stamp,
identity}`. Each target checks properties, not just "no panic":

| Target | Input | Properties |
| --- | --- | --- |
| `ws_events` | an object's WebSocket-over-HTTP answer | parse → encode → parse is the same and no longer; events built from the bytes round-trip |
| `grip_publish` | a guest's GRIP publish | ≤ 64 items; messages no bigger than the publish; where the pre-N-72 `serde_json::Value` parser also accepts it, both give the same messages (differential) |
| `txt_record` | a `_nanovm` TXT record | what the proxy accepts, the node accepts as the same app name, trust pairs and floor (builds the headers `Proxy::resolve` sends and parses them as `AppRef::from_headers`) |
| `app_ref` | the `nanovm-app`/`-trust`/`-min-version`/`-host` headers | a valid sha256 digest; the cached parse equals a fresh one; the app name is one valid path component |
| `journal_log` | an undo log | records start in order and re-encode to their bytes; built records round-trip; every cut of a log (a crash mid-append) decodes to exactly the whole records before it; cancelling a rename keeps the framing |
| `telemetry_stamp` | a worker's OTLP batch | a stamped batch that decodes has only the stamped resource and no identity attribute anywhere; when the input decodes, the output equals decode-replace-scrub (N-74's oracle, for any batch) |

Seeds (`fuzz/seeds.py`, including the crash inputs found) and dictionaries (`fuzz/dicts`).
`.github/workflows/fuzz.yml` runs every target for 5 minutes nightly and on demand, uploading a
crash's input.

**Runs** (ASan, stable rustc with `RUSTC_BOOTSTRAP=1`, 10 minutes each, 4 shared cores):
`ws_events` 1.84 M executions, `grip_publish` 5.5 M, `telemetry_stamp` 2.9 M, `journal_log` 174 k
(it re-decodes 64 cuts per input): no failures, and no disagreement between the typed and the
generic publish parsers.

**Found:**
- **N-507** (`txt_record`, in seconds): the proxy accepted `trust=https://a,b s`; the node splits
  `nanovm-trust` on commas and refused every request for the host. Fixed on `ideas/fix-trust-comma`
  (bd058de, merged into this branch so the target passes): the proxy refuses such a record;
  `accepted_records_are_policies_the_node_accepts` fails without it.
- **N-506** (`app_ref`, after 240 k executions): a `nanovm-host` of `.com` gives an app name starting
  with `.`, which isn't a valid name. Only the proxy sets that header, from a DNS owner name, so it's
  low; open, and the property is scoped around it.

**Not done:** a second round after the fixes. The ASan build of nanovm's dependencies is 4 GB and
was deleted when the shared disk ran short; the stripped binaries (84 MB) ran the rounds above.
Rebuild with `RUSTC_BOOTSTRAP=1 cargo fuzz build --fuzz-dir fuzz` (or nightly), then
`python3 fuzz/seeds.py` and `cargo fuzz run --fuzz-dir fuzz <target> fuzz/corpus/<target> --
-dict=fuzz/dicts/<target>.dict -max_total_time=600`.

## D-46: model-based quota test (`ideas/quota-model`)

**What it is.** `crates/nanovm/tests/quota_model.rs` (proptest) drives a test-only component,
`crates/nanovm/tests/fsops` (outside `guests/**`), on an in-process node with a 1 MiB / 4-entry
quota. Each case is a fresh object and up to 19 requests of 1-5 random operations on `/data`
(write at an offset, including sparse and over-budget writes; truncate; rename, including over
files and directories and across directories; unlink; mkdir; rmdir) over nine overlapping paths.
A request commits (75 %) or rolls back. A model of the volume predicts every result (`ok`,
`insufficient-space`, `quota` or another error) from POSIX semantics plus the quota's rules: the
entry check before the parent check, short writes cut at the budget, holes charged, truncation
charged and credited, renames over files freeing them, and the journal's separate budget for
removed files. The guest checks each result against the prediction and stops at the first
difference. After every request:

- the volume on disk must equal the model's committed state (every file's contents);
- in half the steps, a probe request (in the same instance after a commit, in a fresh one after a
  rollback) must be able to add exactly the model's remaining bytes and entries, and not one more:
  the quota's usage equals the model's, observed through enforcement rather than read out.

Where the request's undo log nears its budget (old bytes of pre-existing files it saved), where the
journal refuses a write depends on how the write's stream is chunked; the model then accepts
`insufficient-space` for that op, reads that file back, and leaves the request's later ops out.

**Found: N-502 (fixed on `ideas/fix-recover-enotdir`, now on `cf/main-next` as 6fdb057).** The
first run shrank to two requests: commit an empty file `d`; then `mkdir d/a` in a request that
fails. `creating` recorded `Created(d/a)`, the mkdir failed with ENOTDIR, and on restart recovery
treated `symlink_metadata`'s ENOTDIR as an error: the object answered 503 `instantiate-failed`
forever. The class it belongs to (N-502..N-504) is fixed on `cf/main-next` (98f25be), which this
branch is now merged with; the test passes on it.

**Coverage of one 64-case run** (op and result counts): writes 151 ok / 102 short or refused for
space / 37 refused at the entry limit / 231 other errors; truncates 26 ok / 10 refused for space;
renames 84 ok / 350 refused; unlinks 48 ok; mkdirs 99 ok / 37 at the entry limit; rmdirs 21 ok;
511 committed and 175 rolled-back requests, 330 probes; a few undo-log-full writes. 64 cases take
10-20 s (`QUOTA_MODEL_CASES=1000` for longer runs).

**Does it catch quota bugs?** Mutations of `quota.rs`, each run once with 64 cases:

| Mutation | Result |
| --- | --- |
| a write charges every byte written, not just growth | killed (probe: short of room at 786,432 of 1,048,476 bytes) |
| truncation frees nothing | killed (probe) |
| a directory renamed over an empty one keeps its entry | killed (probe: entry limit one early) |
| unlink keeps the entry | killed (probe) |
| a failed rename's restore forgets the target's bytes | survived: only reachable when the undo log refuses a rename record, which the generator rarely reaches; `rename_log_refusal_restores_target_and_usage` covers it |

Run: `cargo test --locked -p nanovm --test quota_model` (builds the fixture into
`target/test-guest`). Adds `proptest` 1.12 as a dev-dependency (std only: proptest, rand_xorshift,
unarray in `Cargo.lock`; dev builds also rebuild num-traits, chrono, tar and oci-client).

## D-58: hostile guest, second edition (`ideas/hostile2-final`)

**What it is.** A new test-only component, `crates/nanovm/tests/hostile2` (outside `guests/**`),
and `crates/nanovm/tests/hostile2.rs`, nine tests on in-process nodes with the first edition's
limits (32 MiB, 100 ms CPU, 1 s deadline, 8 MiB disk, 2 outbound). Each attack runs beside a
canary object where that matters. Numbers from one run on the shared 4-core host (load ~13):

| Attack | Contained by | Evidence |
| --- | --- | --- |
| runaway recursion | a wasm trap (the guest's shadow stack runs out in linear memory first: `out of bounds memory access`; or `call stack exhausted`) | 500 `trap`; canary's slowest answer under 200 ms; the object restarts |
| pathological regex (`(a+)+$` on `a…a!`, n=40, by backtracking) | the CPU budget | 500 `cpu` well under the 1 s deadline; canary under 200 ms |
| hash flooding (header names solved to share one bucket of `http::HeaderMap`'s FNV hash) | the map's switch to a keyed hash after a long probe, and the 64 KiB per-map limit | 800 colliding names: median 470 µs per map vs 222 µs for ordinary names (2.1x, test bound 3x) |
| huge header counts | the 64 KiB limit both ways | a guest asking for 1,000,000 response headers gets 830; 5,000 request headers get 431 and the server carries on |
| slow uploads (1 byte per 100 ms) | the body is read before the request is queued | the uploading object answers others meanwhile; but nothing bounds how long an upload may take (N-505) |
| outbound connection storm (50 at once) | `max_outbound_concurrency` | 2 answered, 48 refused at once (`ConnectionLimitReached`); upstream peak 2 |
| rename storm (one file back and forth, then fail) | CPU budget / deadline, journal budget | ended after 1.2-2.6 s (`timeout` or `cpu`; fsync waits aren't charged as CPU); rolled back; the next start took 0.27 ms; canary under 10 ms |
| unusual names (NFC/NFD, RTL override, zero-width, emoji, newline, `.`/`..` look-alikes, 255 and 256 bytes) | — | commit and roll back exactly; 256 bytes gives `name-too-long` |
| a directory chain past PATH_MAX, one descriptor at a time | the class fix | refused with `name-too-long` at the depth where the path would reach PATH_MAX; a 12-deep chain commits and rolls back |

**Found (fixed on `ideas/recover-class`, one commit with N-502):**
- **N-503:** `/names?then=fail` (a 256-byte name in a failing request) left the object answering 503
  `instantiate-failed` ("File name too long") for good: `creating` recorded the create because
  `symlink_metadata` failed with ENAMETOOLONG, and recovery treated that as fatal.
- **N-504:** a chain of 16 directories of 255-byte names (path past PATH_MAX under a 72-byte volume
  path) committed with `commit-failed` ("File name too long") and then bricked the object the same
  way; depths 12-15 were fine.

The fix closes the class at once (as the orchestrator asked): `creating` and `removing_dir` record
only for NotFound and return any other error (mapped to the WASI code the syscall gives),
`renaming` refuses moves that would put anything at or past PATH_MAX, and recovery treats
path-shape errors as "nothing there" for every record. Its journal tests include a seeded random
test over these path shapes. This branch carries that commit; the names and deep-path tests fail
without it.

**Open: N-505.** Request bodies have no deadline at any hop (proxy, supervisor, worker): a client
can hold a connection, a link and a task as long as it trickles bytes, within
`max_request_body_kb`. It doesn't hold the object. Per-client limits in the proxy (IDEAS G-75) are
the natural place.

Run: `cargo test --locked -p nanovm --test hostile2 -- --nocapture` (builds the fixture into
`target/test-guest`; ~15 s).

## D-51: soak test (`ideas/soak`)

**What it is.** `crates/nanovm/tests/soak.rs` (ignored by default) and `scripts/soak.sh [seconds]
[csv]`. Two in-process nodes: `counter` with 64 live slots over 600 objects (constant LRU
eviction and cold starts) and `hostile` with 32 slots over 200 objects. Six clients run a fixed,
seeded mix: counter increments (a synced `/data` write each), reads and key-value put/get/delete;
hostile commits and rollbacks (files and KV), 1 MiB `/tmp` writes, echoes up to 64 KiB, and every
kill the host makes (CPU, memory, trap, resource table: 8 % of requests). Every 30 s it samples
the process's RSS (`VmRSS`), open fds (`/proc/self/fd`) and memory mappings (`/proc/self/maps`)
into a CSV. It compares the median of the second quarter (warm) with the last quarter: RSS may
grow 10 % or 32 MiB, fds 32, mappings 100. Unexpected answers are counted, not fatal (at most
0.1 %; `busy`, `cpu` and `timeout` are tolerated on a loaded host).

**Result: 30 minutes at `cf/main-next` 98f25be** (debug test build, 4 shared cores under other
agents' builds; raw samples in `notes/quality-soak.csv`): 320,466 requests (178/s), no unexpected
answers, passed.

| | first sample (30 s) | second quarter, median (min-max) | last quarter, median (min-max) |
| --- | --- | --- | --- |
| RSS | 93 MiB | 101 MiB (97-130) | 99 MiB (96-175) |
| open fds | 209 | 209 (206-216) | 210 (206-216) |
| mappings | 2,567 | 2,943 (2,927-2,991) | 3,015 (3,015-3,047) |

RSS and fds are flat (RSS spikes are the `/alloc` and `/echo` peaks being sampled). Mappings rise
fast while pool slots and caches warm (2,567 → 2,927 in 6.5 minutes) and then creep, slowing:
+16 per 3 minutes around 10-15 minutes, +8 to +16 per 3 minutes after. It passed the threshold,
but whether the creep levels off is not shown by 30 minutes: the 24-hour run IDEAS asks for, with
`/proc/self/maps` diffs to tell anonymous (allocator) from file mappings, is the next step.

Run: `scripts/soak.sh 1800 soak.csv` (guests built with `scripts/build-guests.sh`).

## D-55 / D-56: coverage and mutation testing (`ideas/mutants`)

**Coverage (D-55): not run.** `cargo-llvm-cov` needs the `llvm-tools` component and an
instrumented build of nanovm and all its dependencies, about 4 GB of target directory; the shared
disk was at 2.5-3.4 GB free. Procedure for a machine with room:
`rustup component add llvm-tools-preview && cargo install --locked cargo-llvm-cov`, then
`ulimit -n 20000; RUST_TEST_THREADS=1 cargo llvm-cov --locked -p nanovm --html` (build the guests
first) and read `target/llvm-cov/html` for `journal.rs`, `quota.rs` and `proxy.rs`.

**Mutation testing (D-56): partial.** `cargo-mutants` 27.1 lists 121 mutants in `ws.rs`, 53 in
`registry.rs`, 118 in `egress.rs` and 4 in `jail.rs`. Each needs a rebuild of nanovm's test target
(2-4 minutes here) and a 540 MB test binary with incremental state, so a full run (about 10 hours)
didn't fit the time or the disk. Seven `registry.rs` mutants ran with `--in-place` from a source
copy and unit tests only, but the run's verdicts proved unreliable (two "missed" mutants are
killed by existing tests when applied by hand: some test phases reported 0 s). So the mutants that
matter were applied by hand, one at a time, and the module's unit tests run:

| Mutant | Existing tests | New test |
| --- | --- | --- |
| `registry.rs`: the reference cache keeps entries over 1 KiB (`<=` → `>`) | survive | `the_reference_cache_keeps_apps_apart_and_small` (16 long references leave < 4.8 KB held) kills it |
| `registry.rs`: the cache answers with a different app's reference (`==` → `!=`) | killed (`app_must_be_a_reference_with_a_digest`) | also kills it |
| `registry.rs`: trust pairs refused (match arm deleted) | killed (4 tests) | — |
| `registry.rs`: `MAX_METADATA` 4 MiB → 0 (`<<` → `>>`) | survives unit tests | not added: only reachable through a registry (integration tests download manifests; not run per mutant) |
| `jail.rs`: the W^X mask becomes 0 (`\|` → `&`): writable and executable memory allowed | survives everything (no test checked W^X) | `the_worker_filter_refuses_what_it_should` kills it |

`the_worker_filter_refuses_what_it_should` re-runs the test binary as a child that installs the
worker's seccomp filter and checks it directly: W+X `mmap` and `mprotect`, inet sockets, datagram
socketpairs, `clone` without `CLONE_THREAD` and `F_SETOWN` are refused with ENOSYS; read-write and
read-exec mappings, stream socketpairs and `F_GETFL` are allowed. Before it, only the integration
tests' `Seccomp: 2` check touched the filter. `counting::held` (test-only) measures what a closure
leaves allocated.

Run: `cargo test --locked -p nanovm --lib -- jail:: registry::`.
