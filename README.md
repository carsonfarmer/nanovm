# nanovm

A wasmtime-only host for untrusted WASIp3 HTTP components, run as Durable-Objects-style objects:

- Each `(app, object-id)` has exactly one live instance, which serves one request at a time.
- Each object has its own memory cap, CPU budget, deadlines and handle cap.
- Each object has its own quota'd filesystem: scratch `/tmp` and a persistent `/data` volume.
- Each app has an egress allowlist, within the operator's.
- Each object has a composed Spin KV store in its volume, and each app its host-injected `wasi:config` values.
- Objects accept WebSockets and publish to their own sockets.
- Memory is lost on eviction; `/data` survives eviction and restarts.

Apps are signed artifacts in OCI registries. A proxy finds each host's app in DNS; each app runs in its own jailed worker process, under a supervisor that runs no guest code and owns the network (see [The jail](#the-jail)). [PLAN.md](PLAN.md) has the design and what comes next.

**Current scope (2026-11-08):** single-node durable acceptance and rejected-request
rollback, tested offline backup/restore, and opt-in local committed-state capture.
Shipping committed versions to a store, and with several nodes remote
acknowledgement and restore on takeover (below), are tested in process, not yet in
production. One node acknowledges a write once it is durable on its own disk.
Runtime is 5,320 lines, above the under-5k goal. Older checkpoint
counts and candidate designs below are historical, not current delivery claims.
The line goal guides simplicity; it is not a hard limit or correctness tradeoff.

