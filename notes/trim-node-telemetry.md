# Outgoing telemetry cap (2026-11-16)

Base: `origin/trim/node` at `228aa3883a5bbdf89d69ae9a0ce3768b5942abe7`.
Worktree/branch: `nanovm-trim-node`, `fix/trim-node-telemetry`.

The input cap is insufficient: inserting the supervisor's trusted resource
identity can expand a valid worker batch above 1 MiB. `Relay::send` now checks
the final byte length before opening the collector request, returning 413 for
oversized bodies. This is also the send path for direct host exports. The SDK
already bounds its producer batches; no new dependency or serialization path
is needed. Stamping still uses the bounded-resource wire rewriter, not full
protobuf decoding.

`relay_stamps_identity_and_bounds_worker_requests` sends valid trace and log
batches through a real loopback relay and collector. Each hostile input is
strictly below 1 MiB but becomes exactly 1 MiB + 1 byte after stamping. It must
receive 413 without adding a collector request. Each control becomes exactly
1 MiB and must receive 200, with that exact body size at the collector. Existing
identity scrubbing, malformed/oversized input, timeout, partial-success and
UTF-8 log bounds remain covered.

## Reproduction

Use Rust 1.99.1, unset `RUST_BACKTRACE`, `ulimit -n 20000`, serial tests and
an independent target directory. The dependency cache was hardlinked from the
parent target; local nanovm fingerprints/output binaries and all hardlinked
Cargo lock files were replaced before building. Inherited inactive incremental
directories were removed to relieve disk pressure. No parent target was used
as a build destination.

```sh
ulimit -n 20000
env -u RUST_BACKTRACE RUST_TEST_THREADS=1 CARGO_BUILD_JOBS=1 \
  cargo +1.99.1 test --locked -p nanovm --lib \
  relay_stamps_identity_and_bounds_worker_requests -- --nocapture
env -u RUST_BACKTRACE RUST_TEST_THREADS=1 CARGO_BUILD_JOBS=1 \
  cargo +1.99.1 test --locked -p nanovm --lib telemetry::tests -- --nocapture
env -u RUST_BACKTRACE CARGO_BUILD_JOBS=1 \
  cargo +1.99.1 check --locked -p nanovm --bin nanovm
```

Before the fix, the new regression fails: `traces expanded beyond cap`, actual
HTTP 200 versus expected 413. With the fix, all seven telemetry tests pass.
The fixed unit-test executable also passes all 194 enabled unit tests serially
(one ignored), in 64.20 seconds. Production `cargo check` passes.

Mutation check: stage the fixed source, remove only the outgoing guard, rerun
the regression: it fails again with HTTP 200 versus expected 413. Restore using
`git checkout -- crates/nanovm/src/telemetry.rs`; its unstaged diff is empty.
After restoration, all seven telemetry tests pass again (6.36 seconds).

Runtime-line delta using the CI counter: 8,634 → 8,635 (**+1**).

`cargo fmt --all -- --check` reports existing repository-wide formatting
differences, beginning in `examples/bench.rs`; no unrelated files were formatted.
Strict `cargo clippy --locked -p nanovm --all-targets --all-features -- -D warnings`
fails on 14 diagnostics in unchanged code (including the existing `unit_arg`
in telemetry's wire rewriter); no diagnostic targets an added line. Rustdoc
(`cargo doc --locked -p nanovm --lib --no-deps`) passes with 10 existing warnings.
Full integration/demo and alternating CPU benchmarks were not run in this
concurrently building, disk-constrained orb. No throughput claim is made.

## PR #3 review integration (2026-11-16)

Merged the committed local `fix/pr3-review` at
`9965f4678cf868517d2bb34671c3ab5b36deddb7` into this branch with a real merge,
not a rebase or an `origin/main` merge. Git merged it without textual conflicts.
The parent advanced its local branch during validation; those later follow-ups
are deliberately not part of this integration.

Merge decisions checked against the pinned second parent:

- Keep node trims, including prost's varint APIs, and the final stamped-body
  cap plus trace/log exact-boundary controls.
- Keep PR #3's schema-aware protobuf string redaction. Secret matching is
  applied only to string slices; IDs, bytes, numeric fields and length framing
  remain untouched. Literal and host Debug-escaped error-secret masking stay.
- Gate, proxy, quota, secrets, shipping and integration tests match the pinned
  PR #3 commit exactly. N-930–N-938 remain unchanged; N-903 stays distinct.
- Keep cleanup deletions and dependency choices: base64 0.23 and memchr 2.
  Cargo manifests/lock and publication filter match the pinned PR #3 commit.

Combined checks, Rust 1.99.1, `RUST_BACKTRACE` unset, fd limit 20,000,
`RUST_TEST_THREADS=1`, `CARGO_BUILD_JOBS=1`, this worktree's target:

- Focused telemetry: **9 passed**, including redaction/framing/binary-field
  controls, recursive redaction bounds and final stamped export bounds.
- Focused secrets: **6 passed**, including escaped and overlapping matches.
- `cargo test --locked -p nanovm --lib`: **206 passed, one ignored** (63.65 s).
- `cargo check --locked -p nanovm --bin nanovm`: **passed**.
- `git diff --cached --check`: **passed**.

Runtime: 8,635 → 8,745 (**+110**) against this branch before integration;
8,821 → 8,745 (**−76**) against the pinned PR #3 parent. This merge adds no
new runtime mechanism beyond those parents. Parent-owned follow-ups, aggregate
integration/deployment tests and benchmarks remain for the parent. No push or
PR. The disposable worktree target is removed after all builds finish; source,
documentation and the parent target remain untouched.
