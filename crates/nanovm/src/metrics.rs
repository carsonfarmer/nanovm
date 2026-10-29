//! Node-wide counters, latency histograms, an event log and process RSS.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Serialize, Serializer};

/// Monotonic nanoseconds since process start.
pub fn now_ns() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Log2-bucketed histogram of microseconds (32 buckets reach 71 minutes).
#[derive(Default)]
pub struct Histogram {
    buckets: [AtomicU64; 32],
    count: AtomicU64,
    sum_us: AtomicU64,
}

impl Histogram {
    pub fn record(&self, d: Duration) {
        let us = d.as_micros() as u64;
        self.buckets[((64 - us.leading_zeros()) as usize).min(31)].fetch_add(1, Relaxed);
        self.count.fetch_add(1, Relaxed);
        self.sum_us.fetch_add(us, Relaxed);
    }

    /// Upper bound (µs) of the bucket holding quantile `q`.
    pub fn quantile(&self, q: f64) -> u64 {
        let target = ((self.count.load(Relaxed) as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, b) in self.buckets.iter().enumerate() {
            seen += b.load(Relaxed);
            if seen >= target {
                return 1u64 << i;
            }
        }
        0
    }
}

impl Serialize for Histogram {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let count = self.count.load(Relaxed);
        let mean_us = self.sum_us.load(Relaxed).checked_div(count).unwrap_or(0);
        let [p50_us, p99_us, p999_us] = [0.5, 0.99, 0.999].map(|q| self.quantile(q));
        serde_json::json!({ "count": count, "mean_us": mean_us, "p50_us": p50_us, "p99_us": p99_us, "p999_us": p999_us })
            .serialize(s)
    }
}

/// Why an object's instance was torn down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poison {
    Cpu,
    Memory,
    Timeout,
    Trap,
    ResourceTable,
}

impl Poison {
    pub const ALL: [Poison; 5] = [
        Poison::Cpu,
        Poison::Memory,
        Poison::Timeout,
        Poison::Trap,
        Poison::ResourceTable,
    ];

    pub fn as_str(self) -> &'static str {
        ["cpu", "memory", "timeout", "trap", "resource-table"][self as usize]
    }
}

#[derive(Serialize, Clone)]
pub struct Event {
    pub seq: u64,
    pub t_ms: u64,
    pub kind: &'static str,
    pub app: String,
    pub object: String,
    pub detail: String,
}

/// Node-wide counters; serialized as-is into `/_nanovm/stats`.
#[derive(Default, Serialize)]
pub struct Metrics {
    pub requests: AtomicU64,
    #[serde(serialize_with = "by_class")]
    pub status: [AtomicU64; 6],
    pub instantiate_failures: AtomicU64,
    pub stores_live: AtomicI64,
    pub stores_peak: AtomicI64,
    #[serde(serialize_with = "by_poison")]
    pub poisons: [AtomicU64; 5],
    pub evicted_idle: AtomicU64,
    pub evicted_lru: AtomicU64,
    pub rejected_busy: AtomicU64,
    pub rejected_capacity: AtomicU64,
    pub egress_allowed: AtomicU64,
    pub egress_denied: AtomicU64,
    pub fs_denied: AtomicU64,
    #[serde(rename = "guest_cpu_ms", serialize_with = "as_ms")]
    pub guest_cpu_ns: AtomicU64,
    pub latency: Histogram,
    pub instantiate: Histogram,
    #[serde(skip)]
    events: Mutex<EventLog>,
}

/// Status counts keyed `1xx`..`5xx`.
fn by_class<S: Serializer>(v: &[AtomicU64; 6], s: S) -> Result<S::Ok, S::Error> {
    s.collect_map((1..6).map(|i| (format!("{i}xx"), v[i].load(Relaxed))))
}

fn by_poison<S: Serializer>(v: &[AtomicU64; 5], s: S) -> Result<S::Ok, S::Error> {
    s.collect_map(Poison::ALL.iter().map(|p| (p.as_str(), v[*p as usize].load(Relaxed))))
}

fn as_ms<S: Serializer>(v: &AtomicU64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64(v.load(Relaxed) / 1_000_000)
}

#[derive(Default)]
struct EventLog {
    seq: u64,
    ring: VecDeque<Event>,
}

const EVENT_RING: usize = 512;

impl Metrics {
    pub fn status(&self, code: u16) {
        self.status[(code / 100).min(5) as usize].fetch_add(1, Relaxed);
    }

