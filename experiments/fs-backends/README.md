# Bounded filesystem backend comparison

Storage screening for W-18/Q78, not a replacement WASI filesystem. The requirement
is a coherent, immediately visible tentative view that the host can accept or
reject. It does not prescribe an undo log. No production code, guests, root
dependencies or Wasmtime patches changed in the original storage-only experiment.
The separate runtime candidate below is now available for comparison, not adoption.

## Runtime integration checkpoint (2026-11-05)

`cargo test --locked -p nanovm --features page-fs` selects the real hash-addressed
WASIp3 adapter; the default remains the journal. This temporary A/B feature is not
a compatibility layer or a migration. Use separate disposable data roots: old
ordinary-file volumes are not imported. No `guests/**` changes or Wasmtime patches
are part of this integration.

The candidate has hierarchical stable inode identities, bounded range streams,
shared logical quotas, held-unlinked charging, and atomic DATA/TMP roots in one
file-backed redb database. Opening clears TMP and reclaims unheld orphans. Both
host acceptance gates use Immediate/two-phase commits; guest sync cannot publish.
Read-only acceptance performs no storage writes. `storage_mb=256` is an independent,
provisional database-file-length ceiling, not a proven multiplier of `disk_mb`.
The redb cache target is 64 KiB, not a total-memory guarantee.

Integration exposed two real issues: the actor's live counter retired before all
database/stream resources dropped, and redb continued without exclusion when the
jail returned ENOSYS for OFD locking. Retirement now follows complete teardown;
the jail permits OFD locks and flock but still denies signal-owner commands.
The jailed locking regression opens the real capped adapter, not just bare redb.

Independent review caught a missing directory sync for the new database name.
The real syscall-error regression failed with an acknowledged mutation before
the fix; initialization now syncs database contents then its directory before
running guest work. Failure rejects initialization. Removing that sync fails the
test; restoration uses `git checkout --` and an empty unstaged diff. This is not
physical power-loss certification. The post-return failure test now reopens twice,
requiring earlier accepted bytes and a stable, wholly old-or-new later generation.
The inode's persisted linkage replaces a redundant ORPHANS table; recovery already
streams inode metadata for accounting, with no namespace cross-product scan.

Restored full candidate validation passes 88 unit and 68 integration tests, with
11 optional tests ignored. Default validation passes 91 unit and 67 integration
tests on rerun: its first run hit N-32's setup CPU-budget failure, not a claimed
fix. The opt-in composed SQL contract also passes, including mixed SQL/file/KV
rejection, obstructed startup/retry and restart. The demo against the saved page
binary and a fresh disposable root passes all 70,000 normal requests, contains
100 attacks, and serves 486 canary requests without failure or reported counter
kills. Services and disposable data were removed afterward. Remaining optional
SQL, stream-lifetime, hostile resource-bound and ancestor-directory durability
gates remain to be completed. In
particular, old optional tests that inspect host-side guest filenames are not
valid oracles for this backend and must be adapted, not silently skipped.

Three alternating release pairs use the identical composed counter, Wasmtime
50.0.0, the existing `kv_bench`, isolated disposable volumes, and no overlapping
builds/tests. Medians in milliseconds per request:

| Operation | Journal | Page candidate |
|---|---:|---:|
| GET 128 B | 0.190 | 0.207 |
| GET 16 KiB | 0.232 | 0.266 |
| GET 1 MiB | 3.033 | 3.032 |
| PUT 128 B | 2.817 | 0.911 |
| PUT 16 KiB | 3.291 | 0.948 |
| PUT 1 MiB | 7.282 | 5.217 |
| Cold PUT 4 B | 4.079 | 4.468 |

These PUTs repeat one value and contain identical pages; they favor deduplication.
`kv_bench <counter.wasm> --unique` now changes every page between PUTs, with data
generation excluded from timing and final byte equality checked. Three additional
alternating pairs on the simplified candidate give median PUT 128 B **2.884/0.914
ms**, 16 KiB **3.421/1.170 ms**, and 1 MiB **7.154/16.764 ms** (journal/pages).
Large unique writes are **2.34× slower**, despite small-write wins. GETs remain
repeated-content controls: **0.187/0.192**, **0.234/0.256**, **2.970/2.994 ms**;
cold PUT is **3.882/4.182 ms**. Neither benchmark uses network HTTP.

