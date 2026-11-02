//! Bounded OTLP/HTTP export. Workers use the existing egress link; only the
//! supervisor knows the collector and stamps the resource's tenant identity.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Full, Limited};
use opentelemetry::{Context, KeyValue};
use opentelemetry::logs::{LogRecord, Logger, LoggerProvider};
use opentelemetry::trace::TraceContextExt;
use opentelemetry::trace::{Tracer, TracerProvider};
use opentelemetry_http::{HttpClient, HttpError};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_proto::tonic::{collector, common::v1 as common, resource::v1::Resource};
use opentelemetry_sdk::logs;
use opentelemetry_sdk::trace;
pub use opentelemetry_sdk::trace::Span;
use prost::Message;

use crate::egress::Upstream;

pub fn duration(span: &mut Span, field: &'static str, since: std::time::Instant) {
    use opentelemetry::trace::Span as _;
    span.set_attribute(KeyValue::new(field, since.elapsed().as_secs_f64() * 1000.0));
}

pub fn response<B>(span: &mut Span, response: &Response<B>) {
    use opentelemetry::trace::Span as _;
    span.set_attribute(KeyValue::new("http.response.status_code", response.status().as_u16() as i64));
    if response.status().is_server_error() {
        span.set_status(opentelemetry::trace::Status::error("request failed"));
        let reason = response.headers().get("x-nanovm-error").and_then(|v| v.to_str().ok()).unwrap_or("http-error");
        span.set_attribute(KeyValue::new("error.type", reason.chars().take(128).collect::<String>()));
    }
}

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
                if let Some(upstream) = client.upstream {
                    let req = Request::post(format!("http://nanovm{PATH}{signal}"))
                        .body(Full::new(request.into_body()).map_err(|e| match e {}).boxed_unsync())?;
                    let res = upstream.send(req).await?;
                    let (parts, body) = res.into_parts();
                    Ok(Response::from_parts(parts, Limited::new(body, 4096).collect().await?.to_bytes()))
                } else {
                    client.relay.as_ref().expect("direct client has relay")
                        .send(&signal, request.into_body()).await.map_err(|status| format!("collector transport failed: {status}").into())
                }
            }).await.map_err(|e| -> HttpError { Box::new(e) })?
        });
        task.await.map_err(|e| -> HttpError { Box::new(e) })?
    }
}

