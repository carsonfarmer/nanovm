# Ops notes

Operations and packaging work from IDEAS.md (sections E, F, L, M), one branch per
item on `cf/main-next` `3dafe33`; `ideas/ops` merges them all. Plain language;
numbers and how to reproduce. `beta/deployable` merged `ideas/ops` onto round 3;
[beta-deployable.md](beta-deployable.md) has what changed there and what was
verified again. Numbers below are from the `3dafe33` base unless it says otherwise.

| Item | Branch | What |
|---|---|---|
| L-111 | `ideas/systemd-units` | hardened units, sysctl and cgroup drop-ins, nspawn test |
| L-113/L-114 | `ideas/single-node` (on systemd-units) | single-node guide, Caddy, install.sh, check-host.py, sizing |
| L-110 | `ideas/container-image` | Dockerfile, run flags, local test; N-551 fix |
| N-552 | `ideas/worker-exe` | workers from the supervisor's own image |
| L-116 | `ideas/release-process` | tag-triggered release workflow, packaging, changelog |
| E-59 | `ideas/bench-history` | bench --quick history, compare, chart, nightly workflow |
| F-71 | `ideas/errors-guide` | every x-nanovm-error reason, with a trigger script |
| L-115 | `ideas/runbooks` | runbooks and drills |
| E-63 | `ideas/fs-matrix` | filesystem matrix for durable writes |
| F-67 | `ideas/alerts` | Prometheus rules and alerts with promtool tests |

## L-111 systemd units (`ideas/systemd-units`)

**What:** `deploy/systemd/`: hardened `nanovm.service` and `nanovm-proxy.service`,
example `nanovm.toml`/`proxy.toml`, a sysctl drop-in for `vm.max_map_count`, an
optional cgroup v2 drop-in, and a container test that boots systemd.

