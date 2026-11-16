//! Node-wide admission/benchmark counters and process RSS; events go to OTel.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

/// CPU time of the calling thread in nanoseconds, user and kernel: not the
/// time it waited for a core or the disk. A system call (about 0.2 µs),
/// unlike [`now_ns`].
pub fn thread_cpu_ns() -> u64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // Can't fail for this clock, on Linux or macOS.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) };
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
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

/// Infrastructure state used by admission, benchmarks and snapshot events.
#[derive(Default)]
pub struct Metrics {
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
    /// CPU time spent polling objects: guest code and the host work it does
    /// inline, commits included.
    pub guest_cpu_ns: AtomicU64,
    pub(crate) telemetry: OnceLock<std::sync::Weak<crate::telemetry::Telemetry>>,
}

impl Metrics {
    pub fn poison(&self, p: Poison) {
        self.poisons[p as usize].fetch_add(1, Relaxed);
    }

    pub fn event(&self, kind: &'static str, object: &str, detail: impl AsRef<[u8]>, context: &opentelemetry::Context) {
        if let Some(telemetry) = self.telemetry.get().and_then(std::sync::Weak::upgrade) {
            telemetry.log(object, ("event.name", kind), detail.as_ref(), context);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poison_counts() {
        let m = Metrics::default();
        m.poison(Poison::ResourceTable);
        m.poison(Poison::Cpu);
        m.poison(Poison::Cpu);
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
