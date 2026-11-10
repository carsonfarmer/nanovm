//! nanovm: a dense host for untrusted wasip3 HTTP components, run as durable
//! objects (one live instance per object id, one request at a time) with
//! per-object memory, CPU, disk and egress limits.

pub mod app;
mod committed;
pub mod config;
pub mod egress;
pub mod engine;
pub mod jail;
pub mod journal;
pub mod link;
pub mod metrics;
pub mod object;
pub mod proxy;
pub mod quota;
pub mod registry;
pub mod server;
pub mod supervisor;
pub mod table;
pub mod telemetry;
pub mod ws;

/// Per-thread allocation counts, for tests that bound what parsing costs.
#[cfg(test)]
mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        /// Bytes allocated in all, bytes held now, and the most held.
        static C: Cell<(usize, usize, usize)> = const { Cell::new((0, 0, 0)) };
    }

    struct Counting;

    // `realloc` keeps its default: an `alloc` and a `dealloc`, both counted.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let _ = C.try_with(|c| {
                let (all, held, peak) = c.get();
                c.set((all + l.size(), held + l.size(), peak.max(held + l.size())))
            });
            unsafe { System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            let _ = C.try_with(|c| {
                let (all, held, peak) = c.get();
                c.set((all, held.saturating_sub(l.size()), peak))
            });
            unsafe { System.dealloc(p, l) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    /// Bytes this thread has allocated so far.
    pub fn allocated() -> usize {
        C.with(|c| c.get().0)
    }

    /// What `f` left held on this thread (allocated and not freed).
    pub fn held(f: impl FnOnce()) -> usize {
        let before = C.with(|c| c.get().1);
        f();
        C.with(|c| c.get().1).saturating_sub(before)
    }

    /// The most `f` held at once on this thread.
    pub fn peak(f: impl FnOnce()) -> usize {
        let held = C.with(|c| {
            let (all, held, _) = c.get();
            c.set((all, held, held));
            held
        });
        f();
        C.with(|c| c.get().2) - held
    }
}
