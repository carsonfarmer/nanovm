# Response cap without changing HTTP framing

Base: `trim/edge` at `2efdd1a`. Worktree: `nanovm-trim-edge`, branch
`fix/trim-edge-framing`. The approved rate-limit removal is unchanged.

## Cause and change

`http-body-util` 0.1.5's `Limited::size_hint` changes an exact declared length
larger than the cap into an exact cap. The response still carries the guest's
Content-Length. Hyper 1.12.0 debug-asserts that these disagree; without that
assertion its fixed-length encoder can stop polling at the cap without seeing
the limiter's error. A successful truncated answer is not acceptable.

Keep a byte counter inside the existing `GuardedBody`, reject the whole frame
that would exceed the cap, and forward the original body's size hint. Keep the
existing abort and transmitted-notification behavior. The existing library
limiter was inspected, but it cannot forward the underlying hint unchanged.
No new dependency or second wrapper is needed. Runtime delta: **+10** nonblank,
noncomment lines before test modules, using CI's counting rule.

## Regression

`object::tests::guarded_body_worker_link_framing` uses the real worker-link
`Dialer` and `serve`, Unix socketpairs passed by SCM_RIGHTS, and Hyper HTTP/1.1.
The synthetic guest body reports exact remaining lengths and yields between
frames so response headers can reach the reader before the overflow.

- Hostile: Content-Length 7; frames `12`, `34`, `567`; cap 4. The first two
  frames reach the cap exactly. Headers must arrive with length 7, and collecting
  the response must fail rather than complete successfully.
- Controls: Content-Length 4 with frames `12`, `34`, and Content-Length 3 with
  frames `1`, `23`. Both must return their complete original bytes.
- Before the runtime fix, the regression failed: Hyper panicked with
  `payload claims content-length of 4, custom content-length header claims 7`;
  the dialer failed before receiving headers (`connection closed before message
  completed`). Exit status 101.
- With the fix, all three `guarded_body` tests passed, including existing cap,
  abort, and transmitted-notification checks.
- Mutation: replace the checked subtraction/error with saturating subtraction
  (allow every frame through). The same HTTP regression failed because the
  oversized response completed successfully. Exit status 101. Restored with
  `git checkout -- crates/nanovm/src/object.rs` from the staged fixed version;
  `git diff --exit-code -- crates/nanovm/src/object.rs` confirmed no mutation
  remained.
- The fixed unit-test executable's complete serial suite passed: 193 passed,
  one ignored, no failures (59.88 seconds).
- After restoring the mutation, `cargo +1.99.1 test --locked -p nanovm --lib`
  with `RUST_TEST_THREADS=1` rebuilt the final source and passed again: 193 passed,
  one ignored, no failures (57.10 seconds). Production
  `cargo +1.99.1 check --locked -p nanovm --bin nanovm` also passed.
- `cargo +1.99.1 doc --locked -p nanovm --no-deps` passed with 11 warnings
  in unchanged documentation (private/broken links and a bare URL).
- Strict `cargo +1.99.1 clippy --locked -p nanovm --all-targets --all-features
  -- -D warnings` failed on ten diagnostics in unchanged lines: `journal.rs`
  (`some_filter`), `object.rs`'s existing nested reset check, `ship.rs`
  (`needless_question_mark`), `supervisor.rs` (two large error returns and an
  unnecessary cast), `table.rs` (two large error returns and `ok_expect`),
  and `telemetry.rs` (`unit_arg`). No unrelated lint cleanup was made.
- `cargo +1.99.1 fmt --all -- --check` failed on broad formatting drift;
  the branch is not globally rustfmt-clean. Unrelated formatting was left alone.

The existing cap unit test now checks the same error text instead of the
removed library wrapper's error type; the response-body integration contract's
`length limit exceeded` message is unchanged.

