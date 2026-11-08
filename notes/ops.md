# Ops notes

Operations and packaging work from IDEAS.md (sections E, F, L, M), one branch per
item, each from `cf/main-next`. Plain language; numbers and how to reproduce.

## L-110 container image (`ideas/container-image`)

**What:** `deploy/container/`: a multi-stage Dockerfile (release build in
`rust:1.99.1-slim-trixie`, binary + CA bundle + configs on `debian:trixie-slim`,
no `RUN` in the runtime stage), a `Dockerfile.dockerignore` so the context is only
`Cargo.toml`, `Cargo.lock`, `crates/nanovm` and the two configs, documented
`docker run` flags, and `test/local.sh`.

**Not built as written:** pulling base images was not approved here, so neither
stage ran with its real base. Checked instead:
- Context completeness: those files alone pass `cargo metadata --locked --offline`
  (lockfile consistent, all declared targets present). A release build from them
  was not run (it would rebuild every dependency: a debug attempt was stopped
  after 15 minutes at load average ~15).
- Runtime stage and flags: `test/local.sh` builds the real Dockerfile with BuildKit
  `--build-context build=<dir>` standing in for the build stage (a locally built
  dev binary and this host's CA bundle) and `RUNTIME_IMAGE` set to a stand-in
  imported from this host's glibc and busybox. Docker 29.9.2, default seccomp
  profile, cgroup v1, `--cap-drop ALL` plus the four capabilities,
  `--security-opt no-new-privileges`, `nofile` 20000 (this sandbox's hard cap).
  11/11 checks pass: workers as uid 200000 with seccomp and `no_new_privs`,
  hostile spin (500) and escapes (403) stopped, data survives a new container on
  the volume, proxy works as nobody on a read-only root, `docker stop` 120 ms
  with `--init`, and workers start with the supervisor as PID 1.

**Findings:**
- N-551: as PID 1 (no `--init`, or Kubernetes), every worker refused to start
  (`entering the jail: supervisor exited`): the worker read "parent is pid 1" as
  "supervisor died". Fixed in the first commit (compare with the supervisor's pid;
  +2 runtime lines); the PID-1 check fails before and passes after.
- As PID 1 the supervisor ignores SIGTERM, so `docker stop -t 3` took 3,151 ms (the
  full timeout, then SIGKILL); with `--init`, 120 ms. Recommend `--init`.
- `vm.max_map_count` is not namespaced: Docker's `--sysctl` cannot set it; set it
  on the host.
- Docker mounts `/sys/fs/cgroup` read-only: per-app `cgroup` limits don't work in
  an unprivileged container; use `--memory/--cpus/--pids-limit` for the node.
- Docker's `no-new-privileges` keeps `CAP_SETUID` (unlike systemd's; see the
  systemd notes), and the node works under it.
- The node needs a CA bundle at all, else it refuses to start.

**Tests run:** full suite with the N-551 change: 103 unit pass; integration 67
passed, 3 failed at load average ~16 with CPU-budget errors.
`churn_past_capacity_never_fails` and
`the_proxy_outlasts_idle_and_excess_connections` pass alone;
`kv_counts_against_the_disk_quota` (in-process, never enters the jail) still hit
"used more than 100ms of CPU" alone at that load. `jail_blocks_escapes` passes.

Reproduce:
```sh
cargo build -p nanovm --features dev --bin nanovm --example dev-registry
dockerd …   # any running daemon; DOCKER_HOST pointing at it
deploy/container/test/local.sh       # NOFILE=20000 where the hard limit is lower
```