One matched `bench --quick` pair reports journal/page cold starts **2,775/958 per
second**, warm concurrent GET **90,527/85,829**, concurrent writes **754/4,131**,
serial writes **394/1,077**, recovery **1,437/290**, churn **4,298/769**, and observed
incremental density **144/210 KiB per object**. Both have no request or instantiation
failures. These expose cold/reopen/memory regressions; write gains alone do not
clear the gate. Raw logs and saved binaries are in `.amp/in/fs-integrated/`.
The simplified candidate's final quick rerun confirms the regressions: cold 1,049/s,
churn 830/s, recovery 278/s, density 211 KiB/object; concurrent/serial writes
4,366/1,241 per second, with no request or instantiation failures.

Count all replacement code: old quota+journal **564+476=1,040** lines versus
adapter+inode+cap **611+699+68=1,378**. The checked-in A/B tree is 6,464 runtime
lines. Deleting the old modules would give **5,424 versus 5,074 (+350)**, before
removing temporary feature wiring. Provider code is unchanged. No LOC reduction
or overall simplicity improvement is claimed.

**Successor prompt:** continue `hashed-fs-integration`, not a production swap.
Preserve the integrated correctness fixes and actual file-length cap; finish the
review's real-stream and persistent-cap/runtime cases, replace SQL filename
oracles with guest-visible assertions, and investigate eager database creation
and reopen cost. Measure unique pages and density alongside writes, simplify
actual ownership rather than formatting, and rerun both complete suites, demo
and matched benchmarks before any merge. Guest publication still needs approval.

## Candidates and protocol

- **journal:** the actual `nanovm::journal::Journal`, ordinary files, same-directory
  staging/rename for replacement, protected positional writes, recovery on reject.
- **pages:** redb 4.4.0 transactions, a file-length table and `(name, page index)`
  mapping to bytes, with 4 KiB logical pages.
- **hashed:** the same redb transaction/length/page structure, but page values are
  SHA-256 addresses into shared content and reference-count tables. Last-reference
  deletion reclaims content transactionally. This is not a Merkle tree.

Both redb variants use `Durability::Immediate` and two-phase commit, including
initialization. Neither batches durability across requests. Read-only requests
abort their empty transaction; both page variants skip unchanged page content
(byte comparison versus digest comparison). File length still updates on replace.
They use redb's default cache limit (1 GiB, not a claim of allocated RSS). This run
does **not** establish many-object memory density or a production cache budget.

The harness uses one disposable store per case, prepares data outside timing, and
rotates the three backends over three rounds. It reports JSONL per-run mean,
p50/p99, commit/reject time, Linux `/proc/self/io` write bytes, and final allocated
regular-file blocks (`st_blocks * 512`) after close/reopen. Requests are serial;
all reads are warm. Timings include begin/operation/end, allocation and reads'
byte-equality assertion, but exclude payload generation and final verification.
They exclude Wasm, HTTP, quotas, startup and the second empty host commit boundary.
They must not be compared directly with end-to-end KV timings.

Payloads are deterministic varied binary bytes. Replacements change every page;
the identical case changes nothing. `batch` writes 32 distinct 128-byte files in
one transaction; `shared-batch` writes the same 16 KiB value to 32 files, changing
that value each request. The latter deliberately measures cross-file deduplication.
The journal baseline does not detect no-op writes. No candidate uses compression.

## Reproduce

Run from the repository root, on Linux, with no overlapping builds or load:

```bash
CARGO_TARGET_DIR=target cargo build --release --locked --manifest-path experiments/fs-backends/Cargo.toml
env -u RUST_BACKTRACE target/release/fs-backends check
env -u RUST_BACKTRACE target/release/fs-backends > fs-backends.jsonl
```

`check` runs correctness and process-death checks only. The normal run repeats
those before its 108 measurements (12 cases × 3 backends × 3 rounds). Successful
runs remove only their newly created `target/fs-backends-PID` tree; failed runs
retain it. The independent Cargo workspace/lock isolates redb from the host.

## Measured results: COW helps small mutations; hashing is workload-dependent

2026-11-03, Linux 6.1.158+, x86-64 orb, ext4, Rust 1.99.1, release build.
Median of three run means, **microseconds per storage transaction**, lower is better:

