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
use opentelemetry::trace::{SpanKind, Tracer, TracerProvider};
use opentelemetry_http::{HttpClient, HttpError};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_proto::tonic::resource::v1::Resource;
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

/// A request's method and path, as every span of one carries them.
pub fn request<B>(span: &mut Span, req: &Request<B>) {
    use opentelemetry::trace::Span as _;
    span.set_attribute(KeyValue::new("http.request.method", req.method().as_str().chars().take(32).collect::<String>()));
    let path = req.uri().path();
    span.set_attribute(KeyValue::new("url.path", path[..path.floor_char_boundary(path.len().min(1024))].to_owned()));
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

    pub fn span(&self, name: &'static str, object: &str, parent: &Context, kind: SpanKind) -> Span {
        // Request metadata and phase durations stay on this one event.
        let mut attributes = Vec::with_capacity(if name == "request" { 16 } else { 4 });
        attributes.push(KeyValue::new("nanovm.object", object.to_owned()));
        self.tracer.span_builder(name)
            .with_kind(kind)
            .with_attributes(attributes)
            .start_with_context(&self.tracer, parent)
    }

    /// A forwarding hop ends at response headers; the worker's request event
    /// separately owns the guest lifecycle through its final commit.
    pub async fn forward<B, R, F, Fut>(&self, name: &'static str, mut req: Request<B>, send: F) -> Response<R>
    where F: FnOnce(Request<B>) -> Fut, Fut: Future<Output = Response<R>> {
        use opentelemetry::{propagation::TextMapPropagator, trace::Span as _};
        let propagator = opentelemetry_sdk::propagation::TraceContextPropagator::new();
        let parent = propagator.extract(&opentelemetry_http::HeaderExtractor(req.headers()));
        let object = crate::table::route(req.uri().path()).map_or("", |(id, _)| id);
        let mut span = scopeguard::guard(self.span(name, object, &parent, SpanKind::Server), |mut span| {
            span.set_status(opentelemetry::trace::Status::error("cancelled"));
            span.set_attribute(KeyValue::new("error.type", "cancelled"));
        });
        request(&mut span, &req);
        let context = parent.with_remote_span_context(span.span_context().clone());
        propagator.inject_context(&context, &mut opentelemetry_http::HeaderInjector(req.headers_mut()));
        let res = send(req).await;
        let mut span = scopeguard::ScopeGuard::into_inner(span);
        response(&mut span, &res);
        res
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

/// Replaces each resource of a worker's OTLP batch with the stamped one and
/// drops identity attributes from scopes, spans, events, links and log
/// records, at the wire level: the rest is copied as bytes. Decoding the
/// batch instead cost up to 200 times its size, as 2-byte empty spans (N-74).
fn stamp(signal: &str, body: &[u8], app: &str, digest: &str) -> anyhow::Result<Vec<u8>> {
    let top = match signal {
        "traces" => Kind::Batch(true),
        "logs" => Kind::Batch(false),
        _ => anyhow::bail!("unknown telemetry signal"),
    };
    let resource = Resource {
        attributes: opentelemetry_proto::transform::common::tonic::Attributes::from(identity(app, digest)).0,
        ..Default::default()
    }.encode_to_vec();
    let mut out = Vec::with_capacity(body.len() + 64);
    rewrite(top, body, &resource, &mut out)?;
    Ok(out)
}

/// An OTLP message holding resources, scopes or attributes; `true` for
/// traces, `false` for logs.
#[derive(Clone, Copy)]
enum Kind { Batch(bool), Resource(bool), Scope(bool), ScopeInfo, Span, Event, Link, Log }

fn rewrite(kind: Kind, mut m: &[u8], resource: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
    use Kind::*;
    if let Resource(_) = kind {
        nested(out, 1 << 3 | LEN, |out| Ok(out.extend_from_slice(resource)))?;
    }
    let mut resources = 0;
    while !m.is_empty() {
        let start = m;
        let (key, value) = field(&mut m)?;
        if let (Batch(_), 1) = (kind, key >> 3) {
            resources += 1;
            anyhow::ensure!(resources <= 16, "too many telemetry resources");
        }
        let child = match (kind, key >> 3) {
            (Batch(traces), 1) => Some(Resource(traces)),
            (Resource(_), 1) => continue,
            (Resource(traces), 2) => Some(Scope(traces)),
            (Scope(_), 1) => Some(ScopeInfo),
            (Scope(true), 2) => Some(Span),
            (Scope(false), 2) => Some(Log),
            (Span, 11) => Some(Event),
            (Span, 13) => Some(Link),
            (ScopeInfo, 3) | (Span, 9) | (Event, 3) | (Link, 4) | (Log, 6) if !trusted(key, value)? => continue,
            _ => None,
        };
        match child {
            Some(kind) if key & 7 == LEN => nested(out, key, |out| rewrite(kind, value, resource, out))?,
            Some(_) => anyhow::bail!("bad telemetry message"),
            None => out.extend_from_slice(&start[..start.len() - m.len()]),
        }
    }
    Ok(())
}

/// Whether an attribute (`KeyValue`) may pass: its key, as decoders read it
/// (the last), is not one the supervisor stamps, and is not a string-table
/// index.
fn trusted(key: u64, mut kv: &[u8]) -> anyhow::Result<bool> {
    anyhow::ensure!(key & 7 == LEN, "bad attribute");
    let (mut name, mut index) = (&[][..], 0);
    while !kv.is_empty() {
        match field(&mut kv)? {
            (k, value) if k == 1 << 3 | LEN => name = value,
            (k, mut value) if k == 3 << 3 => index = varint(&mut value)?,
            (k, _) if matches!(k >> 3, 1 | 3) => anyhow::bail!("bad attribute"),
            _ => {}
        }
    }
    Ok(index == 0 && !matches!(name, b"service.name" | b"service.version" | b"nanovm.app"))
}

const LEN: u64 = 2;

/// Writes field `key` holding what `f` writes.
fn nested(out: &mut Vec<u8>, key: u64, f: impl FnOnce(&mut Vec<u8>) -> anyhow::Result<()>) -> anyhow::Result<()> {
    put_varint(key, out);
    // Room for the length (5 bytes reach 32 GiB), shrunk to fit after.
    let at = out.len();
    out.extend([0; 5]);
    f(out)?;
    let mut len = [0; 5];
    let n = put_varint((out.len() - at - 5) as u64, &mut len.as_mut_slice());
    out[at..at + n].copy_from_slice(&len[..n]);
    out.copy_within(at + 5.., at + n);
    out.truncate(out.len() - (5 - n));
    Ok(())
}

/// The next field: its key, and its contents (a varint's bytes, for one).
fn field<'a>(m: &mut &'a [u8]) -> anyhow::Result<(u64, &'a [u8])> {
    let key = varint(m)?;
    let n = match key & 7 {
        0 => m.iter().position(|b| b & 0x80 == 0).map_or(m.len() + 1, |i| i + 1),
        1 => 8,
        LEN => varint(m)? as usize,
        5 => 4,
        _ => anyhow::bail!("unsupported wire type"),
    };
    anyhow::ensure!(n <= m.len(), "truncated telemetry batch");
    let (value, rest) = m.split_at(n);
    *m = rest;
    Ok((key, value))
}

fn varint(m: &mut &[u8]) -> anyhow::Result<u64> {
    let mut v = 0;
    for i in 0..10 {
        let (&b, rest) = m.split_first().ok_or_else(|| anyhow::anyhow!("truncated telemetry batch"))?;
        *m = rest;
        v |= u64::from(b & 0x7f) << (7 * i);
        if b < 0x80 {
            return Ok(v);
        }
    }
    anyhow::bail!("bad varint")
}

/// Writes `v` as a varint, returning its length.
fn put_varint(mut v: u64, out: &mut impl std::io::Write) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        let _ = out.write_all(&[v as u8 | 0x80]);
        v >>= 7;
        n += 1;
    }
    let _ = out.write_all(&[v as u8]);
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::{collector, common::v1 as common};
    use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn attr(key: &str, value: &str) -> common::KeyValue {
        common::KeyValue { key: key.into(), value: Some(common::AnyValue {
            value: Some(common::any_value::Value::StringValue(value.into())),
        }), ..Default::default() }
    }

    fn raw(fields: &[(u64, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (key, value) in fields {
            put_varint(*key, &mut out);
            if key & 7 == LEN { put_varint(value.len() as u64, &mut out); }
            out.extend_from_slice(value);
        }
        out
    }

    /// A worker's 1 MiB batch of 2-byte empty spans costs the supervisor
    /// about its size to stamp, not the 200 MiB that decoding it did (N-74).
    #[test]
    fn stamping_a_batch_costs_about_its_size() {
        // A scope of empty spans: field 2, length 0, again and again.
        let scope = vec![[2 << 3 | LEN as u8, 0]; (1 << 20) / 2 - 16].concat();
        let batch = raw(&[(1 << 3 | LEN, &raw(&[(2 << 3 | LEN, &scope)]))]);
        assert!(batch.len() <= MAX_BATCH);
        let before = crate::counting::allocated();
        let stamped = stamp("traces", &batch, "app", "digest").unwrap();
        let allocated = crate::counting::allocated() - before;
        assert!(allocated < 4 * MAX_BATCH, "stamping allocated {allocated} bytes");
        let resource = Resource { attributes: opentelemetry_proto::transform::common::tonic::Attributes::from(identity("app", "digest")).0, ..Default::default() };
        let expected = raw(&[(1 << 3 | LEN, &raw(&[(1 << 3 | LEN, &resource.encode_to_vec()), (2 << 3 | LEN, &scope)]))]);
        assert!(stamped == expected, "the spans pass unchanged under the stamped resource");
    }

    /// Stamping at the wire level gives what decoding, replacing the
    /// resource and dropping identity attributes everywhere would: also for
    /// repeated resources, a key given twice (decoders keep the last), keys
    /// by string-table index, events, links, scopes and log records.
    #[test]
    fn stamping_matches_decoding_and_scrubbing() {
        use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
        use opentelemetry_proto::tonic::trace::v1::span::{Event, Link};
        let forged = || vec![attr("nanovm.app", "victim"), attr("service.name", "x"), attr("service.version", "old"),
            common::KeyValue { key_strindex: 7, ..attr("", "victim") }, attr("kept", "yes")];
        let trusted = Resource { attributes: opentelemetry_proto::transform::common::tonic::Attributes::from(identity("app", "d")).0, ..Default::default() };
        let scrub = |a: &mut Vec<common::KeyValue>| a.retain(|kv| kv.key_strindex == 0 && !matches!(kv.key.as_str(), "service.name" | "service.version" | "nanovm.app"));
        // A key given twice: first forged, then harmless, and the other way round.
        let twice = |a: &str, b: &str| raw(&[(1 << 3 | LEN, a.as_bytes()), (1 << 3 | LEN, b.as_bytes())]);
        let span = Span {
            name: "request".into(), attributes: forged(),
            events: vec![Event { name: "e".into(), attributes: forged(), ..Default::default() }],
            links: vec![Link { trace_id: vec![1; 16], attributes: forged(), ..Default::default() }],
            ..Default::default()
        }.encode_to_vec();
        let span = [span, raw(&[(9 << 3 | LEN, &twice("nanovm.app", "fine")), (9 << 3 | LEN, &twice("fine", "nanovm.app"))])].concat();
        let scope = common::InstrumentationScope { name: "nanovm".into(), attributes: forged(), ..Default::default() }.encode_to_vec();
        let scope_spans = raw(&[(1 << 3 | LEN, &scope), (2 << 3 | LEN, &span), (2 << 3 | LEN, &span)]);
        let resource = Resource { attributes: forged(), ..Default::default() }.encode_to_vec();
        let resource_spans = raw(&[(1 << 3 | LEN, &resource), (2 << 3 | LEN, &scope_spans), (1 << 3 | LEN, &resource), (3 << 3 | LEN, b"schema")]);
        let batch = raw(&[(1 << 3 | LEN, &resource_spans), (1 << 3 | LEN, &raw(&[(2 << 3 | LEN, &scope_spans)]))]);
        let mut expected = collector::trace::v1::ExportTraceServiceRequest::decode(&*batch).unwrap();
        for r in &mut expected.resource_spans {
            r.resource = Some(trusted.clone());
            for s in &mut r.scope_spans {
                scrub(&mut s.scope.as_mut().unwrap().attributes);
                for span in &mut s.spans {
                    scrub(&mut span.attributes);
                    span.events.iter_mut().for_each(|e| scrub(&mut e.attributes));
                    span.links.iter_mut().for_each(|l| scrub(&mut l.attributes));
                }
            }
        }
        assert_eq!(expected.resource_spans[0].scope_spans[0].spans[0].attributes.iter().map(|kv| &*kv.key).collect::<Vec<_>>(), ["kept", "fine"]);
        let stamped = stamp("traces", &batch, "app", "d").unwrap();
        assert_eq!(collector::trace::v1::ExportTraceServiceRequest::decode(&*stamped).unwrap(), expected);

        let logs = collector::logs::v1::ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource { attributes: forged(), ..Default::default() }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(common::InstrumentationScope { attributes: forged(), ..Default::default() }),
                    log_records: vec![LogRecord { attributes: forged(), event_name: "e".into(), ..Default::default() }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let mut expected = logs.clone();
        let r = &mut expected.resource_logs[0];
        r.resource = Some(trusted);
        scrub(&mut r.scope_logs[0].scope.as_mut().unwrap().attributes);
        scrub(&mut r.scope_logs[0].log_records[0].attributes);
        let stamped = stamp("logs", &logs.encode_to_vec(), "app", "d").unwrap();
        assert_eq!(collector::logs::v1::ExportLogsServiceRequest::decode(&*stamped).unwrap(), expected);

        let many = raw(&vec![(1 << 3 | LEN, &[][..]); 17]);
        assert!(stamp("traces", &many, "app", "d").is_err(), "more than 16 resources");
        for bad in [&b"not protobuf"[..], &[0x0a, 5, 1], &raw(&[(1 << 3, &[1])]), &raw(&[(1 << 3 | LEN, &raw(&[(2 << 3 | 3, &[])]))])] {
            assert!(stamp("traces", bad, "app", "d").is_err(), "{bad:?}");
        }
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
            for _ in 0..256 { drop(telemetry.span("request", "room", &Context::new(), SpanKind::Server)); }
            tokio::time::timeout(Duration::from_secs(2), arrived.notified()).await.unwrap();
            // Complete requests while an export is in flight. A short burst
            // survives; overload drops at the bound instead of blocking.
            for _ in 0..burst { drop(telemetry.span("request", "room", &Context::new(), SpanKind::Server)); }
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
        let span = telemetry.span("last-request", "room", &Context::new(), SpanKind::Server);
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
