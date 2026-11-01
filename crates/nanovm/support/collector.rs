//! Small OTLP collector for integration tests and the local demo, not a server API.
use std::collections::{BTreeMap, VecDeque};

use opentelemetry_proto::tonic::{collector, common::v1::{KeyValue, any_value}};
use prost::Message;
use serde_json::{Map, Value, json};

#[derive(Default)]
pub struct Collector {
    snapshots: BTreeMap<(String, String), (Map<String, Value>, u64, u128)>,
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
            "/v1/logs" => {
                for resource in collector::logs::v1::ExportLogsServiceRequest::decode(body)?.resource_logs {
                    let a = attrs(&resource.resource.as_ref().ok_or_else(|| anyhow::anyhow!("missing resource"))?.attributes);
                    for log in resource.scope_logs.into_iter().flat_map(|s| s.log_records) {
                        let mut fields = attrs(&log.attributes);
                        if fields.get("event.name").and_then(Value::as_str) == Some("nanovm.snapshot") {
                            let key = (a["nanovm.app"].as_str().unwrap_or_default().to_owned(),
                                fields.get("nanovm.worker.app").and_then(Value::as_str).unwrap_or_default().to_owned());
                            if self.snapshots.get(&key).is_some_and(|(_, timestamp, _)| *timestamp > log.observed_time_unix_nano) { continue }
                            if !self.snapshots.contains_key(&key) && self.snapshots.len() >= 1024 { self.snapshots.pop_first(); }
                            fields.insert("service.version".into(), a["service.version"].clone());
                            let received = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
                            self.snapshots.insert(key, (fields, log.observed_time_unix_nano, received));
                            continue;
                        }
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

    /// Project decoded snapshot events for tests and demo jq expressions.
    /// Never show a retired worker's snapshot as its replacement's state.
    pub fn snapshot(&self) -> Value {
        let mut root = json!({"workers": {}, "apps": {}, "rss_mb": 0});
        let mut received = BTreeMap::new();
        let supervisor_time = self.snapshots.get(&("_supervisor".into(), String::new())).map_or(0, |(_, timestamp, _)| *timestamp);
        for ((app, worker), (fields, timestamp, time)) in &self.snapshots {
            if app == "_supervisor" {
                if worker.is_empty() {
                    root["rss_mb"] = json!(fields["process.memory.usage"].as_f64().unwrap_or(0.0) / 1048576.0);
                    root["uptime_ms"] = json!(fields["process.uptime"].as_f64().unwrap_or(0.0) * 1000.0);
                    received.insert(app.clone(), *time);
                } else {
                    // Each collection starts with the supervisor event. A
                    // worker absent from that collection has been retired.
                    if *timestamp < supervisor_time { continue }
                    let w = &mut root["workers"][worker];
                    for (key, attr) in [("up", "nanovm.worker.up"), ("pid", "process.pid"),
                        ("digest", "nanovm.worker.digest"), ("version", "nanovm.worker.version"),
                        ("policies", "nanovm.worker.policies"), ("starts", "nanovm.worker.starts"),
                        ("link_errors", "nanovm.worker.link_errors")] {
                        w[key] = fields[attr].clone();
                    }
                }
                continue;
            }
            let v = &mut root["apps"][app];
            for (name, value) in fields {
                if let Some(reason) = name.strip_prefix("nanovm.poisons.") { v["poisons"][reason] = value.clone(); }
                else if let Some(phase) = name.strip_prefix("nanovm.objects.phase.") { v["phases"][phase] = value.clone(); }
                else {
                    match name.as_str() {
                        "process.memory.usage" => v["rss_mb"] = json!(value.as_f64().unwrap_or(0.0) / 1048576.0),
                        "nanovm.objects.live" => v["live_objects"] = value.clone(),
                        "nanovm.objects.limit" => v["max_live_objects"] = value.clone(),
                        "nanovm.stores.live" => v["stores_live"] = value.clone(),
                        "nanovm.stores.peak" => v["stores_peak"] = value.clone(),
                        _ => if let Some(key) = name.strip_prefix("nanovm.") { v[key] = value.clone(); },
                    }
                }
            }
            received.insert(app.clone(), *time);
        }
        let workers = root["workers"].clone();
        root["apps"].as_object_mut().unwrap().retain(|app, _| {
            let fields = &self.snapshots[&(app.clone(), String::new())].0;
            workers[app]["up"] == true && workers[app]["digest"] == fields["service.version"]
                && workers[app]["pid"] == fields["process.pid"]
        });
        root["collected_at_ms"] = json!(workers.as_object().unwrap().iter().filter(|(_, w)| w["up"] == true)
            .map(|(app, _)| if root["apps"].get(app).is_some() { *received.get(app).unwrap_or(&0) } else { 0 })
            .chain([*received.get("_supervisor").unwrap_or(&0)]).min().unwrap_or(0));
        root
    }
}
