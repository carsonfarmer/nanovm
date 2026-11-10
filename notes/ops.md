# Ops notes

Operations and packaging work from IDEAS.md (sections E, F, L, M), one branch per
item, each from `cf/main-next`. Plain language; numbers and how to reproduce.

## N-552 workers from the supervisor's own image (`ideas/worker-exe`)

**Problem (found writing the upgrade runbook):** the supervisor spawned workers
from the path it started with. A binary replaced under it (any `install`, `mv`
or package upgrade before the restart) gave new apps workers of the new version
under the old supervisor.

**Change:** `main.rs` passes `/proc/self/exe` (Linux) as the worker executable;
in the forked child that names the supervisor's own image, even after the file is
replaced or deleted. `supervisor.rs` sets argv[0] to `nanovm`, so `ps` and
`pgrep -f 'nanovm worker'` read as before. Runtime lines 5,419 → 5,420 (on `3dafe33`).

**Evidence:** `docs/ops/drills/binary-swap.sh` (on `ideas/runbooks`): old =
`cf/main-next`, new = the N-551 build: a new app gets `502 worker-down` (`worker
spec: invalid length 3, expected a tuple of size 4`); old = this branch, same new
binary: the new app answers `{"object":"h","hits":1}`. The hardened systemd units'
container test passes with this binary (plain and cgroup, `ProtectProc=invisible`
included). Repeated on `3dafe33`: base build swapped for the N-551 build fails the
new app the same way; this branch's build serves it. Unit tests 109/109; the 10
integration tests named "worker" pass; of the full integration run at load ~20,
two CPU-budget tests failed and pass alone, and `kv_counts_against_the_disk_quota`
fails alone exactly as on the plain base.