pub struct Telemetry {
    runtime: tokio::runtime::Handle,
    traces: trace::SdkTracerProvider,
    logs: logs::SdkLoggerProvider,
    tracer: trace::SdkTracer,
    logger: logs::SdkLogger,
    snapshot: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl Telemetry {
    pub fn new(endpoint: &str, app: &str, digest: &str, upstream: Option<Upstream>) -> anyhow::Result<Self> {
        let client = Client {
            runtime: tokio::runtime::Handle::current(),
            relay: if upstream.is_none() { Some(Arc::new(Relay::new(endpoint)?)) } else { None },
            upstream,
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
            .with_sampler(trace::Sampler::AlwaysOn)
            .with_span_processor(trace::BatchSpanProcessor::builder(exporter!(SpanExporter, "traces"))
                .with_batch_config(trace::BatchConfigBuilder::default().with_max_queue_size(2048)
                    .with_max_export_batch_size(256).with_scheduled_delay(INTERVAL).build()).build())
            .build();
        let logs = logs::SdkLoggerProvider::builder().with_resource(resource.clone())
            .with_log_processor(logs::BatchLogProcessor::builder(exporter!(LogExporter, "logs"))
                .with_batch_config(logs::BatchConfigBuilder::default().with_max_queue_size(256)
                    .with_max_export_batch_size(64).with_scheduled_delay(INTERVAL).build()).build())
            .build();
        Ok(Self {
            runtime: client.runtime.clone(),
            tracer: traces.tracer("nanovm"),
            traces,
            logger: logs.logger("nanovm"),
            logs,
            snapshot: Default::default(),
        })
    }

    pub fn span(&self, name: &'static str, object: &str, parent: &Context) -> Span {
        // Request metadata and phase durations stay on this one event.
        let mut attributes = Vec::with_capacity(if name == "request" { 16 } else { 1 });
        attributes.push(KeyValue::new("nanovm.object", object.to_owned()));
        self.tracer.span_builder(name)
            .with_attributes(attributes)
            .start_with_context(&self.tracer, parent)
    }

    /// The callback holds only a weak owner. Neither the timer nor SDK owns
    /// a node/worker lifetime, and explicit flush includes a fresh snapshot.
    pub fn snapshots(self: &Arc<Self>, snapshot: impl Fn() + Send + Sync + 'static) {
        assert!(self.snapshot.set(Box::new(snapshot)).is_ok());
        let weak = Arc::downgrade(self);
        self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(INTERVAL).await;
                let Some(telemetry) = weak.upgrade() else { break };
                telemetry.snapshot.get().unwrap()();
            }
        });
    }

    pub fn snapshot(&self, attributes: Vec<(opentelemetry::Key, opentelemetry::logs::AnyValue)>) {
        let mut record = self.logger.create_log_record();
        record.add_attribute("event.name", "nanovm.snapshot");
        record.add_attributes(attributes);
        self.logger.emit(record);
    }

    pub fn log(&self, object: &str, field: (&'static str, &'static str), bytes: &[u8], context: &Context) {
        // A guest write can be arbitrarily large. Export at most one bounded
        // record per write, including expansion of invalid UTF-8. The queue
        // retains at most 1 MiB of log bodies (plus attributes and SDK overhead).
        let mut record = self.logger.create_log_record();
        let mut body = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).into_owned();
        body.truncate(body.floor_char_boundary(4096.min(body.len())));
        record.set_body(body.into());
        record.add_attribute(field.0, field.1);
        record.add_attribute("nanovm.object", object.to_owned());
        let span = context.span();
        let sc = span.span_context();
        if sc.is_valid() { record.set_trace_context(sc.trace_id(), sc.span_id(), Some(sc.trace_flags())); }
        self.logger.emit(record);
    }

    /// Call from a blocking task: SDK flushing waits for its export threads,
    /// whose link operations still need the async runtime to be running.
    pub fn flush(&self) -> anyhow::Result<()> {
        if let Some(snapshot) = self.snapshot.get() { snapshot(); }
        self.traces.force_flush().and(self.logs.force_flush()).map_err(Into::into)
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        // SDK shutdown waits for export threads. Keep that wait off both the
        // async runtime and a snapshot callback dropping its last Node reference.
        let traces = self.traces.clone();
        let logs = self.logs.clone();
        self.runtime.spawn_blocking(move || {
            let _ = traces.shutdown_with_timeout(DEADLINE);
            let _ = logs.shutdown_with_timeout(DEADLINE);
        });
    }
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

    pub async fn handle(&self, req: Request<hyper::body::Incoming>, app: &str, digest: &str) -> Result<Response<Bytes>, u16> {
        if req.method() != http::Method::POST { return Err(405) }
        let signal = req.uri().path().strip_prefix(PATH).unwrap_or("").to_owned();
        let _slot = self.slots.try_acquire().map_err(|_| 429u16)?;
        match tokio::time::timeout(DEADLINE, async {
            let body = Limited::new(req.into_body(), MAX_BATCH).collect().await.map_err(|_| 413u16)?;
            let body = stamp(&signal, &body.to_bytes(), app, digest).map_err(|_| 400u16)?;
            self.send(&signal, body.into()).await
        }).await { Ok(response) => response, Err(_) => Err(504) }
    }

    async fn send(&self, signal: &str, body: Bytes) -> Result<Response<Bytes>, u16> {
        if body.len() > MAX_BATCH { return Err(413) }
        let res = self.http.post(format!("{}/v1/{signal}", self.endpoint))
            .header("content-type", "application/x-protobuf").body(body).send().await.map_err(|_| 502u16)?;
        let status = res.status();
        let body = Limited::new(reqwest::Body::from(res), 4096).collect().await.map_err(|_| 502u16)?;
        Ok(Response::builder().status(status).body(body.to_bytes()).unwrap())
    }
}

