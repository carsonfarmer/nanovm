//! Node-wide admission/benchmark counters and process RSS; events go to OTel.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

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
}

impl Histogram {
    pub fn record(&self, d: Duration) {
        let us = d.as_micros() as u64;
        self.buckets[((64 - us.leading_zeros()) as usize).min(31)].fetch_add(1, Relaxed);
        self.count.fetch_add(1, Relaxed);
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

/// Node-wide counters used by admission, benchmarks and OTel observers.
#[derive(Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub status: [AtomicU64; 6],
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
    pub(crate) telemetry: OnceLock<std::sync::Weak<crate::telemetry::Telemetry>>,
}

impl Metrics {
    pub fn status(&self, code: u16) {
        self.status[(code / 100).min(5) as usize].fetch_add(1, Relaxed);
    }

    pub fn poison(&self, p: Poison) {
        self.poisons[p as usize].fetch_add(1, Relaxed);
    }

    pub fn event(&self, kind: &'static str, object: &str, detail: impl Into<String>) {
        let detail = detail.into();
        if let Some(telemetry) = self.telemetry.get().and_then(std::sync::Weak::upgrade) {
            telemetry.event(object, kind, &detail);
        }
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

    /// Statuses count by class, out-of-range ones in the nearest.
    #[test]
    fn status_and_poison_counts() {
        let m = Metrics::default();
        for code in [101, 200, 204, 404, 500, 503, 999] {
            m.status(code);
        }
        m.poison(Poison::ResourceTable);
        m.poison(Poison::Cpu);
        m.poison(Poison::Cpu);
        assert_eq!(m.status.each_ref().map(|v| v.load(Relaxed)), [0, 1, 2, 0, 1, 3]);
        assert_eq!(m.poisons.each_ref().map(|v| v.load(Relaxed)), [2, 0, 0, 0, 1]);
        for (i, p) in Poison::ALL.iter().enumerate() {
            assert_eq!(*p as usize, i);
        }
    }

    /// The process's resident size is read, and is plausible.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn rss_is_measured() {
        assert!(rss_bytes() > 1 << 20);
    }
}
