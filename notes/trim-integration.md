# Integrated trim candidate

The candidate combines PR #3's review fixes, all three corrected trim branches
and the repaired current-v1 benchmark harness. It is based on `d86d94b`, the
PR #5 merge into `beta/integration`. No published history was rewritten.

## Merge decisions

- Node: preserve schema-aware telemetry masking and enforce the 1 MiB cap
  after identity stamping. Use prost's varint helpers.
- Storage: preserve app-local in-flight growth accounting, zero-byte object
  reclamation and shipping-head invalidation after deletion/recreation. Encode
  complete node names in session keys so slash normalization cannot alias them.
- Edge: preserve original response framing while enforcing byte caps. Put
  connection admission on the underlying I/O in `server::serve_conn`, covering
  public and worker-link upgrades. Retain cancelled-route reclamation.
- Request-rate limits remain removed by owner decision. Connection caps remain;
  the benchmark uses that one current-v1 contract, without old-format adapters.
- Combine rustix's `process` and `time` features; both trims need them. Preserve
  main's removed experiments and base64 0.23, and the review fix's memchr use.
- Keep the parent failover and worker-OOM fixture corrections. Each node has
  separate local storage; only capture storage is shared.

Runtime, using CI's nonblank/noncomment count before test modules:
**8,821 → 8,331, −490 lines (5.6%)**. This is a simplicity improvement, not
evidence of a performance improvement. Documentation-only conflict resolutions
retain all agents' evidence and distinct issue IDs.

## Evidence and delivery

Each corrected branch passed its combined library suite and production check.
The framing, upgrade-lifetime, lease-key and telemetry-expansion regressions
have failing baseline and caught-mutation evidence in their individual notes:

- `trim-edge-framing.md`
- `trim-node-telemetry.md`
- `trim-storage.md`

The aggregate full serial suite passed: **392 tests** (207 library, 1 config,
11 dev, 29 examples, 127 integration, 17 JavaScript; 14 ignored). All ten
multi-node tests ran. All **12 SQLite** cases and the production check passed.
Commands use Rust 1.99.1, `env -u RUST_BACKTRACE`, `RUST_TEST_THREADS=1`
and fd limit 20,000. Release binaries were rebuilt with `--release --features
dev` in the separate `target/bench-release` directory.

Privileged OOM, reserve refusal/recovery, app-budget isolation and full-disk
new-object refusal passed. Administrative deletion passed with the capability
bounding set extracted from the base systemd unit. An initial capsh invocation
could not establish its requested capabilities; dropping the complement of
the unit's bounding set corrected the runner. The actual deletion test then
passed. Ordinary unprivileged suite counts include self-skipping privileged
tests and are not used as evidence for those paths.

The ops Python suites passed **18 tests** (8 installer, 6 history, 4 release);
the benchmark harness passed its separate **18 tests**. The final trimmed
release passed an actual registry/CLI/admin/collector/Rust/JS smoke run and
exported fresh live/store counts of 1/1 for the seed object. These smoke
counts validate wiring, not performance. No new strict lint/format clearance.

PR #3 is merged as `ba87b87`, after both combined-head CI runs passed.
The trim branch records that main merge without changing source. Trim CI,
main integration and fresh idle-machine performance results remain pending.
