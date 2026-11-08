# Ops notes

Operations and packaging work from IDEAS.md (sections E, F, L, M), one branch per
item, each from `cf/main-next`. Plain language; numbers and how to reproduce.

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
