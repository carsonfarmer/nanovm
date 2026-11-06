# Batch 2 — pinned benchmark and component tools

Implemented NH14/NH19/NH18 and synthetic NH23/NH24, plus a bounded local load
fixture. All source changes remain in this directory. Batch 1 tools and tests
remain; its summarizer now also accepts missing latency as `null`, reports
`runs_without_p99`, and never drops an all-failed run to invent a percentile.

No runtime/guest/shared-document/workflow edits, root dependencies, publishing,
production traffic or backup/restore dependencies. Python code uses the standard
library. External binaries are explicitly supplied, version checked and hashed;
the scripts do not download/install anything or accept remote load targets.

## Pins and acquisition

Validated on Linux x86-64 with Python 3.11.6 using these upstream release assets:

| Tool | Release asset | SHA-256 of downloaded asset |
| --- | --- | --- |
| oha 1.16.0 | [oha-linux-amd64](https://github.com/hatoo/oha/releases/download/v1.16.0/oha-linux-amd64) | `620bb9e16fb53eabc9a3fc45f88bdb41fefa3fee5c05e75892011ce320391716` |
| wasm-tools 1.263.0 | [x86_64-linux.tar.gz](https://github.com/bytecodealliance/wasm-tools/releases/download/v1.263.0/wasm-tools-1.263.0-x86_64-linux.tar.gz) | `21c036c1740a0552124c79c6e2170cb61c5b016005671a6d5bc06cf642165431` |
| otelcol-contrib 0.164.0 | [linux_amd64.tar.gz](https://github.com/open-telemetry/opentelemetry-collector-releases/releases/download/v0.164.0/otelcol-contrib_0.164.0_linux_amd64.tar.gz) | `f0c1ce9f661399beb4a58b711924716e08ea70c3802db0715fcfde3cbbada2d4` |

Download to a disposable tools directory, compare hashes before execution, and
extract the two archives there. No global installation is necessary. The extracted
wasm-tools binary SHA-256 is `694cb37dcda84a5d557334c1db882b986e43061abdb0af976b41b1614ee4530b`;
Collector binary SHA-256 is `57911a9d902cfe2c287dac09fcee4b054630b4ec248fa190a3115d860a32f81f`.
These observed hashes pin this reproduction; they are **not signature or supply-chain
attestation verification**. Use your approved binary acquisition policy. Tools run
with your user privileges; a self-reported version is not a security boundary.

Set `NICE_OHA`, `NICE_WASM_TOOLS` and `NICE_COLLECTOR` to absolute executable paths.
Different versions are rejected rather than silently trusting another JSON schema.

## Normalize oha without mistaking HTTP errors for success

[benchmark_import.py](benchmark_import.py) has `manifest` and `import-oha` commands.
Start with [scenario.json](fixtures/scenario.json), replace its public labels,
planned request count, accepted status codes and actual expected/received spans.
Choose `measured` only for observed runs; the provided template is synthetic.
Zero spans means none expected/received, not “unknown.” Use a setup ID that binds
the build flags, topology, concurrency, object distribution and Collector policy.

```sh
python3 -B experiments/nice-to-haves/benchmark_import.py manifest \
  --repo . --raw /path/to/run.oha.json --scenario /path/to/scenario.json \
  --oha "$NICE_OHA" --artifact component=/path/to/component.wasm > /path/to/run.manifest.json
python3 -B experiments/nice-to-haves/benchmark_import.py import-oha \
  --raw /path/to/run.oha.json --manifest /path/to/run.manifest.json > /path/to/normalized.jsonl
python3 -B experiments/nice-to-haves/toolkit.py summarize /path/to/normalized.jsonl
```

For the pinned format, the importer sums status-code counts and transport-error
counts for total attempts. Errors = transport failures + statuses outside the
declared accepted set. It uses `summary.total` seconds and converts
`latencyPercentiles.p99` from seconds to milliseconds. It does **not** use
`summary.successRate`, rounded latency metrics, or rate × elapsed time for counts.
Reported latency covers HTTP responses, not transport failures or only accepted
statuses; successful-only p99 is not available from this aggregate format.

The manifest binds exact raw bytes by length and SHA-256. Changed raw files,
incompatible tool versions, invalid/nonfinite numeric fields and mismatched planned
counts fail. A cancelled/partial run is retained as raw evidence, not normalized as
a completed fixed-budget run. Duration-mode and uncounted retries need another
explicit schema; do not fake planned counts to pass the check. All-transport-error
runs import with 100% errors and `p99_ms: null` and remain in throughput totals.

The success oracle is **status-only**. It cannot validate returned bodies, domain
state, unique span IDs, or acceptance semantics. Store the manifest beside normalized
JSONL; the row format does not carry every metadata field. Inputs remain trusted
local files, and their provenance is asserted, not cryptographically attested.

## Capture metadata without dumping secrets

The manifest emits only schema/scenario fields, raw/artifact/tool hashes and sizes,
tool version, Git HEAD, tracked diff hash/dirty flag, untracked count, OS/architecture,
kernel/Python version, logical CPU count and current CPU affinity. Artifact paths
are replaced by explicit public labels. Git runs with external diff/textconv disabled.
It never emits environment values, command lines, URLs, headers, remote URLs,
hostnames, usernames, filenames or file contents. Errors expose only exception class.

This is a snapshot at capture time, not proof of what code/machine generated the
run. Untracked **contents** are deliberately not captured: archive exact source
separately or supply an explicit artifact digest. Cgroup quota, physical-core/SMT
topology and load shape are not inferred from logical CPU count. Caller labels
must not contain secrets; hashes reveal equality and are not anonymization.

## Inspect external component interfaces

```sh
python3 -B experiments/nice-to-haves/wit_inventory.py /path/to/component.wasm \
  --wasm-tools "$NICE_WASM_TOOLS"
```

Reads a snapshot, hashes it, validates it with `--features all`, and asks pinned
wasm-tools for its JSON WIT graph. Returns each world's imported/exported interface,
function and type names, package versions, interface member names and all packages
in the type graph. Includes tool identity and validation feature set.

Validation with all proposals is **not** a nanovm support or safety check. Package
names in the type graph are not a runtime dependency/SBOM list. Composed providers
can be internal and absent from the external imports. It does not recurse through
embedded code, print full signatures, or discover source licenses. Core modules
are rejected because they do not have a component WIT surface.

The [type-only WAT fixture](fixtures/inventory.wat) checks a nested component,
function/type imports and a versioned interface in both directions. An existing
443,534-byte composed `target/keyvalue/tests.wasm` also validated and produced
20 external imports, one export and eight type-graph packages (including root).
SHA-256: `fc98eb32808bb350a0ce139d327848648bf527a800a7f4e14bc5b54bb82b5013`.
No component was built from or written into `guests/**`.

## Local load and Collector checks

```sh
# The output parent must exist; the output directory must not.
python3 -B experiments/nice-to-haves/local_load_fixture.py \
  --oha "$NICE_OHA" --repo . --output /tmp/nice-load-new-run
python3 -B experiments/nice-to-haves/collector_fixture.py --collector "$NICE_COLLECTOR"
```

The load fixture makes exactly four runs of 12 requests, concurrency 2, one-second
request timeout, no redirects, and a 20-second process deadline per run. It starts
its own loopback server and accepts **no target URL**. Server-side observations
independently check normalized counts: 0/12/8/12 errors for success/HTTP503/mixed/
disconnect-only. Raw outputs, manifests, normalized JSONL and a report go to a new
directory; overwrite is refused. See [archived fixtures](fixtures/oha/README.md).
This is not a general skew/rate-controlled load generator (NH13 remains partial).
`http-node` labels this direct fixture HTTP hop, not an actual nanovm node.

The Collector fixture validates [collector.yaml](fixtures/collector.yaml), starts
the pinned executable on a temporary loopback port, sends an empty readiness
request, then **one** synthetic two-span payload. It never retries that payload.
After clean shutdown/flush it decodes the temporary file export and checks exact
count, trace parentage, app identity, retained operation ID, environment enrichment
and absence of the secret canary. The child process is always terminated/reaped;
temporary config/output/log data are deleted. Port reservation/rebinding assumes
a trusted local test machine, not hostile co-tenants.

The policy deletes only `demo.secret` from span/resource attributes in traces. It
does not redact arbitrary fields, nested values, events, logs, URLs or real secrets.
The environment value is fixture-owned. No tail sampling, metrics/dashboard,
durable backend, TLS/authentication or loss-under-load guarantee is tested.

## Verification recorded 2026-11-06

```sh
NICE_OHA="$NICE_OHA" NICE_WASM_TOOLS="$NICE_WASM_TOOLS" NICE_COLLECTOR="$NICE_COLLECTOR" \
  python3 -B -m unittest discover -s experiments/nice-to-haves -v
```

**25 tests pass with all three pinned tools, no skips.** Without external-tool
variables: 19 pass, six explicitly skip. Batch 1's 14 tests still pass unchanged.
Four raw oha fixtures check 48 requests and all-error missing latency. The metadata
test puts a fake secret in an environment variable, tracked filename/content,
untracked file and remote credential URL; no canary appears in output.

Five temporary in-memory mutations were caught by assertion failures: ignore HTTP
status errors; omit seconds-to-ms conversion; bypass raw hash binding; dump the
environment into metadata; use exports as imports. Two real-Collector config
mutations also fail the oracle: disable canary redaction and supply wrong environment
enrichment. Sources remain unchanged by these experiments; no cleanup patches
touch runtime files. The full Rust suite is not relevant to these isolated tools
and was not run. No blockers for this batch; production claims remain excluded.
