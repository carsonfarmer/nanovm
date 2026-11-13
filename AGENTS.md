# AGENTS.md

Instructions for every agent working on nanovm. Read this first. [HANDOFF.md](HANDOFF.md) has the
history and owner decisions, [PLAN.md](PLAN.md) the design, [ISSUES.md](ISSUES.md) the shared list of
known problems and work.

## Rule 1: never write compatibility code

**There is exactly one version of nanovm. It is called v1. We only move forward.**

- Never write code that reads, converts, migrates, detects, tolerates or names an older version,
  format, schema, config key, API or on-disk layout. That includes:
  - readers or adapters for an earlier file or wire format;
  - upgrade or migration steps, adoption of state an earlier build left behind;
  - fallbacks, aliases, deprecated config keys, shims, version branches;
  - error messages or checks that recognise "an older build" specifically.
- Every format, schema or protocol version constant is `1` / `v1`. When a format changes, change it
  in place and keep the version at v1. Data written by earlier builds is simply not supported:
  operators start from a fresh data directory.
- Generic validation stays: data that doesn't parse as v1 is invalid and refused, without
  mentioning or special-casing anything older.
- If a reviewer, test or issue asks for a migration or compatibility path, the answer is "not
  supported, see AGENTS.md rule 1". Record it in ISSUES.md; don't build it.
- This applies to every file: runtime, components, SDK, JavaScript runtime, tools, tests, docs.

## What nanovm is

A wasmtime-based host that runs untrusted WASIp3 HTTP components as durable objects.

- **Objects:** one live instance per `(app, object)`, serving one request at a time, with
  per-object limits on CPU, memory, deadlines, handles, disk and files.
- **Storage:** each object has a private `/data` volume whose changes commit atomically per request
  through an undo journal. KV and SQLite are Wasm components composed over that filesystem.
- **Apps and routing:** apps are signed OCI artifacts (Sigstore). A proxy routes hosts via DNS TXT
  records.
- **Isolation:** `nanovm serve` is a supervisor that runs no guest code. It runs one jailed worker
  per app (uid, Landlock, seccomp, cgroup v2).

## Repository layout

- `crates/nanovm/src`: the runtime. Start with `object.rs` (actor and limits), `journal.rs` and
  `quota.rs` (storage), `supervisor.rs` and `jail.rs` (workers), `proxy.rs`, `registry.rs` (our own
  hardened OCI client), `egress.rs`, `link.rs`.
- `crates/nanovm/tests`: integration tests (`integration.rs`, `dev.rs`, `js.rs`, `examples.rs`).
- `guests/`: test guests (`counter`, `hostile`). `components/`: KV and SQLite providers.
- `sdk/rust`, `js/`, `tools/build-app.sh`, `examples/`: the developer surface.
- `notes/`: evidence for past work (method, numbers, how to reproduce).

## Build and test

- **Toolchain:** the Rust version CI uses (`rustup update stable`) with the `wasm32-wasip2`
  target, `wac` 0.12 (`cargo install wac-cli --version 0.12.0 --locked`), and WASI SDK 34 for
  SQLite (`WASI_SDK_PATH`).
- **Host setup:** `sudo sysctl -w vm.max_map_count=1048576` (it resets on restart); `ulimit -n 20000`
  or higher. Never set `RUST_BACKTRACE` (wasmtime captures a backtrace per guest error, about 3×
  slower).
- **Guests:** `bash scripts/build-guests.sh`. SQLite fixtures:
  `bash experiments/sqlite-filesystem/build.sh --compose`. Example and JS apps: `tools/build-app.sh`
  (see `tests/dev.rs`, `tests/examples.rs`, `tests/js.rs`).
- **Full suite:** `env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo test --locked -p nanovm`.
  - SQLite durability suite: add `--test integration sqlite_ -- --ignored`.
  - Production check: `cargo check --locked -p nanovm --bin nanovm`.
  - `.github/workflows/ci.yml` is the reference for what must pass.
- **Timing tests:** they can fail under heavy machine load. Rerun alone before believing a failure,
  but never accept a flaky test as "rerun-only": find the cause.

## How changes are made

- **Root causes, not band-aids.** Design from the problem, not from the code we have. Look for an
  existing library before building something substantial, and say what you found. A security
  boundary can justify owning a small piece of code; for example `registry::Oci` exists because
  general OCI clients can't bound reads or refuse private addresses.
- **Every fix has a test that fails without it.** Show both runs. Every limit is proven by a hostile
  test. Mutation-check new tests: break the mechanism and confirm a test fails, then restore with
  `git checkout -- <file>` and confirm `git diff` is empty.
- **Minimal runtime lines.** CI reports the count (non-blank, non-comment lines in
  `crates/nanovm/src` before test modules). Say the delta in every commit. Test lines don't count.
  Remove unnecessary defense in depth and repeated logic.
- **Benchmarks must not regress.** Measure CPU per request from `/proc` and alternate old/new
  builds for at least 3 rounds on an idle machine; report medians and ranges. Say exactly what was
  measured, e.g. sync syscalls, not device flushes.
- **Commits:** one coherent, buildable change each. The message gives problem, change, evidence
  (numbers and method), test, and runtime-line delta.
- **Docs move with the code**, in the same push: README (what exists), PLAN (design), HANDOFF (state
  and decisions), ISSUES (status), `notes/` (evidence). Plain language, short.
- **ISSUES.md is the shared list.** Check it before starting, mark what you take, record what you find
  (even if you don't fix it), and mark it fixed with the commit.
- **Outward-facing actions need the owner's OK:** publishing packages, new workflows that publish,
  anything on remote hosts, opening or merging pull requests unless asked.

## Git and working alongside other agents

- Work on your own branch, in your own worktree. Never push to `main` or to another agent's
  branch. Never force-push, never rewrite pushed history.
- **Never `git stash`.** The stash is shared by every worktree of the repo.
- **Own files, not features.** Whoever is rewriting a file owns it until it lands. Record findings in
  someone else's files in ISSUES.md for them.
- **Commit and push often.** Containers restart without warning.
- **Separate `target/` per worktree.** If you seed one by hardlinking another worktree's `target/`,
  delete `target/debug/.fingerprint/nanovm-*` and the test binaries
  (`target/debug/deps/{integration,dev,js,examples}-*`) first. Otherwise cargo reuses binaries
  that carry the other checkout's paths, and your tests run someone else's code.
- `pkill -f <pattern>` can kill your own shell; use `pkill -x nanovm` or pid files.

## Working with the owner

- Keep replies brief and plain. Ask few questions at a time, numbered, each with a recommended
  answer.
- Don't agree to be agreeable: if the owner is likely wrong, say so with the evidence.
- Report outcomes faithfully: what passed, what failed, and what wasn't verified.
