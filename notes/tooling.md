# Tooling notes

Section C of IDEAS.md (developer tooling) and spike K-100, one branch per item
from `cf/main-next` (37dd88e; `ideas/tooling-preinit` from 98f25be after
`cf/main-next` moved), merged into `ideas/tooling`. None changes how the node
runs: the one runtime change is a 3-line public `app::import_allowed`
(runtime lines 5,422 → 5,425). Tools are listed in [tools/README.md](../tools/README.md).

## C-37 `tools/inspect` (`ideas/tooling-inspect`)

**Problem:** an author learns that a node refuses a component only when it
fails to load: an import outside the allowlist, a key-value import not yet
composed, too many memories for the pool, `memory_mb` under the component's
own starting memory, a misspelt `nanovm.json` key.

**What it does:** `tools/inspect app.wasm [--config nanovm.json] [--node nanovm.toml]`
prints the imports (REFUSED where the node refuses them), the exports, whether
the node's own engine (`engine::build`: pool limits, memory reservation) and
linker (`app::linker`, `ServicePre`) accept the component, its composition
(nested components, which provider's export feeds which import, each core
module's memory, tables and data) and the declared limits beside the node's
ceilings and the clamped values. It exits 1 with the list. The verdicts are
the runtime's own code; the walk uses `wasmparser` at the version and features
wasmtime already builds (a dev-dependency, no rebuild).

**Numbers:** 0.75-0.84 s CPU per run (debug build), mostly Cranelift. On the
SQLite example it shows 9 core instances and 3 memories against the pool's 16
and 4 per component: composing one more Rust component would be refused.

**Tests:** `cargo test -p nanovm --example inspect`: `refuses_what_the_node_refuses`
(composed counter loads; the uncomposed one has `spin:key-value` refused and a
link error), `checks_the_declared_limits` (unknown key, `memory_mb` 2 under a
2.12 MiB start, a node with `memory_reservation_mb = 1`), `walks_the_composition`.

## C-38 `tools/volume.py` (`ideas/tooling-volume`)

**Problem:** copying one object's volume by hand can catch a request half done,
carry a crash-left undo log's tentative changes, or land on a stale journal or
history; `scripts/backup.py` snapshots whole roots through restic.

**What it does:** `export ROOT APP OBJECT OUT.tar` writes the volume in local
capture's TAR format (`nanovm.json`, then `data/`; sequence 0 marks an offline
export) while holding the app's storage lock, and refuses an object with an
undo log or staged capture. `export --committed [N]` copies a committed
version as it is (it already is an export; immutable, so no lock: works while
the node runs). `import IN.tar ROOT [--app] [--object]` refuses an existing
volume, journal or history, extracts only files and directories under `data/`
into a staging directory, syncs, renames it into place, and as root gives the
files the app directory's owner. `list ROOT` shows apps, objects, sizes,
pending undo logs and history lengths.

**Tests:** `tools/test_volume.py` (7, as root and as nobody) and
`crates/nanovm/tests/volume.rs`: files and key-value written by a real node come
back in a fresh root and take further writes across a restart; a captured
version taken while the node runs imports under another id;
`sqlite_volume_export_import_round_trip` (ignored, run by CI's SQLite step)
moves SQL rows, a file and a key together.

## C-44 `tools/doctor` (`ideas/tooling-doctor`, on `ideas/single-node`)

**What it does:** extends `deploy/single-node/check-host.py` rather than adding
a second checker; `tools/doctor [nanovm.toml]` runs it (the defaults without a
config). New checks, each saying what to do: address space against `ulimit -v`
(N-671) and strict overcommit; Landlock ABI (fails on 0 and 1, N-670); seccomp
(kernel support, filter actions, and a real filter installed in a forked child);
the capabilities a root supervisor uses (CAP_CHOWN, SETUID, SETGID, KILL); mount
options (ro, nobarrier, sync, overlay, network filesystems); a probe of the
calls the journal makes in the data directory (file and directory fsync,
flock, rename between directories, hard link); cgroup v2 and the delegated
cgroup (the parent hands the controllers down, no processes in it, writable);
the clock source and whether the clock is synchronized.

**Numbers:** the address-space need is `4 × (max_live_objects + 32) ×
(memory_reservation_mb + 64 KiB)`, about 517 GiB at the defaults; checked by
building the node's engine for 33 slots (16.5 GiB) under `prlimit --as`: fails
at 15 and 16 GiB, starts at 17, 18 and 20 GiB. On this host it warns that the
clock is unsynchronized (adjtimex `TIME_ERROR`) and the disk is mounted with
`discard`.

**Tests:** `deploy/single-node/test_check_host.py`, 15 (9 new), as root and nobody.

## C-40 `tools/local-stack` (`ideas/tooling-stack`)

**What it does:** an OCI registry (persistent storage), a DNS-over-HTTPS stub
answering `_nanovm` TXT and CNAME queries, an OTLP sink, and the real
`nanovm serve` (dev mode, jailed workers) and `nanovm proxy` processes with
their own config files; each app is deployed at `<host>.localhost`, so
`curl http://counter.localhost:8443/alice/` takes the full routed path.
Rebuilding an app's `.wasm` redeploys it within the 1 s tag TTL; the registry
and volumes persist in `target/local-stack` across runs (an app's identity
includes the registry address, so its port is fixed, `--registry`).

**Tests:** `crates/nanovm/tests/stack.rs` `local_stack_serves_the_routed_path`
(about 1.4 s): requests by Host (port and case ignored), the app named for the
record's owner, 404 without a record, a deploy moving the tag while the volume
stays.

## C-42 debugging (`ideas/tooling-debugging`)

[docs/debugging.md](../docs/debugging.md): reading the node's answer (linking
`docs/ops/errors.md` on `ideas/errors-guide` rather than repeating it), getting
a backtrace, debug-info builds, what the node's wasmtime leaves out, editors.
`tools/debug-run app.wasm [METHOD] /object/path` runs one request outside the
node with the imports it links (stock filesystem, egress refused, no CPU
budget, `/data` a throwaway copy of `--data DIR`) and prints a trap's whole
wasm backtrace, which the node never reports.