    pub fn poison(&self, p: Poison) {
        self.poisons[p as usize].fetch_add(1, Relaxed);
    }

    pub fn event(&self, kind: &'static str, app: &str, object: &str, detail: impl Into<String>) {
        let mut log = self.events.lock();
        log.seq += 1;
        let ev = Event {
            seq: log.seq,
            t_ms: now_ns() / 1_000_000,
            kind,
            app: app.to_string(),
            object: object.to_string(),
            detail: detail.into(),
        };
        if log.ring.len() == EVENT_RING {
            log.ring.pop_front();
        }
        log.ring.push_back(ev);
    }

    pub fn events_since(&self, seq: u64) -> Vec<Event> {
        let log = self.events.lock();
        log.ring.iter().filter(|e| e.seq > seq).cloned().collect()
    }
}

/// Resident set size of this process in bytes.
pub fn rss_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    unsafe {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
        let n = libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        );
        if n == size { info.pti_resident_size } else { 0 }
    }
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
        let pages: u64 = statm
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us(n: u64) -> Duration {
        Duration::from_micros(n)
    }

    /// A quantile is the exclusive upper bound of the power-of-two bucket
    /// that holds it; an empty histogram reports 0 and the last bucket
    /// takes everything beyond it.
    #[test]
    fn histogram_quantiles_are_bucket_bounds() {
        let h = Histogram::default();
        assert_eq!(h.quantile(0.5), 0);
        for _ in 0..99 {
            h.record(us(3));
        }
        h.record(us(1000));
        assert_eq!(
            [h.quantile(0.0), h.quantile(0.5), h.quantile(0.99), h.quantile(0.999)],
            [4, 4, 4, 1024]
        );
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"count": 100, "mean_us": 12, "p50_us": 4, "p99_us": 4, "p999_us": 1024})
        );

        let edges = Histogram::default();
        for n in [0, 1, 1024] {
            edges.record(us(n));
        }
        assert_eq!(
            [edges.quantile(0.3), edges.quantile(0.6), edges.quantile(1.0)],
            [1, 2, 2048]
        );
        let top = Histogram::default();
        top.record(Duration::from_secs(1 << 40));
        assert_eq!(top.quantile(1.0), 1 << 31);
    }

    /// Statuses count by class, out-of-range ones in the nearest; poisons
    /// by name; guest CPU is reported in whole milliseconds; the event log
    /// stays out of the stats.
    #[test]
    fn stats_serialize_by_class_and_name() {
        let m = Metrics::default();
        for code in [101, 200, 204, 404, 500, 503, 999] {
            m.status(code);
        }
        m.poison(Poison::ResourceTable);
        m.poison(Poison::Cpu);
        m.poison(Poison::Cpu);
        m.guest_cpu_ns.store(2_999_999, Relaxed);
        m.event("x", "a", "o", "d");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(
            v["status"],
            serde_json::json!({"1xx": 1, "2xx": 2, "3xx": 0, "4xx": 1, "5xx": 3})
        );
        let poisons = serde_json::json!({"cpu": 2, "memory": 0, "timeout": 0, "trap": 0, "resource-table": 1});
        assert_eq!(v["poisons"], poisons);
        assert_eq!(v["guest_cpu_ms"], 2);
        assert!(v.get("events").is_none() && v.get("guest_cpu_ns").is_none());
        for (i, p) in Poison::ALL.iter().enumerate() {
            assert_eq!(*p as usize, i);
        }
    }

    /// Events are numbered from 1, read back after a sequence number, and
    /// the log keeps only the newest `EVENT_RING`.
    #[test]
    fn event_log_is_a_numbered_ring() {
        let m = Metrics::default();
        m.event("kill", "app", "obj", "why");
        let e = &m.events_since(0)[..];
        assert_eq!(
            (e.len(), e[0].seq, e[0].kind, &*e[0].app, &*e[0].object, &*e[0].detail),
            (1, 1, "kill", "app", "obj", "why")
        );
        assert!(m.events_since(1).is_empty());
        for _ in 0..EVENT_RING + 10 {
            m.event("x", "a", "o", "");
        }
        let all = m.events_since(0);
        assert_eq!(all.len(), EVENT_RING);
        assert_eq!((all[0].seq, all.last().unwrap().seq), (12, EVENT_RING as u64 + 11));
        assert_eq!(m.events_since(EVENT_RING as u64 + 9).len(), 2);
    }

    /// The process's resident size is read, and is plausible.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn rss_is_measured() {
        assert!(rss_bytes() > 1 << 20);
    }
}
