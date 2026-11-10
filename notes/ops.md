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

## L-111 systemd units (`ideas/systemd-units`)

**What:** `deploy/systemd/`: hardened `nanovm.service` and `nanovm-proxy.service`,
example `nanovm.toml`/`proxy.toml`, a sysctl drop-in for `vm.max_map_count`, an
optional cgroup v2 drop-in, and a container test that boots systemd.

**How it was tested.** systemd is not PID 1 here, so `test/nspawn.sh` boots this
host's own Ubuntu 24.04 (systemd 255) in `systemd-nspawn` (its `/usr` under a
throwaway overlay, a copy of `/etc`, private network) and runs
`test/in-container.sh`: install the files, start the node, drive the counter and
hostile apps from the loopback dev registry, restart, start the proxy, stop both.
Result: plain 16/16 checks and cgroup mode 19/19 pass (`RESULT fail=0` twice),
and, after the base moved to `3dafe33`, a third mode with `[capture]` on (17/17:
three versions published under `~committed/` through the unit's syscall filter),
`systemd-analyze verify` exit 0, `shellcheck -S warning` clean.
`systemd-analyze security`: node 2.6 (2.8 with the cgroup drop-in), proxy 1.1,
bare unit 9.6.

**Findings, each from a failing run before the fix:**
- `NoNewPrivileges=yes` breaks per-app uids under systemd 255: the supervisor's
  permitted set lost `CAP_SETUID` (CapPrm 0x61 vs 0xe1, bisected over every
  option), so workers failed `setresuid(200001)`. Left off; workers still end with
  `NoNewPrivs: 1` (checked) because Landlock sets it.
- `SystemCallFilter=@system-service` must add `seccomp` (worker: `seccomp: …
  Operation not permitted`) and the three `landlock_*` calls (worker killed by
  SIGSYS without them).
- Capabilities: removing `CAP_CHOWN`, `CAP_SETUID` or `CAP_SETGID` each breaks
  every worker start (errors in the README table). Without `CAP_KILL` normal
  version switches still work, because a worker exits when the supervisor hangs
  up, but `kill -0` of a worker from a unit without it fails while with it
  succeeds: the drain-deadline SIGKILL needs it.
- cgroup drop-in: `ProtectControlGroups=no` is needed (`creating cgroup …: Read-only
  file system`); `ReadWritePaths` turned out unnecessary and was removed. With
  `DelegateSubgroup=supervisor` the supervisor sits in `supervisor/`, each worker
  in `apps/<app>`, the service group itself empty (checked). A `[worker]` limit
  whose controller is missing fails that worker's start, naming the file, with no
  stray process.
- `ProcSubset=pid` hides `/proc/sys/vm/max_map_count` (`cat`: No such file),
  which makes the worker's preflight skip its mappings check, so the node unit
  doesn't use it.
- The node refuses to start with no CA certificates at all (`No CA certificates
  were loaded from the system`): images need `ca-certificates`.
- N-550: README understated mappings per object threefold (8 vs the code's 24);
  fixed in README.

**Not verified:** controllers actually enforced under the drop-in (this host gives
memory/cpu/pids to cgroup v1, so the container's v2 hierarchy had none; the
supervisor/apps layout and writes were exercised with zero limits), the sysctl
applied inside the container (read-only `/proc/sys`; `sysctl -p` of the file on
the host works), real DNS/TLS, other distributions or systemd versions. nspawn
needed `--system-call-filter` for Landlock, since its own filter lacks it.

Reproduce:
```sh
cargo build -p nanovm --features dev --bin nanovm --example dev-registry
sudo apt-get install -y systemd-container
sudo deploy/systemd/test/nspawn.sh
```

## L-113/L-114 single-node reference deployment (`ideas/single-node`, on `ideas/systemd-units`)

**What:** `deploy/single-node/`: a guide (Caddy TLS → proxy → node → workers,
Collector on its own cores), `Caddyfile`, CPU/memory drop-ins for nanovm and
`otelcol-contrib`, `install.sh` (idempotent, refuses to swap the binary under a
running node, keeps configs), `check-host.py` (per-worker fds and mappings from
`engine::preflight`'s formulas, Landlock ABI, root, data_dir searchable by app
uids, filesystem and mount options, cgroup controllers, CA bundle,
RUST_BACKTRACE; also covers IDEAS C-44's "doctor" checks), and `capacity.json`,
inputs for the nice-to-haves capacity calculator (measured values cited from
README). `deploy/systemd/test/nspawn.sh` now takes `SCRIPT=` and `MODES=`.

**Evidence:** `test/e2e.sh` in the nspawn container, 8/8: install, three writes
and a read over Caddy's verified internal-CA TLS through proxy (DoH stand-in,
dev registry tag) to a jailed worker; client `nanovm-app` header ignored; 404 for
a host without a record; 8080/8444 on 127.0.0.1 only; spans from all three layers
at the Collector port; data survives a node restart. `caddy validate` (2.6.2) and
`systemd-analyze verify` pass. check-host: 5 tests; on this host it flags the
scratch directory's 0700 ancestors (the failure the fs matrix first hit:
`… is not accessible to the worker: Permission denied`). The nice-to-haves
calculator runs on `capacity.json` (17,100/s application ceiling, 40,800/s
Collector, ~69,000 resident objects in 6 GiB, CPU for Caddy/proxy excluded).

**Not run:** otelcol-contrib (no binary), real ACME/DNS, signed apps, AllowedCPUs
enforcement (cgroup v1 here). No Terraform/Ansible: a tested shell installer.

**After the base moved to `3dafe33` (W-9 local capture):** check-host also fails
a root whose objects have capture history while `[capture]` is off (they would
refuse to start) and reports capture's storage when on (6 unit tests); the
README's disk sizing counts capture. `test/e2e.sh` passes 8/8 again on a dev
build of `3dafe33`. `deploy/systemd/test/nspawn.sh` keeps `SCRIPT=`/`MODES=` and
gains the systemd branch's capture mode.