Commands use `ulimit -n 20000`, unset `RUST_BACKTRACE`, Rust `+1.99.1`,
`--locked`, an independent worktree target and one build job (the initial
dependency build used two). Dependency artifacts were hardlink-seeded, local
package fingerprints/outputs removed, hardlinked Cargo locks unlinked, and
inactive inherited incremental caches removed under disk pressure. Nothing in
the parent target was edited.

This is real HTTP worker-link coverage, not a jailed Wasm fixture or a release
build test. No throughput claim: concurrent agents were compiling in the same
orb, so an idle-machine alternating CPU/request benchmark was not available.

## Local PR3 integration (2026-11-16)

Merged the committed local `fix/pr3-review` at
`9965f4678cf868517d2bb34671c3ab5b36deddb7`, not `origin/main` and not the
parent's uncommitted follow-ups. Preserved both histories and the removals of
superseded experiments. Framing is N-960 and the pre-existing lint/format
limitations are N-961; PR3's N-930–N-938 entries remain intact.

The gate conflict is resolved by moving the parent's socket-lifetime idea into
the shared `server::serve_conn`: a generic I/O wrapper owns the admission guard
and drops the socket first. Both public TCP gate guards and worker-link
semaphore permits now follow upgraded I/O. Keeping a guard only in the HTTP
service releases it when Hyper transfers the socket to an upgrade consumer.
The parent gate regression is retained; a real Unix worker-link upgrade proof
also checks live byte transfer, held admission and release when I/O drops.
The parent's returned connection JoinHandle is preserved so the real disconnect
test can await cancellation of an abandoned DNS lookup.

The proxy cache retains the parent's abandoned-initializer reclamation and
last-found-answer preservation, including active-waiter capacity tests. Cargo
keeps `memchr` because secrets masking uses it, keeps the base64 0.23 cleanup
used by WebSockets and registry authentication, and does not restore governor
or request-rate admission.

Before the shared I/O fix, both upgrade tests failed: gate admission was
`(counted, readmitted, free) = (0, true, 1)` rather than `(1, false, 0)`;
worker-link free permits were 1 rather than 0 while upgraded I/O remained live.
With the shared wrapper, both upgrade regressions passed. The same fixed
executable also passed all three `guarded_body` tests.
Mutation-check: put `()` in the I/O wrapper and capture the admission guard in
the HTTP service instead. Both upgrade regressions failed again with exactly
the early-release observations above. Restored `server.rs` from the staged
fixed version and confirmed `git diff --exit-code -- crates/nanovm/src/server.rs`
was empty before rebuilding the serial library suite.

Final integrated checks (Rust 1.99.1, one build job, fd limit 20,000,
`RUST_BACKTRACE` unset):

- Serial `cargo test --locked -p nanovm --lib`: **206 passed, one ignored**,
  no failures, 61.23 seconds after the mutation was restored. This includes
  both upgrade proofs, response framing, all retained PR3 library proofs and
  the ordinary HTTP/link controls.
- Focused `cancelled_` rerun of the final executable: **four passed** in
  5.17 seconds (three proxy cases and a journal cancellation control).
- Production `cargo check --locked -p nanovm --bin nanovm`: passed.
- Diff checks: no conflict markers/unmerged paths or whitespace errors;
  shipping, secrets, telemetry and systemd service match the committed PR3
  parent; `object.rs` matches the tested framing commit; no deleted experiments
  differ from the PR3 parent. The only quota difference from PR3 is the trim's
  empty-response control in its telemetry test.

CI runtime-line count: **8,590**, **+100** versus first parent `da7b733`
(8,490) and **-231** versus second parent `9965f46` (8,821). These deltas
include the imported review fixes and retained trims, not just the shared I/O
wrapper. Aggregate integration/Wasm suites remain the parent's responsibility;
no new release-mode or idle-machine performance claim is made.

After every build/test process finished, verified that no lock referenced this
worktree's target and removed only `/home/user/workspace/nanovm-trim-edge/target`.
The disposable target was 26 GiB including shared hardlinks; filesystem free
space rose from 8.4 GiB to 25 GiB. Parent target and tracked source stayed intact.
