# Validation record

Current through batch 3: **29 tests pass with the three pinned tools**, no skips.
[Batch 2](BATCH2.md) and [batch 3](BATCH3.md) record newer tooling and limits.
Work is on local branch `nice-to-haves`; nothing pushed or integrated into main.

## Batch 1 validation — 2026-11-06

Local orb, Python 3.11.6. All source changes are confined to
`experiments/nice-to-haves/`. Runtime, shared HANDOFF/ISSUES, root configuration,
guests and publishing workflows are untouched. No package, production service,
external HTTP endpoint, runtime benchmark or backup/recovery drill was exercised.

## Executed checks

From the repository root:

```sh
python3 -B -m unittest discover -s experiments/nice-to-haves -v
```

**14 tests passed**. Contracts covered: capacity units and bottleneck switch,
boundary utilization and target equality, absent provenance/nonfinite inputs,
time-weighted rates, path/environment/provenance grouping, duplicate-run rejection,
per-run export count discrepancies, encoded Wasm sizes and malformed envelopes,
Markdown spaces/fences/missing paths/symlink containment, CLI JSON and error exit,
exact HTTP body/ID, bounded response, 302/307/503 without follow/retry, disconnect,
malformed chunk, short body, and invalid URL/header values.

A short `Content-Length` response initially returned `http-response` instead of
`unknown`. The extended HTTP test failed before the length check and passed after
it. Python's size-limited `read(n)` can return fewer bytes without raising, so the
client now compares the declared length (as interpreted by `http.client`) against
the bounded read. This is not a full HTTP security parser or total-time deadline.

Four in-memory source mutations each failed their targeted existing test with an
assertion failure (not an import/runtime error):

| Wrong implementation | Test that rejected it |
| --- | --- |
| Choose maximum instead of minimum CPU ceiling | `test_capacity_units_and_limiting_budget` |
| Average run rates instead of weighting elapsed time | `test_summary_weights_time_not_run_rates` |
| Match only aggregate span counts | `test_summary_cannot_hide_loss_with_extra_spans_in_another_run` |
| Allow urllib's normal redirect handler | `test_does_not_follow_redirects_or_retry_http_errors` |

Mutations were compiled into temporary Python namespaces; files never changed.
No retained test helper mutates repository sources.

## Synthetic command outputs, not nanovm measurements

```sh
python3 -B experiments/nice-to-haves/toolkit.py capacity experiments/nice-to-haves/fixtures/capacity.json
python3 -B experiments/nice-to-haves/toolkit.py summarize experiments/nice-to-haves/fixtures/benchmarks.jsonl
```

Capacity fixture: application ceiling 14,000/s; Collector ceiling 35,000/s;
resident objects 7,168; mean in-flight demand 50. **Every input is assumed.**
These numbers are arithmetic examples, not recommended production limits.

Benchmark fixture: 10,000 requests / 4 seconds = 2,500/s, or 2,477.5 successful/s
after 90 errors. Median run rate is 2,000/s, demonstrating why it is not the
aggregate. Per-run p99 range is 8–12 ms; 19,999 / 20,000 spans means count match
is false. **Both fixture rows are synthetic.**

## Existing Wasm artifact envelope inspection

The inspector read existing build outputs without building, modifying, validating
semantics or publishing any component. Header plus encoded section lengths
reconciled exactly to file size for all three:

| Path under `target/keyvalue/` | Bytes | SHA-256 |
| --- | ---: | --- |
| `tests.wasm` | 443,534 | `fc98eb32808bb350a0ce139d327848648bf527a800a7f4e14bc5b54bb82b5013` |
| `wasm32-wasip2/release/spin_kv_client.wasm` | 260,700 | `5af6afa96cc765f0936bc3ab4a873b98ce7bc8864d5e1bd17b9ec49321ad2ded` |
| `wasm32-wasip2/release/spin_kv_provider.wasm` | 176,176 | `bf1cb9228d78116163814573f430f7398af7c99153d1eee7ec5950e02663d436` |

All have component headers. Their presence does not establish that they match the
current source commit; the digest, not the filename, identifies the inspected data.
The tool intentionally does not infer runtime dependencies or validity from sizes.

## Scope and remaining validation

```sh
python3 -B experiments/nice-to-haves/toolkit.py links --root . experiments/nice-to-haves/*.md
```

This checks the new documents' simple local file links only, not repository-wide
documentation correctness or anchor/external URL availability. Cookbook queries
are aligned with the existing reference config but not executed in a backend.
Application deduplication and chat are recipes, not tested services. TLS, real
nanovm HTTP integration and cross-platform Python behavior remain untested.
The full Rust suite is intentionally not run: there are no runtime/build changes.

Remaining priorities: benchmark import adapter + metadata manifest (NH14/NH19),
pinned WIT inventory (NH18), then local-only load and Collector examples. There
are no blockers for this batch; those follow-ons require their own fixtures and
tools. The [backlog](BACKLOG.md) distinguishes all 48 items by priority and status.

## Follow-on batches

The statements above describe batch 1. [Batch 2](BATCH2.md) records real pinned
oha/WIT/Collector checks; [batch 3](BATCH3.md) records transcripts, histograms and
topology; [batch 4](BATCH4.md) adds six passing in-memory application protocol tests
and a deterministic CLI transcript. These models do not validate a deployed chat
service, storage persistence, authentication or concurrency. All batches remain
independent of runtime/recovery work and local to `experiments/nice-to-haves/`.

[Batch 5](BATCH5.md) adds seven passing synthetic-planning contract tests; the queue
CLI fixture returns exact waits `[0,3,5,0]` and finishes `[6,8,8,13]`. Other tested
calculators cover per-file block rounding, label-product overflow, separate cost
alternatives and seeded balanced order/popularity. No live measurements are implied.

[Batch 6](BATCH6.md) adds four passing offline-review tests and real read-only Cargo
metadata, existing-component size and local Git-range checks. Unicode line framing
was caught and corrected during validation. OCI signatures remain NOT VERIFIED;
license declarations and changelog candidates require human review.

[Batch 7](BATCH7.md) adds two passing Node/Python wire tests and separately executed
pinned language probes: both synchronous components validate and pass four string
round trips; native async JS fails to build, Python async validates but execution
is unverified because the pinned host binding generator rejects its type encoding.

[Batch 8](BATCH8.md) adds six passing tests: three TOML-lint, two parser-backed
Markdown, and one real bounded-load contract. Python 3.11 is required for tomllib;
optional Markdown tests need the pinned `NICE_MARKDOWN_TOOLS` installation.

[Batch 9](BATCH9.md) adds exact OTLP interval/error tests and an opt-in real
Prometheus dashboard-query test. The cumulative suite passes 56 tests with zero
skips using all optional dependencies and `NICE_SYNTHETIC_PROMETHEUS=1` against
the disposable local fixture. Queries return 100 requests/s, 20% errors, 35% queue,
80/20 outcomes and 5 dropped spans/s; the deliberately incorrect two-layer sum
returns 200. Chromium DOM assertions, inspected screenshots (including narrow,
empty and warning states) and pointer recordings validate the browser examples.
These UI checks are manual recorded validation, not a permanent browser suite.
All changes remain isolated; no runtime/guest or production guarantees are added.
