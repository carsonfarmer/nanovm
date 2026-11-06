# Operator and measurement cookbook

Guidance only. Queries below are aligned with the existing
[Collector reference](../../demo/otel-collector.yaml) and [README](../../README.md),
which pin `otelcol-contrib` 0.164.0. These queries have not been executed in
Prometheus or Grafana. [Batch 2](BATCH2.md) separately tests synthetic trace
redaction/enrichment through a local Collector file exporter, not these metrics,
a remote backend or a production deployment.

## Select one telemetry layer

Worker execution: `span_name="request"`. Node ingress: `supervisor.request`.
Public frontend: `proxy.request`. Adding them double/triple counts logical requests.
Worker duration includes post-return work; it is not client response latency.

Object executions per second:

```promql
sum(rate(nanovm_spans_calls_total{span_name="request"}[5m]))
```

Worker span error fraction (includes errors after response headers):

```promql
sum(rate(nanovm_spans_calls_total{span_name="request",status_code="STATUS_CODE_ERROR"}[5m]))
/
sum(rate(nanovm_spans_calls_total{span_name="request"}[5m]))
```

An absent error series can produce no data, not zero. Configure the dashboard's
no-traffic/no-data state explicitly; don't use a zero-fill that hides missing
telemetry. Error spans are not identical to HTTP5xx or a product's SLO failures.

Worker lifecycle p99 in seconds, using matching histogram buckets:

```promql
histogram_quantile(0.99,
  sum by (le) (rate(nanovm_spans_duration_seconds_bucket{span_name="request"}[5m])))
```

Keep object IDs, raw paths and operation IDs out of metric dimensions. Use trace
records for high-cardinality investigation. Observe `otel_metric_overflow`, SDK
stderr drop/export warnings and Collector refusal/export failure counters. Validate
exact exporter metric names on upgrade. Accepted/sent counts are useful evidence,
not a durable exactly-once ledger. Derive aggregates before sampling or use the
backend's documented weighted query semantics. A `sample_rate` label alone is not enough.

Suggested dashboard panels (NH17, **not built**): selected-layer throughput,
error fraction, p50/p95/p99 lifecycle latency, Collector accepted/sent/refused,
export failure and process CPU/RSS. Every panel should show its unit, layer,
window and no-data state. Keep application and Collector budgets separate.

## Reproducibility checklist

- [ ] Record exact source commit and dirty diff, component digest, toolchain and build flags.
- [ ] Record CPU model, physical-core/SMT map, affinity, cgroup quota and memory budget.
- [ ] Record in-process / HTTP-node / HTTP-proxy path, client location, TLS and routing.
- [ ] Pin load-generator and Collector versions/config digests; name the actual backend.
- [ ] Declare cold/warm state, object distribution, concurrency/rate, body bytes and seed.
- [ ] Specify success oracle, expected span layers/counts and shutdown/drain window.
- [ ] Warm up separately; alternate matched control/candidate runs with no parallel builds.
- [ ] Save raw results and all failed/slow runs; record why any run is excluded before comparing.
- [ ] Record application CPU, Collector CPU and client CPU separately; no hidden free cores.
- [ ] Normalize into [JSONL](fixtures/benchmarks.jsonl), retain provenance and unique run IDs.
- [ ] Compare aggregate successes/time and per-run latency distributions, not averaged p99s.
- [ ] State untested topology/backend and uncertainty; never promote a short microbenchmark to capacity.

## workerd / Spin comparison protocol

No comparative measurements or compatibility claims are supplied. Before timing,
pin all projects and toolchains and make an equivalence table for routing,
serialization per object, persistent-state semantics, isolation, authentication,
HTTP path, response acceptance and telemetry. A stateless handler is not an
equivalent durable-object implementation. Label any unsupported cell, rather than
silently disabling a feature in one candidate.

Use a common external HTTP client, equal physical-core allocations, identical
payload/data distributions and a separately budgeted telemetry path. Measure cold
startup, warm requests and resident-object slope separately. Include product
dependencies/sidecars in resource totals. Repeat matched runs, save raw data and
publish non-equivalences with results. Do not reuse historic nanovm numbers as
controls for another environment.

## Tenant onboarding checklist

- [ ] Confirm domain/DNS record ownership and intended aliases; app identity uses record owner and repository.
- [ ] Confirm immutable artifact digest, allowed issuer/subject and minimum version policy.
- [ ] Keep supervisor and private publish ingress inaccessible to tenant clients.
- [ ] Put TLS termination at the approved frontend; do not assume node TLS exists.
- [ ] Set explicit operator ceilings; tenant-declared limits are clamped, not additional capacity.
- [ ] Review egress destinations, application authentication and configuration-secret handling.
- [ ] Define operation-ID retention and unknown-outcome handling if the app exposes mutations.
- [ ] Verify harmless application requests through intended routing and inspect sanitized telemetry.
- [ ] Budget the Collector independently and validate the real backend's privacy/export policy.
- [ ] Record who approves deployments and package publication; onboarding is not permission to publish guests.

## Dependency assessment template

Copy this section into a review, filling evidence rather than defaulting to “safe.”

| Question | Required evidence |
| --- | --- |
| What changes? | Pinned old/new versions, upstream release/source references, direct/transitive scope |
| Interface compatibility? | WIT/component ABI and feature changes, minimal fixture build/run |
| Reproducibility? | Lockfile diff, toolchain requirement, artifact digests |
| Security/license? | Advisories reviewed, license diff, new network/build-script behavior |
| Cost? | Matched size/startup/CPU/memory controls or explicit “not measured” |
| Verification? | Exact commands and failed as well as passed checks |
| Delivery? | Owner/reviewer, source landing separate from package/deployment authorization |

## Release checklist

- [ ] Define the exact diff and intended destination; preserve other agents' work.
- [ ] Run relevant tests, inspect negative cases and retain reproducible evidence.
- [ ] Separate proposals, local checks and actual environment measurements in notes.
- [ ] Verify no accidental `guests/**`, workflow, credentials or generated build outputs.
- [ ] Obtain explicit permission for package publication and deployment, separately from source integration.
- [ ] After an authorized push, check CI for that exact head; don't infer from an earlier green build.
- [ ] Record actual committed/pushed/merged/deployed state without treating these as synonyms.

## Troubleshooting without guessing the cause

| Observation | Safe first check | What it does not prove |
| --- | --- | --- |
| Browser cannot reach app | DNS record owner, frontend routing/TLS, frontend error response | The worker ran or data changed |
| Component import mismatch | Pinned WIT/toolchain and component composition outside published guests | A host runtime change is required |
| HTTP succeeds but traces are missing | SDK stderr, Collector accepted/sent/refused, correct query layer/time window | HTTP success implies export success |
| Good in-process rate, slower HTTP | Same build, worker/proxy path, client CPU, topology, warmup | Telemetry is necessarily the bottleneck |
| High p99 with one object | Hot-object distribution and queue rejections | More clients create more service capacity |
| Unknown client result | Application status lookup or documented same-ID reconciliation | Resending with a new ID is safe |

Keep private bodies, IDs and credentials out of shared diagnostics. Storage failure,
recovery, ownership/fencing and runtime-correctness investigation belong to their
owners and are deliberately excluded from this cookbook.