**How it was tested.** systemd is not PID 1 here, so `test/nspawn.sh` boots this
host's own Ubuntu 24.04 (systemd 255) in `systemd-nspawn` (its `/usr` under a
throwaway overlay, a copy of `/etc`, private network) and runs
`test/in-container.sh`: install the files, start the node, drive the counter and
hostile apps from the loopback dev registry, restart, start the proxy, stop both.
Result: plain 16/16 checks and cgroup mode 19/19 pass (`RESULT fail=0` twice),
and, after the base moved to `3dafe33`, a third mode with `[capture]` on (17/17:
three versions published under `~committed/` through the unit's syscall filter),
`systemd-analyze verify` exit 0, `shellcheck -S warning` clean.
`systemd-analyze security`: node 2.6 (2.8 with the cgroup drop-in), proxy 1.1,
bare unit 9.6.

**Findings, each from a failing run before the fix:**
- `NoNewPrivileges=yes` breaks per-app uids under systemd 255: the supervisor's
  permitted set lost `CAP_SETUID` (CapPrm 0x61 vs 0xe1, bisected over every
  option), so workers failed `setresuid(200001)`. Left off; workers still end with
  `NoNewPrivs: 1` (checked) because Landlock sets it.
- `SystemCallFilter=@system-service` must add `seccomp` (worker: `seccomp: …
  Operation not permitted`) and the three `landlock_*` calls (worker killed by
  SIGSYS without them).
- Capabilities: removing `CAP_CHOWN`, `CAP_SETUID` or `CAP_SETGID` each breaks
  every worker start (errors in the README table). Without `CAP_KILL` normal
  version switches still work, because a worker exits when the supervisor hangs
  up, but `kill -0` of a worker from a unit without it fails while with it
  succeeds: the drain-deadline SIGKILL needs it.
- cgroup drop-in: `ProtectControlGroups=no` is needed (`creating cgroup …: Read-only
  file system`); `ReadWritePaths` turned out unnecessary and was removed. With
  `DelegateSubgroup=supervisor` the supervisor sits in `supervisor/`, each worker
  in `apps/<app>`, the service group itself empty (checked). A `[worker]` limit
  whose controller is missing fails that worker's start, naming the file, with no
  stray process.
- `ProcSubset=pid` hides `/proc/sys/vm/max_map_count` (`cat`: No such file),
  which makes the worker's preflight skip its mappings check, so the node unit
  doesn't use it.
- The node refuses to start with no CA certificates at all (`No CA certificates
  were loaded from the system`): images need `ca-certificates`.
- N-550: README understated mappings per object threefold (8 vs the code's 24);
  fixed in README.

**Not verified:** controllers actually enforced under the drop-in (this host gives
memory/cpu/pids to cgroup v1, so the container's v2 hierarchy had none; the
supervisor/apps layout and writes were exercised with zero limits), the sysctl
applied inside the container (read-only `/proc/sys`; `sysctl -p` of the file on
the host works), real DNS/TLS, other distributions or systemd versions. nspawn
needed `--system-call-filter` for Landlock, since its own filter lacks it.

Reproduce:
```sh
cargo build -p nanovm --features dev --bin nanovm --example dev-registry
sudo apt-get install -y systemd-container
sudo deploy/systemd/test/nspawn.sh
```

## L-113/L-114 single-node reference deployment (`ideas/single-node`, on `ideas/systemd-units`)

**What:** `deploy/single-node/`: a guide (Caddy TLS → proxy → node → workers,
Collector on its own cores), `Caddyfile`, CPU/memory drop-ins for nanovm and
`otelcol-contrib`, `install.sh` (idempotent, refuses to swap the binary under a
running node, keeps configs), `check-host.py` (per-worker fds and mappings from
`engine::preflight`'s formulas, Landlock ABI, root, data_dir searchable by app
uids, filesystem and mount options, cgroup controllers, CA bundle,
RUST_BACKTRACE; also covers IDEAS C-44's "doctor" checks), and `capacity.json`,
inputs for the nice-to-haves capacity calculator (measured values cited from
README). `deploy/systemd/test/nspawn.sh` now takes `SCRIPT=` and `MODES=`.

**Evidence:** `test/e2e.sh` in the nspawn container, 8/8: install, three writes
and a read over Caddy's verified internal-CA TLS through proxy (DoH stand-in,
dev registry tag) to a jailed worker; client `nanovm-app` header ignored; 404 for
a host without a record; 8080/8444 on 127.0.0.1 only; spans from all three layers
at the Collector port; data survives a node restart. `caddy validate` (2.6.2) and
`systemd-analyze verify` pass. check-host: 5 tests; on this host it flags the
scratch directory's 0700 ancestors (the failure the fs matrix first hit:
`… is not accessible to the worker: Permission denied`). The nice-to-haves
calculator runs on `capacity.json` (17,100/s application ceiling, 40,800/s
Collector, ~69,000 resident objects in 6 GiB, CPU for Caddy/proxy excluded).

**Not run:** otelcol-contrib (no binary), real ACME/DNS, signed apps, AllowedCPUs
enforcement (cgroup v1 here). No Terraform/Ansible: a tested shell installer.

**After the base moved to `3dafe33` (W-9 local capture):** check-host also fails
a root whose objects have capture history while `[capture]` is off (they would
refuse to start) and reports capture's storage when on (6 unit tests); the
README's disk sizing counts capture. `test/e2e.sh` passes 8/8 again on a dev
build of `3dafe33`. `deploy/systemd/test/nspawn.sh` keeps `SCRIPT=`/`MODES=` and
gains the systemd branch's capture mode.

## L-110 container image (`ideas/container-image`)

**What:** `deploy/container/`: a multi-stage Dockerfile (release build in
`rust:1.99.1-slim-trixie`, binary + CA bundle + configs on `debian:trixie-slim`,
no `RUN` in the runtime stage), a `Dockerfile.dockerignore` so the context is only
`Cargo.toml`, `Cargo.lock`, `crates/nanovm` and the two configs, documented
`docker run` flags, and `test/local.sh`.

**Not built as written:** pulling base images was not approved here, so neither
stage ran with its real base. Checked instead:
- Context completeness: those files alone pass `cargo metadata --locked --offline`
  (lockfile consistent, all declared targets present). A release build from them
  was not run (it would rebuild every dependency: a debug attempt was stopped
  after 15 minutes at load average ~15).
- Runtime stage and flags: `test/local.sh` builds the real Dockerfile with BuildKit
  `--build-context build=<dir>` standing in for the build stage (a locally built
  dev binary and this host's CA bundle) and `RUNTIME_IMAGE` set to a stand-in
  imported from this host's glibc and busybox. Docker 29.9.2, default seccomp
  profile, cgroup v1, `--cap-drop ALL` plus the four capabilities,
  `--security-opt no-new-privileges`, `nofile` 20000 (this sandbox's hard cap).
  11/11 checks pass: workers as uid 200000 with seccomp and `no_new_privs`,
  hostile spin (500) and escapes (403) stopped, data survives a new container on
  the volume, proxy works as nobody on a read-only root, `docker stop` 120 ms
  with `--init`, and workers start with the supervisor as PID 1.

**Findings:**
- N-551: as PID 1 (no `--init`, or Kubernetes), every worker refused to start
  (`entering the jail: supervisor exited`): the worker read "parent is pid 1" as
  "supervisor died". Fixed in the first commit (compare with the supervisor's pid;
  +2 runtime lines); the PID-1 check fails before and passes after.
- As PID 1 the supervisor ignores SIGTERM, so `docker stop -t 3` took 3,151 ms (the
  full timeout, then SIGKILL); with `--init`, 120 ms. Recommend `--init`.
- `vm.max_map_count` is not namespaced: Docker's `--sysctl` cannot set it; set it
  on the host.
- Docker mounts `/sys/fs/cgroup` read-only: per-app `cgroup` limits don't work in
  an unprivileged container; use `--memory/--cpus/--pids-limit` for the node.
- Docker's `no-new-privileges` keeps `CAP_SETUID` (unlike systemd's; see the
  systemd notes), and the node works under it.
- The node needs a CA bundle at all, else it refuses to start.

**Tests run (on `3dafe33`):** full suite with the N-551 change: 109 unit pass;
integration 69 passed, 1 failed: `kv_counts_against_the_disk_quota` ("used more
than 100ms of CPU" at load average ~22), which fails the same way twice on the
plain base (in-process; it never enters the jail). `jail_blocks_escapes` passes.
`test/local.sh` 11/11 with this branch's build; with a build of the plain base
the PID-1 check fails (`entering the jail: supervisor exited`). Runtime lines
5,419 → 5,421.

Reproduce:
```sh
cargo build -p nanovm --features dev --bin nanovm --example dev-registry
dockerd …   # any running daemon; DOCKER_HOST pointing at it
deploy/container/test/local.sh       # NOFILE=20000 where the hard limit is lower
```

## N-552 workers from the supervisor's own image (`ideas/worker-exe`)

**Problem (found writing the upgrade runbook):** the supervisor spawned workers
from the path it started with. A binary replaced under it (any `install`, `mv`
or package upgrade before the restart) gave new apps workers of the new version
under the old supervisor.

**Change:** `main.rs` passes `/proc/self/exe` (Linux) as the worker executable;
in the forked child that names the supervisor's own image, even after the file is
replaced or deleted. `supervisor.rs` sets argv[0] to `nanovm`, so `ps` and
`pgrep -f 'nanovm worker'` read as before. Runtime lines 5,419 → 5,420 (on `3dafe33`).

**Evidence:** `docs/ops/drills/binary-swap.sh` (on `ideas/runbooks`): old =
`cf/main-next`, new = the N-551 build: a new app gets `502 worker-down` (`worker
spec: invalid length 3, expected a tuple of size 4`); old = this branch, same new
binary: the new app answers `{"object":"h","hits":1}`. The hardened systemd units'
container test passes with this binary (plain and cgroup, `ProtectProc=invisible`
included). Repeated on `3dafe33`: base build swapped for the N-551 build fails the
new app the same way; this branch's build serves it. Unit tests 109/109; the 10
integration tests named "worker" pass; of the full integration run at load ~20,
two CPU-budget tests failed and pass alone, and `kv_counts_against_the_disk_quota`
fails alone exactly as on the plain base.

## L-116 release process (`ideas/release-process`)

**What:** `.github/workflows/release.yml`, triggered only by pushed `v*` tags:
builds release binaries (no `dev` feature) for x86_64 and aarch64 Linux, checks
the tag equals the crate version, packages each with `tools/release/package.sh`,
verifies the per-archive checksums, writes `SHA256SUMS`, attests build provenance
and creates the GitHub release (prerelease for `-` tags) with notes from
`tools/release/changelog.py`. `tools/release/README.md` is the process.

**Evidence:** changelog tests 4/4 pass on a throwaway repository; a preview over
`origin/main..cf/main-next` lists the 13 commits with their N- ids. `package.sh`
on a local binary: archive verifies, two runs produce identical bytes, a
non-nanovm binary and a non-version tag are refused; shellcheck clean. The
release job's checksum and prerelease logic were run by hand. The workflow has
not run (needs a tag; no actionlint here); pushing this branch triggers only CI.

## E-59 benchmark history (`ideas/bench-history`)

**What:** `tools/bench-history/history.py` (`record` runs `bench --quick` and
appends one JSON line keyed by commit with machine, toolchain and raw output;
`compare` flags metrics >5% worse than the median of the previous five commits
on the same CPU model and core count; `chart` writes an HTML page of SVG trend
lines; `export` emits rows for the nice-to-haves summarizer) and
`.github/workflows/bench-history.yml` (nightly 03:17 UTC and manual, on the
default branch; carries the history forward through its own artifact).

**Evidence:** 6 unit tests pass. A real `bench --quick` here (dev profile; the
release profile would rebuild every dependency) was recorded (55 metrics),
charted (28 trend lines), exported and summarized by
`origin/nice-to-haves:experiments/nice-to-haves/toolkit.py summarize` (9 groups,
no schema errors), and a copy with warm GET 10% slower was flagged (exit 1).
That run, at load average ~17, also shows why one run is not evidence: warm
`/incr` serial was 54/s and 3 warm GETs were killed by the 50 ms CPU budget.
The workflow has not run: scheduled and manual runs start only from the default
branch, so pushing this branch runs only CI.

## F-71 "why did my object die?" (`ideas/errors-guide`)

**What:** `docs/ops/errors.md`: every `x-nanovm-error` reason the code emits
(worker, supervisor, proxy, WebSockets), its status, cause, whether the request's
writes roll back, and what to do; plus where else to look (span `error.type`,
events, node stderr) and the failures that cannot carry a header (truncated
bodies, post-return commits, 431s). `docs/ops/errors-check.sh` triggers them.

**Evidence:** the list was built from every `error_response` call and
`metrics::Poison`. `errors-check.sh` (dev node + proxy, counter and hostile
guests) produced 20 checks covering 17 reasons, each with the expected status and
header; the quoted details are its output. After the base moved to `3dafe33`
(W-9 local capture), two checks cover capture: a full history answers
`500 commit-failed` "no storage space", and a node restarted without `[capture]`
over objects with history answers `503 instantiate-failed` "committed history
requires capture to remain enabled". Not triggered: `commit-failed`,
`object-unavailable`, `bad-request-body`, `bad-uri`, the 500 `worker-down` and the
WebSocket refusals.

**Findings:** 4xx reasons never reach span `error.type` (only 5xx do), so
dashboards keyed on it miss client errors by design. At load average ~20, a
request allocating 64 MiB was killed for `cpu` before `memory` (CPU is wall time
in polls); with 40 MiB it hit `memory`. A failed worker start is replayed from
memory for 1 s (`RESTART_BACKOFF`), so a crash-looping app answers 502 quickly
rather than respawning per request.

## L-115 runbooks (`ideas/runbooks`)

**What:** `docs/ops/runbooks.md`: upgrade, rollback (nanovm and app), backup and
restore with `scripts/backup.py`, disk full, worker crash loop, Collector outage,
for the systemd single-node layout. `docs/ops/drills/` has four scripts that
reproduce the behaviour each runbook quotes.

**Drills (dev builds, this sandbox):**
- `backup-restore.sh` (restic 0.16.4, as root): count 3 and KV value `kept`
  before; backup, move the root away, restore into a fresh root: same values,
  app uids 200000/200001 preserved, next write gives 4.
- `disk-full.sh` (data_dir on a full 32 MiB tmpfs): every write, to existing or
  new objects, `500 commit-failed` ("journal requires recovery"); reads return the
  last committed state; two logs left in `~journal`; after freeing space each
  object recovered on its next request (counter 3 → 4, failed KV write gone, no
  logs left).
- `binary-swap.sh`: `cp` over the running binary fails (text file busy);
  `install` succeeds, the running app keeps serving, a new app's worker fails
  `worker spec: invalid length 3, expected a tuple of size 4` (old supervisor, new
  worker after N-551). Recorded as N-552 (open; fix sketched).
- `collector-outage.sh`: 20,000 warm GETs per variant, two rounds, Collector up
  / port closed / address unroutable: all 200 (except one 500 and one 502 at load
  average ~40, in the "blackhole" and "up" variants), 4.6–7.7k/s with no pattern
  by variant; 0 spans during an outage, ~750–810 stderr warnings per run. A
  separate check: after the sink came back, 180 of 3,000 earlier spans (still
  queued) and all 3,000 later ones arrived.

**Checked against the code:** journal path and recovery at instance start
(`journal.rs`, `quota.rs:75`), 1 s failed-start memory (`RESTART_BACKOFF`),
worker exit and last-words log lines, preflight messages, caches vs state
under `data_dir`, deny-unknown-fields config, tag TTL and version floor.

**After the base moved to `3dafe33` (W-9 local capture):** the runbooks now
cover `~committed/` (never prune; never drop `[capture]` from a root with
history; no rollback past capture support), capture's storage in sizing, and a
full capture history (`commit-failed` "no storage space" on one object while the
disk has room), each checked with `docs/ops/errors-check.sh` on a dev build of
`3dafe33`.

## E-63 filesystem matrix (`ideas/fs-matrix`)

**What:** `experiments/fs-matrix/` (`run.sh`, `fsync.py`, `summarize.py`, raw
results and a README with the table). Measurement only: `data_dir` on this
disk's ext4, tmpfs, and ext4/ext4+discard/XFS/btrfs on loop devices; fsync
latency for the journal's two patterns and a dev node's `POST /incr` serially
and at 64 concurrent; 3 alternating rounds.

**Results (six alternating rounds over two sessions, load average 19–34, so
relative only):** one `/incr` makes 9 flushes (6 `fsync` + 3 `fdatasync`,
`strace -c` over 200 requests, the same on `0ac2849` and `3dafe33`); tmpfs (not
durable) is 9–33× faster serially and 11–80× concurrently by median; btrfs held at
39–46/s at 64 concurrent in all six rounds (p99 4–5 s) while XFS never fell below
85/s; ext4's online `discard` lowered the loop setup's concurrent rate in five of
six rounds (median 60 vs 100/s; prefer `fstrim`); XFS led ext4 on the same setup
in five of six concurrent rounds and had the fastest write-rename-fsync in all
six. Hand-off for the storage owners: flush count per request, not the
filesystem, is the lever. `run.sh` now records non-200 statuses (they were 500s).

**Not measured:** real devices for XFS/btrfs, NVMe/cloud volumes, release
builds, a quiet machine. Rerun with `run.sh` on the target hardware.

## F-67 alerts and recording rules (`ideas/alerts`)

**What:** `deploy/monitoring/`: Prometheus recording rules per layer (rate,
errors, error ratio, per-app error ratio, p99) and nine alerts (node and app
error ratios, node latency, span-metric overflow, Collector down / refusing /
failing exports / queue filling, data disk), each pointing at its runbook; a
minimal `prometheus.yml`; promtool unit tests.

**Evidence:** promtool 2.45.3 (apt): `check config`, `check rules` (14 rules) and
`test rules` (5 groups, every alert asserted to fire with its labels and summary,
and not to fire below its traffic or duration floor) pass. The tests caught a
real gap: an app with no error series read as no data, not 0; fixed with
`or … * 0`. Not run against a live Collector (no binary); metric names follow
README's validated names, Collector ones matched with and without `_total`.
