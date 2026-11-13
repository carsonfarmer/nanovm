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
| `-v …:/var/lib/nanovm` | volumes, undo journals, registry and compile caches. Its backup is the capture store (README, "Backup and restore"), shipped as the node runs |
| `--network container:nanovm-node` | the proxy shares the node's loopback: the node (8080) and publish (8444) listeners stay unreachable from anywhere else. Publish only 8443, on the node container, behind TLS |
| proxy `--user 65534 --read-only --cap-drop ALL` | the proxy needs no privilege and writes nothing |

Mount your own config over `/etc/nanovm/nanovm.toml` (read-only) to change limits,
`telemetry_endpoint` (a Collector in the same network namespace can use the
default `127.0.0.1:4318`), or `max_live_objects`. Do not set `RUST_BACKTRACE`.

**App secrets.** Set `secrets = "/etc/nanovm/secrets"` in that config and mount
a host directory only root may use (`chmod 700`) there, read-only: `-v
/etc/nanovm-secrets:/etc/nanovm/secrets:ro`. What goes in it: the
[single-node guide](../single-node/README.md#6-app-secrets), step 6.

**Not in a container: per-app cgroups.** Docker mounts `/sys/fs/cgroup`
read-only in an unprivileged container, so leave `cgroup` unset and bound the
whole node with `--memory`, `--cpus` and `--pids-limit`. Per-app worker limits
need the host units ([deploy/systemd](../systemd/README.md)) or a privileged
container. Don't use `--privileged` for anything else.

## What was validated, and what wasn't

On round 3 (`beta/deployable`), with Docker 29.9.2 (cgroup v1, its default
seccomp profile) and a dev build of that branch; method and output in
[notes/beta-deployable.md](../../notes/beta-deployable.md):

- The runtime stage and the run flags, with [test/local.sh](test/local.sh): the
  real Dockerfile, with its build stage replaced by a locally built binary
  (BuildKit `--build-context build=…`) and `debian:trixie-slim` by a stand-in
  imported from this host's glibc and busybox (for `wget` and `ps` in the
  checks). With the flags above (`nofile` at 20000, this sandbox's cap): 11/11
  checks. Workers run as uid 200000 with seccomp and `no_new_privs`, the hostile
  guest's spin and escapes are stopped, data survives a new container on the
  same volume, the proxy answers as nobody on a read-only root, `docker stop`
  with `--init` takes 0.2 s (3.2 s, the full `-t 3`, as PID 1), and workers
  start with the supervisor as PID 1 (N-551). The image was 92 MB.
- The same runtime stage on the real `debian:trixie-slim` (pulled), with the
  image's own `nanovm.toml` and `proxy.toml`: the node is ready (204) and the
  proxy answers (502 with no network for its resolver). 200 MB with the debug
  binary.
- The build stage: its base tag, `rust:1.99.1-slim-trixie`, exists (`docker
  manifest inspect`); `Cargo.toml`, `Cargo.lock` and `crates/nanovm` alone
  (what the Dockerfile copies) pass `cargo metadata --locked --offline`, and
  the crate includes nothing from outside its directory.

Not validated: a full build of the image (the release build in the build stage
needs several GB of disk this shared host didn't have), image size with a
release binary, arm64, Podman, rootless Docker, user namespaces, cgroup v2,
Kubernetes. Docker mounts `/sys/fs/cgroup` read-only (seen on cgroup v1 here).
