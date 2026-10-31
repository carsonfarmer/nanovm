//! Bounded OTLP/HTTP export. Workers use the existing egress link; only the
//! supervisor knows the collector and stamps the resource's tenant identity.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Full, Limited};
use opentelemetry::{Context, KeyValue};
use opentelemetry::logs::{LogRecord, Logger, LoggerProvider};
use opentelemetry::metrics::MeterProvider;
use opentelemetry::trace::{TraceContextExt, Tracer, TracerProvider};
use opentelemetry_http::{HttpClient, HttpError};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_proto::tonic::{collector, common::v1 as common, metrics::v1 as metrics, resource::v1::Resource};
use opentelemetry_sdk::{logs, metrics as sdk_metrics, trace};
use prost::Message;

use crate::egress::Upstream;

pub(crate) const PATH: &str = "/telemetry/";
const MAX_BATCH: usize = 1 << 20;
const DEADLINE: Duration = Duration::from_secs(2);
const INTERVAL: Duration = Duration::from_secs(1);

fn identity(app: &str, digest: &str) -> [KeyValue; 3] {
    [KeyValue::new("service.name", "nanovm"), KeyValue::new("nanovm.app", app.to_owned()),
        KeyValue::new("service.version", digest.to_owned())]
}

/// Export threads poll outside Tokio. Spawn link I/O on the worker's runtime
/// and await its result without blocking that runtime or opening any sockets.
#[derive(Clone)]
struct Client {
    runtime: tokio::runtime::Handle,
    upstream: Option<Upstream>,
    relay: Option<Arc<Relay>>,
    app: String,
    digest: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("TelemetryClient") }
}

#[async_trait::async_trait]
impl HttpClient for Client {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        if request.body().len() > MAX_BATCH { return Err("telemetry batch too large".into()) }
        let client = self.clone();
        let task = self.runtime.spawn(async move {
            tokio::time::timeout(DEADLINE, async {
                let signal = request.uri().path().rsplit('/').next().unwrap_or("").to_owned();
                let status = if let Some(upstream) = client.upstream {
                    let req = Request::post(format!("http://nanovm{PATH}{signal}"))
                        .body(Full::new(request.into_body()).map_err(|e| match e {}).boxed_unsync())?;
                    let res = upstream.send(req).await?;
                    let status = res.status();
                    Limited::new(res.into_body(), 4096).collect().await?;
                    status.as_u16()
                } else {
                    client.relay.as_ref().expect("direct client has relay")
                        .send(&signal, request.into_body(), &client.app, &client.digest).await
                };
                Ok::<_, HttpError>(Response::builder().status(status).body(Bytes::new())?)
            }).await.map_err(|e| -> HttpError { Box::new(e) })?
        });
        task.await.map_err(|e| -> HttpError { Box::new(e) })?
    }
}

pub struct Telemetry {
    traces: trace::SdkTracerProvider,
    meters: sdk_metrics::SdkMeterProvider,
    logs: logs::SdkLoggerProvider,
    tracer: trace::SdkTracer,
    logger: logs::SdkLogger,
    requests: opentelemetry::metrics::Counter<u64>,
    latency: opentelemetry::metrics::Histogram<f64>,
}

