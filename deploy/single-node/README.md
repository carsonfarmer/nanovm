# Single-node reference deployment

One VM runs everything: Caddy for TLS, `nanovm proxy`, the node with its jailed
workers, and an OpenTelemetry Collector with its own CPU and memory budget.

```
clients ──► Caddy :443 (ACME TLS) ──► nanovm proxy 127.0.0.1:8443 ──► node 127.0.0.1:8080 ──► one jailed worker per app
                                       │  ▲ committed publishes 127.0.0.1:8444 ◄──┘
           DNS-over-HTTPS (TXT) ◄──────┘  │
           OCI registry ◄──── proxy (tag → digest) and node (manifest, signatures, blobs)
           Collector 127.0.0.1:4318 ◄──── proxy and node (OTLP/HTTP) ──► your backend
```

Only Caddy listens publicly (80, 443). Everything else is on loopback: the node
trusts the `nanovm-*` headers only the proxy may send, and 8444 accepts
publishes from nodes.

## 1. Size it

- **CPU and memory**: the capacity calculator on the `nice-to-haves` branch
  (`experiments/nice-to-haves/toolkit.py capacity`) with [capacity.json](capacity.json),
  whose measured inputs come from README (105 µs of node+worker CPU and 14.7 µs
  of Collector CPU per request, 87 KB per live object). For 3 cores for nanovm
  and 1 for the Collector at 60% utilization it estimates 17,100 requests/s for
  the application and 40,800 for the Collector, and about 69,000 resident
  objects in 6 GiB. Excluded: Caddy, the proxy, TLS, and your guests' own work
  and memory. Replace the assumed inputs with your own measurements.
  ```sh
  git show origin/nice-to-haves:experiments/nice-to-haves/toolkit.py > /tmp/toolkit.py
  python3 -I /tmp/toolkit.py capacity deploy/single-node/capacity.json
  ```
- **Host limits**: [check-host.py](check-host.py) reads `nanovm.toml` and
  checks what each worker needs against this host: `2 × (max_live_objects + 32)
  + 1024` fds, `24 × (max_live_objects + 32) + 4096` mappings, Landlock, root,
  a data directory app uids can reach, the cgroup, CA certificates, and the
  filesystem (see [experiments/fs-matrix](../../experiments/fs-matrix/README.md):
  prefer ext4 or XFS without the `discard` mount option; avoid btrfs).
- **Disk**: an object holds up to `disk_mb`, and up to 3 × `disk_mb` while a
  request runs; quotas don't cap the total. Add the registry and compile caches
  (each version's component, ≤ `max_component_mb`, and its compiled code).

## 2. Install

On Ubuntu 24.04 (systemd 255, cgroup v2, Linux ≥ 5.13), as root:

```sh
cargo build --release --locked -p nanovm --bin nanovm   # or a release archive (tools/release)
deploy/single-node/install.sh target/release/nanovm     # add --cgroup for per-app worker limits
```

[install.sh](install.sh) installs the binary (keeping the previous one as
`nanovm.prev`), the [units](../systemd/README.md), the sysctl drop-in and, if
absent, `/etc/nanovm/{nanovm,proxy}.toml`, then runs `check-host.py`. It refuses
to replace the binary while the node runs. Set the operator ceilings in
`/etc/nanovm/nanovm.toml` (`[limits]`, `max_live_objects`, `max_apps`).

**Collector.** Install `otelcol-contrib` 0.164.0, the version
[demo/otel-collector.yaml](../../demo/otel-collector.yaml) is validated against,
copy that file to `/etc/otelcol-contrib/config.yaml` and replace its `debug`
exporter with your backend's (credentials go there, never to workers). Give it
its own cores and memory, and keep nanovm off them:

```sh
install -D -m644 deploy/single-node/dropins/otelcol-contrib-budget.conf /etc/systemd/system/otelcol-contrib.service.d/budget.conf
install -D -m644 deploy/single-node/dropins/nanovm-cpus.conf /etc/systemd/system/nanovm.service.d/cpus.conf
install -D -m644 deploy/single-node/dropins/nanovm-cpus.conf /etc/systemd/system/nanovm-proxy.service.d/cpus.conf
```

Pick whole physical cores (`lscpu -e=CPU,CORE`); SMT siblings share one.

**Caddy.** `apt install caddy`, then put your app hosts in
[Caddyfile](Caddyfile) at `/etc/caddy/Caddyfile`. Caddy passes `Host` through and
proxies WebSockets; the nanovm proxy drops any `nanovm-*` header a client sends.
List hosts explicitly: Caddy's on-demand TLS would need an `ask` endpoint that
admits only hosts with a `_nanovm` record, which nanovm doesn't provide, and
without one anyone could make the VM request certificates.

## 3. DNS, start, verify

For each app host, point A/AAAA at the VM and publish its route:

```
_nanovm.app.example.com. TXT "app=ghcr.io/org/app:prod; trust=https://token.actions.githubusercontent.com https://github.com/org/app/.github/workflows/publish.yml@refs/heads/main"
```

```sh
systemctl enable --now otelcol-contrib nanovm nanovm-proxy caddy
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/_nanovm/ready    # 204
curl -s https://app.example.com/<object>/                                       # your app
curl -s 127.0.0.1:8889/metrics | grep nanovm_spans_calls_total                  # derived metrics
```

Keep 80/443 open and nothing else. Then operate with
[the runbooks](../../docs/ops/runbooks.md) (upgrades, backups: `scripts/backup.py`
needs the node stopped, so schedule a window) and
[the error guide](../../docs/ops/errors.md).

## How this was checked

- [test/e2e.sh](test/e2e.sh), run by `SCRIPT=$PWD/deploy/single-node/test/e2e.sh
  MODES=plain deploy/systemd/test/nspawn.sh` (this host's Ubuntu 24.04 booted in
  systemd-nspawn, dev build): `install.sh` and its host check, Caddy 2.6.2 with
  its internal CA, the proxy asking a DNS-over-HTTPS stand-in, the dev registry,
  and a counting sink on the Collector's port. 8/8 checks pass: three writes and
  a read through verified TLS → proxy → node → jailed worker; a client-sent
  `nanovm-app` header is ignored; a host with no record gets 404; 8080 and 8444
  listen on 127.0.0.1 only; `proxy.request`, `supervisor.request` and `request`
  spans arrive; the count survives a node restart.
- `caddy validate` passes on the Caddyfile; `systemd-analyze verify` on the
  drop-ins (it rejects a bad `AllowedCPUs`).
- `check-host.py`: 5 unit tests (`python3 -m unittest discover -s
  deploy/single-node`), and run on this host.
- Not run: `otelcol-contrib` (no binary here; the reference config's validation
  is README's), real ACME, real DNS, signed apps, `AllowedCPUs` enforcement
  (cgroup v1 here). IDEAS L-113 suggested Terraform or Ansible; this is a tested
  shell installer plus this page, and an Ansible role would wrap the same steps.
