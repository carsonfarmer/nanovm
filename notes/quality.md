# Quality notes (quality agent, round 3)

Test and verification work from IDEAS.md sections D and G. Each item is on its own branch from
`cf/main-next`; this file collects the notes. Commands run from the repository root.

## D-50: seccomp drift check (`ideas/seccomp-drift`)

**What it is.** `scripts/seccomp-drift.py` builds the integration tests, runs the 17 that start
jailed workers under `strace -f`, and keeps only the syscalls that worker threads made after the
filter applied to them: it rebuilds the process tree from `clone`/`clone3`/`vfork` results, finds
`execve(".../nanovm", [..., "worker"])`, and starts counting a thread at its own `seccomp(...) = 0`
or at its birth from a jailed thread. It parses the allowlist out of `jail.rs` (`seccomp_filter`,
honouring the `x86_64`-only block and noting which syscalls have argument rules) and reports:

- **drift**: jailed calls the filter refused (`ENOSYS`), split into "not allowlisted" and
  "arguments outside its rule" (e.g. an `fcntl` command), with examples. `clone3` is the one
  expected denial (glibc falls back to `clone`, as `jail.rs` says).
- allowlisted syscalls never seen (to review, not an error);
- two consistency checks that make the run fail (exit 2): a refused syscall that *succeeded* in a
  jailed thread, or a worker thread running unjailed after its process entered the jail (the filter
  is applied per thread, without TSYNC, so a thread started before `Jail::enter` would escape it).

`.github/workflows/seccomp-drift.yml` runs it nightly and on demand with `--fail-on-drift`, writes
the report to the job summary, emits one `::warning` per drifting syscall and uploads the JSON.
`scripts/test_seccomp_drift.py` tests the parser on a synthetic trace (harness and non-worker
processes ignored, pre-jail calls ignored, resumed calls, argument denials, a pre-jail thread).

**Result at `cf/main-next` (0ac2849), x86_64, kernel 6.18.** 17 tests passed under strace in 52 s
(2 m 11 s with the build). 32 worker processes, 338 threads, 324,508 jailed syscalls, 56 distinct.
**No drift**; `clone3` refused 32 times (once per worker). Every worker thread was jailed.
Allowlisted but not seen (22): `access brk clock_getres clock_gettime epoll_pwait exit_group
faccessat2 getcwd membarrier mremap newfstatat preadv prlimit64 pwritev readlinkat readv renameat2
rmdir sendto set_tid_address tgkill utimensat`. Most are deliberate (`wasi:filesystem` calls the
tests don't make, `getcwd` for a relative `data_dir`, `membarrier` on aarch64, `tgkill` for abort,
vDSO clocks); none is a reason to change the filter without a closer look.

**Does it catch drift?** Mutation: deleting `SYS_sched_yield` from the allowlist and re-running two
tests reports `sched_yield: 564 times, not allowlisted` and exits 1. Both tests still *passed*:
tokio treats the refused yield as harmless, which is exactly the silent kind of drift a dependency
upgrade could introduce.

Run: `python3 scripts/seccomp-drift.py [--json out.json] [--fail-on-drift] [test filters…]`;
`python3 -m unittest discover -s scripts -p test_seccomp_drift.py`. Needs `strace` and the guests
built (`scripts/build-guests.sh`). The trace is ~125 MB, in a temporary file deleted afterwards
(`--keep FILE` keeps it).