impl Telemetry {
    pub fn new(endpoint: &str, app: &str, digest: &str, upstream: Option<Upstream>) -> anyhow::Result<Self> {
        let client = Client {
            runtime: tokio::runtime::Handle::current(),
            relay: if upstream.is_none() { Some(Arc::new(Relay::new(endpoint)?)) } else { None },
            upstream, app: app.into(), digest: digest.into(),
        };
        let resource = opentelemetry_sdk::Resource::builder_empty().with_attributes(identity(app, digest)).build();
        // Programmatic bounds override SDK environment settings. No retries:
        // telemetry is best effort, and a dead collector must not build a backlog.
        macro_rules! exporter {
            ($kind:ident, $signal:literal) => {
                opentelemetry_otlp::$kind::builder().with_http()
                    .with_endpoint(concat!("http://nanovm/v1/", $signal))
                    .with_http_client(client.clone()).with_timeout(DEADLINE)
                    .with_max_request_body_size(MAX_BATCH)
                    .with_retry_policy(opentelemetry_otlp::RetryPolicy::default().with_max_retries(0)).build()?
            }
        }
        let traces = trace::SdkTracerProvider::builder().with_resource(resource.clone())
            .with_span_processor(trace::BatchSpanProcessor::builder(exporter!(SpanExporter, "traces"))
                .with_batch_config(trace::BatchConfigBuilder::default().with_max_queue_size(1024)
                    .with_max_export_batch_size(64).with_scheduled_delay(INTERVAL).build()).build())
            .build();
        let logs = logs::SdkLoggerProvider::builder().with_resource(resource.clone())
            .with_log_processor(logs::BatchLogProcessor::builder(exporter!(LogExporter, "logs"))
                .with_batch_config(logs::BatchConfigBuilder::default().with_max_queue_size(256)
                    .with_max_export_batch_size(64).with_scheduled_delay(INTERVAL).build()).build())
            .build();
        let meters = sdk_metrics::SdkMeterProvider::builder().with_resource(resource)
            .with_reader(sdk_metrics::PeriodicReader::builder(exporter!(MetricExporter, "metrics"))
                .with_interval(INTERVAL).build())
            .with_view(|_| sdk_metrics::Stream::builder().with_cardinality_limit(2048).build().ok()).build();
        let meter = meters.meter("nanovm");
        Ok(Self {
            tracer: traces.tracer("nanovm"), logger: logs.logger("nanovm"),
            requests: meter.u64_counter("nanovm.requests").build(),
            latency: meter.f64_histogram("nanovm.request.duration").with_unit("s").build(),
            traces, meters, logs,
        })
    }

    pub fn span(&self, name: &'static str, object: &str, parent: &Context) -> Span {
        let span = self.tracer.span_builder(name)
            .with_attributes([KeyValue::new("nanovm.object", object.to_owned())])
            .start_with_context(&self.tracer, parent);
        Span(parent.with_span(span))
    }

    pub fn request(&self, object: &str, status: u16, elapsed: Duration) {
        let attrs = [KeyValue::new("nanovm.object", object.to_owned()), KeyValue::new("http.response.status_code", status as i64)];
        self.requests.add(1, &attrs);
        self.latency.record(elapsed.as_secs_f64(), &attrs);
    }

    pub fn observe(&self, node: &Arc<crate::table::Node>) {
        use std::sync::atomic::Ordering::Relaxed;
        let meter = self.meters.meter("nanovm");
        macro_rules! counters {
            ($($field:ident),* $(,)?) => { $(
                let weak = Arc::downgrade(node);
                meter.u64_observable_counter(concat!("nanovm.", stringify!($field)))
                    .with_callback(move |o| {
                        if let Some(node) = weak.upgrade() { o.observe(node.metrics.$field.load(Relaxed), &[]); }
                    }).build();
            )* }
        }
        counters!(instantiate_failures, evicted_idle, evicted_lru, rejected_busy,
            rejected_capacity, egress_allowed, egress_denied, fs_denied, guest_cpu_ns);
        let weak = Arc::downgrade(node);
        meter.u64_observable_counter("nanovm.poisons").with_callback(move |o| {
            if let Some(node) = weak.upgrade() {
                for p in crate::metrics::Poison::ALL {
                    o.observe(node.metrics.poisons[p as usize].load(Relaxed), &[KeyValue::new("reason", p.as_str())]);
                }
            }
        }).build();
        macro_rules! gauge {
            ($name:literal, $node:ident, $value:expr) => {
                let weak = Arc::downgrade(node);
                meter.u64_observable_gauge($name).with_callback(move |o| {
                    if let Some($node) = weak.upgrade() { o.observe($value as u64, &[]); }
                }).build();
            }
        }
        gauge!("nanovm.stores.live", n, n.metrics.stores_live.load(Relaxed));
        gauge!("nanovm.stores.peak", n, n.metrics.stores_peak.load(Relaxed));
        gauge!("nanovm.objects.live", n, n.live_objects());
        gauge!("nanovm.objects.limit", n, n.cfg.max_live_objects);
        gauge!("process.memory.usage", _n, crate::metrics::rss_bytes());
        gauge!("nanovm.uptime", n, n.started.elapsed().as_secs());
    }

