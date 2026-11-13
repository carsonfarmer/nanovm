# systemd units

Hardened units for a node (`nanovm serve`) and the proxy (`nanovm proxy`) on one
Linux host, with a sysctl drop-in and an optional cgroup v2 drop-in.

| File | Install as |
|---|---|
| [nanovm.service](nanovm.service) | `/etc/systemd/system/nanovm.service` |
| [nanovm-proxy.service](nanovm-proxy.service) | `/etc/systemd/system/nanovm-proxy.service` |
| [nanovm.toml](nanovm.toml), [proxy.toml](proxy.toml) | `/etc/nanovm/` |
| [90-nanovm.conf](90-nanovm.conf) | `/etc/sysctl.d/90-nanovm.conf` |
| [nanovm-cgroup.conf](nanovm-cgroup.conf) (optional) | `/etc/systemd/system/nanovm.service.d/cgroup.conf` |
| [nanovm-store.conf](nanovm-store.conf) (with a capture `store`) | `/etc/systemd/system/nanovm.service.d/store.conf` |

```sh
cargo build --release --locked -p nanovm --bin nanovm
sudo install -m755 target/release/nanovm /usr/local/bin/nanovm
sudo install -m644 deploy/systemd/*.service /etc/systemd/system/
sudo install -d /etc/nanovm && sudo install -m644 deploy/systemd/{nanovm,proxy}.toml /etc/nanovm/
sudo install -m644 deploy/systemd/90-nanovm.conf /etc/sysctl.d/ && sudo sysctl --system
sudo systemctl daemon-reload && sudo systemctl enable --now nanovm nanovm-proxy
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/_nanovm/ready   # 204
```

The node needs the host's CA certificates (`ca-certificates`): it refuses to start
without any ("No CA certificates were loaded from the system").

## What the node keeps, and why

The supervisor runs as root because per-app uids depend on it (`geteuid() == 0`
in `supervisor::app_dirs`); without root every app shares one uid. Its bounding
set keeps four capabilities, each shown necessary in a container test by removing it:

| Capability | Used for | Without it |
|---|---|---|
| `CAP_CHOWN` | giving `objects/`, `tmp/`, `cache/<app>` to the app's uid | every worker start fails, `Operation not permitted` |
| `CAP_SETUID` | the worker's `setresuid` before its jail | `entering the jail: setresuid(200000) failed` |
| `CAP_SETGID` | the worker's `setgroups`/`setresgid` | `entering the jail: setgroups failed` |
| `CAP_KILL` | SIGKILL of a worker at its drain deadline (other uid) | `kill(2)` across uids fails; workers still exit when the supervisor hangs up, but a stuck one outlives its drain |

With a capture `store`, the [store drop-in](nanovm-store.conf) adds a fifth,
`CAP_DAC_OVERRIDE`: the supervisor records what it shipped in each object's
directory (`objects/<app>/<shard>/<object>/shipped`) and removes shipped
versions from its `committed/`, directories the app's uid owns. Without it the
supervisor ships each object's first version and then nothing, silently
(B-604). It adds little: `CAP_SETUID` already lets the supervisor act as any
app uid. The drop-in also makes `/srv/nanovm-store` writable for a directory
store and reads an object store's `AWS_*` credentials from
`/etc/nanovm/store.env`.

Deliberately **not** set on the node, each for a reason seen in testing:

- `NoNewPrivileges=yes`: under systemd 255 it left the supervisor without
  `CAP_SETUID` in its permitted set (0x61 instead of 0xe1), so every worker failed
  `setresuid`. Each worker sets `no_new_privs` itself (Landlock) before any guest
  code; the test checks it.
- `MemoryDenyWriteExecute=yes`: wasmtime makes compiled code executable with
  `mprotect`. The worker's seccomp filter already refuses W+X memory.
- `UMask=0077`: workers run as other uids and must traverse `data_dir/objects/`.
- `ProtectControlGroups=yes` with the cgroup drop-in: creating `apps/worker-<n>` fails
  with `EROFS`.
- `ProcSubset=pid`: it hides `/proc/sys/vm/max_map_count`, and the worker's
  preflight then silently skips its mappings check.

`SystemCallFilter=@system-service` needs `seccomp` and the three `landlock_*`
calls added: workers inherit the filter and then build their own jail. The
proxy needs no privilege at all (`DynamicUser=yes`, empty bounding set,
`MemoryDenyWriteExecute=yes`). `systemd-analyze security` rates the node 2.6
(2.8 with the cgroup drop-in) and the proxy 1.1, against 9.6 for a bare unit.

**Stopping is not graceful.** SIGTERM ends the supervisor; workers die with it
(`PR_SET_PDEATHSIG`) and in-flight requests roll back, their clients unsure
whether they committed. Drain at the TLS terminator or load balancer first.
`RUST_BACKTRACE` is unset on purpose (README, Host requirements).

## cgroup v2 limits per app

Install the drop-in and set `cgroup = "/sys/fs/cgroup/system.slice/nanovm.service/apps"`
in `nanovm.toml`. `DelegateSubgroup=supervisor` keeps the supervisor out of the
group that delegates controllers (cgroup v2 forbids processes there), and a
privileged `ExecStartPre` enables `cpu memory pids` for `apps/`. A controller the
host doesn't give the unit stays off; a `[worker]` limit that needs it then
fails that worker's start with `writing "…" to …/memory.max: Permission denied`.

## How these were tested

`test/nspawn.sh` boots this host's own Ubuntu 24.04 (systemd 255) in
`systemd-nspawn` and runs `test/in-container.sh` there four times: plain, with
the cgroup drop-in on a cgroup v2 hierarchy, with local capture on (its
version publication uses `link`, which `@system-service` allows), and with
capture shipped to a directory store through the store drop-in: install,
start, drive the counter and hostile apps from a loopback registry in dev mode,
check worker uid, seccomp and `no_new_privs`, the workers (one per app and the
spare the supervisor keeps ready), restart (data and uids survive), start the
proxy, stop both, and check nothing is left. On round 3 (`beta/deployable`, a
dev build) all four pass, 70 checks; the store mode fails without
`CAP_DAC_OVERRIDE` (one version shipped of three). `systemd-analyze verify`
passes on both units with the cgroup and CPU drop-ins.

Not tested here: controllers actually enforced under the drop-in (this host
gives memory, cpu and pids to cgroup v1, so the v2 hierarchy had none to hand
down; `scripts/vm-test.sh` covers nanovm's own cgroup code), applying the
sysctl inside the container (`/proc/sys` is read-only there; the host already
had 1048576), real DNS or TLS, and other distributions or systemd versions.
The container needed `--system-call-filter` for Landlock because nspawn's own
filter lacks it; a host systemd has no such filter.
