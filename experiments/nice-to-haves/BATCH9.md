# Batch 9 — synthetic visual teaching examples

All examples are isolated, non-production and independent of recovery work.
No new nanovm throughput or Collector health measurements are claimed.

## NH17: a real dashboard over invented metrics

`dashboard_fixture.py` serves only loopback `/metrics`. Its `nice_*` series are
invented, not aliases for a particular Collector release's internal metrics.
Supervisor and proxy each count the same synthetic traffic; the dashboard selects
only supervisor. No tenant/object labels are generated. The five panels show
100 requests/s, 20% application errors, 35% queue occupancy, outcomes 80/20 per
second and 5 dropped spans/s. Omitting the layer selector produces 200 requests/s.

Validated with Linux amd64 Grafana OSS 13.3.0 (commit
`6f8ce3c693b493043ebcd4787566d2b6b118b0f0`) and Prometheus 3.16.0.
Downloaded release tarball SHA-256 values, recorded for reproduction rather than
as independent signature verification:

- Grafana: `cec8f98cc59bc1a1945833b652371a073be678c5c159a9b1695ad63c04fae5a7`
- Prometheus: `68727009472a2c02e1ff7f062fd5b3e637285bffc8fc2d340c76fb449d6e0f34`

Download locations:

- https://dl.grafana.com/oss/release/grafana-13.3.0.linux-amd64.tar.gz
- https://github.com/prometheus/prometheus/releases/download/v3.16.0/prometheus-3.16.0.linux-amd64.tar.gz

Run the exporter with Python 3.11+ and `PORT=8790`. Start Prometheus with
`--config.file=experiments/nice-to-haves/fixtures/prometheus.yaml`,
`--storage.tsdb.path=/tmp/nice-prom-data` and
`--web.listen-address=127.0.0.1:8791`. Run Grafana's `bin/grafana server` with
`--homepath` pointing to its extracted release and these environment values:

```sh
GF_SERVER_HTTP_ADDR=127.0.0.1
GF_SERVER_HTTP_PORT=8792
GF_PATHS_DATA=/tmp/nice-grafana-data
GF_PATHS_LOGS=/tmp/nice-grafana-logs
GF_PATHS_PROVISIONING="$PWD/experiments/nice-to-haves/fixtures/grafana"
NICE_DASHBOARDS="$PWD/experiments/nice-to-haves/fixtures/grafana/content"
GF_AUTH_ANONYMOUS_ENABLED=true
GF_AUTH_ANONYMOUS_ORG_ROLE=Viewer
GF_AUTH_DISABLE_LOGIN_FORM=true
GF_ANALYTICS_REPORTING_ENABLED=false
GF_ANALYTICS_CHECK_FOR_UPDATES=false
GF_ANALYTICS_CHECK_FOR_PLUGIN_UPDATES=false
GF_PLUGINS_PREINSTALL_DISABLED=true
```

These are environment assignments for the Grafana process, not shell exports.
Use separate terminals or supervised services; in an Amp orb use
`amp orb service start NAME --command 'COMMAND' --port PORT`.
Wait for at least two scrapes (prefer 30 seconds), then run:

```sh
NICE_SYNTHETIC_PROMETHEUS=1 python3 -B -m unittest discover -s experiments/nice-to-haves -p test_dashboard.py -v
```

The test executes the actual dashboard expressions, checks the asymmetric 80/20
outcomes, and checks the incorrect two-layer control. It uses the fixed local
fixture endpoint, never a user-supplied target. Open Grafana's
`/d/nice-synthetic` route locally; use an authenticated orb portal for remote
review, never expose this anonymous teaching instance to production networks.
The inspected Grafana screenshot shows the expected values and both outcome
lines. Native Grafana truncates long panel headings at narrower widths; full
headings remain in the accessibility tree. Initial empty time range before the
exporter started is expected, not a zero observation. No alert rules or live
Collector deployment are supplied. Stop these disposable services after testing.

## NH42: offline trace intervals, not a critical-path analysis

```sh
python3 -B experiments/nice-to-haves/trace_view.py experiments/nice-to-haves/fixtures/traces.json /tmp/nice-traces.html
```

The output path must not exist. Open generated HTML in a browser; do not open
`trace_template.html` directly. OTLP JSON hex IDs and decimal-string uint64
timestamps are required. At most 1000 spans are accepted. Duplicate IDs within
a trace, parent cycles, invalid identifiers and reversed intervals are rejected.
Missing parents and children outside parent time ranges remain visible. Empty
input renders an explicit empty state. Normalized rows omit raw IDs, names,
attributes and absolute times, but relative timing/topology can still identify
workloads: **this is not anonymization or a general privacy guarantee**.

The asymmetric fixture renders durations 12/9/3 ms at offsets 0/1/4 ms; another
trace has a missing parent and duration 4 ms. Tests cover these exact values,
canary omission, malformed IDs, timestamp overflow, zero duration, reversed time,
cycles, duplicate spans, empty input and outside-parent flags. Browser checks
verified the rendered intervals, missing-parent selector state, empty state and
outside-parent warning. Chromium narrow layout was inspected at 360 CSS pixels.
No additive latency, complete tracing, clock synchronization or critical-path
claim is made. The tool reads the entire input into memory before span limiting.

## NH47: browser chat model

Open `chat_demo.html` directly or serve this directory with a local static server.
Everything stays in the page; there is no fetch, WebSocket, authentication, TTL,
persistence, encryption or host integration. Eight in-memory receipts and a
four-event replay ring bound the example. Reset discards the teaching world.

Browser assertions and inspected captures checked:

- Lost reply starts unknown; same-ID retry replays, leaving one server event and
  one displayed message. Changed-body retry conflicts without changing it.
- Offline reconnect does not silently send; explicit retry delivers once.
- Rejected operations cannot retry; a new message sent with Enter works.
- HTML-like message text renders literally, not as elements.
- History gap leaves cursor zero and requests explicit resynchronization.
- 360 CSS-pixel viewport has no horizontal overflow.

Pointer recordings show moving across neighboring controls, retry, conflict and
reconnect; trace selection has a separate recording. Both were inspected using
5 fps contact sheets. UI checks are recorded manual validation, not a permanent
browser suite. Chromium was used, not Safari, mobile hardware or a real service.

## Combined result and remaining boundaries

56 unittest tests passed with **zero skips** when the optional pinned oha,
wasm-tools, Collector, Markdown dependencies and live synthetic Prometheus were
configured. Without those dependencies their integration checks explicitly skip.
No runtime or guest test suite was modified or claimed to pass.

All 48 backlog items now have a tested bounded tool, a written recipe, or an
explicit partial outcome. Remaining NH26 async execution/startup evidence needs
a compatible host binding/toolchain; NH28 non-Linux/platform-jail claims need
actual platform access; NH37 signature trust and online registry adoption need
trust policy/access decisions. NH29 remains a conceptual scheduler contract,
not approval to implement alarms. These are deferred, not silently completed.
Further production integration, package publishing, recovery work or general
runtime changes are outside this work's authorization.
