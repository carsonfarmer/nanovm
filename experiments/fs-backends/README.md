# Bounded filesystem backend comparison

Storage screening for W-18/Q78, not a replacement WASI filesystem. The requirement
is a coherent, immediately visible tentative view that the host can accept or
reject. It does not prescribe an undo log. No production code, guests, root
dependencies or Wasmtime patches change in this experiment.

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
host state also consume memory. Directory membership currently scans the bounded
name table on final close; restart can be quadratic in file count. Those choices
avoid a second persisted index in this small proof, not an adoption-ready scale
claim. Persistent inode numbers use the existing string-keyed page store to share
the comparison code; no final filesystem format is selected.

**Successor prompt:** continue `fs-page-semantics`; read Q79 and this section.
Prefer hash-addressed pages where measured costs are close, without hiding its
large-write cost or the cache/RSS distinction. Before building a production WASI
adapter, settle total memory and physical-storage budgets, eliminate namespace
scan scaling, and test storage failures. Then prove mixed SQL/KV/raw-file behavior
through the actual host path. Keep production unchanged and do not merge migrated
guests/publish packages without approval.