`cargo run --locked -p nanovm --example committed_state` runs W-9's bounded
storage-only experiment: immutable versions from the real journal, independent
receiver processes, ordered/idempotent receipt, interrupted-transfer retry and
fresh-root recovery after deleting the source. It also demonstrates that a locally
accepted but unsent write is lost. This is not a runtime replication feature,
remote-host-loss proof or an acknowledgement policy; see [the contract and remaining
gaps](PLAN.md#w-9-contract-and-bounded-proof-2026-11-08).

**Capture and shipping:** optional node `[capture]` configuration captures a
version at both dirty commit gates: the whole volume every `snapshot_every` (16)
versions, otherwise what the commit changed (listings of changed directories, new
or moved files whole, files changed in place as their changed byte ranges). Versions
are staged before the journal's commit decision and published under
`objects/<app>/~committed/<object>/`; recovery discards a rejected one or completes
an accepted one before new work. A commit with capture costs 2 more syncs. With
`store` set (a directory, `file:///path`, or an object store URL as for `leases`,
e.g. `s3://bucket/prefix`, credentials and region from `AWS_*`), the supervisor (or
an in-process node) ships each object's versions in order. Each is put once at
`<app>/<object>/<sequence>-<sha256>.tar`, a key naming its bytes, and counts only
once the object's head, `<app>/<object>/head`, names it; the head moves one version
at a time, each following the last, by a write conditional on the head read
(`If-Match`; a lock and a rename on a directory store). Shipped versions are removed
here but the last. `max_versions` and `max_mb` bound what waits here: with the store
down, writes are refused once they are reached; nothing is silently dropped. A
commit is acknowledged once durable here unless `remote_ack = true`, which also
waits (2 s at most, then fails the request: the commit still stands locally) for the
head to name its version; with `leases` a `store` is required, and a config file's
`remote_ack` defaults to true. Versions carry the lease epoch they were committed under. With leases,
a node claims an object's head at its lease's epoch before serving the object, and
ships only under its live lease, once it has claimed the head with it, versions that
follow the head: a node that lost the object can't make a version count, however
late its upload lands (W-90). The claim also makes the node's copy of the object the
head's, restoring it from the store if need be, so a takeover never serves state
older than the head. `nanovm::ship::restore` rebuilds an object from the store on a
fresh root, from the last snapshot before the head, and keeps its last version, so
history goes on. Capture defaults off. Not yet: remote retention (every version
stays in the store). Do not manually prune history or
disable capture on a root with history. Tests exercise both commit gates,
process-exit and failed-sync recovery, history exhaustion, random change sequences
restored at every version, shipping, outages and restore on a fresh root; these do
not certify physical power loss or remote hosts.

## Build an app

One command builds and runs an app on your machine, one publishes it. Rust apps use
[nanovm-sdk](sdk/rust/README.md) (one async function from a request to a response);
JavaScript apps are a Workers-style module ([js/](js/README.md)). Once:

```bash
rustup target add wasm32-wasip2
cargo install --locked wac-cli@0.12.0                # composes the key-value provider into apps
git clone https://github.com/carsonfarmer/nanovm
cargo install --locked --path nanovm/crates/nanovm   # the CLI: a release build of the node
```

Then an app, in a directory next to the clone:

```bash
cargo new --lib hello && cd hello
cargo add --git https://github.com/carsonfarmer/nanovm nanovm-sdk
printf '\n[lib]\ncrate-type = ["cdylib"]\n' >> Cargo.toml
cp ../nanovm/examples/hello/src/lib.rs src/       # the 7 lines in sdk/rust/README.md
../nanovm/tools/build-app.sh --run                # cargo build, compose kv, nanovm run hello.wasm
curl http://127.0.0.1:8080/alice/                 # alice has had 1 visits (from another terminal)
```

`nanovm run` serves `/{object}/…` with the node's own request path, holds WebSockets as the
proxy does, prints a line per request and what the app prints, and restarts when the `.wasm`
changes (run `tools/build-app.sh` again after an edit). It is for your own code on your own
machine: loopback only, no registry, signature or jail. `nanovm publish hello.wasm
ghcr.io/you/hello:1.0.0` checks the app as a node will, pushes it as nodes pull it ([Apps](#apps)),
and prints how to sign it and the TXT record that routes a host to it.
[examples/chat](examples/chat) is a live multi-room chat on the same SDK. A recorded run and the
reasons for each choice: [notes/onramp.md](notes/onramp.md). Twenty-two more examples, from a URL
shortener to a job queue, each with an end-to-end test: [notes/examples.md](notes/examples.md).

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

CI reports runtime size (currently 5,305 nonblank/noncomment lines before test modules, down from 5,315 at W-2 delivery); the hard LOC budget is suspended while correctness takes priority. Egress uses the existing scope-guard library for call cleanup and checks DNS results directly without another collection; observability coverage and bounds are unchanged. Guest and infrastructure logs share one bounded record-emission path. Collector responses are bounded to 4 KiB and passed intact to the SDK, including partial-success reports. The CLI writes SDK warnings to stderr rather than back into the failing export stream; library embedders install their own diagnostic subscriber.

The whole-runtime simplification review is in [PLAN.md](PLAN.md#runtime-simplification-analysis-2026-11-03): proposed ownership changes make the actor own each request lifecycle, storage own complete mutations, and one dependency-call path own tracing/admission/body lifetime. A stock-WASI selective-override experiment may remove filesystem forwarding machinery. These are unimplemented designs, not measured savings or a LOC ceiling. Reviewed Rust and grilling skills live in `.agents/skills/`. The registry version-switch test now verifies tiny file/KV persistence and actual worker lifetime instead of making bulk quota writes under a 100 ms CPU budget or comparing client scheduling order. Repeated test failures must be fixed, not accepted because reruns pass; [N-32](ISSUES.md) separates the demonstrated fix from unconfirmed historical failures.

One request span lasts through the final post-return commit. It carries HTTP method/path (no query), status/error, cold-start status and queue, instantiate, guest, commit and post-return durations in milliseconds. `nanovm.response.duration_ms` measures time to the response headers; span duration also includes subsequent work. Calls between objects remain child spans. Only immutable context crosses tasks. The tracing feature, metric exporters and local request histograms are removed; OpenTelemetry's SDK owns recording, batching and export. Requests record even when an incoming parent is unsampled: sampling belongs downstream. Guest-defined spans/enrichment are follow-up W-17.

Requests use SERVER spans; inter-object and external HTTP calls use CLIENT spans with bounded method/path, response status and errors. External calls propagate W3C trace context and record destination host/port; object calls record the target object. Queries are omitted from spans and egress-denial logs. A failed dependency is visible even when its caller handles the failure successfully. Client 4xx/5xx responses are errors; server 4xx responses are not automatically failures. Call duration ends when response headers arrive. This follows the [OpenTelemetry HTTP conventions](https://opentelemetry.io/docs/specs/semconv/http/http-spans/) within the [Honeycomb wide-event model](https://docs.honeycomb.io/get-started/observability/concepts/instrumentation).

Dependency calls cancelled before response headers record `error.type = cancelled`. External HTTP errors use status codes, not untrusted upstream `x-nanovm-error` headers. Request-scoped runtime diagnostics (traps, quota/egress denials, rollback and instantiation failures) carry explicit trace/span IDs. Infrastructure-only events stay unparented. A failed final commit records `error.type = post-return-commit-failed` and a correlated, bounded I/O diagnostic while preserving the already-sent HTTP status. A gated test component verifies actual post-return calls, stderr and final-commit failures without changing published guests.

`proxy.request` and `supervisor.request` spans cover forwarding through response headers, including pre-worker failures. They parent the worker's `request` lifecycle span. The proxy also accepts `telemetry_endpoint`; both default to the same Collector base URL. Readiness traffic is excluded. For counts and latency, select the layer you mean: proxy for client-facing requests, supervisor for node ingress, `request` for object execution. Summing them would double/triple count. Direct HTTP load now produces two spans per ordinary request; proxy traffic produces three. The 256-span batch, 2048-span queue and 1 MiB body bound are our implementation choices, not OTLP limits; the protobuf payload is the standard SDK wire format. The ~64k complete-export requests/s target is not an achieved production baseline.

Infrastructure uses structured OTLP logs (`event.name = nanovm.snapshot`), not artificial request spans: memory, uptime, live stores/objects, phases, resource-limit counters, worker identity/state and restarts/link errors. Runtime events use `event.name`; guest logs use `log.iostream` and trace/span IDs. Derive request counts, latency distributions and SLOs from request events in the Collector/backend. Bounded queues can drop events under overload or collector outage; these are not an exact accounting ledger. Derive exact aggregates before tail sampling, or use a backend's supported weighted queries; a sample-rate field alone does not correct every sampling policy. SDK shutdown exports pending records off the async runtime thread; abrupt process kills can still lose buffered telemetry.

The measurements above precede the final hot-path optimization. A native CPU profile identified repeated OCI reference parsing; a bounded per-thread cache now stores only pure parse results (16 entries, inputs ≤1024 bytes), never host, trust, version floor or authorization. Common span attributes reserve enough capacity upfront. All supervisor/worker/proxy/dependency spans remain; the owner rejected removing supervisor observations for throughput.

Post-commit controls use three alternating 400,000-request runs per variant, c256 over 100 warm counter objects, one frontend, identical release build flags, oha 1.16.0 and official Collector 0.164.0 with derived metrics. Both applications receive the same CPU allocation. With physical SMT core pairs separated (application CPUs 0–1, Collector/load generator 2–3), main/optimized aggregate throughput is **18,631 / 18,227 requests/s (2.17% lower)**. Active node+worker CPU is 103.45 / 105.00 µs/request, with Collector another 14.70 µs/request. Fully colocated on all four logical CPUs, main/optimized is **27,548 / 25,459 requests/s (7.58% lower)**; application CPU 109.42 / 110.64 µs/request, Collector another 13.62. Each optimized matrix returned all 1.2m HTTP200 responses and exported all 2.4m spans. A sustained 2m-request run delivered all 4m spans at 25,561/s, p99 25.84ms; long-path/trace-state runs also delivered all spans, including explicit queue overload.

The isolated application result meets the few-percent overhead gate for the recommended separately budgeted Collector deployment. Its additional CPU is real; the 7.58% colocated result does not meet that gate. W-2 host instrumentation is complete. Runtime is 5,315 lines versus main's 4,955 before W-2 (+360, 7.3%), using CI's nonblank/noncomment count before test modules. These are workload/environment observations, not universal capacity or durable-backend validation. HANDOFF retains earlier results (including the more favorable pre-commit 0.74% isolated / 4.45% colocated), exact reproduction steps and rejected experiments. The remembered ~64k is in-process, not an HTTP baseline.

### Collector and operator checks

**Recommended deployment:** run a separately budgeted Collector as a standalone service, following the standard [OpenTelemetry gateway pattern](https://opentelemetry.io/docs/collector/deploy/gateway/). Provision CPU/memory independently of the application; dedicated physical cores on the same host are also valid. This separates backend credentials/policy and resource contention from request serving. Our same-host physical-core test validates application overhead; it does **not** test a remote network or durable backend. Across hosts, configure private receiver ingress and TLS/access controls, then repeat completeness, latency and outage tests for that topology. The supervisor—not jailed workers—connects to the configured OTLP/HTTP endpoint. Keep SDK queue-loss/export alerts and separately monitor Collector saturation; adding a Collector does not make telemetry lossless.

For same-host isolation, check physical-core/SMT topology: assigning different logical CPUs that share one physical core does not isolate CPU contention. Size Collector CPU and memory for the chosen processors and backend, and account for those resources separately from nanovm. W-2 was delivered on main in `49d0ae4`; the documented 2.17% isolated result does not imply the same overhead when the Collector shares the application's CPU budget.

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
  - `fcntl` for descriptor flags and OFD file locks, plus `flock`: no `F_SETOWN` or `F_SETSIG`, which would have the kernel signal another process of the same uid
  - `socketpair` for a local stream pair only: a datagram socket could `sendto` any named or abstract socket on the host

  Anything else fails with `ENOSYS`. With no way to create a socket, a worker's only channels are its two control sockets to the supervisor.
- **`no_new_privs`**, and the worker dies with its supervisor.
- **cgroup v2** (optional, `cgroup = "<delegated dir>"`). Each worker runs in a cgroup of its own, `<cgroup>/worker-<n>`, which it enters before it runs and which is removed once it has exited. Its app's `[worker]` memory, CPU and pids limits are set on it before the worker is given the app, and they cover the whole process: also the engine a worker builds before it has an app (about 2 KiB a slot of `max_live_objects`, half of it kernel memory for the stacks' mappings: 19 MiB in all at 8,192 slots), since memory stays charged to the cgroup a process was in when it allocated it.

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
app=ghcr.io/org/app:prod; trust=<issuer> <subject>; trust=…; alias=www.example.com; min=1.2.0
```

That is the repository and the tag deployed, each identity whose signature is accepted, the other names that may serve the app (below), and an optional version floor. Whoever controls the domain controls its routing; deploying moves the tag, and rolling back moves it back.

**The operator decides which hosts are served and who may deploy.** Two lists in `proxy.toml`, both empty by default, so a new proxy serves nothing:

```toml
hosts = ["example.com", "*.apps.example.com"]
signers = [
  "https://token.actions.githubusercontent.com https://github.com/acme/*",
  "https://accounts.google.com alice@example.com",
]
```

- `hosts`: a name, or `*.name` for any name under it at any depth (`a.apps.example.com`, not `apps.example.com`). Any other host gets 403 before its record is looked up.
- `signers`: `<issuer> <subject>`, the subject exact or ending in `*` for any subject that starts with what comes before it (end the prefix with `/`: `https://github.com/acme` would also match `https://github.com/acme-evil/…`). A record's `trust=` entries are cut to the ones a signer matches, and only those go to the node, so an app signed by any other identity is refused even if its record trusts it. A record left with none gets 403.

**Aliases.** An app belongs to the name its record is found at, after CNAMEs (W-15), so `_nanovm.www.example.com CNAME _nanovm.example.com` makes both hosts one app with one set of data, if example.com's record says `alias=www.example.com`. A name the record doesn't list gets 403: otherwise any domain could CNAME onto another's record and serve that app, with its data, under its own origin, and keep its users' cookies. To run someone else's repository under your own name, write your own record; that is a separate app with its own data.

The proxy:
- asks a DNS-over-HTTPS resolver (`resolver`, Cloudflare's by default) and keeps the answer for its TTL; an answer other than NOERROR or NXDOMAIN (SERVFAIL, say) is a failure, not "no record";
- asks the registry which digest the tag names, and keeps that for `tag_ttl_s` (30 s);
- looks each host or tag up once however many requests ask at once, and keeps a failed lookup's answer for 5 s rather than asking again;
- remembers up to 10,000 hosts and tags with an answer (an app, a digest), and apart from them up to 1,000 without one: no record (kept 30 s), refused, failed, or being looked up. While 1,000 of those are fresh, a host it doesn't know gets 503 rather than a lookup. So names made up under a `*.` entry cost at most 1,000 lookups at a time, however many come, and never push out a host that has an app (`made_up_names_cost_bounded_lookups_and_keep_live_routes`);
- drops every `nanovm-*` header a client sent, and hop-by-hop headers (`Connection` and what it lists, `Keep-Alive`, `TE`, `Trailer`, `Proxy-Authorization`), names the app to the node (below), and forwards HTTP requests; it holds WebSockets itself and forwards their events;
- speaks plain HTTP: TLS is left to a terminator in front (Caddy, a cloud load balancer, Cloudflare), which also renews and reloads certificates;
- gives a client 10 s for each request's headers (or between requests), and waits out running out of file descriptors rather than exiting (the node's front end does too);
- makes a client keep sending a request's body: it may keep the proxy waiting 10 s in all, plus a second for every 4 KiB it sends, or gets a 408 (`body-timeout`) and its connection is closed. A body trickled a byte at a time is cut after about 10 s; an upload at 4 KiB/s or faster never is. The node applies the same rule as it reads a body, so a client that reaches it directly is held to it too;
- refuses a request head over 64 KiB (431), as every hop does (node, worker links, egress responses).

A host with no record is a 404; a host the proxy doesn't serve, an alias its record doesn't list, or a record trusting no allowed signer, a 403; a malformed record, or more than one, a 502.

**Many nodes.** The proxy may place objects on several nodes: list them in `nodes` (each `host:port`, as that node's `name`). An object's home is chosen by rendezvous hashing, and the nodes keep leases on their objects in an object store (`leases = "s3://bucket/prefix"` on every node, with `name`; credentials from `AWS_*`), so an object has one live instance across nodes and a node that lost its lease can't commit. A node that doesn't hold an object answers 421 and the proxy tries the one that does (or the next), for up to `failover_ms`. When a node stops, its objects move after its `lease_ms` (10 s; each node's session record carries its own, which the others wait out). With leases a node ships its objects' committed versions to `capture.store` (required), and answers a write once it counts there; the node taking an object over restores it from there before serving it, so a failover loses no answered write and serves no older state. A node that took a request but hasn't answered within `failover_ms` gets the client a 504 (the request may have run there); the proxy never sends it again. Calls between objects then go through the proxy's private endpoint (the node's `proxy`). See `notes/multinode.md`.

DNS as the routing table has limits: a record is as trustworthy as its resolver (hence DNS-over-HTTPS, and apps keyed by the name that owns their record, so a record reaches no other name's app); a change takes up to its TTL to arrive; a host serves one app (several hosts can share one through CNAMEs its record lists); the table is public; and a cold lookup adds a round trip.

## Apps

Every app comes from a registry. The proxy names it with each request:

```
nanovm-app: ghcr.io/org/app@sha256:…        # by digest only
nanovm-trust: <issuer> <subject>[, …]        # whose signature is accepted
nanovm-min-version: 1.4.0                    # optional floor
```

The node trusts these headers because only the proxy can reach it, and strips them before the guest sees the request. A request without them reaches only the node's own `/_nanovm/` routes.

**An app is a DNS name's deployment of a repository.** Its name on the node (`example.com_ghcr.io_org_app-<hash>`), its volumes and its uid come from the name that owns its `_nanovm` record (`nanovm-host`) and `registry/repository`, so every version that name runs, whoever signed it, sees the same data. The owner is the name the record was found at after CNAMEs, so `_nanovm.www.example.com CNAME _nanovm.example.com` makes both hosts one app, if the owner's record lists the alias (`alias=www.example.com`; see "The proxy"). Whoever controls that name chooses the version it runs, so no domain can name another's repository and switch its app to an old version on its data (a review finding), and no domain can serve another's app and data under its own name without that name's record listing it. Only those who can push to a repository can put a signed artifact in it, so naming another identity never reaches a different app's data. The registry is written one way only: lower case, without `:443`. A team signs from CI, or lists each member's identity.

What happens on the first request for a digest:
1. **Signature first.** The supervisor pulls the manifest anonymously, then the Sigstore bundles attached to it as referrers. It verifies them offline against Sigstore's public-good trusted root, embedded in the binary: an exact issuer and identity from the trust header, over the manifest digest. It also checks the version (`org.opencontainers.image.version` in the manifest) against the floor. Nothing else is downloaded until that passes.
2. **Then the blobs.** The supervisor fetches the config blob (the app's limits and env) and the `application/wasm` layer, checks them against the digests the signed manifest pins, and caches them by digest under `data_dir/registry`.
3. **Then the worker.** The app's limits are clamped to the operator's `limits` (an instance's resource table also holds at most `memory_mb × 16` handles, since each may hold a 64 KiB head on the host: a header map, or a request's headers with the method, scheme, authority and path the guest set on it, which together may not pass 64 KiB; 256 at 16 MiB, 1,024 at 64 MiB), and its worker gets `worker`. Its egress reaches only what both its `egress` and the operator's `limits.egress` allow: an app asking for `*.com` gets only the hosts the operator lists, and with no operator list (the default) none; private addresses stay refused in both. The component goes to a fresh jailed worker, which compiles it.

**A running version under a new policy** (other identities, or a higher floor) is checked against it before the worker serves it; a refusal leaves the version running. The supervisor remembers up to 16 accepting policies per running version, then forgets them and checks each again as it comes.

**How many apps.** A node runs at most `max_apps` workers (100). A new app takes the slot of the least recently used idle app, one with no requests in flight, whose worker is retired (its next request starts it again). Open proxy sockets do not pin workers. With none idle, the new app is refused (502) until one is. The node forgets apps with no worker once it knows of `2 × max_apps`. Since the DNS of any host the proxy serves can name an app (any name under a `*.` entry in its `hosts`), this is what keeps a node's processes, uids and memory bounded; it does not stop one party from keeping every slot busy, which needs per-client limits in the proxy.

**Bounds.** Every registry read is bounded: a manifest, config, token or signature at 4 MiB, a component at `max_component_mb` (64), and 16 signatures per load. At most four registry fetches run at once across the node, each within `load_timeout_ms` (60 s) in all, so a registry that answers a byte at a time holds nothing for long. Digests are checked before they name anything. Registries, and their redirects and token servers, must be on public addresses: a deploy names the registry, so it must not reach the node's own network.

**Dev mode** (`dev = true`, refused unless the binary was built with `--features dev` and `listen` is loopback, so a default release build can't be put in dev mode) is for local development: apps need no signature, registries and egress entries may be on private addresses, and loopback registries are plain HTTP. The tests and the demo use it with a registry on loopback; signatures are tested against the CI-signed guests on ghcr.io.

**Switching versions.** A request naming a digest the app's worker isn't running switches the app. The new version is fetched and verified while the old one keeps serving. The old worker then drains: no new requests, and in-flight ones (and their response bodies) finish, bounded by its longest request time. It is then killed, and only once its process has exited does the new worker start. Both versions share the app's volumes, so an object is never live in two workers. A switch, once begun, finishes even if the request that began it goes away.

**Uids.** An app's uid is the owner of its `objects/<app>` directory, or the next free one for a new app. It is stable across restarts, with no separate record.

**Publishing.** `nanovm publish app.wasm <registry>/<repository>:<version> [--manifest nanovm.json]` pushes an app as nodes pull it: artifact type `application/vnd.nanovm.app.v1+json`, its `nanovm.json` as the config, the component as its one `application/wasm` layer, the tag as its version. It first checks the component as a node will (imports, handler export, manifest), and prints the digest, how to sign and the TXT record. Registry credentials come from `NANOVM_REGISTRY_USER`/`NANOVM_REGISTRY_TOKEN`. [.github/workflows/publish-app.yml](.github/workflows/publish-app.yml) is a manual template that builds one app, publishes it and attaches a GitHub artifact attestation, signed with the workflow's short-lived identity; [publish-guests.yml](.github/workflows/publish-guests.yml) does the same for the test guests with `oras`. No key exists anywhere. A person can sign from a laptop with `cosign` and a browser sign-in (not yet tested end to end, W-102).

## Key-value and config

**`spin:key-value/key-value@3.0.0`** is supplied by a [composed Wasm provider](components/keyvalue/README.md) over the object's filesystem. The counter and hostile guest packages compose this provider; `tools/build-app.sh` composes it into any app that imports it. Bucket `b` is `/data/.kv/b`; each key is a file named by unpadded base64url. Native synchronous KV, atomics and batch are removed.
- Values count against the object's disk budget and keys against its file limit, like any other file.
- Replacement writes a temporary file and renames it over the key. It needs room for both values and a temporary entry; old descriptors retain the old value. Only the host commits, together with all the request's file/SQL changes.
- Values are bounded to 1 MiB. Key enumeration streams with backpressure and an error completion. A combined 32-store/listing cap lasts through completion delivery; host resource limits also apply.
- `wasi:filesystem` stays exposed. A guest can reach `.kv` through it; the store lists only well-formed key files, and refuses to write over anything else.

We adopt Spin's WIT, not its deployment/storage model: all valid labels are allowed within each object's isolated volume, without a `spin.toml` allowlist or shared cross-object store. There is no SQLite dependency for KV. Caller cancellation is deferred across a mutation; traps, deadlines and crashes roll back through the host.

Profiling found a large-value cost in Wasmtime 50's byte-by-byte component-list
adapter, not an inherent filesystem cost. An isolated bulk-copy experiment cuts
1 MiB GET from 3.149 to 1.600 ms (native 1.404 ms); it is not an enabled dependency
patch. The retained sibling-temp optimization removes extra directory syncs.
See the [measurements and limitations](components/keyvalue/README.md#follow-up-measured-costs-not-inherent-wasm-overhead).
Tentative changes must be visible and rejectable; an undo log is the current
implementation, not a requirement on future snapshots or replicated storage.

The [bounded backend comparison](experiments/fs-backends/README.md) measures the
actual journal against plain transactional pages and hash-addressed pages, outside
Wasm. Plain pages improve small writes/batches but slow warm reads and often write
more bytes; hashing helps shared-content batches, not unique large writes. The
experiment includes rejection, restart and process-death checks, but is not a WASI
filesystem replacement or production durability certification. Runtime is unchanged.

The [inode follow-up](experiments/fs-backends/README.md#inode-quota-and-cache-proof-q79-2026-11-04)
adds shared descriptor/rename/unlink semantics, logical quotas and bounded-cache
measurements for both candidates. Hash addressing is preferred when measured costs
are close; unique large writes remain slower. This is a flat-namespace proof, not
a production filesystem or a guarantee that cache size bounds total memory.

Integration is in progress on `hashed-fs-integration`; the production backend has
not switched. Acceptance requires mixed KV/SQL/raw-file tests through the real
WASI/jail path and measured total-code, latency/CPU and memory improvements.
The separate improvement branch has been assessed for reusable ideas (HANDOFF,
Q80); its combined scheduler/guest/runtime results are not filesystem benchmarks.
The storage experiment now uses an atomic orphan index rather than namespace scans, and
passes 818 injected mutation/commit/recovery I/O failures, including partial
writes. Six additional cases exhaust fixed database-file-length ceilings, reopen
under the same cap, reclaim shared-content orphans, and retain usable small
writes/deletes and exclusive locks. The ceiling bounds file length, not allocated
blocks or host memory; the cache target is not an RSS cap.
These are bounded storage checks, not production integration or physical
power-loss certification; [protocol and remaining gates](experiments/fs-backends/README.md#hardening-orphan-lookup-and-storage-failures-2026-11-05).

**Storage decision:** keep journaling after measuring both hash and optimized plain
pages. Plain pages improve small durable writes about 3× and match large-write
latency, but cold/recovery/churn remain 3–5× slower and replacement code is 294
lines larger. This rejects the current per-object redb design for adoption, not
page-based filesystems in principle. [Simple comparison, costs and reproduction](experiments/fs-backends/README.md#adoption-decision-retain-journaling-2026-11-05).

The journal improvement branch reduces idle memory from 143 to 119 KiB/object
and improves warm throughput about 15% in three matched runs. Lazy KV bucket
creation reduces a fresh read-only request from 2.550 to 0.690 ms; opening or
reading an empty bucket consumes no directory quota. Small durable writes and
large-value writes remain essentially unchanged. Cleanup saved 53 runtime lines,
but the complete performance/durability work ends at **5,082 versus 5,074**;
this is not a net code reduction. [Measurements and limitations](experiments/fs-backends/README.md#journal-follow-through-2026-11-05).
Page adapters remain only on the experimental branches.

The historical [runtime candidate](experiments/fs-backends/README.md#runtime-integration-checkpoint-2026-11-05)
was integrated on its experimental branch behind temporary `page-fs`, using stock WASIp3 bindings and one
file-backed transactional database for `/data` and `/tmp`. Use disposable separate
data roots; no migration or compatibility path exists. Its provisional `storage_mb`
limit (default 256) bounds database file length independently of logical `disk_mb`;
64 KiB is a cache target, not an RSS cap. Full candidate tests pass, including real
directory-sync failure and jailed capped-adapter locking. **It is not ready to
replace the journal:** repeated-value KV writes improve (128 B: 2.82 → 0.91 ms),
but unique 1 MiB PUT regresses 7.15 → 16.76 ms, as do cold/reopen throughput and
density. All replacement code would raise
runtime from 5,074 to about 5,424 lines; no reduction or overall simplicity win is
claimed. The linked protocol records review gaps and matched results.

The real-component stream regression now covers descriptor drop, unlink/replacement,
acceptance, continued reader/writer/directory-stream use, exact entry-quota reclamation,
rejection and restart. Premature inode release fails it; restored code passes.
This test-only follow-up does not change runtime LOC or the adoption gates.

**`wasi:config/store`** (0.2.0-rc.1) serves the `"config"` table of the app's `nanovm.json`. Environment variables (`env`) still work alongside it.

## Calls between objects

An object calls another object of its own app with an ordinary outgoing request to `http://nanovm/objects/{id}/{path}`, as a Durable Object calls a stub:
- The callee gets it as a request for `/{path}`, through its queue like any other, so it is still served one request at a time.
- The callee commits before it answers, so the caller only ever sees durable changes. Each object commits on its own: a caller that fails afterwards doesn't undo the callee's changes.
- Calls need no egress rule. They count against the caller's `max_outbound_concurrency`, and their bodies against the app's `max_request_body_kb`.
- **Cycles are refused.** With one request at a time per object, a call that waits on its own caller, directly (`a` calls `a`) or through other calls (`a` calls `b`, which calls `a`), could never be served. Each object records the objects its calls wait on. A call that would close a cycle gets `508 call-cycle` at once, instead of hanging until a deadline kills both requests and rolls them back. The check and the record happen under the table lock, so two objects calling each other at the same moment can't both get through.
- **The callee knows its caller.** A call arrives with `nanovm-callers: a,b,c`: the objects waiting on it, the calling object last. Only nodes set it. The caller's node writes it over whatever the guest put on its call, the proxy drops every `nanovm-*` header a client sends, and between nodes it keeps only this one on its private route. So a request without the header came from outside the app, and an object can keep a route to its app's objects, or to one caller, by checking it. Work after an answer calls with its own id only. A node reached without the proxy trusts what it is sent, as it does `nanovm-app`.

## WebSockets

The front proxy holds client sockets; the node only answers requests. Objects see sockets as ordinary requests, in [Pushpin's WebSocket-over-HTTP](https://pushpin.org/docs/protocols/websocket-over-http/), so a guest stays a plain HTTP handler and existing GRIP libraries work. No separate Pushpin process is needed.

Committed publishes travel from the supervisor to the proxy's private `publish_listen` (`127.0.0.1:8444`), at `POST /publish`; an in-process node (`nanovm run`, tests) delivers them to sockets of its own after the commit, and refuses any other `http://nanovm/` path with 404. Set the node's `proxy` accordingly (default `http://127.0.0.1:8444`). **Only nodes may reach this control port**, just as only proxies may reach the node. The supervisor names the app, the worker names the object; channels cannot cross either boundary. Publish bodies have a 30 s deadline.
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

## Backup and restore

`python3 scripts/backup.py` is an **offline, operator-only** wrapper around
[restic](https://restic.readthedocs.io/en/stable/). It adds no runtime endpoint or
host dependencies. Install Python 3.9+, restic 0.14+ and GNU coreutils on Linux.
Restic supplies encryption, authenticated content, deduplication and remote
repository support; nanovm supplies the app-lock boundary and restore drill.

1. Stop ingress, drain requests if possible, then stop the supervisor and **all**
   embedded writers. Disable automatic restarts for the entire backup. Holding
   existing app locks rejects lingering owners but cannot prevent a supervisor
   creating a new app. `--stopped` acknowledges this operational requirement;
   it does not stop processes. Keep the directory inodes stable.
2. Configure `RESTIC_REPOSITORY` and a securely stored `RESTIC_PASSWORD_FILE`
   (use absolute paths). Initialize a repository with `restic init` once. Use
   root for root-run jailed volumes; use the same UID for an unprivileged node.
   The repository must be outside the data root; off-node storage is needed
   to survive losing the node or disk. Store the password separately: losing
   it makes the encrypted backup unrecoverable. No credentials go into git.
3. Run `python3 scripts/backup.py backup /srv/nanovm --stopped`. Save its full
   snapshot ID and the deployment record. The wrapper locks all app directories
   before invoking restic and reads **all of `objects/`**, including `~journal`
   logs and retained files. Never exclude or manually clear journals: tentative
   data can remain after a crash. Scratch, compilation caches and registry caches
   are deliberately omitted. Only exit-zero backups receive the wrapper's
   completion tag; an incomplete restic snapshot is not an approved recovery point.
4. Run `python3 scripts/backup.py restore FULL_SNAPSHOT_ID /srv/nanovm-restored`
   with the repository credentials. The destination's parent must exist and be
   trusted; the destination itself must not exist, even as an empty directory or
   symlink. Restore verifies file contents and syncs the destination filesystem
   before returning success. On error, an incomplete destination may remain:
   do not serve it, and retry into another new root. Restore only trusted
   repositories; completion tags are an operational convention, not authorization
   against someone with repository write credentials.
5. Reinstall the **same nanovm build**, restore operator configuration, and point
   `data_dir` at the fresh root. Preserve numeric app UIDs/GIDs and the original
   `jail_uid_base` for root-run nodes. Keep the same app identity (DNS record owner
   plus repository), immutable OCI manifest digests, trust policy and guest config.
   Save those deployment records and ensure the signed OCI artifacts remain
   available separately: a data snapshot is not a deployment/package backup.
   The worker recovers the saved undo logs on first object access. Exercise reads
   and a new write/restart before switching ingress. **Fence the old node first**;
   a copied volume does not make two writers safe.

The snapshot recovers local committed state at quiescence, not external effects,
open sockets or lost publishes. Writes after the snapshot are outside its recovery
point. This is not live backup, point-in-time recovery, synchronous replication or
automatic failover. Keep several independently checked recovery points; choose
retention and `restic forget/prune` policy explicitly rather than deleting the
last known-good backup automatically. Run `restic check --read-data` periodically
and after repository transfer; encrypted storage is still sensitive tenant data.
Remote upload performance, backend durability and a second physical host have not
been tested here.

The executable drill uses the existing composed SQL/KV fixture: commit values,
kill a jailed worker after tentative SQL/file/KV writes, stop its supervisor,
back up, move the original root away, restore into a fresh root without caches,
verify rollback through HTTP, and write/restart again. Run it after building the
SQLite fixture with `bash experiments/sqlite-filesystem/build.sh --compose`:

```sh
env -u RUST_BACKTRACE cargo test --locked -p nanovm --test integration \
  sqlite_backup_restores_into_a_fresh_root -- --ignored --nocapture
python3 -m unittest discover -s scripts -p test_backup.py -v
sudo python3 -m unittest discover -s scripts -p test_backup.py -v
```

The Python checks use disposable repositories to cover active-owner refusal,
incomplete backups, missing repository content, journals, empty directories,
numeric ownership/modes and refusal to overwrite existing destinations. The
root invocation checks UID/GID preservation; the unprivileged one induces a real
permission-denied backup. Neither touches an operator's existing volumes.
The complete real-process drill also passes as root with jailed worker UIDs.
Six deliberate mutations are caught, including omitted journals and final sync.
Recovery delivery's main CI passed; its parallel branch run exposed an unrelated
max-app admission fixture race (N-43). That fixture now gates observed upstream
requests instead of assuming short guest sleeps survive cold startup.
CI runs both privilege variants of the CLI tests and the SQL restore drill;
orb setup installs restic idempotently. Runtime size remains 5,085 lines, with
89 additional operator-tool lines counted separately.

## Commits

A request's changes to `/data` (files, SQL and key-value alike) commit locally before its initial response is released. A guest error, trap, CPU/time limit or worker crash before commit rolls back those changes. An answer of any status commits. Held WebSocket publishes are released after their corresponding commit, but delivery is best effort: a crash or failed send can lose a publish after storage commits. This is not a durable outbox.

Outbound HTTP and calls to other objects execute immediately, not behind the caller's commit gate. Their effects cannot be undone by caller rollback. Post-return writes have a second commit; already-visible headers and body bytes cannot be recalled if it fails. These are narrower guarantees than a general network output gate.

A host I/O failure during commit, a crash at its commit marker, or a lost response can leave the caller unsure whether the request committed. Do not interpret every error as a rollback: use application-level operation IDs for idempotent retries. Durability assumes a filesystem and device that honor successful file and directory syncs; process-crash tests do not certify physical power-loss behavior.

Persistence between backups depends on the original storage surviving. Offline backup/restore is described below; there is no replication or automatic failover. App admission takes a nonblocking exclusive lock on its objects directory before scratch cleanup or journal recovery. Supervisor directory preparation takes the same lock before changing ownership. Locks last through all `App` owners: embedded callers must drain and drop their `Node` and any `App` handles before reopening. A final actor may still retain ownership briefly after `shutdown()` returns; admission fails closed until it exits.

This is cooperative local exclusion on a stable directory inode, not distributed fencing or an atomic supervisor-to-worker reservation. Keep one supervisor per data root; cgroup provisioning and noncooperating writers are not fenced. Do not replace a locked directory. Per-object logical quotas do not cap physical blocks or the aggregate storage of cold objects. RAM may cache durable data or stage tentative work, but memory-only acceptance would not preserve these guarantees.

The supervisor syncs app-directory ancestors before launching a jailed worker;
the worker's journal cannot reach those ancestors after confinement. Embedded
`Node` callers must arrange durable storage-root preparation themselves.

**How:** an undo journal (`journal.rs`), as SQLite's rollback journal, under our `wasi:filesystem` host (`quota.rs`).
- **Before each change, the host records what the change would destroy, and fsyncs it** (records made back to back, such as a rename's and the file it replaces, share one fsync):
  - a file's size, the first time it is written;
  - the bytes a write or truncation overwrites;
  - "remove this", for a new entry;
  - "rename it back", for a rename;
  - "recreate this", for a removed directory.

  A removed or replaced file is renamed into the journal, not deleted.
- **Then the change happens in place,** so open files and `mmap` keep working, and SQLite can sit on `/data`.
- **Commit** (before the response is released, and again after post-return work) fsyncs what changed, up to 16 paths at a time, then writes a new random key into the log's first block and fsyncs it: records are signed with the key, so none is valid any more. The log itself is made once per object and kept (at most 68 KiB between requests), so a commit rewrites a block that already exists instead of deleting a file and syncing its directory. The commit runs on a blocking thread, not the object's. Held publishes are sent after it.
- **Rollback** has one path. A failed request ends its instance, and every instance start replays any log left behind backwards, so a crash is handled the same way. Recovery trims each step off the log as it completes, so a crash during recovery resumes where it stopped.
- **Costs:**
  - One fsync per change that destroys data (first write to a file, overwrite, truncate, remove, rename), and one round of fsyncs per committing request: one per changed path, then the key. A counter's `POST /incr` (create, write, rename over) costs 5 device flushes. Requests that change nothing pay nothing. A guest's own `sync` and `sync-data` do nothing: its writes are durable at commit and rolled back otherwise, synced or not.
  - Waiting for the disk isn't charged to the guest's CPU budget, and neither is the journal's own CPU (records and commit).
  - The byte quota counts logical file lengths, not allocated filesystem blocks or metadata. While a request runs, the log and retained files may each hold as much again: at most three quota-sized logical content budgets. Small files and filesystem metadata add physical overhead; `max_files` independently bounds directory entries. Size the backing disk for both limits, not just `3 × disk_mb`.
- **Safety:**
  - Guest paths reach only the volume. The journal sits beside it, out of reach.
  - A recovery error preserves the failing undo and refuses to start the object. Repair the storage problem and retry; never delete the journal to force startup. Already-applied or never-performed operations remain safe to replay.
  - A commit failure poisons that journal instance, so retrying cannot falsely report success. Recovery syncs both parents of a reversed rename before retiring its record.
  - A failed rename restores its staged destination and storage accounting, keeps dirty-source sync tracking, and durably cancels its rename undo before paths can be reused. If restoration or cancellation fails, the request cannot commit and must recover. Cancelled records still consume log space.
- **One version, v1:** the log's first block holds its tag (`nanovmj1`) and key, and every committed version says `"format": 1`. Recovery refuses a log, and a restore a version, that isn't v1, rather than taking it as empty. There is exactly one version of nanovm, v1: data written by earlier builds is not supported, there is no upgrade path from them, and operators start from a fresh data directory ([AGENTS.md](AGENTS.md), rule 1).
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

The supervisor and each worker check these at startup and refuse to start, naming the fix, rather than failing requests at runtime. The limits apply per worker process, so per app:

- **File descriptors:** each live object holds 2 (`/data` and `/tmp`). The check needs about `2 × max_live_objects + 1024`. The worker raises its soft limit to the hard limit itself, before it jails itself; the demo's 8,000 objects per app need `ulimit -n` of at least 17,088.
- **Memory mappings:** about 8 per pool slot. The default `vm.max_map_count` of 65,530 fits roughly 7,600 objects; 8,000 needs `sysctl -w vm.max_map_count=68352` or more.
- **Address space:** the engine reserves each pool slot's 4 memories (`memory_reservation_mb` + 64 KiB each), stacks and tables up front, about 522 GiB at the defaults. Under `ulimit -v` or `LimitAS=` below that, `nanovm serve` refuses to start with the number; remove the limit or lower `max_live_objects` or `memory_reservation_mb`.
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
- key-value: persistence, shared filesystem byte/file quotas, bad names, 1 MiB read/write bounds, streamed keys/backpressure and completion lifetimes, cancellation-safe replacement, raw-file visibility and jailed execution. Optional probes cover mutation recovery cuts, 1,000 same-request replacements and 10,000-key scale.
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
- **The counter could lose its count.** `/incr` truncated `/data/count` and rewrote it in place, so a kill in between left an empty file that read back as 0. Guest writes to `/data` now go to a temporary file that is renamed into place (`util::write_file_atomic`).
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

W-18's [Spin KV provider](components/keyvalue/README.md) replaces native KV on
this branch. The obsolete synchronous experiment is removed; its negative ABI
result remains recorded in HANDOFF Q73. Env/config stay injected.

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
| `components/keyvalue/` | Composed Spin async KV over the object's filesystem |
| `crates/nanovm/src/journal.rs` | Per-request commits: the undo journal, commit and crash recovery |
| `crates/nanovm/src/ws.rs` | WebSockets: the proxy's sockets, WebSocket-over-HTTP events and per-object channels |
| `crates/nanovm/support/load.rs` | In-process load generator shared by the benchmark and the tests (not part of the node) |
| `guests/` | `counter`, `hostile` and shared `util` (wasm32-wasip2 with wasip3 bindings) |

## Known limits of this phase

- **Inline filesystem calls can't be interrupted by the epoch.** Each one is bounded by the quotas and charged to the guest's CPU budget.
- **The CPU budget is the CPU time of the thread polling the guest** (user and kernel, so the guest's own file calls count; waiting for a core or the disk doesn't). An overloaded node slows a request down to its wall-clock deadline (504) rather than failing it for CPU ([N-640](ISSUES.md)).
- **Without root, apps share the supervisor's uid.** Landlock and seccomp still separate them.
- **A worker's stderr lines are passed on whole up to 4 KiB**; longer ones are split.