fn scrub(attrs: &mut Vec<common::KeyValue>) {
    attrs.retain(|kv| kv.key_strindex == 0 && !matches!(kv.key.as_str(), "service.name" | "service.version" | "nanovm.app"));
}

fn stamp(signal: &str, body: &[u8], app: &str, digest: &str) -> anyhow::Result<Vec<u8>> {
    let resource = Resource {
        attributes: opentelemetry_proto::transform::common::tonic::Attributes::from(identity(app, digest)).0,
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

    #[tokio::test]
    async fn brief_collector_stall_retains_a_request_burst() {
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        let received = Arc::new(AtomicUsize::new(0));
        let arrived = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (count, first, gate) = (received.clone(), arrived.clone(), release.clone());
        let server = tokio::spawn(crate::server::serve_http1(listener, move |req| {
            let (count, first, gate) = (count.clone(), first.clone(), gate.clone());
            async move {
                let body = Limited::new(req.into_body(), MAX_BATCH).collect().await.unwrap().to_bytes();
                let batch = collector::trace::v1::ExportTraceServiceRequest::decode(body).unwrap();
                let n = batch.resource_spans.iter().flat_map(|r| &r.scope_spans).flat_map(|s| &s.spans).count();
                if count.fetch_add(n, Relaxed) == 0 {
                    first.notify_one();
                    gate.notified().await;
                }
                Response::new(Full::new(Bytes::new()).map_err(|e| match e {}).boxed_unsync())
            }
        }));
        for (burst, expected) in [(1536, 1792), (4096, 2304)] {
            received.store(0, Relaxed);
            let telemetry = Telemetry::new(&endpoint, "burst", "v1", None).unwrap();
            for _ in 0..256 { drop(telemetry.span("request", "room", &Context::new())); }
            tokio::time::timeout(Duration::from_secs(2), arrived.notified()).await.unwrap();
            // Complete requests while an export is in flight. A short burst
            // survives; overload drops at the bound instead of blocking.
            for _ in 0..burst { drop(telemetry.span("request", "room", &Context::new())); }
            let flush = tokio::task::spawn_blocking(move || telemetry.flush());
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(!flush.is_finished(), "flush returned before the in-flight export completed");
            release.notify_one();
            flush.await.unwrap().unwrap();
            assert_eq!(received.load(Relaxed), expected, "burst={burst}");
        }
        server.abort();
    }

    #[tokio::test]
    async fn collector_responses_reach_the_sdk_and_remain_bounded() {
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        use opentelemetry_sdk::trace::SpanExporter as _;
        use tracing_subscriber::util::SubscriberInitExt;
        #[derive(Clone)]
        struct Output(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Output {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let output = Output(Arc::default());
        let writer = output.clone();
        let _subscriber = tracing_subscriber::fmt().with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
            .with_ansi(false).with_writer(move || writer.clone()).finish().set_default();
        let mode = Arc::new(AtomicUsize::new(0));
        let behavior = mode.clone();
        let partial = collector::trace::v1::ExportTraceServiceResponse {
            partial_success: Some(collector::trace::v1::ExportTracePartialSuccess {
                rejected_spans: 7, error_message: "collector test rejection".into(),
            }),
        }.encode_to_vec();
        let reply = partial.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay = Arc::new(Relay::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap());
        let collector = tokio::spawn(crate::server::serve_http1(listener, move |req| {
            let behavior = behavior.clone();
            let reply = reply.clone();
            async move {
                req.into_body().collect().await.unwrap();
                let mode = behavior.load(Relaxed);
                let body = if mode == 4 {
                    http_body_util::StreamBody::new(futures::stream::pending::<Result<hyper::body::Frame<Bytes>, HttpError>>()).boxed_unsync()
                } else {
                    let bytes = if mode >= 2 { vec![42; 4094 + mode] } else { reply };
                    Full::new(Bytes::from(bytes)).map_err(|e| match e {}).boxed_unsync()
                };
                Response::builder().status(if mode == 1 { 503 } else { 200 }).body(body).unwrap()
            }
        }));
        let (ctl, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let forwarded = relay.clone();
        let link = tokio::spawn(crate::link::serve(tokio::net::UnixStream::from_std(peer).unwrap(), vec![], move |req| {
            let relay = forwarded.clone();
            async move { relay.handle(req, "app", "digest").await
                .map(|r| r.map(|b| Full::new(b).map_err(|e| match e {}).boxed_unsync()))
                .unwrap_or_else(|s| crate::object::error_response(s, "", "")) }
        }));
        let upstream = crate::link::Dialer::new(ctl, 1);
        for upstream in [None, Some(upstream)] {
            let client = Client { runtime: tokio::runtime::Handle::current(), upstream, relay: Some(relay.clone()) };
            for behavior in 0..5 {
                mode.store(behavior, Relaxed);
                let request = Request::post("http://nanovm/v1/traces").body(Bytes::new()).unwrap();
                let response = tokio::time::timeout(Duration::from_secs(4), client.send_bytes(request)).await.expect("collector deadline");
                if behavior < 3 {
                    let response = response.unwrap();
                    assert_eq!(response.status(), if behavior == 1 { 503 } else { 200 });
                    assert_eq!(response.body().as_ref(), if behavior == 2 { vec![42; 4096] } else { partial.clone() });
                } else if let Ok(response) = response {
                    assert!(matches!(response.status().as_u16(), 502 | 504), "unbounded collector reply accepted");
                }
            }
            mode.store(0, Relaxed);
            output.0.lock().unwrap().clear();
            let exporter = opentelemetry_otlp::SpanExporter::builder().with_http()
                .with_endpoint("http://nanovm/v1/traces").with_http_client(client).build().unwrap();
            exporter.export(Vec::new()).await.unwrap();
            let diagnostic = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
            assert!(diagnostic.contains("collector test rejection") && diagnostic.contains("rejected_spans=7"), "{diagnostic}");
        }
        link.abort();
        collector.abort();
    }

    #[tokio::test]
    async fn dropping_telemetry_exports_pending_records_without_blocking_the_runtime() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(crate::server::serve_http1(listener, move |req| {
            let tx = tx.clone();
            async move {
                let path = req.uri().path().to_owned();
                let body = req.into_body().collect().await.unwrap().to_bytes();
                tx.send((path, body)).unwrap();
                http::Response::new(http_body_util::Full::new(Bytes::new()).map_err(|e| match e {}).boxed_unsync())
            }
        }));
        let telemetry = Telemetry::new(&endpoint, "lifecycle", "v1", None).unwrap();
        use opentelemetry::trace::Span as _;
        let span = telemetry.span("last-request", "room", &Context::new());
        assert!(span.is_recording());
        drop(span);
        telemetry.log("room", ("log.iostream", "stdout"), b"last log", &Context::new());
        let started = std::time::Instant::now();
        drop(telemetry);
        assert!(started.elapsed() < Duration::from_millis(250), "SDK shutdown blocked the runtime");
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut seen = std::collections::HashSet::new();
            while seen.len() < 2 {
                let (path, body) = rx.recv().await.unwrap();
                match path.as_str() {
                    "/v1/traces" => assert!(collector::trace::v1::ExportTraceServiceRequest::decode(body).unwrap()
                        .resource_spans.iter().flat_map(|r| &r.scope_spans).flat_map(|s| &s.spans)
                        .any(|s| s.name == "last-request")),
                    "/v1/logs" => assert!(collector::logs::v1::ExportLogsServiceRequest::decode(body).unwrap()
                        .resource_logs.iter().flat_map(|r| &r.scope_logs).flat_map(|s| &s.log_records)
                        .any(|l| l.body.as_ref().and_then(|v| v.value.as_ref()) == Some(&common::any_value::Value::StringValue("last log".into())))),
                    _ => panic!("unexpected signal: {path}"),
                }
                seen.insert(path);
            }
        }).await.expect("all pending signals exported on drop");
        server.abort();
    }

    /// Exercise the private relay as a compromised worker, not a guest:
    /// spoofed identity, oversized input, and a body that never finishes.
    #[tokio::test]
    async fn relay_stamps_identity_and_bounds_worker_requests() {
        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let output = captured.clone();
        let partial = collector::trace::v1::ExportTraceServiceResponse {
            partial_success: Some(collector::trace::v1::ExportTracePartialSuccess {
                rejected_spans: 1, error_message: "collector rejected a span".into(),
            }),
        }.encode_to_vec();
        let reply = partial.clone();
        let collector = tokio::spawn(crate::server::serve_http1(listener, move |req| {
            let output = output.clone();
            let reply = reply.clone();
            async move {
                assert!(matches!(req.uri().path(), "/v1/traces" | "/v1/logs"));
                assert!(req.headers().get("nanovm-app").is_none());
                let body = req.into_body().collect().await.unwrap().to_bytes();
                output.lock().push(body);
                Response::new(Full::new(Bytes::from(reply)).map_err(|e| match e {}).boxed_unsync())
            }
        }));
        let relay = Arc::new(Relay::new(&endpoint).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(crate::server::serve_http1(listener, move |req| {
            let relay = relay.clone();
            async move { relay.handle(req, "real-app", "real-digest").await
                .map(|r| r.map(|b| Full::new(b).map_err(|e| match e {}).boxed_unsync()))
                .unwrap_or_else(|status| crate::object::error_response(status, "", "")) }
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
        let response = http.post(&url).header("nanovm-app", "victim").header("nanovm-app", "other")
            .body(message).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.bytes().await.unwrap().as_ref(), partial.as_slice(), "partial-success body must reach the exporter");
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
        assert_eq!(http.post(format!("http://{addr}{PATH}metrics")).body("").send().await.unwrap().status(), 400);
        assert_eq!(http.post(&url).body(vec![0; (1 << 20) + 1]).send().await.unwrap().status(), 413);
        assert_eq!(http.post(&url).body("not protobuf").send().await.unwrap().status(), 400);
        let mut slow = tokio::net::TcpStream::connect(addr).await.unwrap();
        slow.write_all(b"POST /telemetry/traces HTTP/1.1\r\nHost: nanovm\r\nContent-Length: 10\r\nConnection: close\r\n\r\nx").await.unwrap();
        let mut out = String::new();
        tokio::time::timeout(Duration::from_secs(4), slow.read_to_string(&mut out)).await.unwrap().unwrap();
        assert!(out.starts_with("HTTP/1.1 504"), "{out}");
        assert_eq!(captured.lock().len(), 2, "invalid batches must never reach the collector");

        // The producer must bound the UTF-8 body, not merely its input bytes.
        let telemetry = Telemetry::new(&endpoint, "real-app", "real-digest", None).unwrap();
        telemetry.log("room", ("log.iostream", "stdout"), &vec![0xff; 8192], &Context::new());
        tokio::task::spawn_blocking(move || telemetry.flush().unwrap()).await.unwrap();
        let logs = collector::logs::v1::ExportLogsServiceRequest::decode(captured.lock()[2].clone()).unwrap();
        let record = &logs.resource_logs[0].scope_logs[0].log_records[0];
        let Some(common::any_value::Value::StringValue(body)) = record.body.as_ref().and_then(|b| b.value.as_ref()) else { panic!("log body missing") };
        assert_eq!(body, &"\u{fffd}".repeat(1365));
        collector.abort();
        server.abort();
    }
}
