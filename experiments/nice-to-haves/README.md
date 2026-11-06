# Independent developer toolbox

Noncritical tools and recipes, isolated from the runtime and active backup work.
Python 3.10+; Python standard library only. Batch 1 needs no external tools;
batch 2 optionally runs pinned oha, wasm-tools and Collector binaries. No root
changes, guest edits, package publication, or production services. Run from the repository root.
These are developer aids for trusted local files, not hostile-input services.

## Catalog

| Item | Entry point | Evidence / status |
| --- | --- | --- |
| Capacity budget calculator | `toolkit.py capacity` | Tested arithmetic and input contracts; example inputs **all assumed** |
| Benchmark summarizer | `toolkit.py summarize` | Tested on synthetic runs; no new nanovm performance measurements |
| Wasm size inventory | `toolkit.py wasm` | Tested binary envelopes, plus existing build artifacts; not a validator |
| Local Markdown file-link check | `toolkit.py links` | Tested missing paths, encoded spaces, fences and symlink escape; limited grammar |
| HTTP operation client | [http_operation.py](http_operation.py) | Tested against disposable Python HTTP server; not a nanovm guest integration |
| oha import and metadata | [benchmark_import.py](benchmark_import.py) | Pinned 1.16.0 raw fixtures, status oracle, missing latency and secret-canary tests |
| Component interface inventory | [wit_inventory.py](wit_inventory.py) | Pinned wasm-tools 1.263.0 validation/extraction; external WIT surface, not SBOM |
| Local load fixture | [local_load_fixture.py](local_load_fixture.py) | 48 real oha requests to a synthetic loopback handler; no user-supplied target |
| Collector redaction/enrichment fixture | [collector_fixture.py](collector_fixture.py) | Pinned Collector 0.164.0, two synthetic spans, negative configuration tests |
| Workload / deduplication cookbook | [RECIPES.md](RECIPES.md) | Design recipes only; no running deduplication service |
| Telemetry queries and comparison protocol | [OPERATIONS.md](OPERATIONS.md) | Source-aligned guidance, not a new Collector/backend validation |
| Prioritized backlog | [BACKLOG.md](BACKLOG.md) | 48 concrete items with acceptance gates; proposals clearly marked |
| Reproduction and evidence | [Batch 1](VALIDATION.md), [batch 2](BATCH2.md) | Commands, observations, limitations and pinned-tool setup |

## Run the tools

```sh
python3 -B -m unittest discover -s experiments/nice-to-haves -v
python3 -B experiments/nice-to-haves/toolkit.py capacity experiments/nice-to-haves/fixtures/capacity.json
python3 -B experiments/nice-to-haves/toolkit.py summarize experiments/nice-to-haves/fixtures/benchmarks.jsonl
python3 -B experiments/nice-to-haves/toolkit.py links --root . experiments/nice-to-haves/*.md
# Supply an already-built artifact; this tool never builds/publishes guests.
python3 -B experiments/nice-to-haves/toolkit.py wasm target/keyvalue/tests.wasm
```

All analysis commands output JSON. Exit 0 means analysis completed (not that the
input workload is healthy); link problems exit 1, invalid input/file errors exit 2.
Capacity and benchmark results deliberately retain unfavorable observations.
External-tool tests explicitly skip unless the `NICE_OHA`, `NICE_WASM_TOOLS`, and
`NICE_COLLECTOR` variables point to the pinned binaries; see [batch 2](BATCH2.md).

### Capacity input and formulas

[capacity.json](fixtures/capacity.json) defines the exact required fields. Each has
`value`, `basis` (`measured` or `assumed`), and a nonempty `source`. For measurements,
record commit, command, workload, CPU topology and raw evidence path in `source`.
The tool cannot authenticate that provenance. Do not relabel the sample as measured.

- Application ceiling = application cores × target utilization × 1,000,000 / application CPU µs per request.
- Collector ceiling uses its **separate** core allocation and CPU per request, including all emitted spans.
- Combined ceiling is the smaller CPU ceiling; it is not an admission limit.
- Resident objects = floor((memory budget MiB − fixed memory MiB) × 1024 / observed KiB per resident object).
- Mean in-flight demand = target requests/s × mean latency ms / 1000 (Little's law, stable workload assumption).

Physical CPU sharing, I/O, hot-object serialization, burst demand, backend/network,
pool caps and cgroups can dominate. Heap density is not `memory_reservation_mb`.
No configured runtime limit is inferred or modified.

### Benchmark schema

[benchmarks.jsonl](fixtures/benchmarks.jsonl) is the schema by example. One JSON
object per run; all fields are required. `requests` includes errors; `errors` is
the subset failing your stated workload oracle. `seconds` is the measured wall
interval and `p99_ms` is that run's client percentile, or `null` when unavailable.
All-failed runs remain in throughput/error totals; `runs_without_p99` counts missing
percentiles, and the range excludes them (or is `null` if none exist). `expected_spans` and
`received_spans` must describe the same interval, layers and drain policy.
Declare sampling in the environment description; do not compare sampled counts
as though they were full export. Zero expected spans means none expected, not unknown.

Group keys include variant, workload, request path, environment and provenance.
Give environment a stable ID that includes CPU allocation, build flags, component
digest, Collector configuration, concurrency and object distribution. The tool
cannot detect incompatible setups given the same ID. Duplicate run IDs within
a group fail, rather than silently overweighting reruns.

Aggregate throughput = total requests / total time, **not** mean run throughput.
Successful throughput removes errors. Per-run p99 range is not a pooled p99.
Count matches must hold on every run; compensating extra/missing spans do not pass.
Equal counts alone do not establish unique delivery.

### Wasm inventory boundaries

Reports SHA-256, file bytes, top-level section offsets and payload/encoded bytes,
and custom section names. Header + total section bytes should equal file size.
Supports core-module v1 and current component binary headers. It rejects truncated
envelopes and overflowing u32 sizes but does not validate section ordering, IDs,
instructions, WIT, imports or nested modules. It reads the whole file into memory.
The separate [batch 2 inventory](BATCH2.md) wraps pinned `wasm-tools validate` and
`wasm-tools component wit` for validation and external interface names.

### Link checker boundaries

Checks existence of simple inline Markdown file targets relative to each document.
Percent-encoded paths and quoted titles work; fragment text is ignored. Absolute
paths and symlinks outside `--root` are errors. Never fetches external links.
Fenced examples are skipped. Reference-style links, HTML, nested parentheses,
inline-code parsing and heading-anchor correctness are outside its grammar;
zero problems is **not** a complete documentation audit.

### Single-attempt HTTP client

Run only against an endpoint you own and are authorized to mutate:

```sh
python3 -B experiments/nice-to-haves/http_operation.py "$APP_OPERATION_URL" \
  --operation-id invoice-2026-11-06-001 --body /path/to/request.json
```

The URL is an application endpoint behind the proxy, not a proposed nanovm API.
The client sends exact file bytes as JSON with an `Idempotency-Key` header. The
header has **no built-in host deduplication semantics**. Use the same ID and body
only under your application's documented deduplication contract. Example counter
endpoints do not become idempotent by receiving it.

No retries, redirect following, environment proxy inheritance, authentication or
credential store. HTTPS uses Python's default certificate verification. Socket
timeout is not a total deadline (DNS and a slow trickle can exceed it). Output
includes at most 64 KiB of response bytes decoded as UTF-8; it may contain private
application data. Keep it out of public logs. It does not validate response JSON
or business success. HTTP 2xx exits 0; other responses/unknown transport outcomes
exit 1; invalid inputs exit 2. A transport failure does not prove the operation
failed. See the application recipe before adding retries.
