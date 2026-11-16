# PR #3 review fixes

Follow-up to PR #3 at `7988cae`, on `fix/pr3-review`. The new trim,
research and benchmark branches are not
included. N-930–N-937 in ISSUES.md track this work.

## Baseline

Seven independent test-only commits retain the failing proofs: `cc227b6`,
`e5be3d6`, `4527f99`, `b15b85b`, `c81b072`, `f0d8404`, `6e70e63`.
Each proof failed at its intended assertion before runtime fixes.
An existing integration test, `an_operator_deletes_an_object_and_an_app`,
failed with PermissionDenied when root lacked DAC override/read-search,
and passed with normal root capabilities.

The baseline ordinary suites passed when excluding only the seven new
proofs: 194 library tests (one ignored), 128 reported integration passes
(14 ignored), 1 config, 11 developer, 29 example, 17 JavaScript and 2
committed-state tests. Twelve integration cases self-skipped privileged
work. All 12 SQLite tests and production cargo check passed.

Privileged capacity, cgroup placement, engine-memory accounting and CPU/PID
reruns passed. The existing worker OOM test failed with eight 500 responses
and no OOM event; this does not establish OOM isolation, and its cause
was not established here.

## Scope of disk admission

Growth grants are capped by measured free space above the threshold,
minus the app's still-outstanding grants. They remain outstanding through
journal preparation and the actual write or resize, and are released on
success, partial writes and errors. Zero-growth operations remain allowed
below the threshold.

This is not strict node-wide physical reservation. Workers are separate
processes, and journal copies, metadata, sparse-file allocation, capture
and outside writers consume storage too. N-936 remains open for that
stronger guarantee. A shared ledger/coordinator would be a separate design,
not an implicit addition to this localized fix.

## Follow-up verification

Rust 1.99.1, RUST_BACKTRACE unset, RUST_TEST_THREADS=1,
file limit 20,000, vm.max_map_count=1048576.

The combined library suite passed: 206 passed, 0 failed, 1 ignored,
including all seven originally failing proofs. Focused quota, proxy,
gate/server and secrets/telemetry suites passed too. The added reserve
concurrency test failed when outstanding-grant subtraction was removed:
24,306,647,040 granted against 12,153,323,520 free, with half reserved.
After restoring it, all 18 quota tests passed.

The ordinary full-suite rerun passed: 206 library, 128 reported integration,
1 config, 11 developer, 29 example, 17 JavaScript and 2 committed-state
tests; 15 ignored overall. Twelve ordinary integration cases self-skip
privileged work, so these counts do not prove those paths. The first run's
deletion fixture collided with the root-owned directory left by the earlier
capability check. Inspection confirmed the stale directory/cache symlink;
removing that disposable fixture unblocked the unchanged test. The final
capability rerun passed too, and its fixture was removed afterward.

Full command: `ulimit -n 20000; env -u RUST_BACKTRACE RUST_TEST_THREADS=1
cargo +1.99.1 test --locked -p nanovm`.

All 12 separately enabled SQLite integration tests passed against the
fixed build. Production `cargo +1.99.1 check --locked -p nanovm --bin nanovm`
passed. Privileged small-disk tests passed for reserve refusal/recovery,
app-budget isolation and new-object refusal on a full disk.

The admin-delete integration test passed under a bounding set derived
directly from the corrected base systemd unit: CHOWN, SETUID, SETGID,
KILL and DAC_OVERRIDE. This exercises the real supervisor/worker/admin
path, not a full systemd or container deployment. The container validation
script passes bash syntax checking; `git diff --check` passes.

Runtime-line count by CI's method: 8,711 before, 8,821 after (+110).
No formatter-only rewrites were retained. Rust 1.99.1 lacks the rustfmt
and clippy components; the installed formatter reports broad pre-existing
differences, so there is no repository-wide format/clippy sign-off.

No new performance measurements yet; no performance sign-off implied.
The runtime fixes and documentation are packaged together for integration
into PR #3; the seven original proof commits remain separately retained.
Delivery now includes resolving the cleanup merge on main and rerunning
the combined checks before merging PR #3.