| Workload | Operations/round | Journal | Pages | Hashed |
|---|---:|---:|---:|---:|
| Read 128 B | 500 | 2.0 | 3.5 | 4.4 |
| Read 16 KiB | 500 | 3.1 | 8.3 | 10.5 |
| Read 1 MiB | 100 | 198.3 | 503.9 | 801.0 |
| Replace 128 B | 100 | 2663.1 | 364.2 | 387.6 |
| Replace 16 KiB | 100 | 3215.5 | 516.7 | 512.0 |
| Replace 1 MiB | 40 | 5486.0 | 4929.1 | 8893.9 |
| Identical 1 MiB replacement | 40 | 4937.7 | 802.0 | 1245.8 |
| 4 KiB positional write in 1 MiB | 100 | 2027.2 | 454.1 | 507.1 |
| Reject 16 KiB replacement | 100 | 4667.8 | 13.6 | 52.3 |
| Create 1,000 distinct 128 B files, commit each | 1000 | 2205.4 | 400.1 | 471.8 |
| 32 distinct 128 B files per transaction | 40 | 51139.4 | 523.0 | 778.2 |
| 32 shared 16 KiB files per transaction | 40 | 70123.6 | 2308.6 | 1626.2 |

These are local measurements, not confidence intervals or hardware-independent
speedups. Short runs' p99 values are diagnostic only. The experiment does not
establish that redb is the fastest COW library or SHA-256 the fastest useful hash.

A second complete three-round run after removing an incidental journal box
allocation confirms the tradeoff: journal/pages/hashed 128 B replacement
2534.5/373.1/371.6 µs; 1 MiB read 179.3/459.8/750.2 µs; changed 1 MiB replacement
5379.9/4487.5/8284.3 µs; shared batches 69039.5/2492.8/1538.7 µs. Both raw JSONL
matrices and their successful check logs are under `.amp/in/artifacts/`, named
`fs-backends-final` and `fs-backends-repeat`. They contain per-run p50/p99 and
write/allocation counters; the table above retains the first matrix, not a mix
of favorable samples from both.

**Why:** the existing undo implementation syncs records before individual mutations
and syncs touched files at acceptance; COW leaves tentative pages unpublished and
commits a transaction together. Rejection can discard it without undo I/O. Hashing
adds digest computation, lookup and reference maintenance. It pays off here for
cross-file repetition, not for unique large writes or unchanged data that a plain
byte comparison can already identify. The identical result is not proof that the
ordinary-file path could not also benefit from no-op detection.

Fewer sync round trips do not mean fewer bytes written. Median **kernel-accounted
write bytes per transaction**, excluding setup and reopen:

| Workload | Journal | Pages | Hashed |
|---|---:|---:|---:|
| Replace 128 B | 8,192 | 32,768 | 40,960 |
| Replace 1 MiB | 1,056,768 | 2,150,400 | 2,183,373 |
| 4 KiB positional write | 12,288 | 45,056 | 66,929 |
| Reject 16 KiB | 32,768 | 0 | 0 |
| 32 distinct 128 B files | 385,536 | 40,960 | 49,152 |
| 32 shared 16 KiB files | 914,227 | 1,089,536 | 86,016 |

This is `/proc` accounting, not physical device traffic; delayed writeback,
cancelled writes, filesystem metadata and the storage stack limit interpretation.
Final regular-file allocation for 1,000 tiny files plus the seed was
512,512 / 393,216 / 540,672 bytes. A single untouched 128-byte seed used
512 / 45,056 / 53,248 bytes. After shared batches it was
540,672 / 2,641,920 / 204,800 bytes. These exclude directory/inode metadata,
open-unlinked files and peak usage; database free pages remain allocated.

## Correctness gates and limits

Executed checks verify exact bytes for cross-page writes, tentative reads,
multi-file rejection, shrink followed by sparse extension, empty files, deletion,
and close/reopen persistence. Hash audits compare the complete page reference
multiset with reference counts, verify stored content hashes, and reject orphan
blobs. SIGKILL children before acceptance, racing acceptance and after acceptance
recover both files consistently: old before, new after, either complete state
in the race. Nine cuts are smoke tests, not exhaustive fault injection. The race
does not guarantee a kill inside commit, and SIGKILL is not physical power loss.

