# nanovm

A wasmtime-only host for untrusted WASIp3 HTTP components, run as Durable-Objects-style objects:

- Each `(app, object-id)` has exactly one live instance, which serves one request at a time.
- Each object has its own memory cap, CPU budget, deadlines and handle cap.
- Each object has its own quota'd filesystem: scratch `/tmp` and a persistent `/data` volume.
- Each app has an egress allowlist.
- Each object has a `wasi:keyvalue` store in its volume, and each app its `wasi:config` values.
- Objects accept WebSockets and publish to their own sockets.
- Memory is lost on eviction; `/data` survives eviction and restarts.

Apps are signed artifacts in OCI registries. A proxy finds each host's app in DNS; each app runs in its own jailed worker process, under a supervisor that runs no guest code and owns the network (see [The jail](#the-jail)). [PLAN.md](PLAN.md) has the design and what comes next.

## Build an app

`nanovm run app.wasm` serves one component on loopback, as a node and its proxy would (objects, volumes, commits, WebSockets), without a registry, signatures or jail, and restarts when the file changes. [guests/sdk](guests/sdk/README.md) has the five-minute quickstart; [examples/chat](examples/chat) is a live multi-room chat. Production nodes still run only signed registry apps.

## SQLite beta

[Build and run the SQLite example](components/sqlite/README.md). A composed Wasm
provider implements the upstream Spin async SQLite interface over the existing
filesystem, quotas and request journal; there is no host SQLite service. The
example demonstrates persistence, object isolation, and atomic SQL/file/KV
rollback. One connection, provider-owned database files and memory-only temporary
storage define the initial support contract; see the linked limitations before
using it in an app.

## Try it

```bash
demo/demo.sh
```

The script:
1. Builds the guests and the node.
2. Publishes a benign `counter` app and a `hostile` app to a registry on loopback (`cargo run --example dev-registry`).
3. Starts a node in dev mode from [demo/nanovm.toml](demo/nanovm.toml), which runs them unsigned.
4. Pings a canary object throughout.
5. Drives it over HTTP with `curl --parallel`: per-object volumes, path escapes and egress, 5,000 cold starts, 50,000 warm requests, `/data` writes, and a storm of 100 simultaneous attacks.

It leaves the node running. Always-on OpenTelemetry exports rich request spans, correlated guest stdout/stderr and periodic infrastructure snapshot events to `telemetry_endpoint` (default `http://127.0.0.1:4318`). Point this OTLP/HTTP base URL at your Collector for enrichment, redaction, sampling and vendor routing. Jailed workers export over their existing link; the supervisor owns the destination and stamps app/manifest-digest identity. Object IDs appear on request records and logs. Export queues, batch sizes, concurrency and deadlines are bounded; a missing collector does not prevent requests or commits. Worker startup uses a separate private readiness response. The demo's loopback `dev-collector` writes decoded OTLP to `target/demo-telemetry.json`; it refuses stale snapshots. The stats/events HTTP APIs and event ring are removed, with no compatibility endpoints. W-2 validation is complete for the separately budgeted Collector deployment described below.

HANDOFF records matched measurements and rejected experiments. The Linux CLI and benchmark use jemalloc; the library leaves allocator selection to its embedder. Controlled allocator swaps improved full-export throughput from roughly 54–59k/s to 79–84k/s, with about 20 MB more retained memory in that workload. SDK batches remain 256 spans with a 1 MiB body cap; the bounded queue now holds 2048 spans to absorb brief export stalls. No sampling or detail was removed.

`cargo run --locked --release -p nanovm --example bench -- --export` measures three 200,000-request warm runs with a decoding OTLP sink, checks all 601,000 request spans including warmup, and fails on missing exports. Three complete invocations measured 75–79k/s (74.7–78.1k/s including shutdown/export drain), with no missing spans. The sink shares the orb's cores; this is not a production Collector or proxy/jailed-worker throughput claim, nor proof of being within a few percent of main.

Earlier real HTTP validation exposed export loss ([N-36](ISSUES.md)): oha 1.16.0 drove the supervisor and jailed worker at 35–39k/s with no request failures, but OpenTelemetry Collector 0.164.0 received only 406,006 of 600,000 request spans. The worker's bounded queue overflowed. A concurrent SDK exporter eliminated those losses in one experiment but failed an in-flight flush regression, so it was not adopted. Those historical rates are not matched controls for the current build.

The focused continuation isolates the supervisor's control/egress runtime from its HTTP frontends. Previously the first frontend also drove relay I/O; under load this delayed the serial SDK exporter and filled its bounded queue. Temporary timing probes measured mean worker export round trips falling from 12.6 ms to 3.5 ms with isolation. Three uninstrumented HTTP runs with the official Collector and derived metrics delivered all 1,200,000 expected spans at 26.0–26.4k requests/s. This fixes the reproduced scheduling-induced loss, not every possible overload; full-export HTTP throughput at 64k/s remains unproven.

Matched benchmark paths must stay separate. On this 4-core orb, three 200,000-request runs at concurrency 256 over 100 warm objects measured:

| Build | In-process requests/s | HTTP supervisor → jailed worker requests/s |
|---|---:|---:|
| Historical main `617668e` | 63,706–64,889 | 27,145–29,340 |
| Current main `40c5895` | 62,888–63,981 | 23,812–25,237 |
| W-2 with control-runtime isolation | 75,093–77,772 | 17,214–23,319 |

Main has no OTLP export. W-2's in-process decoding sink received 601,000/601,000 spans including warmup (75,482/s including drain); HTTP used oha 1.16.0 and Collector 0.164.0 on the same cores, with two frontends and two spans per request. The table's current main/W-2 HTTP rows are three alternating clean-build runs; each W-2 run delivered all 400,000 spans. The historical main row was measured earlier in the session. Earlier W-2 runs were 26.0–26.4k/s; a longer 2,000,000-request run was 25,282/s with all 4,000,000 spans, p99 28.6 ms. All HTTP requests returned 200. This variation and the final paired regression prevent a performance-clearance claim. A shared-target Cargo cache reused a different worktree's benchmark once; that control was discarded and main explicitly rebuilt. The remembered ~64k main result is confirmed in-process, not over HTTP. These measurements neither establish a safe production rate nor include a durable remote storage backend.

CI reports runtime size (currently 5,315 nonblank/noncomment lines before test modules); the hard LOC budget is suspended while correctness takes priority. Guest and infrastructure logs share one bounded record-emission path. Collector responses are bounded to 4 KiB and passed intact to the SDK, including partial-success reports. The CLI writes SDK warnings to stderr rather than back into the failing export stream; library embedders install their own diagnostic subscriber.

One request span lasts through the final post-return commit. It carries HTTP method/path (no query), status/error, cold-start status and queue, instantiate, guest, commit and post-return durations in milliseconds. `nanovm.response.duration_ms` measures time to the response headers; span duration also includes subsequent work. Calls between objects remain child spans. Only immutable context crosses tasks. The tracing feature, metric exporters and local request histograms are removed; OpenTelemetry's SDK owns recording, batching and export. Requests record even when an incoming parent is unsampled: sampling belongs downstream. Guest-defined spans/enrichment are follow-up W-17.

