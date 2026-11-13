# Single-node beta: operator guide

The one page to follow to run nanovm on one VM for a few trusted developers:
TLS, the proxy, the node, its store and backups, and its limits. Everything
after the first start is in [the runbooks](../../docs/ops/runbooks.md); this page
links to them rather than repeating them.

```
clients ──► Caddy :443 (ACME TLS) ──► nanovm proxy 127.0.0.1:8443 ──► node 127.0.0.1:8080 ──► one jailed worker per app
                                       │  ▲ committed publishes 127.0.0.1:8444 ◄──┘   │
           DNS-over-HTTPS (TXT) ◄──────┘  │                                          └──► capture store (off the VM)
           OCI registry ◄──── proxy (tag → digest) and node (manifest, signatures, blobs)
           Collector 127.0.0.1:4318 ◄──── proxy and node (OTLP/HTTP) ──► your backend
```

Only Caddy listens publicly (80, 443). Everything else is on loopback: the node
trusts the `nanovm-*` headers only the proxy may send, and 8444 accepts
publishes from the node.

**What this is not.** One node, no failover: while the VM is down, every app is
down. Data survives as far as the disk does, plus what has shipped to the
capture store or is in a backup (step 7). There is one version of nanovm, v1
([AGENTS.md](../../AGENTS.md), rule 1): an upgrade starts the node on a fresh
data directory ([runbooks, Upgrade](../../docs/ops/runbooks.md#upgrade-nanovm)),
so tell your developers their objects don't survive a nanovm upgrade.

Sections marked **Placeholder** are filled in as the other beta branches land.

## 0. Before you start

- A VM with Ubuntu 24.04 (systemd 255, cgroup v2, Linux ≥ 5.13 with Landlock),
  root, and 80/443 reachable.
- A DNS zone for the app hosts.
- An OCI registry the apps are published to, readable anonymously, and each
  app's signing identity (CI, keyless: README, "Publishing").
- Off the VM: a restic repository for backups and, for the capture store, an
  S3-compatible bucket or a disk of its own.

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
  checks what each worker needs against this host, as `engine::preflight` will:
  `2 × (max_live_objects + 32) + 1024` fds, `24 × (max_live_objects + 32) +
  4096` mappings, the address space the engine reserves (about 522 GiB at the
  defaults, so no `LimitAS`), Landlock, root, a data directory app uids can
  reach, the cgroup, CA certificates, capture, and the filesystem (see
  [experiments/fs-matrix](../../experiments/fs-matrix/README.md): prefer ext4 or
  XFS without the `discard` mount option; avoid btrfs).
- **Disk**: an object holds up to `disk_mb`, and up to 3 × `disk_mb` while a
  request runs; quotas don't cap the total. Add the registry and compile caches
  (each version's component, ≤ `max_component_mb`, and its compiled code).
  With capture, each object's history here holds up to `[capture] max_mb`
  (256 MiB) until it ships.

## 2. Install the node and the proxy

As root, from a checkout of the release you run:

```sh
cargo build --release --locked -p nanovm --bin nanovm   # or a release archive (tools/release)
deploy/single-node/install.sh target/release/nanovm     # add --cgroup for per-app worker limits
```

[install.sh](install.sh) installs the binary (keeping the previous one as
`nanovm.prev`), the [units](../systemd/README.md), the sysctl drop-in and, if
absent, `/etc/nanovm/{nanovm,proxy}.toml`, makes `/etc/nanovm/secrets` (step
6), then runs `check-host.py`. It refuses
to replace the binary while the node runs, and starts nothing. A release build
has no `dev` feature: apps must be signed, and registries and egress must be on
public addresses.

Every key and its default is in `crates/nanovm/src/config.rs`; an unknown key
stops the node or the proxy from starting, naming it. `tests/configs.rs` loads
every config shipped here with the real loaders, so these stay current.

## 3. Configure the node and its limits

`/etc/nanovm/nanovm.toml` ([the reference](../systemd/nanovm.toml)):

| Key | Reference | Why |
|---|---|---|
| `listen` | `127.0.0.1:8080` | only the proxy may reach the node |
| `data_dir` | `/var/lib/nanovm` | the unit's `StateDirectory`; keep it absolute |
| `proxy` | `http://127.0.0.1:8444` | where committed WebSocket publishes go (the proxy's `publish_listen`) |
| `telemetry_endpoint` | `http://127.0.0.1:4318` | the Collector (step 8); only the supervisor connects |
| `jail_uid_base` | `200000` | app *i* runs as uid `200000 + i`; never change it on a node with data |
| `max_live_objects` | `1000` | per app worker; sizes the pool and the host limits above |
| `max_apps` | `100` | app workers at once; a new app takes the least recently used idle one's slot |

**Limits.** Each app's own limits come from its `nanovm.json` and are clamped
to the node's:

- `[limits]`: the ceiling for every object: `memory_mb`, `cpu_ms` (CPU time
  per request), `request_timeout_ms`, `post_return_timeout_ms`, `disk_mb` and
  `max_files` for `/data` and `/tmp`, `max_request_body_kb`,
  `max_response_body_kb`, `max_queue`, `max_outbound_concurrency`,
  `max_resources`. `egress` is different: not a ceiling to clamp to but
  your list of the hosts any app may reach (step 5).
- `[worker]` with `cgroup`: memory, CPUs and pids for each app's whole worker.
  Install with `install.sh --cgroup` (the [cgroup drop-in](../systemd/nanovm-cgroup.conf));
  without `cgroup` these aren't applied, and check-host warns.
- Node-wide: `max_apps`, `max_live_objects`, `max_component_mb` (64),
  `load_timeout_ms` (60 s for a registry fetch).
- The proxy (`/etc/nanovm/proxy.toml`): `max_sockets` (10,000),
  `max_sockets_per_app` (1,000), `max_sockets_per_object` (256),
  `max_request_body_kb`, `max_response_body_kb`, `socket_backlog_kb`.

> **Placeholder (`beta/limits`):** rate limits, connection caps and disk
> budgets, and cgroup caps on by default: their keys, reference values and
> what a client sees past each. Filled in when `beta/limits` is integrated.

## 4. TLS and the proxy

`/etc/nanovm/proxy.toml` ([the reference](../systemd/proxy.toml)): `listen =
"127.0.0.1:8443"` behind Caddy, `nodes = ["127.0.0.1:8080"]` (this node, as its
`listen`), `publish_listen = "127.0.0.1:8444"`, the DNS-over-HTTPS `resolver`
(Cloudflare's by default), `tag_ttl_s` (30 s), and the `hosts` it serves and
the `signers` who may deploy (step 5).

**Caddy.** `apt install caddy`, then put your app hosts in
[Caddyfile](Caddyfile) at `/etc/caddy/Caddyfile` and set its `email`. Caddy
gets and renews certificates (ACME) for the names listed, passes `Host` through
and proxies WebSockets; the nanovm proxy drops any `nanovm-*` header a client
sends. List hosts explicitly: Caddy's on-demand TLS would need an `ask` endpoint
that admits only hosts with a `_nanovm` record, which nanovm doesn't provide,
and without one anyone could make the VM request certificates. Add a host and
`systemctl reload caddy`.

## 5. Who may deploy, and egress

An app host is routed by its DNS: point its A/AAAA at the VM and publish

```
_nanovm.app.example.com. TXT "app=ghcr.io/org/app:prod; trust=https://token.actions.githubusercontent.com https://github.com/org/app/.github/workflows/publish.yml@refs/heads/main"
```

The node runs only a version signed by a `trust=` identity, at or above an
optional `min=` floor. Deploying is moving the tag; rolling an app back is
moving it back ([runbooks](../../docs/ops/runbooks.md#roll-back-nanovm), last
paragraph).

**You decide which hosts are served and who deploys** (README, ["The
proxy"](../../README.md#the-proxy)). Pointing DNS at the VM isn't enough: in
`proxy.toml`,

```toml
hosts = ["app.example.com", "www.app.example.com"]   # or "*.apps.example.com"
signers = ["https://token.actions.githubusercontent.com https://github.com/org/*"]
```

- `hosts`: names, or `*.name` for any name under it. Any other host gets 403
  before its record is looked up. Keep it the same as the Caddyfile's list
  (B-407).
- `signers`: `<issuer> <subject>`, the subject exact or ending in `*` (end
  the prefix with `/`). A record's `trust=` entries are cut to these; an app
  signed by anyone else is refused even if its record trusts them, and a
  record trusting none of them gets 403.
- Both are empty by default: a proxy without them serves nothing. The
  [reference](../systemd/proxy.toml) lists the Caddyfile's example hosts and
  an example CI identity; replace them with yours.
- **To add a developer:** their host in `hosts` and the Caddyfile, their CI
  identity (or their `cosign` identity) in `signers`, then `systemctl restart
  nanovm-proxy` and `systemctl reload caddy`. They publish the `_nanovm`
  record.
- **Aliases:** a name that CNAMEs onto another's `_nanovm` record serves that
  app only if the record lists it (`alias=www.app.example.com`); otherwise
  403.

**Egress.** The node's `[limits] egress` in `nanovm.toml` is your cap on every
app: a request out must match both the app's `egress` list and yours, so an
app asking for `*.com` reaches only what you list. Empty (the default) means
no app reaches anything. The [reference](../systemd/nanovm.toml) lists
GitHub's hosts, which examples/oauth signs in with; list the APIs your
developers' apps need. Private addresses are refused in both lists.

**Outages.** The proxy keeps serving a host's last route and a tag's last
digest for up to a day while the resolver or the registry fails (serve-stale,
asking again every 30 s), but a worker that restarts meanwhile needs the
registry (B-406), and a restarted proxy has no stale routes.

## 6. App secrets

An app's `env` and `config` travel in its published package, which anyone can
pull, so `nanovm publish` refuses a package that looks like it holds a secret.
Its secrets come from you instead ([README, Secrets](../../README.md#secrets)):

- **Where:** `secrets = "/etc/nanovm/secrets"` (the reference), a directory
  only root may use: `install.sh` makes it `0700`; with any group or other
  bits the node doesn't start.
- **What:** one file per app, `<app>.json`, a JSON object of strings, with the
  keys its `nanovm.json` declares in `"secrets"`: `{"client_secret": "…"}`.
  `<app>` is the app's name on the node, as its `objects/<app>` directory is
  named (README, Secrets, shows how to compute it); a request to an app whose
  file is missing a declared key gets 502 naming the file it wants, never a
  value. The developer tells you the keys; they read them with their config.
- **Who reads them:** the supervisor, when it starts the app's worker, which
  gets them on its stdin. Only that app's jailed worker has them; its uid
  can't read the directory. The host masks the values in telemetry and in
  error details (exact matches only, B-101).
- **Rotation:** write the new file whole and rename it into place. The node
  checks each running app's file every second: a change (even `touch`)
  retires the worker, whose requests in flight finish, and the next request
  starts one with the new values. Removing the file stops an app that
  declares secrets.
- **At rest** the files are plain text behind the directory's mode: keep the
  disk encrypted, or decrypt into the directory at boot (a systemd
  credential, `sops`/`age`). Back them up apart from the data directory.

## 7. Objects, the store and backups

Each object has its own `/data` volume under
`/var/lib/nanovm/objects/<app>/<object>`, created by its first request. Never
edit or delete anything under `objects/*/~journal/`, `~committed/` or
`~shipped/` ([runbooks](../../docs/ops/runbooks.md), the three facts).

> **Placeholder (`beta/objects`):** the cap on how many objects an app may
> create, how idle ones are cleaned up, and deleting an object or an app's
> objects with `nanovm admin` (the node's root-only control socket). Filled in
> when `beta/objects` is integrated.

**Capture store (continuous).** With `[capture]` and a `store`, the supervisor
ships each object's committed versions off the VM as they happen (README,
"Capture and shipping"): the whole volume every `snapshot_every` (16)
versions, the changes otherwise, and an object's `head` naming the last one
that counts. Shipped versions are removed here but the last.

```toml
[capture]
store = "s3://bucket/nanovm"     # or "/srv/nanovm-store", a disk of its own mounted there
# max_mb = 256 and max_versions = 16 bound what waits here while the store is
# down; past them writes are refused, never dropped.
```

- Install the [store drop-in](../systemd/nanovm-store.conf) for either kind:
  `install -D -m644 deploy/systemd/nanovm-store.conf
  /etc/systemd/system/nanovm.service.d/store.conf`. Without it the node ships
  each object's first version and then nothing (B-604). It makes
  `/srv/nanovm-store` writable, and reads an S3 store's `AWS_*` credentials
  from `/etc/nanovm/store.env` (mode 0600); workers never see them.
- With one node, a commit is answered once it is durable on this disk, so a
  write answered just before the disk is lost may not have shipped.
  `remote_ack = true` waits (2 s at most) for the store to name it, and fails
  the request otherwise (the commit stands here).
- The store keeps every version: there is no remote retention yet. Watch its
  size.

**Offline backups.** `scripts/backup.py` (restic) backs up the whole data
directory with the node stopped, so schedule a window:
[runbooks, Back up and restore](../../docs/ops/runbooks.md#back-up-and-restore).
Keep the restic repository and its password off the VM, and restore one
regularly: a backup you haven't restored is a hope.

> **Placeholder (`beta/recovery`):** live backup, and restoring one app or one
> object (from the store or a backup) on a running node with `nanovm admin`,
> with the runbook it adds. Filled in when `beta/recovery` is integrated.

## 8. Observability

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
**Alerts:** [deploy/monitoring](../monitoring/README.md) has Prometheus rules
over the Collector's metrics and node_exporter's disk, each alert linked to its
runbook. **Errors:** [docs/ops/errors.md](../../docs/ops/errors.md) explains
every `x-nanovm-error` reason.

## 9. Start and verify

```sh
systemctl enable --now otelcol-contrib nanovm nanovm-proxy caddy
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/_nanovm/ready    # 204
curl -s https://app.example.com/<object>/                                       # your app
curl -s 127.0.0.1:8889/metrics | grep nanovm_spans_calls_total                  # derived metrics
```

Keep 80/443 open and nothing else.

## 10. Operate

[The runbooks](../../docs/ops/runbooks.md): upgrade and roll back nanovm, roll
back an app, back up and restore, disk full, a worker crash loop, a Collector
outage. [The error guide](../../docs/ops/errors.md) for a status a developer
asks about. [deploy/systemd](../systemd/README.md) for what the units keep and
why; [deploy/container](../container/README.md) to run the node and proxy in
Docker instead.

## How this was checked

On `beta/deployable` (round 3), with a dev build of that branch; method and
output in [notes/beta-deployable.md](../../notes/beta-deployable.md).

- [test/e2e.sh](test/e2e.sh), run by `SCRIPT=$PWD/deploy/single-node/test/e2e.sh
  MODES=plain deploy/systemd/test/nspawn.sh` (this host's Ubuntu 24.04 booted in
  systemd-nspawn): `install.sh` and its host check, Caddy 2.6.2 with its
  internal CA, the proxy asking a DNS-over-HTTPS stand-in, the dev registry, and
  a counting sink on the Collector's port. 8/8 checks pass: three writes and a
  read through verified TLS → proxy → node → jailed worker; a client-sent
  `nanovm-app` header is ignored; a host with no record gets 404; 8080 and 8444
  listen on 127.0.0.1 only; `proxy.request`, `supervisor.request` and `request`
  spans arrive; the count survives a node restart.
- The units in four modes (plain, cgroup, capture, and capture shipped to a
  directory store through the store drop-in): `deploy/systemd/test/nspawn.sh`,
  70 checks.
- `caddy validate` passes on the Caddyfile; `systemd-analyze verify` on the
  units with the cgroup and CPU drop-ins.
- `check-host.py`: 7 unit tests (`python3 -m unittest discover -s
  deploy/single-node`), and run on this host.
- Not run: `otelcol-contrib` (no binary here), real ACME, real DNS, signed apps
  from a public registry, an S3 store, `AllowedCPUs` enforcement (cgroup v1
  here). IDEAS L-113 suggested Terraform or Ansible; this is a tested shell
  installer plus this page, and an Ansible role would wrap the same steps.
