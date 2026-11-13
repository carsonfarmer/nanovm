# Container image

[Dockerfile](Dockerfile) builds a release `nanovm` (no `dev` feature) in a Rust image
and copies it, with the CA bundle and [nanovm.toml](nanovm.toml)/[proxy.toml](proxy.toml),
onto `debian:trixie-slim`. The same image runs the node (default) and the proxy.

```sh
docker build -f deploy/container/Dockerfile -t nanovm .
```

Pin both base images by digest for reproducible builds.

## Run

```sh
# On the host: vm.max_map_count is not namespaced, so `--sysctl` cannot set it.
sudo sysctl -w vm.max_map_count=1048576       # and persist it: deploy/systemd/90-nanovm.conf

docker run -d --name nanovm-node --init --security-opt no-new-privileges \
  --ulimit nofile=1048576:1048576 \
  --cap-drop ALL --cap-add CHOWN --cap-add SETUID --cap-add SETGID --cap-add KILL \
  -v nanovm-data:/var/lib/nanovm \
  -p 127.0.0.1:8443:8443 \
  nanovm

docker run -d --name nanovm-proxy --init --network container:nanovm-node \
  --security-opt no-new-privileges --cap-drop ALL --user 65534:65534 --read-only \
  nanovm proxy --config /etc/nanovm/proxy.toml
```

| Flag | Why |
|---|---|
| host `vm.max_map_count` | each worker needs `24 × (max_live_objects + 32) + 4096` mappings and refuses to start below it |
| `--ulimit nofile=…` | each worker needs about `2 × (max_live_objects + 32) + 1024` fds; the supervisor caps worker links at a quarter of its limit |
| `--cap-drop ALL --cap-add CHOWN SETUID SETGID KILL` | the supervisor runs as root to give each app its own uid (see [deploy/systemd](../systemd/README.md) for what each is for). Docker's defaults include more; these four are enough |
| `--security-opt no-new-privileges` | works here, unlike systemd's `NoNewPrivileges=yes` (which drops `CAP_SETUID`): Docker keeps the bounding set's capabilities |
| `--init` | without it the supervisor is PID 1 and ignores SIGTERM, so `docker stop` waits out its timeout and SIGKILLs. Stopping is never graceful: drain at the TLS terminator first |
| `-v …:/var/lib/nanovm` | volumes, undo journals, registry and compile caches. Back it up with `scripts/backup.py` while the node is stopped |
| `--network container:nanovm-node` | the proxy shares the node's loopback: the node (8080) and publish (8444) listeners stay unreachable from anywhere else. Publish only 8443, on the node container, behind TLS |
| proxy `--user 65534 --read-only --cap-drop ALL` | the proxy needs no privilege and writes nothing |

Mount your own config over `/etc/nanovm/nanovm.toml` (read-only) to change limits,
`telemetry_endpoint` (a Collector in the same network namespace can use the
default `127.0.0.1:4318`), or `max_live_objects`. Do not set `RUST_BACKTRACE`.

**Not in a container: per-app cgroups.** Docker mounts `/sys/fs/cgroup`
read-only in an unprivileged container, so leave `cgroup` unset and bound the
whole node with `--memory`, `--cpus` and `--pids-limit`. Per-app worker limits
need the host units ([deploy/systemd](../systemd/README.md)) or a privileged
container. Don't use `--privileged` for anything else.

## What was validated, and what wasn't

Building needs two base images from Docker Hub, which this sandbox may not pull,
so **the image as written has not been built here**. Checked instead:

- The build stage's context: `Cargo.toml`, `Cargo.lock` and `crates/nanovm`
  alone (what the Dockerfile copies) pass `cargo metadata --locked --offline`.
  A full release build from them was not run.
- The runtime stage and the run flags, with [test/local.sh](test/local.sh): the
  real Dockerfile, with its build stage replaced by a locally built binary
  (BuildKit `--build-context build=…`) and `debian:trixie-slim` by a stand-in
  imported from this host's glibc and busybox. With the flags above (except
  `nofile`, which this sandbox caps at 20000), Docker 29.9.2's default seccomp
  profile and cgroup v1: workers run as uid 200000 with seccomp and
  `no_new_privs`, the hostile guest's spin and escapes are stopped, data survives
  a new container on the same volume, the proxy answers as nobody on a
  read-only root, and `docker stop` with `--init` takes about 0.1 s (3.2 s,
  the full `-t 3`, as PID 1). Docker mounts `/sys/fs/cgroup` read-only
  (seen on cgroup v1 here).
- Running as PID 1 (no `--init`, or a Kubernetes pod) failed every worker with
  `entering the jail: supervisor exited`: the worker took "parent is pid 1" to
  mean its supervisor had died. Fixed in this branch (the worker now compares
  its parent with the supervisor's pid); the same test passes as PID 1 after it.

Not validated: the real base images and tags, image size (the stand-in image
was 66 MB with a debug binary), arm64, Podman, rootless Docker, user namespaces,
cgroup v2, Kubernetes.
