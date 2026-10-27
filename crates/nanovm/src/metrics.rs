//! Node-wide counters, latency histograms, an event log and process RSS.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;

/// Monotonic nanoseconds since process start.
pub fn now_ns() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Log2-bucketed histogram of microseconds.
pub struct Histogram {
    buckets: [AtomicU64; 40],
    count: AtomicU64,
    sum_us: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    pub fn record(&self, d: Duration) {
        let us = d.as_micros() as u64;
        let b = (64 - us.leading_zeros()) as usize;
        self.buckets[b.min(39)].fetch_add(1, Relaxed);
        self.count.fetch_add(1, Relaxed);
        self.sum_us.fetch_add(us, Relaxed);
    }

    /// Upper bound (µs) of the bucket holding quantile `q`.
    pub fn quantile(&self, q: f64) -> u64 {
        let total = self.count.load(Relaxed);
        if total == 0 {
            return 0;
        }
        let target = ((total as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, b) in self.buckets.iter().enumerate() {
            seen += b.load(Relaxed);
            if seen >= target {
                return if i == 0 { 1 } else { 1u64 << i };
            }
        }
        u64::MAX
    }

    pub fn snapshot(&self) -> HistSnapshot {
        let count = self.count.load(Relaxed);
        HistSnapshot {
            count,
            mean_us: self.sum_us.load(Relaxed).checked_div(count).unwrap_or(0),
            p50_us: self.quantile(0.50),
            p99_us: self.quantile(0.99),
            p999_us: self.quantile(0.999),
        }
    }
}

#[derive(Serialize, Clone, Copy)]
pub struct HistSnapshot {
    pub count: u64,
    pub mean_us: u64,
    pub p50_us: u64,
    pub p99_us: u64,
    pub p999_us: u64,
}

/// Why an object's instance was torn down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
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
        match self {
            Poison::Cpu => "cpu",
            Poison::Memory => "memory",
            Poison::Timeout => "timeout",
            Poison::Trap => "trap",
            Poison::ResourceTable => "resource-table",
        }
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

#[derive(Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub status: [AtomicU64; 6],
    pub instantiations: AtomicU64,
    pub instantiate_failures: AtomicU64,
    pub stores_live: AtomicI64,
    pub stores_peak: AtomicI64,
    pub poisons: [AtomicU64; 5],
    pub evicted_idle: AtomicU64,
    pub evicted_lru: AtomicU64,
    pub rejected_busy: AtomicU64,
    pub rejected_capacity: AtomicU64,
    pub egress_allowed: AtomicU64,
    pub egress_denied: AtomicU64,
    pub fs_denied: AtomicU64,
    pub guest_cpu_ns: AtomicU64,
    pub latency: Histogram,
    pub instantiate: Histogram,
    events: Mutex<EventLog>,
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