Mutation checks all failed as intended: accepting rejected page transactions
violates restored-byte assertions; omitting last-reference blob removal triggers
the orphan-content audit; skipping journal write protection violates restored-byte
assertions. Each mutation was restored with `git checkout --` and an empty diff
confirmed. The restored full 108-measurement run passes. Experiment rustfmt and
`cargo clippy --locked --manifest-path experiments/fs-backends/Cargo.toml --no-deps -- -D warnings`
pass. The enum-size lint is explicitly expected: there is one backend per case,
so boxing solely to reduce its bounded stack footprint adds no benefit.

Missing before any backend adoption: directories, rename/unlink with held inode
handles, WASI integration, quota/resource bounds, mixed SQL/KV/raw-file workloads,
sync/I/O failure injection, recoverability during recovery, many-object density,
bounded-cache/cold-read measurements, and controlled-device durability testing.
The prototypes' path-keyed pages do not implement descriptor identity. They do
not exercise SQL merely because redb is a database. All backends run in-process,
outside the worker jail. Runtime line count is unchanged; this harness is about
600 lines and is not replacement-code savings. The existing CI count is
**5,074 → 5,074** host runtime lines (plus the unchanged 213-line KV provider).

## Recommendation and successor prompt

Continue with **plain transactional pages** as the next bounded adapter experiment;
keep the production filesystem unchanged. First prove inode/descriptor semantics,
bounded per-object memory and shared SQL/KV/raw-file behavior. Re-measure against
ordinary files under the same host/guest path before accepting a backend change.
Keep hashing as an optional design candidate for measured dedup/export needs, not
the default local storage optimization.

External durability needs an ordered committed-version manifest plus data,
retention until replica acknowledgement, a local-versus-remote acknowledgement
policy, and writer epochs/fencing. Hashes can identify reusable transfer chunks;
they do not supply ordering, atomic publication or multi-region consistency.
This reference-count implementation immediately frees unreferenced blobs and
does not retain replica history. Neither candidate exports versions yet.

**Successor:** read Q77/Q78, this protocol and W-18. Continue on
`fs-backend-comparison`, based on unmerged `spin-kv-prototype`. No Wasmtime patch
is enabled. Do not merge/publish migrated guests or claim WASI equivalence from
these storage-only timings. Scope the descriptor/quota/cache-budget proof before
implementing a broader filesystem replacement.

## Inode, quota and cache proof (Q79, 2026-11-04)

The owner prefers hash-addressed pages when timings are close, with simplicity
also deciding. Both variants now share `inode.rs`: a **flat regular-file
namespace**, persistent inode identities, explicit descriptor ownership, and
logical byte/file/descriptor limits. Namespace entries and pages commit in the
same redb transaction. An open descriptor continues to address its inode after
rename-over or unlink; the last close reclaims an unlinked inode. Such inodes
remain charged while held, including across commits. Restart reclaims committed
orphans, because no descriptors survive process death. Reject discards the
transaction and invalidates every descriptor, matching whole-instance disposal,
not a savepoint that preserves guest execution.

The same logical bytes are charged for duplicate content: deduplication never
creates free logical capacity. Growth and handle/file quotas are checked before
mutation; tests cover exact limits and one byte/entry beyond, sparse extension,
zero-byte writes, shrink/regrow without old-tail exposure, aliases, commit-held
descriptors, rejection of namespace changes, and orphan recovery. Six additional
SIGKILL cuts test rename-over with a held old inode plus a companion file before,
racing and after acceptance. These are process-death smoke tests, not power-loss
certification. Storage errors terminate the probe; catchable storage-error and
cancellation semantics are not implemented by this synchronous experiment.

Two avoidable page-path costs were removed for **both** candidates: full-page
writes no longer read the old page, and whole-file reads iterate the page range
with tables opened once, copying directly into the output rather than allocating
one intermediate vector per page. Partial writes still preserve untouched bytes.

### Reproduce the bounded proof

Build with the command above, then:

```bash
target/release/fs-backends inode
target/release/fs-backends inode-bench > inode-bench.jsonl
# Run each density invocation in its own fresh process.
target/release/fs-backends density hashed 65536 64 1048576
```

`inode-bench` runs five workloads × two caches × two backends × three rotated
rounds (60 rows), 40 transactions per row. Replacement now creates/writes/closes
a fresh inode then renames it over the old name. This enforces real descriptor
semantics; it is **not** the old path-keyed in-place replacement timing. Unaligned
4 KiB writes start at byte 4093 in a 1 MiB file. Verification after reopen checks
exact bytes, inode/byte counts and hash references. Cache limits are 64 KiB and
1 MiB, not the old 1 GiB default. All acceptance remains immediate/two-phase.

