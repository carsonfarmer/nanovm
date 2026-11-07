# Prioritized independent backlog

48 bounded ideas. No item authorizes a production change, publishing, new runtime
API, or work on storage recovery/ownership. P1 = most immediate developer value;
P2 = useful once there is a real consumer; P3 = exploratory. S = hours, M = a few
days, L = a separate project. Estimates are assumptions, not commitments.

**Tested** means an executable bounded contract was checked in a recorded batch.
**Recipe** means written guidance, not an implemented service or validated deployment.
**Proposed** means no implementation or measurement. IDs are local, not ISSUES IDs.

| ID | Priority / size | Deliverable and completion gate | Status |
| --- | --- | --- | --- |
| NH01 | P1 / S | Capacity CLI with required input provenance, separate Collector CPU, unit/boundary checks | Tested |
| NH02 | P1 / S | JSONL benchmark aggregator that weights elapsed time and separates HTTP/in-process; asymmetric fixtures | Tested |
| NH03 | P1 / S | Single-attempt HTTP operation client; exact bytes/ID, redirect, 503, disconnect tests | Tested |
| NH04 | P1 / S | Local Markdown file-link checker; spaces, missing paths and symlink containment cases | Tested |
| NH05 | P1 / S | Top-level Wasm section inventory; reconcile bytes and reject truncated envelopes | Tested |
| NH06 | P1 / S | Benchmark reproducibility checklist with topology, warmup, error oracle and export drain | Recipe |
| NH07 | P1 / S | Application deduplication protocol; duplicate/different-body/expiry cases and unknown outcomes | Recipe |
| NH08 | P1 / S | Telemetry query cookbook choosing exactly one request layer and bounded metric labels | Recipe |
| NH09 | P1 / S | Tenant onboarding checklist with DNS owner, signer, limits and private ingress checks | Recipe |
| NH10 | P1 / S | Release checklist separating source delivery, CI, deployment and package authorization | Recipe |
| NH11 | P1 / S | Workload matrix for hot rooms, many cold objects, payloads and dependencies with explicit oracles | Recipe |
| NH12 | P1 / S | Dependency update assessment template: interface, build, cost and license evidence | Recipe |
| NH13 | P1 / M | Bounded GET load driver with rate/concurrency/object-skew controls; prove cap on a loopback server | Tested: configurable own-loopback driver, total/concurrency/admission caps, skew and HTTP error oracle |
| NH14 | P1 / M | oha JSON adapter into NH02 schema; pin version and test archived success/error samples | Tested: 1.16.0, status oracle, raw digest, null latency |
| NH15 | P1 / S | Deterministic demo transcript from normalized JSON; fixed ordering and secret-canary tests | Tested: deterministic Markdown, arbitrary labels omitted |
| NH16 | P1 / M | Deployment TOML linter, schema matched to CLI and warnings distinct from errors; no deployment writes | Tested: source-hash-pinned prototype, selected syntax/type/dev checks, not deployment approval |
| NH17 | P1 / M | Collector dashboard JSON using bounded labels; run pinned Grafana/Prometheus on synthetic series | Proposed |
| NH18 | P1 / M | WIT import/export inventory via pinned wasm-tools; nested/composed fixtures and provenance | Tested: 1.263.0 external interfaces, not recursive dependency/SBOM inspection |
| NH19 | P1 / S | Benchmark metadata manifest generator excluding environment values; secret-canary validation | Tested: allowlisted fields, hashes, no environment/remote/path contents |
| NH20 | P1 / M | Client disconnect test harness for application authors; distinguish unknown result from rejected operation | Tested: local handler accepts before disconnect, both clients report unknown, explicit retries |
| NH21 | P2 / M | Chat-client protocol simulator with reconnect cursors and duplicate message IDs; deterministic transcript | Tested: contiguous cursors, history gaps, duplicate delivery; no browser/outbox |
| NH22 | P2 / M | Application dedup model in memory only; bounded TTL and conflict tests, explicitly not persistence proof | Tested: caller/room scope, exact body, capacity, TTL re-execution boundary |
| NH23 | P2 / S | Downstream tracing enrichment example for operator-owned region/env; prove app identity stays untouched | Tested: synthetic environment enrichment and retained identity/parentage |
| NH24 | P2 / M | Collector redaction fixture with seeded secret fields; assert sanitized decoded output | Tested: Collector 0.164.0, trace/resource canaries; no general privacy guarantee |
| NH25 | P2 / M | JS component feasibility probe outside guests; pin tools, show imports and build failure/success | Tested: sync build/validate/4 round trips; native async WIT build fails in pinned 0.23.0 |
| NH26 | P2 / M | Python component feasibility probe outside guests; separate P2/P3 compatibility and startup cost | Partial: sync build/run, async build/validate; async execution and startup cost unverified |
| NH27 | P2 / S | workerd/Spin comparison worksheet; equivalent semantics before any performance claim | Recipe |
| NH28 | P2 / M | Portability matrix from pinned source/docs and actual compile probes; mark untested jail support | Partial: Linux x64 language/tool survey in BATCH7; other platforms/jail untested |
| NH29 | P2 / S | Conceptual scheduler HTTP contract with duplicate delivery/cancellation examples; no live alarms | Recipe: revision/cancellation sketch in BATCH4; product decisions unresolved |
| NH30 | P2 / S | Synthetic storage-amplification calculator comparing logical bytes, blocks and metadata; no backup logic | Tested: per-file rounding, empty-file metadata, null empty ratio |
| NH31 | P2 / M | Offline histogram merger from compatible raw buckets; reject mismatched boundaries/units | Tested: disjoint buckets, nearest-rank intervals, overflow explicit |
| NH32 | P2 / S | Synthetic high-cardinality label budget estimator; demonstrate cardinality product and overflow | Tested: exact product, budget boundary and signed-64-bit overflow flag |
| NH33 | P2 / S | CPU/SMT topology capture into benchmark manifest; fixture test offline lscpu formats | Tested: socket/core grouping, affinity, SMT overlap; bind sidecar digest |
| NH34 | P2 / M | Markdown anchor/reference checker using a real parser; match duplicate GitHub heading IDs | Tested: pinned CommonMark/github-slugger, references/duplicates/Unicode; no full GitHub HTML equivalence |
| NH35 | P2 / S | Tenant-safe incident template; redact identity/body tokens and include evidence confidence | Recipe: manual privacy/evidence worksheet in BATCH4, not automatic redaction |
| NH36 | P2 / S | Developer troubleshooting decision table for routing, guest API mismatch and telemetry loss | Recipe |
| NH37 | P2 / M | Read-only OCI artifact inventory with digest/signature provenance; offline fixture then opt-in registry | Partial: local descriptor integrity tested; signatures explicitly NOT VERIFIED, no online client |
| NH38 | P2 / S | Non-production threat-model exercise for trusting client headers; explicit trust-boundary answer key | Recipe: scenario and answer key in BATCH4, no host vulnerability claim |
| NH39 | P3 / M | Synthetic object-popularity generator (uniform/hotset/Zipf); seeded distribution and count tests | Tested: replayable sequence/counts, partition extremes, no network |
| NH40 | P3 / M | Fair benchmark run-order generator with randomized balanced pairs and replayable seed | Tested: exact AB/BA balance and seed replay |
| NH41 | P3 / S | Component build-size budget diff, keyed by toolchain and digest; no size-as-speed inference | Tested: binary digests, budget boundary, reductions, toolchain labels |
| NH42 | P3 / M | Offline trace waterfall from sanitized OTLP fixture; validate parent IDs and missing-span markers | Proposed |
| NH43 | P3 / S | Energy/cost worksheet with explicit electricity/cloud-rate assumptions; no live price claims | Tested: synthetic kWh, separate cloud/electricity alternatives |
| NH44 | P3 / M | Client language interop fixtures for operation IDs and Unicode bodies; Python/JS wire-byte agreement | Tested: exact files/IDs over real loopback HTTP, whitespace conflict, no canonicalization claim |
| NH45 | P3 / S | Changelog candidate generator from explicit commit range; retain human review and no auto-release | Tested: explicit ancestral commits, Unicode-safe subjects; no release |
| NH46 | P3 / M | License/SBOM review worksheet using existing Cargo metadata; missing-license warnings, no policy verdict | Tested: declaration worksheet and secret-path exclusion; not build SBOM/legal approval |
| NH47 | P3 / M | Browser chat protocol demo against a fake server; inspect reconnect, pending and rejected states | Proposed |
| NH48 | P3 / S | Teach queueing with synthetic arrivals/service times; mark model assumptions and verify exact small cases | Tested: single-server FIFO, idle gaps, simultaneous arrivals, zero service |

## Suggested next bounded batch

Batch 2 completes the bounded NH14/NH19/NH18 tools plus synthetic NH23/NH24.
Batch 3 completes NH15/NH31/NH33.
Batch 4 completes bounded NH21/NH22 models and NH29/NH35/NH38 teaching recipes.
Batch 5 completes synthetic NH30/NH32/NH39/NH40/NH43/NH48 exercises.
Batch 6 completes NH41/NH45/NH46 and the offline portion of NH37.
Batch 7 adds NH20/NH44 wire fixtures, NH25 and bounded NH26/NH28 language probes.
Batch 8 completes bounded NH13/NH16/NH34 controls; tools never deploy or load external targets.
Next are synthetic visual examples; platform/runtime adoption remains out of scope.
NH17 still needs real Grafana/Prometheus synthetic-series validation; no dashboard
is claimed. None requires production access or a host API change.

The language probes (NH25/NH26) must remain outside `guests/**`; successful builds
do not authorize migration or package publication. Comparison and portability work
must cite pinned upstream sources before making claims. No item here is needed by
the parent thread's offline backup/restore or fresh-root recovery drill.