Requests use SERVER spans; inter-object and external HTTP calls use CLIENT spans with bounded method/path, response status and errors. External calls propagate W3C trace context and record destination host/port; object calls record the target object. Queries are omitted from spans and egress-denial logs. A failed dependency is visible even when its caller handles the failure successfully. Client 4xx/5xx responses are errors; server 4xx responses are not automatically failures. Call duration ends when response headers arrive. This follows the [OpenTelemetry HTTP conventions](https://opentelemetry.io/docs/specs/semconv/http/http-spans/) within the [Honeycomb wide-event model](https://docs.honeycomb.io/get-started/observability/concepts/instrumentation).

Dependency calls cancelled before response headers record `error.type = cancelled`. External HTTP errors use status codes, not untrusted upstream `x-nanovm-error` headers. Request-scoped runtime diagnostics (traps, quota/egress denials, rollback and instantiation failures) carry explicit trace/span IDs. Infrastructure-only events stay unparented. A failed final commit records `error.type = post-return-commit-failed` and a correlated, bounded I/O diagnostic while preserving the already-sent HTTP status. A gated test component verifies actual post-return calls, stderr and final-commit failures without changing published guests.

`proxy.request` and `supervisor.request` spans cover forwarding through response headers, including pre-worker failures. They parent the worker's `request` lifecycle span. The proxy also accepts `telemetry_endpoint`; both default to the same Collector base URL. Readiness traffic is excluded. For counts and latency, select the layer you mean: proxy for client-facing requests, supervisor for node ingress, `request` for object execution. Summing them would double/triple count. Direct HTTP load now produces two spans per ordinary request; proxy traffic produces three. The 256-span batch, 2048-span queue and 1 MiB body bound are our implementation choices, not OTLP limits; the protobuf payload is the standard SDK wire format. The ~64k complete-export requests/s target is not an achieved production baseline.

Infrastructure uses structured OTLP logs (`event.name = nanovm.snapshot`), not artificial request spans: memory, uptime, live stores/objects, phases, resource-limit counters, worker identity/state and restarts/link errors. Runtime events use `event.name`; guest logs use `log.iostream` and trace/span IDs. Derive request counts, latency distributions and SLOs from request events in the Collector/backend. Bounded queues can drop events under overload or collector outage; these are not an exact accounting ledger. Derive exact aggregates before tail sampling, or use a backend's supported weighted queries; a sample-rate field alone does not correct every sampling policy. SDK shutdown exports pending records off the async runtime thread; abrupt process kills can still lose buffered telemetry.

The measurements above precede the final hot-path optimization. A native CPU profile identified repeated OCI reference parsing; a bounded per-thread cache now stores only pure parse results (16 entries, inputs ≤1024 bytes), never host, trust, version floor or authorization. Common span attributes reserve enough capacity upfront. All supervisor/worker/proxy/dependency spans remain; the owner rejected removing supervisor observations for throughput.

Post-commit controls use three alternating 400,000-request runs per variant, c256 over 100 warm counter objects, one frontend, identical release build flags, oha 1.16.0 and official Collector 0.164.0 with derived metrics. Both applications receive the same CPU allocation. With physical SMT core pairs separated (application CPUs 0–1, Collector/load generator 2–3), main/optimized aggregate throughput is **18,631 / 18,227 requests/s (2.17% lower)**. Active node+worker CPU is 103.45 / 105.00 µs/request, with Collector another 14.70 µs/request. Fully colocated on all four logical CPUs, main/optimized is **27,548 / 25,459 requests/s (7.58% lower)**; application CPU 109.42 / 110.64 µs/request, Collector another 13.62. Each optimized matrix returned all 1.2m HTTP200 responses and exported all 2.4m spans. A sustained 2m-request run delivered all 4m spans at 25,561/s, p99 25.84ms; long-path/trace-state runs also delivered all spans, including explicit queue overload.

The isolated application result meets the few-percent overhead gate for the recommended separately budgeted Collector deployment. Its additional CPU is real; the 7.58% colocated result does not meet that gate. W-2 host instrumentation is complete. Runtime is 5,315 lines versus main's 4,955 before W-2 (+360, 7.3%), using CI's nonblank/noncomment count before test modules. These are workload/environment observations, not universal capacity or durable-backend validation. HANDOFF retains earlier results (including the more favorable pre-commit 0.74% isolated / 4.45% colocated), exact reproduction steps and rejected experiments. The remembered ~64k is in-process, not an HTTP baseline.

### Collector and operator checks

**Recommended deployment:** run a separately budgeted Collector as a standalone service, following the standard [OpenTelemetry gateway pattern](https://opentelemetry.io/docs/collector/deployment/gateway/). Provision CPU/memory independently of the application; dedicated physical cores on the same host are also valid. This separates backend credentials/policy and resource contention from request serving. Our same-host physical-core test validates application overhead; it does **not** test a remote network or durable backend. Across hosts, configure private receiver ingress and TLS/access controls, then repeat completeness, latency and outage tests for that topology. The supervisor—not jailed workers—connects to the configured OTLP/HTTP endpoint. Keep SDK queue-loss/export alerts and separately monitor Collector saturation; adding a Collector does not make telemetry lossless.

[demo/otel-collector.yaml](demo/otel-collector.yaml) is a validated reference for official `otelcol-contrib` 0.164.0. Run `otelcol-contrib --config demo/otel-collector.yaml` and point node/proxy `telemetry_endpoint` at its OTLP/HTTP base URL. It accepts traces/logs on loopback port 4318, derives bounded-cardinality request/error/duration metrics on port 8889, and exposes Collector health counters on port 8888. It does not sample. The `debug` exporter is a validation sink, **not durable storage**: replace it with your authenticated backend exporter, configure backend queue/retry/storage policy, and repeat the completeness/load check against that backend. Keep receiver and metrics ports private; credentials and redaction policy belong in the Collector, not jailed workers.

For object execution, select `span_name="request"`; for node ingress select `supervisor.request`, or `proxy.request` for the public frontend. For example, `sum(rate(nanovm_spans_calls_total{span_name="request"}[5m]))` gives object executions/s; adding `status_code="STATUS_CODE_ERROR"` selects errors. `nanovm_spans_duration_seconds_bucket` supports latency quantiles for that same layer. These durations include post-return work only on the worker `request` span. Object IDs and raw paths are deliberately not metric dimensions. Overflow collapses extra dimension combinations, losing detail rather than silently discarding the aggregate; watch `otel_metric_overflow`. The pinned connector is upstream alpha, so validate configuration and metric names when upgrading.

The reference pipeline's five-second metric flush is independent of the SDK export interval: wait for metric updates before comparing totals. A real-Collector check of 137 reads, 11 committed writes and 17 traps produced 148 successes and 17 errors in each selected layer, with matching histogram counts. Do not sum layers. Compare Collector `otelcol_receiver_accepted_spans` and `otelcol_exporter_sent_spans` against expected span counts, and alert on refusal/export failure counters and SDK stderr queue-drop/export warnings. Successful HTTP responses alone say nothing about telemetry completeness. Best-effort bounded export can still lose records during overload, backend outages or process kills; no silent sampling or lossless-accounting promise is made.

A request names its app in the headers the proxy sets (see [Apps](#apps)), and its object in the path, `/{object}/{path}`:

```bash
curl -XPOST http://127.0.0.1:8080/alice/incr \
  -H 'nanovm-app: 127.0.0.1:5000/nanovm/counter@sha256:…' -H 'nanovm-trust: dev dev'
```

## What a run of `demo/demo.sh` shows

On the 4-core Linux sandbox, with each app's worker capped at 8,000 objects (the sandbox's fd limit; see Host requirements). These numbers go through curl, which shares the 4 cores, and the HTTP front end; the in-process numbers are under Benchmarks. The second column is the same demo on the unjailed single-process node from before the jail, on the same machine.

| Scenario | Jailed | Unjailed |
|---|---|---|
| 5,000 cold starts (64 in flight) | 3.7K/s, p50 14 ms, 89 KB RSS per new object | 4.0K/s, p50 14 ms, 87 KB |
| 50,000 GETs over 1,000 warm objects | 19K/s, p50 2.2 ms | 27K/s, p50 1.2 ms |
| 10,000 `POST /incr` (each one rewrites `/data/count`) | 6.7K/s | 7.3K/s |
| One object, one client | 3.0K/s, p50 0.3 ms | 5.6K/s, p50 0.1 ms |
| 110 attacks at once (now 100: `/app` is gone) | All contained in 2.2 s; the 2 s wall-clock deadline sets the slowest | the same |
| Canary over HTTP, throughout | 0 failures | 0 failures |

Every request succeeded and each worker started once. The cost of the jail is the hop between the supervisor and the worker; see [What the jail costs](#what-the-jail-costs).

A run in a 4-core Amp orb after phase 7 (the registry setup, with per-request commits): 5,000 cold starts at 2.9K/s (p50 18 ms, 95 KB per new object), 50,000 warm GETs at 15.9K/s (p50 2.7 ms), 10,000 `POST /incr` at 600/s (each now fsyncs its commit; 1 of them was a CPU kill under load), one client at 2.9K/s (p50 0.3 ms), all 100 attacks contained in 2.2 s, and 0 canary failures.

Every attack in the storm, and how it ended:

| Attack | Outcome |
|---|---|
| allocate 1 GiB | killed at the 32 MiB memory cap |
| spin forever | killed at the 100 ms CPU budget |
| sleep 60 s | killed at the 2 s deadline |
| panic | trap; instance discarded |
| leak 1M handles | killed at 2,000 handles |
| fill the disk | write refused at the 8 MiB disk budget |
| create files | refused at 200 files |
| `../` and symlink escapes | refused |
| fetch cloud metadata (169.254.169.254) | denied (not on the allowlist) |
| allowlisted name that resolves to 127.0.0.1 | denied by the resolved-IP check (in this sandbox, which has no DNS, it fails to resolve instead; the `localhost` integration test covers the IP check) |

## The jail

```
client ─► proxy ─► supervisor (nanovm serve): runs no guest code
                    │  HTTP/1.1 front end; routes each request to its app's worker
                    │  makes every outbound request: allowlist, DNS, public-IP filter
                    │  starts and restarts workers; places each in its cgroup
                    ├── fd 3: links for requests ──► worker "counter"  (nanovm worker)
                    └── fd 4: links for egress  ◄──  one Node for one app, jailed
```

A worker reads its config and component from stdin, then jails itself while it still has one thread, before it compiles or runs anything:

- **Its own uid.** When the supervisor runs as root, app `i` runs as `jail_uid_base + i`, and its directories are chowned to it.
- **Landlock.** It gets read-write access to the app's own `objects/`, `tmp/` and `cache/`, and nothing else.
- **seccomp.** The allowlist holds only the syscalls Wasmtime, tokio and the filesystem host make, traced over the test suite:
  - no `socket`, `connect`, `bind`, `execve`, `ptrace`, `mount` or namespace calls
  - `clone` for threads only
  - no memory that is writable and executable at once
  - limits can be read, never raised
  - `fcntl` for descriptor flags only: no `F_SETOWN` or `F_SETSIG`, which would have the kernel signal another process of the same uid
  - `socketpair` for a local stream pair only: a datagram socket could `sendto` any named or abstract socket on the host

  Anything else fails with `ENOSYS`. With no way to create a socket, a worker's only channels are its two control sockets to the supervisor.
- **`no_new_privs`**, and the worker dies with its supervisor.
- **cgroup v2** (optional, `cgroup = "<delegated dir>"`). Each worker goes into `<cgroup>/<app>` with the node's `[worker]` memory, CPU and pids limits, before it is given anything.

Mount and network namespaces were left out on purpose: Landlock already confines the filesystem, and with no `socket()` there is no network to remove.

**Links.** The supervisor and a worker talk HTTP/1.1, one request at a time per connection, over socketpairs. The dialing side passes one end of each pair to the other over its control socket (`SCM_RIGHTS`) and keeps the connections it has made in a pool. A request runs inline in the task that sent it, at both ends: on this path every handoff between tasks is a thread wakeup, and those cost more than the HTTP framing. Each front-end thread (`front_threads`, a quarter of the cores by default) has its own runtime, listener and pool. The supervisor serves a worker at most as many egress links as its objects can use at once (`max_live_objects × (max_outbound_concurrency + 1)`), and all workers together at most a quarter of its file descriptor limit, which it raises to the hard limit at start; a link past either cap is closed unserved.

A worker that dies takes only its app with it. In-flight requests get `502 worker-down`, and the next request starts a fresh worker. Volumes survive.

### What the jail costs

The hop between the processes costs two extra cross-process round trips, about 30–40 µs each in this VM, plus one more HTTP parse. On the 4-core sandbox:

- One client sees about 0.1 ms more latency, the warm GET p50 above.
- At 64 in flight, throughput is about 25% lower. The supervisor needs about a fifth of a request's CPU, and here it shares 4 cores with the worker and with curl.

The in-process benchmark below measures the node inside a worker, and is unchanged.

## The proxy

`nanovm proxy --config proxy.toml` is the public front door. It finds the app a request's host serves in a TXT record at `_nanovm.<host>`:

```
app=ghcr.io/org/app:prod; trust=<issuer> <subject>; trust=…; min=1.2.0
```

That is the repository and the tag deployed, each identity whose signature is accepted, and an optional version floor. Whoever controls the domain controls its routing; deploying moves the tag, and rolling back moves it back. The proxy:
- asks a DNS-over-HTTPS resolver (`resolver`, Cloudflare's by default) and keeps the answer for its TTL;
- asks the registry which digest the tag names, and keeps that for `tag_ttl_s` (30 s);
- looks each host or tag up once however many requests ask at once, keeps a failed lookup's answer for 5 s rather than asking again, and remembers at most 10,000 of each;
- drops every `nanovm-*` header a client sent, and hop-by-hop headers (`Connection` and what it lists, `Keep-Alive`, `TE`, `Trailer`, `Proxy-Authorization`), names the app to the node (below), and forwards HTTP requests; it holds WebSockets itself and forwards their events;
- speaks plain HTTP: TLS is left to a terminator in front (Caddy, a cloud load balancer, Cloudflare), which also renews and reloads certificates;
- gives a client 10 s for each request's headers (or between requests), and waits out running out of file descriptors rather than exiting (the node's front end does too);
- refuses a request head over 64 KiB (431), as every hop does (node, worker links, egress responses).

A host with no record is a 404; a malformed record, or more than one, is a 502.

DNS as the routing table has limits: a record is as trustworthy as its resolver (hence DNS-over-HTTPS, and apps keyed by the name that owns their record, so a record reaches no other name's app); a change takes up to its TTL to arrive; a host serves one app (several hosts can share one through CNAMEs); the table is public; and a cold lookup adds a round trip.

## Apps

Every app comes from a registry. The proxy names it with each request:

```
nanovm-app: ghcr.io/org/app@sha256:…        # by digest only
nanovm-trust: <issuer> <subject>[, …]        # whose signature is accepted
nanovm-min-version: 1.4.0                    # optional floor
```

The node trusts these headers because only the proxy can reach it, and strips them before the guest sees the request. A request without them reaches only the node's own `/_nanovm/` routes.

**An app is a DNS name's deployment of a repository.** Its name on the node (`example.com_ghcr.io_org_app-<hash>`), its volumes and its uid come from the name that owns its `_nanovm` record (`nanovm-host`) and `registry/repository`, so every version that name runs, whoever signed it, sees the same data. The owner is the name the record was found at after CNAMEs, so `_nanovm.www.example.com CNAME _nanovm.example.com` makes both hosts one app. Whoever controls that name chooses the version it runs, so no domain can name another's repository and switch its app to an old version on its data (a review finding); a CNAME to someone else's record only serves their app as their record configures it. Only those who can push to a repository can put a signed artifact in it, so naming another identity never reaches a different app's data. The registry is written one way only: lower case, without `:443`. A team signs from CI, or lists each member's identity.

What happens on the first request for a digest:
1. **Signature first.** The supervisor pulls the manifest anonymously, then the Sigstore bundles attached to it as referrers. It verifies them offline against Sigstore's public-good trusted root, embedded in the binary: an exact issuer and identity from the trust header, over the manifest digest. It also checks the version (`org.opencontainers.image.version` in the manifest) against the floor. Nothing else is downloaded until that passes.
2. **Then the blobs.** The supervisor fetches the config blob (the app's limits and env) and the `application/wasm` layer, checks them against the digests the signed manifest pins, and caches them by digest under `data_dir/registry`.
3. **Then the worker.** The app's limits are clamped to the operator's `limits` (an instance's resource table also holds at most `memory_mb × 16` handles, since each may hold a 64 KiB header map on the host: 256 at 16 MiB, 1,024 at 64 MiB), and its worker gets `worker`. The component goes to a fresh jailed worker, which compiles it.

**A running version under a new policy** (other identities, or a higher floor) is checked against it before the worker serves it; a refusal leaves the version running. The supervisor remembers up to 16 accepting policies per running version, then forgets them and checks each again as it comes.

**How many apps.** A node runs at most `max_apps` workers (100). A new app takes the slot of the least recently used idle app, one with no requests in flight, whose worker is retired (its next request starts it again). Open proxy sockets do not pin workers. With none idle, the new app is refused (502) until one is. The node forgets apps with no worker once it knows of `2 × max_apps`. Since anyone's DNS can name an app, this is what keeps a node's processes, uids and memory bounded; it does not stop one party from keeping every slot busy, which needs per-client limits in the proxy.

**Bounds.** Every registry read is bounded: a manifest, config, token or signature at 4 MiB, a component at `max_component_mb` (64), and 16 signatures per load. At most four registry fetches run at once across the node, each within `load_timeout_ms` (60 s) in all, so a registry that answers a byte at a time holds nothing for long. Digests are checked before they name anything. Registries, and their redirects and token servers, must be on public addresses: a deploy names the registry, so it must not reach the node's own network.

**Dev mode** (`dev = true`, refused unless the binary was built with `--features dev` and `listen` is loopback, so a default release build can't be put in dev mode) is for local development: apps need no signature, registries and egress entries may be on private addresses, and loopback registries are plain HTTP. The tests and the demo use it with a registry on loopback; signatures are tested against the CI-signed guests on ghcr.io.

**Switching versions.** A request naming a digest the app's worker isn't running switches the app. The new version is fetched and verified while the old one keeps serving. The old worker then drains: no new requests, and in-flight ones (and their response bodies) finish, bounded by its longest request time. It is then killed, and only once its process has exited does the new worker start. Both versions share the app's volumes, so an object is never live in two workers. A switch, once begun, finishes even if the request that began it goes away.

**Uids.** An app's uid is the owner of its `objects/<app>` directory, or the next free one for a new app. It is stable across restarts, with no separate record.

**Publishing.** [.github/workflows/publish-guests.yml](.github/workflows/publish-guests.yml) is the reference pipeline. CI builds each test guest, pushes it with `oras` (artifact type `application/vnd.nanovm.app.v1+json`, its `nanovm.json` as the config, version `1.0.<run>`), and attaches a GitHub artifact attestation, signed with the workflow's short-lived identity. No key exists anywhere. A person can sign the same way from a laptop with `cosign` and a browser sign-in.

## Key-value and config

**`wasi:keyvalue`** (`store`, `atomics` and `batch`, 0.2.0-draft) is implemented by the host on the object's own volume. Bucket `b` is the directory `/data/.kv/b`, and each key is one file in it, named by the key in unpadded base64url so that no key can name a path.
- Values count against the object's disk budget and keys against its file limit, like any other file.
- A value is written in place; its request's commit makes it durable, together with everything else the request changed (see Commits).
- A read is bounded by the object's memory limit, and a batch names at most 1,000 keys. Host calls aren't interrupted by the CPU budget, so these bounds keep them short.
- `wasi:filesystem` stays exposed. A guest can reach `.kv` through it; the store lists only well-formed key files, and refuses to write over anything else.

Spin's store, by comparison, is one SQLite table per app shared by all its instances, with no per-store disk quota. Ours is per object, under the object's quotas, with no dependency. Commits give it what SQLite would: a request's values, files included, land together and durably, or not at all.

**`wasi:config/store`** (0.2.0-rc.1) serves the `"config"` table of the app's `nanovm.json`. Environment variables (`env`) still work alongside it.

## Calls between objects

An object calls another object of its own app with an ordinary outgoing request to `http://nanovm/objects/{id}/{path}`, as a Durable Object calls a stub:
- The callee gets it as a request for `/{path}`, through its queue like any other, so it is still served one request at a time.
- The callee commits before it answers, so the caller only ever sees durable changes. Each object commits on its own: a caller that fails afterwards doesn't undo the callee's changes.
- Calls need no egress rule. They count against the caller's `max_outbound_concurrency`, and their bodies against the app's `max_request_body_kb`.
- **Cycles are refused.** With one request at a time per object, a call that waits on its own caller, directly (`a` calls `a`) or through other calls (`a` calls `b`, which calls `a`), could never be served. Each object records the objects its calls wait on. A call that would close a cycle gets `508 call-cycle` at once, instead of hanging until a deadline kills both requests and rolls them back. The check and the record happen under the table lock, so two objects calling each other at the same moment can't both get through.

## WebSockets

The front proxy holds client sockets; the node only answers requests. Objects see sockets as ordinary requests, in [Pushpin's WebSocket-over-HTTP](https://pushpin.org/docs/protocols/websocket-over-http/), so a guest stays a plain HTTP handler and existing GRIP libraries work. No separate Pushpin process is needed.

Committed publishes travel from the supervisor to the proxy's private `publish_listen` (`127.0.0.1:8444`), at `POST /publish`. Set the node's `publish_to` accordingly (default `http://127.0.0.1:8444/publish`). **Only nodes may reach this control port**, just as only proxies may reach the node. The supervisor names the app, the worker names the object; channels cannot cross either boundary. Publish bodies have a 30 s deadline. An in-process node (`nanovm run`, the tests) holds sockets itself, with the same code, and delivers its committed publishes to them.
Sockets are indexed by object within each app; a publish visits that object's sockets, not every socket in the app. CNAME aliases share channels and per-app caps; separate record owners remain isolated even when they name the same package.
- **Opening.** A socket opened at `/{object}/…` arrives as a `POST` of `application/websocket-events` with the event `OPEN` and a `Connection-Id`. The object accepts by answering `OPEN`; any other answer refuses the socket, with the object's own status if it is a 4xx or 5xx (so a `401` reaches the client).
- **Events.** Each later event (`TEXT`, `BINARY`, `CLOSE`, `DISCONNECT`) is a `POST` queued with the object's other requests, so events are ordered with everything else the object serves. The answer carries events for that socket.
  - A socket has one event in flight at a time, and its next message is read once that is answered. Messages for the client are written meanwhile.
  - An event waits for room in a full queue rather than being refused; the sockets bound how many wait.
  - Only the proxy sends events: it refuses a client request any of whose `Content-Type` headers names `application/websocket-events` (in any case, with any parameters), so an object can trust `Connection-Id` however it reads the type. The node trusts events from its private front end just like the app headers.
- **Fan-out within an object**, as in Durable Objects (an object is a room). An object that answers `Sec-WebSocket-Extensions: grip` sends `m:` messages and `c:` controls. `{"type":"subscribe","channel":"…"}` subscribes the socket to a channel of its own object. The object reaches every subscribed socket, from any request, by posting GRIP's publish format to `http://nanovm/publish/`; the worker stamps that request with the object's id.
- **Lifetime.** Sockets outlive instances, workers and version switches. Each event resolves the app's current digest from the proxy's caches; subscriptions remain intact. If the host changes app identity (record owner or repository), or an event cannot reach the node, the socket closes with 1011. Events have a 240 s overall deadline, including cold deployment. A proxy restart drops its sockets.
- **Limits.**
  - Proxy settings: `max_sockets_per_object` (256), `max_sockets_per_app` (1,000), and `max_sockets` (10,000 total).
  - Proxy `max_request_body_kb` (1,024) bounds messages (closed with 1009); `max_response_body_kb` (8,192) bounds answers and publishes. The node's independent request/response caps still apply.
  - Messages waiting to be written count their bytes plus 64 each. A socket more than the proxy's `socket_backlog_kb` (1,024) behind is dropped, and so is one whose write stalls for 30 s. Each socket has its own budget.
  - A socket is pinged every 30 s and dropped if it heard nothing for a whole interval, so a half-open connection doesn't hold its slot.
  - A socket has at most 64 channels and 16 KiB of handshake headers. A publish has at most 64 items.

## Commits

A request's changes to `/data` (files and key-value alike) and what it publishes to WebSockets take effect together when it answers, and are durable before anyone hears of them: Durable Objects' output gate. A guest error, trap, CPU/time limit or worker crash before commit rolls back those changes. An answer of any status commits.

A host I/O failure during commit, a crash at its commit marker, or a lost response can leave the caller unsure whether the request committed. Do not interpret every error as a rollback: use application-level operation IDs for idempotent retries. Durability assumes a filesystem and device that honor successful file and directory syncs; process-crash tests do not certify physical power-loss behavior.

**How:** an undo journal (`journal.rs`), as SQLite's rollback journal, under our `wasi:filesystem` host (`quota.rs`).
- **Before each change, the host records what the change would destroy, and fsyncs it:**
  - a file's size, the first time it is written;
  - the bytes a write or truncation overwrites;
  - "remove this", for a new entry;
  - "rename it back", for a rename;
  - "recreate this", for a removed directory.

  A removed or replaced file is renamed into the journal, not deleted.
- **Then the change happens in place,** so open files and `mmap` keep working, and SQLite can sit on `/data`.
- **Commit** (before the response is released, and again after post-return work) fsyncs what changed and deletes the log. Held publishes are sent after it.
- **Rollback** has one path. A failed request ends its instance, and every instance start replays any log left behind backwards, so a crash is handled the same way. Recovery trims each step off the log as it completes, so a crash during recovery resumes where it stopped.
- **Costs:**
  - One fsync per record that destroys data (first write to a file, overwrite, truncate, remove, rename), and one round of fsyncs per committing request. Requests that change nothing pay nothing.
  - Fsync time isn't charged to the guest's CPU budget.
  - The quota counts committed state. While a request runs, the log and the files it removed may each hold as much again, so an object's disk use peaks at three times its quota. Past that, a change is refused as a full disk.
- **Safety:**
  - Guest paths reach only the volume. The journal sits beside it, out of reach.
  - A recovery error preserves the failing undo and refuses to start the object. Repair the storage problem and retry; never delete the journal to force startup. Already-applied or never-performed operations remain safe to replay.
  - A commit failure poisons that journal instance, so retrying cannot falsely report success. Recovery syncs both parents of a reversed rename before retiring its record.
  - A failed rename restores its staged destination and storage accounting, keeps dirty-source sync tracking, and durably cancels its rename undo before paths can be reused. If restoration or cancellation fails, the request cannot commit and must recover. Cancelled records still consume log space.
- **Downgrades:** cancelled renames use journal tag 5. This version reads older journals, but older binaries cannot recover a log containing that tag. Before downgrading, drain requests and finish any pending recovery with this version; never delete an uncommitted journal to make a downgrade work.
- **`/tmp` is not journaled.** It is per-instance scratch, deleted with the instance, so it's the fast place for temporary files. A rename between `/tmp` and `/data` fails as cross-device, as between real filesystems.

**Alternatives considered:**
- Key-value-only storage: rejected, real files are wanted.
- FUSE or kernel overlay filesystems: rejected. They need mounts per object, and their commit copies.
- btrfs or ZFS snapshots: rejected. They tie nanovm to one filesystem, and a snapshot costs a transaction commit per request.
- Swapping in a shadow copy of the volume: rejected. It copies the tree and breaks files held open.
- Per-file copies with reflink: rejected. They copy whole files on ext4.
- Crates (`fs-transaction`, `txfs`, `kaish-vfs`, `eryx-vfs`, `overlayfs_fuse`): none sits behind POSIX writes through open descriptors.

## Benchmarks

```bash
cargo run --release -p nanovm --example bench -- --json target/bench.json
```

Add `--quick` for a run that is 10× smaller. A full run on the same Mac gave:

| Scenario | Throughput | p50 | p99 |
|---|---|---|---|
| Cold start, 10,000 objects, 256 concurrent | 16.9K/s | 15.1 ms | 32 ms |
| Warm GET, 1,000 objects, 256 concurrent | 327K/s | 0.35 ms | 5.8 ms |
| Warm GET, one client, one object | 71K/s | 0.01 ms | 0.02 ms |
| `POST /incr` (writes `/data`), 256 concurrent | 27K/s | 8.8 ms | 21 ms |
| `POST /incr`, one client, one object | 12.0K/s | 0.08 ms | 0.12 ms |
| First request after a trap (fresh instance) | 3.8K/s | 0.19 ms | 0.27 ms |
| Churn: new object plus LRU eviction, 20,000 through 1,000 slots | 11.8K/s | 5.2 ms | 11.5 ms |
| Neighbours, 100 objects, 64 concurrent | 389K/s | 0.09 ms | 0.81 ms |
| ...the same, with 16 attackers looping | 154K/s | 0.22 ms | 2.4 ms |

Density at 10,000 live objects is 157 KB RSS per object. Instantiation p50 is at most 1 ms, from a pooling allocator and a content-addressed `.cwasm` cache.

### Reading these numbers

- **Neighbours under attack.** The 16 attackers ran 1,008 attacks in 3.3 s, and 928 instances were killed. Under that load, the benign objects kept 39% of their throughput and p99 stayed under 2.5 ms. This is the heavy-lanes scheduler at work: a request that has used 10 ms of CPU must hold one of `heavy_lanes` permits (default: half the cores) to keep running, so CPU hogs can't occupy every worker.
- **File-reading GETs are bounded by macOS.** A GET that reads an existing `/data` file runs at about 113K/s. A plain C loop of `openat`/`pread`/`close` on 14 threads manages only about 88K/s on this Mac, because macOS `openat` doesn't scale across cores. Linux path lookup does scale, so measure this on Linux.
- **RSS.** macOS RSS doesn't fully drop after eviction, so per-object RSS is better measured on Linux.

### Linux

The same benchmark on a 4-core Intel Xeon (2.1 GHz) cloud sandbox, a Firecracker microVM with no `/dev/kvm`, using `--cold 9000` because the sandbox caps fds at 20,000:

| Scenario | Linux, 4 cores | Mac, 14 cores |
|---|---|---|
| Cold start, 256 concurrent | 6.3K/s, p50 41 ms (9,000 objects) | 16.9K/s, p50 15 ms (10,000) |
| RSS per live object | **87 KB** | 157 KB |
| Warm GET, 1,000 objects, 256 concurrent | 80K/s, p50 3.1 ms | 327K/s, p50 0.35 ms |
| Warm GET, one client, one object | 11.5K/s, p50 0.07 ms | 71K/s, p50 0.01 ms |
| `POST /incr`, 256 concurrent | 8.6K/s | 27K/s |
| `POST /incr`, one client, one object | 2.0K/s, p50 0.47 ms | 12.0K/s, p50 0.08 ms |
| First request after a trap | 1.1K/s, p50 0.59 ms | 3.8K/s, p50 0.19 ms |
| Churn, 20,000 objects through 1,000 slots | 3.5K/s | 11.8K/s |
| Neighbours, 100 objects | 88K/s, p99 2.9 ms | 389K/s, p99 0.81 ms |
| ...with 16 attackers looping | 38K/s (43%), p99 4.7 ms | 154K/s (40%), p99 2.4 ms |

Every scenario completed with 0 failed requests.

- **Per core, throughput is roughly on par with the Mac.** The exception is single-client latency. In this VM a cross-core thread wakeup round trip costs 21.5 µs, against 2.3 µs on one core (measured with a futex ping-pong). Each request crosses threads twice, from the caller to the object's actor and back. That is a cost of this nested-virtualisation sandbox, so measure it again on a regular cloud VM.
- **Linux path lookup scales.** `scripts/openbench.c` (`openat` + `pread` + `close`) does 595K/s on 1 thread and 1.26M/s on 4. The Mac managed 88K/s on 14 threads, so the Mac's file-reading-GET ceiling was macOS.
- **`/incr` is slow here.** It costs about 0.5 ms per request even serially. Reads are fast, so the cost is in creating, truncating and renaming files on this VM's ext4. Not yet profiled.

### Host requirements on Linux

Each worker checks these at startup and refuses to start, naming the fix, rather than failing requests at runtime. The limits apply per worker process, so per app:

- **File descriptors:** each live object holds 2 (`/data` and `/tmp`). The check needs about `2 × max_live_objects + 1024`. The worker raises its soft limit to the hard limit itself, before it jails itself; the demo's 8,000 objects per app need `ulimit -n` of at least 17,088.
- **Memory mappings:** about 8 per pool slot. The default `vm.max_map_count` of 65,530 fits roughly 7,600 objects; 8,000 needs `sysctl -w vm.max_map_count=68352` or more.
- **The jail** needs Landlock (Linux 5.13+) and seccomp. A separate uid per app needs the supervisor to run as root.
- **Don't run with `RUST_BACKTRACE` set.** wasmtime then captures a stack backtrace for every guest-visible error. That includes routine ones, like reading a file that doesn't exist yet. With it set, warm throughput across 1,000 objects fell from 63K/s to 23K/s.

## Tests

```bash
scripts/build-guests.sh
cargo test --release -p nanovm
```

There are 64 integration tests and 93 unit tests, plus one that needs external HTTPS and is ignored by default (example.com, through the webpki roots; it succeeded in the demo). They drive real components: [guests/counter](guests/counter) and [guests/hostile](guests/hostile). The hostile guest knows every attack in the storm.

GitHub Actions runs **CI / Build and test** on pushes and pull requests: a production build check without dev features, locally built guests, then the locked unit/integration suite on Ubuntu 24.04. Tests run serially to avoid competing wall-clock CPU budgets; concurrency tests still exercise parallel requests. The workflow has read-only permissions and publishes nothing. Cgroup tests skip when controllers are unavailable; the external HTTPS test remains ignored by default.

The first successful [CI run](https://github.com/carsonfarmer/nanovm/actions/runs/41227208960) passed the production check and all 157 enabled tests.

The tests cover:
- one request at a time per object, with objects running in parallel
- full queues
- every limit, and that a kill doesn't disturb neighbours
- CPU hogs not starving short requests
- volumes surviving eviction and restarts, and not being shared between objects
- `/tmp` wiped on a new instance
- file-size and file-count quotas
- sandbox escapes, and one disk budget across `/data` and `/tmp`, and renames between them refused as cross-device
- commits:
  - a request that errors, traps or runs out of CPU leaves its files and key-value writes untouched, while one that answers, even with 507, commits;
  - a worker killed mid-request is rolled back when the object next starts;
  - a publish followed by a panic reaches no socket;
  - unit tests roll back every kind of change, commit them, survive a crash at each step of recovery, ignore a torn record, journal writes to a file removed while open and to one opened write-only, and record nothing for a failed `rmdir` or a refused change
- calls between objects: the callee's volume and commit, and cycles (an object calling itself, or back through another) refused at once
- an allowed outbound request through the full egress path
- the egress allowlist, including DNS rebinding to loopback
- LRU and idle eviction, and capacity limits
- churn past capacity
- the HTTP front end
- the jail, from a probe process inside it (`jail_blocks_escapes`): another app's files, `/etc`, sockets, fork, exec, setuid, ptrace, signals (sent, or arranged through `F_SETOWN`), namespaces, W+X memory and raising limits are all refused, while its own directory and the cwd are reachable; a sleep survives a stop and continue
- the same attacks through the supervisor and jailed workers, with egress enforced by the supervisor, including an allowed request whose upstream tries to forge the supervisor's verdict header
- a killed worker restarted with its volumes intact, while its neighbour keeps serving
- registry apps against the real, CI-signed guests on ghcr.io:
  - a good signature runs
  - a wrong identity, a wrong issuer, a version below the floor and an unknown digest are each refused, with the reason
  - a running version is checked again under each new policy, a refusal is not remembered and leaves it running, and the trust header is required
  - switching versions lets the old worker's in-flight request finish before the new worker answers, on the same volumes

  These skip, saying why, where ghcr.io can't be reached.
- open response bodies counting against `max_outbound_concurrency`, in-process and through the supervisor
- many requests reusing the links without errors or restarts, and dials waiting out a peer slow to accept
- workers placed in their cgroups, and the cgroup limits enforced: a worker over `memory.max` is OOM-killed and restarted while the other app keeps serving, `cpu.max` throttles, `pids.max` holds. These need cgroup v2 with those controllers and skip otherwise, saying why.
- apps from a registry on loopback, in dev mode: downloads bounded by `max_component_mb` and 4 MiB of metadata, a floor checked against the running version, a switch that finishes when its request goes away, at most `max_apps` workers (taking only idle apps' slots), and slow registries held to four fetches at once and a deadline
- the proxy hanging up on clients that stall their headers, and surviving running out of file descriptors
- the proxy: a host's DNS record names its app at the version its tag names now, a client can name no app itself nor reach `/_nanovm/`, a missing or bad record is refused, moving the tag deploys, and proxy-owned WebSockets send events with the current digest
- key-value: values persisting in the volume, counting against the disk and file quotas, bad names refused, keys paged, reads bounded by the memory limit and batches by size, and the store working in a jailed worker (including a key file the guest replaced through its filesystem)
- config values served through `wasi:config`, locally and from a registry app's `nanovm.json`
- WebSockets through the proxy: echo, binary, committed fan-out scoped by app and object, a guest-initiated close, the object told how a socket ended, refusals by an object that doesn't answer `OPEN`; per-object, per-app and total caps, bounded messages and slow clients, publish deadlines, worker restart and version switches without reconnecting

### Testing cgroups in a VM

Some hosts, this sandbox among them, give the memory, cpu and pids controllers to cgroup v1, so cgroup v2 limits can't be tested on them. `scripts/vm-test.sh` boots a throwaway QEMU VM (an Ubuntu kernel and a busybox initramfs holding the test binaries and guests) whose cgroup v2 has every controller, and runs the cgroup tests and `jail_blocks_escapes` there. It is for testing only: in production the supervisor runs on the host, in a cgroup subtree delegated to it (systemd `Delegate=yes`).

```bash
scripts/vm-test.sh          # cgroups and the jail
scripts/vm-test.sh all      # everything; without KVM, CPU-budget and latency tests fail under emulation
```

On Linux 7.0 (emulated, 4 vCPUs), the default set passes. Under `all`, 35 of 40 pass. The 5 that fail ask a hostile guest for real work inside a 100 ms CPU budget, which emulation can't meet.

## Bugs the tests and benchmarks found

- **`.cwasm` cache race.** Nodes compiling the same component at once wrote the same temp file, so a reader could load a half-written one. Each writer now uses a unique temp name and renames it into place.
- **CPU hogs starved neighbours.** Spinning guests are interrupted every 5 ms, but they yielded straight back onto every worker. The fix is heavy lanes, and `cpu_hogs_leave_cores_for_short_requests` fails without it.
- **Pool exhaustion under churn.** An LRU-evicted object's store lives until its actor next runs, so live stores could outnumber `max_live_objects`, and a new object could find the pool full.
  - Now every store holds one of `max_live_objects` slots, released only after the store is dropped. A new object waits until an evicted object's pool slot is actually free, and instantiation never sees a full pool.
  - `churn_past_capacity_never_fails` checks the peak number of live stores. Without the slots, it sees 25 live stores against a limit of 16.
- **Renaming a file onto itself erased its quota usage.** `rename_at` credited the size of an existing target, but renaming a file onto itself is a POSIX no-op. Repeating it zeroed the mount's byte and file counts, so a guest could write past its quota.
  - The fix compares the file identity of the source and the target, and credits only a real replacement.
  - `rename_quota_accounting` checks both cases: the self-rename credits nothing, and a rename over another file still frees its space.
- **Objects ran out of fds at a third of capacity on Linux.** Each live object held three directory fds, so a 20,000 fd limit capped the node near 6,600 objects. Every cold start after that failed with `EMFILE`. The Mac's higher fd limit hid it.
  - `/app` is now one fd per app, shared read-only, and startup checks the fd and mapping limits.
- **A CPU kill could kill the next instance at birth.** The CPU charge was reset only when a request began. Re-instantiation after a CPU kill started with the old charge already over budget, so an epoch tick during instantiation killed the new instance (a rare 503 `instantiate-failed`). A new object was also charged its time waiting for a slot.
  - Each instantiation now starts a fresh charge. `cpu_kill_does_not_poison_next_instance` fails on its first round without the fix.
- **The counter could lose its count.** `/incr` truncated `/data/count` and rewrote it in place, so a kill in between left an empty file that read back as 0. Guest writes to `/data` then went to a temporary file renamed into place; since per-request commits (below), a plain write is all-or-nothing, and the counter writes in place.
- **The benchmark leaked nodes between scenarios.** Actors hold their node alive, so each scenario inherited the previous one's objects and fds. `Node::shutdown` now releases them.
- **Filesystem calls queued on one mutex.** wasmtime-wasi sends every filesystem call through tokio's `spawn_blocking`, which has a single lock. Under 256 concurrent writers it collapsed throughput 10×.
  - The fix is `allow_blocking_current_thread(true)`, which runs small local-disk calls inline and charges them to the object's CPU budget. It has to be set before the preopens, which capture it.
  - Result: concurrent `/incr` went from 3K/s to 27K/s, and warm GETs got 1.6× faster.

- **A signal could switch off CPU limits in a worker.** Stopping and continuing a jailed worker (strace did it) interrupted the epoch thread's sleep. The kernel resumes such a sleep through `restart_syscall`, which the first seccomp allowlist lacked, so the epoch thread panicked and exited. Without it, CPU budgets and deadlines go unenforced. `restart_syscall` is now allowed, and the process aborts if the epoch thread ever dies. `jail_blocks_escapes` stops and continues its probe mid-sleep.
- **HTTP/2 to the workers dropped every connection after 1,024 resets.** The first version used one HTTP/2 connection per worker. hyper's HTTP/1 front end stops reading a body once it has its Content-Length, which resets the HTTP/2 stream. h2 counts those over the connection's whole life and sends GOAWAY at 1,024. The demo saw it as a worker restarting 9 times. It was also slower: every request crossed a shared connection task in each process. Links replaced it.
- **Pooled links weren't reused.** The same early stop meant a link's response body was dropped before it saw its end, so the connection was closed instead of returned to the pool. The profile showed a socket closed per request. A link now goes back to the pool as soon as its body reports `is_end_stream`.
- **A jail rule could vanish silently.** The Landlock crate skips a path it cannot open, so a `static_dir` the app's uid couldn't reach simply lost its rule. The worker then failed to load, and the supervisor killed it before it could print why. The jail now checks every path it grants, worker stderr is tagged into the supervisor's log, and a failed start reports the worker's last words.
- **Jailed workers didn't limit open egress connections.** The per-object `max_outbound_concurrency` permit was held until the connection finished, and on the supervisor path that "connection" ended as soon as response headers arrived. A guest could hold any number of response bodies open. The permit now lives as long as the response body on both paths. An independent review found it; `open_response_bodies_count_against_outbound_concurrency` fails without the fix.
- **WebSockets, from an independent review.** Each of these was fixed with a test that fails without the fix:
  - A plain request with the events content type could forge any socket's events.
  - An empty published message cost nothing against the backlog, so a guest could grow the supervisor without limit (66 MB to 730 MB in 20 s).
  - A node-wide backlog, added for that, let one app's slow clients get other apps' sockets dropped. Each socket now has a budget of its own.
  - A burst of events from more sockets than `max_queue` was refused with `object-busy`, closing those sockets and losing their `DISCONNECT`s.
  - One app could hold every socket on the node.
  - Nothing was written to a socket while its event was in flight.
  - A drain that ran out of time left the old worker running beside the new version, so an object could be live in two workers. This dated from phase 3. The drain now kills the old worker at its deadline (`a_drain_that_runs_out_of_time_kills_the_old_worker`).
- **Debug test builds failed key-value tests on a slower machine.** Host work inside a guest call is charged to the guest's CPU budget, and unoptimized wasmtime copies a guest's `list<u8>` a byte at a time, so 8 MiB of writes took over 100 ms. Test builds are now optimized (`[profile.dev.package]` in `Cargo.toml`); release builds never had the problem.
- **Commits, from an independent review.** Each fix has a test that fails without it.
  - A guest could reach its own journal through `..`, plant records, and make recovery delete files outside its volume.
  - An ordinary `mkdir` on an existing directory recorded "remove this", so a failed request deleted the directory, or bricked the object if it wasn't empty.
  - A relative `data_dir` (the demo's) silently turned the journal off.
  - A replaced file's size was recorded per path, so renaming another file onto that path escaped the undo.
  - A kv write refused for space deleted the old value.
  - The journal had no bound.
  - My own crash test also found that replaying a recovery from the start could undo a rename twice and lose a file; recovery now trims each step off the log.
- **From a second review** (listed in [ISSUES.md](ISSUES.md)). Each fix has a test that fails without it.
  - A failed `rmdir` was journaled anyway, so rolling back created a directory the file count never saw; repeated, it went past `max_files` (N-5).
  - When the full journal refused a file's size record, the file was still marked as recorded, so a retried append went through with nothing to undo it (N-6).
  - Overwriting a file opened write-only failed: the journal read the old bytes through the guest's descriptor (N-7).
  - Truncating a file after removing it, while still open, freed its bytes a second time, so committed data could exceed the quota (N-8).
  - A key-value replacement that the full journal refused kept its quota adjustment, so usage could fall below what the volume held. Past the reservation, a failure now traps and the request rolls back (N-9).
  - The jail allowed every `fcntl` command. With `F_SETOWN` and `O_ASYNC`, a worker that got out of Wasm could have the kernel signal (and kill) the supervisor or other apps' workers whenever they share its uid (a supervisor not running as root). Only the five commands the test suite uses are allowed now (N-2).
  - The demo failed every request with `503 instantiate-failed`. A worker canonicalizes its object's directory, which for a relative `data_dir` reads the cwd, and the jail refused `getcwd`. The tests use absolute paths, so only the demo hit it (N-21).
  - LRU eviction could pick an object that was idle but had a job queued, or a socket event waiting for room. Its actor drains its queue before exiting, so the next request started a second actor for the same object on the same volume. Only an object with no work queued or waiting is evicted now (N-10).
- **Cold starts failed with `502 worker-down`.** A worker busy instantiating fell behind on its control socket, so the supervisor's non-blocking `sendmsg` failed with `EAGAIN`. Each failed request then made curl send another, which needed another new link. A dial now waits up to 5 s for the peer. The unit test `dials_wait_for_a_slow_acceptor` fails without it.

## Layout

| Path | What it is |
|---|---|
| `crates/nanovm/src/engine.rs` | Engine config, pooling allocator, epoch ticker, `.cwasm` cache |
| `crates/nanovm/src/app.rs` | Component loading and the linker |
| `crates/nanovm/src/object.rs` | The per-object actor: instance lifecycle, limits, CPU accounting, heavy lanes, poisoning |
| `crates/nanovm/src/table.rs` | The node: one app, object id → actor, LRU and idle eviction |
| `crates/nanovm/src/quota.rs` | Per-mount byte and file quotas over WASIp3 filesystem calls |
| `crates/nanovm/src/egress.rs` | Allowlist plus resolved-IP filtering for outgoing HTTP |
| `crates/nanovm/src/server.rs` | A node's HTTP surface: routing, private readiness and telemetry flush |
| `crates/nanovm/src/supervisor.rs` | The supervisor (front end, workers, egress on their behalf) and the worker's entry point |
| `crates/nanovm/src/jail.rs` | uid, Landlock and seccomp, entered by a worker before any guest code |
| `crates/nanovm/src/registry.rs` | Apps: bounded registry reads, Sigstore verification, version floor, digest cache |
| `crates/nanovm/src/proxy.rs` | `nanovm proxy`: DNS routing, tag resolution, forwarding |
| `crates/nanovm/examples/dev-registry.rs` | A loopback registry with the guests published, for dev mode |
| `crates/nanovm/src/link.rs` | HTTP/1.1 over socketpairs passed across a control socket |
| `crates/nanovm/src/kv.rs` | `wasi:keyvalue` on the object's volume |
| `crates/nanovm/src/journal.rs` | Per-request commits: the undo journal, commit and crash recovery |
| `crates/nanovm/src/ws.rs` | WebSockets: the proxy's sockets, WebSocket-over-HTTP events and per-object channels |
| `crates/nanovm/support/load.rs` | In-process load generator shared by the benchmark and the tests (not part of the node) |
| `crates/nanovm/src/main.rs` | The CLI: `serve`, `proxy`, the jailed `worker`, and `run` for local development |
| `guests/sdk` | `nanovm-sdk`, the Rust guest SDK ([quickstart](guests/sdk/README.md)) |
| `guests/` | The test guests `counter` and `hostile`, built on the SDK (wasm32-wasip2 with wasip3 bindings) |
| `examples/chat` | A live multi-room chat built on the SDK, run with `examples/chat/run.sh` |

## Known limits of this phase

- **Inline filesystem calls can't be interrupted by the epoch.** Each one is bounded by the quotas and charged to the guest's CPU budget.
- **The CPU budget is wall time inside each poll of the guest.** It inflates when the host is oversubscribed, which errs on the side of killing hogs early.
- **Without root, apps share the supervisor's uid.** Landlock and seccomp still separate them.
- **A worker's stderr lines are passed on whole up to 4 KiB**; longer ones are split.
