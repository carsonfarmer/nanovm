# Batch 3 — reproducible analysis

NH15/NH31/NH33 are implemented in `analysis_tools.py`. Batches 1–2 were preserved
in local commit `49de2ff` on branch `nice-to-haves`. No push or main integration.
All source changes remain inside this directory and independent of recovery work.

```sh
python3 -B experiments/nice-to-haves/analysis_tools.py transcript experiments/nice-to-haves/fixtures/benchmarks.jsonl
python3 -B experiments/nice-to-haves/analysis_tools.py histograms experiments/nice-to-haves/fixtures/histograms.json
python3 -B experiments/nice-to-haves/analysis_tools.py topology --application 0 --collector 1
```

Transcript output is deterministic Markdown, not a browser UI. Input order is
normalized before floating-point aggregation. Arbitrary run/workload/environment/
variant labels are omitted, not regex-redacted; output contains only validated
path/provenance enums and numeric summaries. Numeric observations may still be
private: this is not an anonymization guarantee. Group numbers are local to each
report, not stable IDs across different datasets. The fixture renders 10,000
requests, 90 errors, 2,477.5 successes/s, 8–12 ms run-p99 range and failed per-run
span-count matching. All these inputs are synthetic.

Histogram input is a JSON array with exact `run`, `unit`, `bounds`, `counts`,
`scope`, `provenance` fields. Bounds are increasing inclusive upper edges starting
above an implicit lower bound of zero; `counts` are **disjoint**, not cumulative,
and include a final unbounded overflow bin. Units (`ms` or `s`), boundaries,
population/sampling scope and provenance must match exactly across runs. Duplicate
run IDs fail. This does not directly consume Prometheus cumulative buckets or oha
histograms with different per-run boundaries. The caller must normalize them with
an explicitly justified method, never average their p99 summaries.

Pooled quantiles report the bin interval containing the nearest-rank sample,
not fabricated within-bin interpolation. Empty populations produce null quantiles;
overflow produces an unbounded upper edge. The asymmetric synthetic fixture merges
to `[2,3,3,2]` across 10 samples: p50 in `(1,5]` ms and p95/p99 above 20 ms.

Topology reads `lscpu --json --extended=CPU,CORE,SOCKET,ONLINE`, or a saved JSON
document with explicit `--affinity 0,1,...`. It groups by **socket and core**,
filters offline/unavailable CPUs, and detects SMT siblings shared across proposed
application and Collector allocations. It does not pin anything. Missing topology
fails rather than assuming isolation. Current orb reports pairs `[0,1]` and `[2,3]`;
allocations 0 versus 1 share a core, while 0,1 versus 2,3 do not in reported topology.
Virtualized topology, cgroup quotas and actual process pinning remain unverified.

Save topology JSON alongside a benchmark and bind it using the existing metadata
option `--artifact topology=/path/to/topology.json`; artifact paths are not emitted.
This preserves the metadata tool's explicit, hash-only artifact boundary.

Validation: four new public-contract tests pass, covering reversed input order,
secret/markup labels, asymmetric histogram ranks, incompatible populations/units,
empty histograms, duplicate IDs, same-core IDs on different sockets, SMT overlap,
offline/affinity exclusions and missing core identity. All three CLI paths ran.
No runtime measurement, deployment, or shared files were changed.