    pub fn log(&self, object: &str, stream: &'static str, bytes: &[u8], context: &Context) {
        // A guest write can be arbitrarily large. Export at most one bounded
        // record per write, including expansion of invalid UTF-8. The queue
        // retains at most 1 MiB of log bodies (plus attributes and SDK overhead).
        let mut record = self.logger.create_log_record();
        let mut body = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).into_owned();
        body.truncate(body.floor_char_boundary(4096.min(body.len())));
        record.set_body(body.into());
        record.add_attributes([("nanovm.object", object.to_owned()), ("log.iostream", stream.to_owned())]);
        let span = context.span();
        let sc = span.span_context();
        if sc.is_valid() { record.set_trace_context(sc.trace_id(), sc.span_id(), Some(sc.trace_flags())); }
        self.logger.emit(record);
    }

    /// Call from a blocking task: SDK flushing waits for its export threads,
    /// whose link operations still need the async runtime to be running.
    pub fn flush(&self) -> anyhow::Result<()> {
        self.traces.force_flush()?;
        self.meters.force_flush()?;
        self.logs.force_flush()?;
        Ok(())
    }
}

pub struct Span(pub Context);
impl Drop for Span {
    fn drop(&mut self) { self.0.span().end(); }
}

/// Shared across all workers. A compromised worker gets neither an arbitrary
/// destination nor an unbounded number of collector connections.
pub(crate) struct Relay {
    endpoint: String,
    http: reqwest::Client,
    slots: tokio::sync::Semaphore,
}

impl Relay {
    pub fn new(endpoint: &str) -> anyhow::Result<Self> {
        let url = reqwest::Url::parse(endpoint)?;
        anyhow::ensure!(matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
            && url.username().is_empty() && url.password().is_none() && url.query().is_none() && url.fragment().is_none(),
            "telemetry_endpoint must be an HTTP(S) base URL without credentials, query or fragment");
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').into(),
            http: reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(DEADLINE).build()?,
            slots: tokio::sync::Semaphore::new(16),
        })
    }

    pub async fn handle(&self, req: Request<hyper::body::Incoming>, app: &str, digest: &str) -> u16 {
        if req.method() != http::Method::POST { return 405 }
        let signal = req.uri().path().strip_prefix(PATH).unwrap_or("").to_owned();
        let Ok(_slot) = self.slots.try_acquire() else { return 429 };
        match tokio::time::timeout(DEADLINE, async {
            match Limited::new(req.into_body(), MAX_BATCH).collect().await {
                Ok(body) => self.send(&signal, body.to_bytes(), app, digest).await,
                Err(_) => 413,
            }
        }).await { Ok(status) => status, Err(_) => 504 }
    }

    async fn send(&self, signal: &str, body: Bytes, app: &str, digest: &str) -> u16 {
        if body.len() > MAX_BATCH { return 413 }
        let body = match stamp(signal, &body, app, digest) { Ok(body) => body, Err(_) => return 400 };
        if body.len() > MAX_BATCH { return 413 }
        match self.http.post(format!("{}/v1/{signal}", self.endpoint))
            .header("content-type", "application/x-protobuf").body(body).send().await {
            Ok(res) if res.status().is_success() => 200,
            Ok(res) => res.status().as_u16(),
            Err(_) => 502,
        }
    }
}

fn scrub(attrs: &mut Vec<common::KeyValue>) {
    attrs.retain(|kv| kv.key_strindex == 0 && !matches!(kv.key.as_str(), "service.name" | "service.version" | "nanovm.app"));
}

