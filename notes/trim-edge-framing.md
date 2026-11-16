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
