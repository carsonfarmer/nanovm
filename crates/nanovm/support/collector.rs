//! Small OTLP collector for integration tests and the local demo, not a server API.
use std::collections::{BTreeMap, VecDeque};

use opentelemetry_proto::tonic::{collector, common::v1::{KeyValue, any_value}, metrics::v1::{ResourceMetrics, metric, number_data_point}};
use prost::Message;
use serde_json::{Map, Value, json};

#[derive(Default)]
pub struct Collector {
    metrics: BTreeMap<String, ResourceMetrics>,
    pub logs: VecDeque<Value>,
}

fn attrs(values: &[KeyValue]) -> Map<String, Value> {
    values.iter().filter_map(|kv| Some((kv.key.clone(), match kv.value.as_ref()?.value.as_ref()? {
        any_value::Value::StringValue(v) => json!(v),
        any_value::Value::IntValue(v) => json!(v),
        any_value::Value::DoubleValue(v) => json!(v),
        any_value::Value::BoolValue(v) => json!(v),
        _ => return None,
    }))).collect()
}

impl Collector {
    pub fn ingest(&mut self, path: &str, body: &[u8]) -> anyhow::Result<()> {
        match path {
            "/v1/traces" => { collector::trace::v1::ExportTraceServiceRequest::decode(body)?; }
            "/v1/metrics" => {
                for resource in collector::metrics::v1::ExportMetricsServiceRequest::decode(body)?.resource_metrics {
                    let a = attrs(&resource.resource.as_ref().ok_or_else(|| anyhow::anyhow!("missing resource"))?.attributes);
                    let app = a["nanovm.app"].as_str().unwrap_or_default().to_owned();
                    if !self.metrics.contains_key(&app) && self.metrics.len() >= 1024 { self.metrics.pop_first(); }
                    self.metrics.insert(app, resource);
                }
            }
            "/v1/logs" => {
                for resource in collector::logs::v1::ExportLogsServiceRequest::decode(body)?.resource_logs {
                    let a = attrs(&resource.resource.as_ref().ok_or_else(|| anyhow::anyhow!("missing resource"))?.attributes);
                    for log in resource.scope_logs.into_iter().flat_map(|s| s.log_records) {
                        let fields = attrs(&log.attributes);
                        let detail = match log.body.and_then(|b| b.value) {
                            Some(any_value::Value::StringValue(s)) => s, _ => String::new(),
                        };
                        if self.logs.len() == 512 { self.logs.pop_front(); }
                        self.logs.push_back(json!({"app": a.get("nanovm.app"), "object": fields.get("nanovm.object"),
                            "kind": fields.get("event.name"), "stream": fields.get("log.iostream"), "detail": detail}));
                    }
                }
            }
            _ => anyhow::bail!("unknown signal"),
        }
        Ok(())
    }

    /// A projection for existing assertions and demo jq expressions. Every
    /// value comes from decoded OTLP; no runtime introspection or stats HTTP.
    pub fn snapshot(&self) -> Value {
        let mut root = json!({"workers": {}, "apps": {}, "rss_mb": 0});
        for (app, resource) in &self.metrics {
            for m in resource.scope_metrics.iter().flat_map(|s| &s.metrics) {
                let points = match &m.data {
                    Some(metric::Data::Gauge(g)) => &g.data_points,
                    Some(metric::Data::Sum(s)) => &s.data_points,
                    Some(metric::Data::Histogram(h)) => {
                        let name = match m.name.as_str() {
                            "nanovm.instantiate.duration" => "instantiate",
                            "nanovm.request.duration" => "latency",
                            _ => continue,
                        };
                        root["apps"][app][name] = json!({"count": h.data_points.iter().map(|p| p.count).sum::<u64>()});
                        continue;
                    }
                    _ => continue,
                };
                for p in points {
                    let value = match p.value {
                        Some(number_data_point::Value::AsInt(n)) => n as f64,
                        Some(number_data_point::Value::AsDouble(n)) => n,
                        None => continue,
                    };
                    let a = attrs(&p.attributes);
                    if app == "_supervisor" {
                        if m.name == "process.memory.usage" { root["rss_mb"] = json!(value / 1048576.0); continue }
                        if m.name == "process.uptime" { root["uptime_ms"] = json!(value * 1000.0); continue }
                        let Some(worker) = a.get("nanovm.worker.app").and_then(Value::as_str) else { continue };
                        let w = &mut root["workers"][worker];
                        match m.name.as_str() {
                            "nanovm.worker.up" => {
                                w["up"] = json!(value != 0.0);
                                for (key, attr) in [("pid", "process.pid"), ("digest", "nanovm.worker.digest"),
                                    ("version", "nanovm.worker.version"), ("policies", "nanovm.worker.policies")] {
                                    w[key] = a.get(attr).cloned().unwrap_or(Value::Null);
                                }
                            }
                            "nanovm.worker.starts" => w["starts"] = json!(value as u64),
                            "nanovm.worker.link_errors" => w["link_errors"] = json!(value as u64),
                            _ => (),
                        }
                        continue;
                    }
                    let v = &mut root["apps"][app];
                    match m.name.as_str() {
                        "nanovm.poisons" => v["poisons"][a["reason"].as_str().unwrap()] = json!(value as u64),
                        "nanovm.objects.phase" => v["phases"][a["phase"].as_str().unwrap()] = json!(value as u64),
                        "process.memory.usage" => v["rss_mb"] = json!(value / 1048576.0),
                        "nanovm.objects.live" => v["live_objects"] = json!(value as u64),
                        "nanovm.objects.limit" => v["max_live_objects"] = json!(value as u64),
                        "nanovm.stores.live" => v["stores_live"] = json!(value as u64),
                        "nanovm.stores.peak" => v["stores_peak"] = json!(value as u64),
                        "nanovm.requests" => v["requests"] = json!(v["requests"].as_u64().unwrap_or(0) + value as u64),
                        name => if let Some(key) = name.strip_prefix("nanovm.") { v[key] = json!(value as u64); },
                    }
                }
            }
        }
        // Retired workers' last resource batches are not live process state.
        let active: Vec<_> = root["workers"].as_object().unwrap().iter()
            .filter(|(_, w)| w["up"] == true).map(|(name, _)| name.clone()).collect();
        root["apps"].as_object_mut().unwrap().retain(|name, _| active.contains(name));
        root
    }
}