fn stamp(signal: &str, body: &[u8], app: &str, digest: &str) -> anyhow::Result<Vec<u8>> {
    let resource = Resource {
        attributes: [("service.name", "nanovm"), ("nanovm.app", app), ("service.version", digest)].into_iter()
            .map(|(key, value)| common::KeyValue { key: key.into(), value: Some(common::AnyValue {
                value: Some(common::any_value::Value::StringValue(value.into())) }), ..Default::default() }).collect(),
        ..Default::default()
    };
    macro_rules! batch {
        ($ty:ty, $resources:ident, $scopes:ident, $records:ident, $record:ident, $clean:block) => {{
            let mut batch = <$ty>::decode(body)?;
            anyhow::ensure!(batch.$resources.len() <= 16, "too many telemetry resources");
            for r in &mut batch.$resources {
                r.resource = Some(resource.clone());
                for scope in &mut r.$scopes {
                    if let Some(s) = &mut scope.scope { scrub(&mut s.attributes); }
                    for $record in &mut scope.$records $clean
                }
            }
            batch.encode_to_vec()
        }}
    }
    Ok(match signal {
        "traces" => batch!(collector::trace::v1::ExportTraceServiceRequest, resource_spans, scope_spans, spans, s, {
            scrub(&mut s.attributes);
            for e in &mut s.events { scrub(&mut e.attributes); }
            for l in &mut s.links { scrub(&mut l.attributes); }
        }),
        "logs" => batch!(collector::logs::v1::ExportLogsServiceRequest, resource_logs, scope_logs, log_records, l, {
            scrub(&mut l.attributes);
        }),
        "metrics" => batch!(collector::metrics::v1::ExportMetricsServiceRequest, resource_metrics, scope_metrics, metrics, m, {
            macro_rules! points { ($data:expr) => { for p in &mut $data.data_points {
                scrub(&mut p.attributes);
                for e in &mut p.exemplars { scrub(&mut e.filtered_attributes); }
            } } }
            if let Some(data) = &mut m.data {
                match data {
                    metrics::metric::Data::Gauge(d) => points!(d),
                    metrics::metric::Data::Sum(d) => points!(d),
                    metrics::metric::Data::Histogram(d) => points!(d),
                    metrics::metric::Data::ExponentialHistogram(d) => points!(d),
                    metrics::metric::Data::Summary(d) => for p in &mut d.data_points { scrub(&mut p.attributes); },
                }
            }
        }),
        _ => anyhow::bail!("unknown telemetry signal"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn attr(key: &str, value: &str) -> common::KeyValue {
        common::KeyValue { key: key.into(), value: Some(common::AnyValue {
            value: Some(common::any_value::Value::StringValue(value.into())),
        }), ..Default::default() }
    }

    /// Exercise the private relay as a compromised worker, not a guest:
    /// spoofed identity, oversized input, and a body that never finishes.
    #[tokio::test]
    async fn relay_stamps_identity_and_bounds_worker_requests() {
        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let output = captured.clone();
        let collector = tokio::spawn(crate::server::serve_http1(listener, move |req| {
            let output = output.clone();
            async move {
                assert!(matches!(req.uri().path(), "/v1/traces" | "/v1/logs" | "/v1/metrics"));
                assert!(req.headers().get("nanovm-app").is_none());
                let body = req.into_body().collect().await.unwrap().to_bytes();
                output.lock().push(body);
                crate::server::ok_json(serde_json::json!({}))
            }
        }));
        let relay = Arc::new(Relay::new(&endpoint).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(crate::server::serve_http1(listener, move |req| {
            let relay = relay.clone();
            async move { crate::object::error_response(relay.handle(req, "real-app", "real-digest").await, "", "") }
        }));
        let forged = vec![attr("nanovm.app", "victim"), attr("nanovm.app", "other"), attr("service.version", "old")];
        let message = collector::trace::v1::ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource { attributes: forged.clone(), ..Default::default() }),
                scope_spans: vec![ScopeSpans {
                    scope: Some(common::InstrumentationScope { attributes: forged.clone(), ..Default::default() }),
                    spans: vec![Span { attributes: [forged.clone(), vec![attr("nanovm.object", "room")]].concat(), ..Default::default() }],
                    ..Default::default()
                }], ..Default::default()
            }],
        }.encode_to_vec();
        let http = reqwest::Client::new();
        let url = format!("http://{addr}{PATH}traces");
        assert_eq!(http.post(&url).header("nanovm-app", "victim").header("nanovm-app", "other")
            .body(message).send().await.unwrap().status(), 200);
        let batch = collector::trace::v1::ExportTraceServiceRequest::decode(captured.lock()[0].clone()).unwrap();
        let r = &batch.resource_spans[0];
        assert_eq!(r.resource.as_ref().unwrap().attributes,
            vec![attr("service.name", "nanovm"), attr("nanovm.app", "real-app"), attr("service.version", "real-digest")]);
        assert!(r.scope_spans[0].scope.as_ref().unwrap().attributes.is_empty());
        assert_eq!(r.scope_spans[0].spans[0].attributes, vec![attr("nanovm.object", "room")]);
        let trusted = r.resource.clone();
        use opentelemetry_proto::tonic::logs::v1::{ResourceLogs, ScopeLogs, LogRecord};
        let logs = collector::logs::v1::ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource { attributes: forged.clone(), ..Default::default() }),
                scope_logs: vec![ScopeLogs {
                    log_records: vec![LogRecord { attributes: forged.clone(), ..Default::default() }],
                    ..Default::default()
                }], ..Default::default()
            }],
        };
        assert_eq!(http.post(format!("http://{addr}{PATH}logs")).body(logs.encode_to_vec()).send().await.unwrap().status(), 200);
        let logs = collector::logs::v1::ExportLogsServiceRequest::decode(captured.lock()[1].clone()).unwrap();
        assert_eq!(logs.resource_logs[0].resource, trusted);
        assert!(logs.resource_logs[0].scope_logs[0].log_records[0].attributes.is_empty());
        let metrics = collector::metrics::v1::ExportMetricsServiceRequest {
            resource_metrics: vec![metrics::ResourceMetrics {
                resource: Some(Resource { attributes: forged.clone(), ..Default::default() }),
                scope_metrics: vec![metrics::ScopeMetrics {
                    metrics: vec![metrics::Metric {
                        name: "requests".into(),
                        data: Some(metrics::metric::Data::Gauge(metrics::Gauge {
                            data_points: vec![metrics::NumberDataPoint {
                                attributes: forged.clone(),
                                exemplars: vec![metrics::Exemplar { filtered_attributes: forged, ..Default::default() }],
                                ..Default::default()
                            }],
                        })), ..Default::default()
                    }], ..Default::default()
                }], ..Default::default()
            }],
        };
        assert_eq!(http.post(format!("http://{addr}{PATH}metrics")).body(metrics.encode_to_vec()).send().await.unwrap().status(), 200);
        let metrics = collector::metrics::v1::ExportMetricsServiceRequest::decode(captured.lock()[2].clone()).unwrap();
        assert_eq!(metrics.resource_metrics[0].resource, trusted);
        let Some(metrics::metric::Data::Gauge(g)) = &metrics.resource_metrics[0].scope_metrics[0].metrics[0].data else { panic!("gauge lost") };
        assert!(g.data_points[0].attributes.is_empty());
        assert!(g.data_points[0].exemplars[0].filtered_attributes.is_empty());
        assert_eq!(http.post(&url).body(vec![0; (1 << 20) + 1]).send().await.unwrap().status(), 413);
        assert_eq!(http.post(&url).body("not protobuf").send().await.unwrap().status(), 400);
        let mut slow = tokio::net::TcpStream::connect(addr).await.unwrap();
        slow.write_all(b"POST /telemetry/traces HTTP/1.1\r\nHost: nanovm\r\nContent-Length: 10\r\nConnection: close\r\n\r\nx").await.unwrap();
        let mut out = String::new();
        tokio::time::timeout(Duration::from_secs(4), slow.read_to_string(&mut out)).await.unwrap().unwrap();
        assert!(out.starts_with("HTTP/1.1 504"), "{out}");
        assert_eq!(captured.lock().len(), 3, "invalid batches must never reach the collector");

        // The producer must bound the UTF-8 body, not merely its input bytes.
        let telemetry = Telemetry::new(&endpoint, "real-app", "real-digest", None).unwrap();
        telemetry.log("room", "stdout", &vec![0xff; 8192], &Context::new());
        tokio::task::spawn_blocking(move || telemetry.flush().unwrap()).await.unwrap();
        let logs = collector::logs::v1::ExportLogsServiceRequest::decode(captured.lock()[3].clone()).unwrap();
        let record = &logs.resource_logs[0].scope_logs[0].log_records[0];
        let Some(common::any_value::Value::StringValue(body)) = record.body.as_ref().and_then(|b| b.value.as_ref()) else { panic!("log body missing") };
        assert_eq!(body, &"\u{fffd}".repeat(1365));
        collector.abort();
        server.abort();
    }
}