**Numbers:** with `debug = "line-tables-only"` and `strip = false` the hostile
guest's panic shows as `hostile/src/lib.rs:112:21` through 14 named frames.
The composed component grows 439 KB → 1,357 KB; compile CPU is unchanged
(0.77-0.84 s both, 3 runs each): the node never reads DWARF.

**Tests:** `cargo test -p nanovm --example debug-run` (2).

## C-39 `tools/replay.py` (`ideas/tooling-replay`)

**What it does:** `record --to URL` forwards requests to the proxy or a node
and logs each request and answer (JSON lines); `replay LOG --to URL [--host
NEW] [--object-prefix P] [--ignore KEY]` sends them again in order, under the
new version's host and onto fresh objects (objects keep state, so a replay
must start where the recording did), and reports every answer whose status,
`x-nanovm-error` or body (less ignored JSON keys) differs; exit 1 if any.
HTTP only.

**Evidence:** through `tools/local-stack`, a 6-request counter session
(increments, read, config, KV put and get) replayed onto fresh objects of the
same version gave 0 differences, and onto a host running the same component
with another `greeting` config, exactly the config read differed.

**Tests:** `tools/test_replay.py` (3).

## K-100 pre-initialization (`ideas/tooling-preinit`)

Full report: [experiments/preinit/README.md](../experiments/preinit/README.md).
In short, by process CPU, 5 alternating rounds, medians (ranges), load 11-13:

| | first request to a new object | private memory per object | component |
|---|---:|---:|---:|
| Rust guest, no start-up state | 321 (289-373) → 312 (289-329) µs | 36 → 32 KiB | 165 KB → 164 KB |
| Rust guest building a 50,000-entry table | 12,916 (12,058-13,698) → 355 (273-435) µs | 3,200 → 38 KiB | 167 KB → 3.05 MB |
| SQLite example, provider pre-initialized | 4,342 (3,648-4,593) → 3,636 (2,922-4,837) µs, no consistent difference | 283 → 278 KiB | 1.69 MB, same |

Pre-initialization helps only state built at start (36× there, and the state
becomes shared memory); the SQLite provider's start-up (`sqlite3_initialize`)
is small next to opening its database, which must stay at run time. Nothing
from `/data` can get in (every import traps; an initializer reading `/data`
fails the build), but std hash keys are frozen into the snapshot and shared by
every instance (W-640, shown on a real node). Recommendation: don't
pre-initialize Rust guests or the provider by default; JavaScript keeps its
own (3.7×, `notes/js.md` on `cf/optimistic-feynman-62f1a0`).

## Issues filed

- N-670 (docs): README said Landlock on Linux 5.13 suffices; the journal's
  renames between directories need ABI 2 (5.19). Fixed in README and the doctor.
- N-671: a worker under `ulimit -v` fails with wasmtime's "failed to create
  memory pool mapping", naming no fix. Documented and checked by the doctor;
  a `preflight` check is suggested (about +6 runtime lines), not made.
- W-640: pre-initialized Rust guests share std hash keys across instances.