Median run means, microseconds; plain / hashed, after the page-path improvements:

| Workload | 64 KiB cache | 1 MiB cache |
|---|---:|---:|
| Replace 128 B | 381.0 / 408.6 | 396.1 / 405.7 |
| Replace 16 KiB | 505.6 / 656.2 | 531.0 / 541.8 |
| Replace 1 MiB | 8039.3 / 13626.5 | 7089.4 / 10734.4 |
| Read 1 MiB | 959.7 / 1025.5 | 962.3 / 945.4 |
| Unaligned 4 KiB write | 427.9 / 639.2 | 415.6 / 577.1 |

Before the improvements, 1 MiB reads at the 1 MiB cache measured
1189.3 / 1328.9 µs. Raw pre/post matrices: `.amp/in/artifacts/inode-bench.jsonl`
and `inode-bench-optimized.jsonl`. Do not compare these storage-only figures to
HTTP/Wasm latency or call the new-inode replacement regression a hashing-only cost.
Small writes and reads are close at the larger cache; unique large replacements
and partial writes still are not. Keep hashing as the preferred candidate in the
near-tie workloads, with plain pages as the control rather than choosing globally.

### Memory and verification results

`density KIND CACHE_BYTES 64 VALUE_BYTES` keeps 64 independent stores' writes
uncommitted, samples RSS, accepts all transactions, checks their bytes, then
samples RSS again. Three fresh processes per combination, 24 processes total.
Median **incremental process RSS in KiB per store**, plain / hashed:

| Cache | Value | All dirty | After acceptance/readback |
|---|---|---:|---:|
| 64 KiB | 128 B | 111.6 / 118.6 | 119.8 / 127.8 |
| 64 KiB | 1 MiB | 231.1 / 234.2 | 259.9 / 235.1 |
| 1 MiB | 128 B | 111.9 / 121.3 | 120.1 / 131.9 |
| 1 MiB | 1 MiB | 1212.2 / 1212.1 | 1220.4 / 1225.3 |

These include allocator retention, amortized process costs and active transaction
metadata. They exclude the guest, Wasmtime, other host objects and kernel page
cache. They are neither peaks nor hard bounds, and an apparent hashed memory win
in one row is not a universal footprint advantage. The lower cache remains viable
in these checks, at the latency cost shown above. Raw final matrix:
`.amp/in/artifacts/inode-density-final.jsonl`. Earlier `inode-density` and
`inode-density-optimized` files committed each store before creating the next;
do not combine those with this dirty-transaction protocol.

Six independent mutations failed: ignoring held references, removing file,
descriptor or write-byte quota checks, retaining truncated tails, and failing to
mark metadata-only changes dirty. Each was restored with `git checkout --` and
an empty diff confirmed. Restored `check` and `inode` modes pass, as do rustfmt
and strict experiment clippy. `env -u RUST_BACKTRACE cargo test --locked -p nanovm`
passes 91 unit and 67 integration tests, eleven ignored. Host runtime remains
5,074 lines; the shared inode/quota layer adds 228 nonblank/noncomment experiment
lines before its checks, plus the validation and measurement code. No production
code savings, new demo result or end-to-end performance claim.

### What remains outside this proof

No production host/guest/Wasmtime changes, directory traversal, links, permissions,
WASI resources/streams, SQL, or remote export. Quotas are logical, not bounds on
database allocation or retained COW versions. The cache setting is **not a total
RSS limit**: transaction metadata, output buffers, allocator retention and other
host state also consume memory. The initial namespace scans are replaced by the
orphan index described below. Persistent inode numbers use the existing string-keyed page store to share
the comparison code; no final filesystem format is selected.

**Successor prompt:** continue `hashed-fs-integration`; read Q79/Q80 and this section.
Prefer hash-addressed pages where measured costs are close, without hiding its
large-write cost or the cache/RSS distinction. Before building a production WASI
adapter, settle total memory and physical-storage budgets and prove resource mapping.
Then prove mixed SQL/KV/raw-file behavior
through the actual host path. Keep production unchanged and do not merge migrated
guests/publish packages without approval.

### Hardening: orphan lookup and storage failures (2026-11-05)

