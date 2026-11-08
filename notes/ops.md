# Ops notes

Operations and packaging work from IDEAS.md (sections E, F, L, M), one branch per
item, each from `cf/main-next`. Plain language; numbers and how to reproduce.

## F-67 alerts and recording rules (`ideas/alerts`)

**What:** `deploy/monitoring/`: Prometheus recording rules per layer (rate,
errors, error ratio, per-app error ratio, p99) and nine alerts (node and app
error ratios, node latency, span-metric overflow, Collector down / refusing /
failing exports / queue filling, data disk), each pointing at its runbook; a
minimal `prometheus.yml`; promtool unit tests.

**Evidence:** promtool 2.45.3 (apt): `check config`, `check rules` (14 rules) and
`test rules` (5 groups, every alert asserted to fire with its labels and summary,
and not to fire below its traffic or duration floor) pass. The tests caught a
real gap: an app with no error series read as no data, not 0; fixed with
`or … * 0`. Not run against a live Collector (no binary); metric names follow
README's validated names, Collector ones matched with and without `_total`.
