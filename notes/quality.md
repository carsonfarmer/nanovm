# Quality notes (quality agent, round 3)

Test and verification work from IDEAS.md sections D and G. Each item is on its own branch from
`cf/main-next`; this file collects the notes. Commands run from the repository root.

## D-55 / D-56: coverage and mutation testing (`ideas/mutants`)

**Coverage (D-55): not run.** `cargo-llvm-cov` needs the `llvm-tools` component and an
instrumented build of nanovm and all its dependencies, about 4 GB of target directory; the shared
disk was at 2.5-3.4 GB free. Procedure for a machine with room:
`rustup component add llvm-tools-preview && cargo install --locked cargo-llvm-cov`, then
`ulimit -n 20000; RUST_TEST_THREADS=1 cargo llvm-cov --locked -p nanovm --html` (build the guests
first) and read `target/llvm-cov/html` for `journal.rs`, `quota.rs` and `proxy.rs`.

**Mutation testing (D-56): partial.** `cargo-mutants` 27.1 lists 121 mutants in `ws.rs`, 53 in
`registry.rs`, 118 in `egress.rs` and 4 in `jail.rs`. Each needs a rebuild of nanovm's test target
(2-4 minutes here) and a 540 MB test binary with incremental state, so a full run (about 10 hours)
didn't fit the time or the disk. Seven `registry.rs` mutants ran with `--in-place` from a source
copy and unit tests only, but the run's verdicts proved unreliable (two "missed" mutants are
killed by existing tests when applied by hand: some test phases reported 0 s). So the mutants that
matter were applied by hand, one at a time, and the module's unit tests run:

| Mutant | Existing tests | New test |
| --- | --- | --- |
| `registry.rs`: the reference cache keeps entries over 1 KiB (`<=` → `>`) | survive | `the_reference_cache_keeps_apps_apart_and_small` (16 long references leave < 4.8 KB held) kills it |
| `registry.rs`: the cache answers with a different app's reference (`==` → `!=`) | killed (`app_must_be_a_reference_with_a_digest`) | also kills it |
| `registry.rs`: trust pairs refused (match arm deleted) | killed (4 tests) | — |
| `registry.rs`: `MAX_METADATA` 4 MiB → 0 (`<<` → `>>`) | survives unit tests | not added: only reachable through a registry (integration tests download manifests; not run per mutant) |
| `jail.rs`: the W^X mask becomes 0 (`\|` → `&`): writable and executable memory allowed | survives everything (no test checked W^X) | `the_worker_filter_refuses_what_it_should` kills it |

`the_worker_filter_refuses_what_it_should` re-runs the test binary as a child that installs the
worker's seccomp filter and checks it directly: W+X `mmap` and `mprotect`, inet sockets, datagram
socketpairs, `clone` without `CLONE_THREAD` and `F_SETOWN` are refused with ENOSYS; read-write and
read-exec mappings, stream socketpairs and `F_GETFL` are allowed. Before it, only the integration
tests' `Seccomp: 2` check touched the filter. `counting::held` (test-only) measures what a closure
leaves allocated.

Run: `cargo test --locked -p nanovm --lib -- jail:: registry::`.