Unlink and rename-over record the disconnected inode in an `orphans` table in the
same transaction as the name change. Collection checks that index and live handles,
not every name. Startup counts lengths once and collects only indexed orphans.
The experiment's data is disposable; there is no previous-prototype format migration.

`inode-scale` creates 1,000/10,000 named one-byte files plus a committed held orphan,
reopens, times 1,000 opens/closes of the last name, and checks every value and exact
quota usage. With 64 KiB caches, the initial paired run at 10,000 files measured:

| Operation | Scanning plain / hashed | Indexed plain / hashed |
|---|---:|---:|
| Reopen | 3,028 / 3,044 ms | 1.52 / 1.56 ms |
| Open + close | 625 / 652 µs | 2.12 / 2.10 µs |

The control restores the old name scans in the otherwise identical harness. These
are storage-only observations, not full-node throughput or a repeated statistical
benchmark. Removing rename-over orphan registration or restart collection fails
the existing inode contracts. Each mutation was restored with `git checkout --`
from the staged implementation, and an empty unstaged diff verified.

`CARGO_TARGET_DIR=target cargo test --release --locked --manifest-path experiments/fs-backends/Cargo.toml -- --nocapture`
runs 217 plain and 467 hashed mutation/commit failure cuts through a real `FileBackend`, preserving
its file locks. Another 57 plain / 77 hashed cuts fail orphan recovery and its
commit, with content shared by the orphan and a named file. It fails each read, write, growth or sync in turn; a failed write
writes its first half before returning an error. After a caught mutation error,
an attempted acceptance must fail and reopening must return the old state. A failed
commit may recover either complete state, never mixed names/data. The successful
control must recover the new state. Exact bytes, quota totals, orphan cleanup,
hash reference counts and the same generation on a second reopen are checked. The
review caught and fixed an oracle that had allowed either generation independently
on each reopening. The test now observes the underlying commit result separately
from starting the next transaction. Redb rejects the later
commit after these I/O errors; no duplicate host poisoning mechanism was added.

This does not simulate physical power loss, failed database initialization/locking,
or every partial-write geometry, and
does not yet establish a physical database-size or total-memory bound. The real
WASI adapter and end-to-end acceptance gates remain pending. Production code and
its 5,074-line count are unchanged; the shared experiment inode layer grows
228 → 238 lines. Restored original/inode checks and strict all-target experiment
clippy pass. Full locked host suite passes 91 unit/67 integration, eleven ignored.
No demo rerun for this experiment-only milestone. Review found no bounded-slice
implementation blocker; that is not approval of a production swap.

### Persistent capacity boundary (2026-11-05)

`capped.rs` delegates to the real `FileBackend`, including all six lock methods,
and refuses growth or a write ending beyond a fixed ceiling before touching the
file. Overflow is refused; shrink and writes inside the cap remain permitted.
Opening existing data above a lowered ceiling refuses it without truncation.
This bounds file length (`st_size`), not allocated filesystem blocks or RSS.

The release tests run both variants at 1,056,768, 2,105,344 and 5,251,072 bytes.
They exhaust storage during a bounded append sequence, attempt caught-error
acceptance, and reopen twice with the **same** cap still enforced. Committed
bytes survive, an orphan sharing content with a named file is reclaimed exactly
once, hash references and logical counters agree, and small writes/deletes still
commit. Competing opens fail before and after exhaustion. File length is checked
during growth, not only after close (redb can shrink then). All six cases pass.
Direct boundary tests cover rejected partial writes, arithmetic overflow, shrink
at the ceiling and lowered-policy refusal. Omitting the growth guard fails the
test; `git checkout --` restores staged good source with empty unstaged diff.

These cases do not establish that every full database can allocate recovery
metadata, or a safe file-length/logical-quota multiplier. Production must fail
closed when same-cap recovery cannot complete. The exact-source resource audit
found that cache budgets count page payload softly, not total heap; uncommitted
rewrites do not inherently retain every historical version. Avoid memory-backed
`/tmp`; use file-backed ephemeral state with shared logical accounting. Worker
cgroups are aggregate protection, not per-object host-memory limits.

**Successor prompt:** continue real WASI integration on `hashed-fs-integration`,
using the fixed-cap backend, stable inode resources and bounded streams. Preserve
both host acceptance gates. Complete jailed KV/SQL/raw-file validation and matched
performance, memory and total-LOC comparisons before swapping production storage.
