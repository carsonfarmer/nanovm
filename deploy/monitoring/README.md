# Alerts and recording rules

[nanovm-rules.yaml](nanovm-rules.yaml) holds Prometheus rules over what the
reference Collector ([demo/otel-collector.yaml](../../demo/otel-collector.yaml))
exposes: derived span metrics on :8889 (`nanovm_spans_calls_total`,
`nanovm_spans_duration_seconds_bucket`, labelled `span_name`, `status_code`,
`nanovm_app`, …) and the Collector's own metrics on :8888 (`otelcol_*`), plus
node_exporter for disk. [prometheus.yml](prometheus.yml) scrapes all three.

Recording rules, per layer (`span_name`: `request` = object execution,
`supervisor.request` = node ingress, `proxy.request` = front door; never sum
layers): request rate, error rate, error ratio, per-app error ratio, p99.
A layer or app with no error series yet reads 0 rather than no data.

| Alert | Fires when | Runbook |
|---|---|---|
| `NanovmNodeErrorRatioHigh` (page) | > 5% of node requests fail for 10 min, above 1 request/s | [crash loop](../../docs/ops/runbooks.md#worker-crash-loop), [errors](../../docs/ops/errors.md) |
| `NanovmAppErrorRatioHigh` (ticket) | one app's object requests fail > 20% for 15 min | [errors](../../docs/ops/errors.md) |
| `NanovmNodeLatencyHigh` (ticket) | node p99 > 1 s for 15 min | [errors](../../docs/ops/errors.md) |
| `NanovmSpanMetricsOverflow` (ticket) | the connector's cardinality limit collapses series | — |
| `NanovmCollectorDown` (page) | the Collector's metrics endpoint is down 5 min | [Collector outage](../../docs/ops/runbooks.md#collector-outage) |
| `NanovmCollectorRefusingSpans` (page) | the receiver refuses spans (memory limiter, overload) | [Collector outage](../../docs/ops/runbooks.md#collector-outage) |
| `NanovmCollectorExportFailing` (page) | exports to the backend fail | [Collector outage](../../docs/ops/runbooks.md#collector-outage) |
| `NanovmCollectorQueueFilling` (ticket) | an exporter's sending queue is > 80% full | — |
| `NanovmDataDiskFilling` (page) | < 15% free on the data filesystem (set its mountpoint) | [disk full](../../docs/ops/runbooks.md#disk-full) |

Spans only exist while telemetry flows: during a Collector outage the
nanovm_spans_* series stop, which is why the Collector's own health pages.
Error spans include a guest's own 5xx; 4xx are not errors (README).
`commit-failed` cannot be told apart from other 500s in these metrics (the
connector keeps status codes, not `error.type`), hence the disk alert.

## How this was checked

`promtool` 2.45.3: `check config` and `check rules` pass, and
`promtool test rules nanovm-rules.test.yaml` passes 5 test groups that feed
synthetic series and assert each alert fires (or stays quiet below its traffic
or duration floor) with the expected labels and summaries. The first run caught
an app with no error series reading as no data instead of 0; the `or … * 0`
terms fix that. Collector metric names are matched with and without the
`_total` suffix because it depends on the Collector's exporter settings. Not
checked against a live Collector (no binary here): the `nanovm_spans_*` names
and labels are the ones README validated with otelcol-contrib 0.164.0.
The nice-to-haves branch has a Grafana dashboard over invented metrics
(`experiments/nice-to-haves/fixtures/grafana`); point its panels at these
recording rules for a real one.
