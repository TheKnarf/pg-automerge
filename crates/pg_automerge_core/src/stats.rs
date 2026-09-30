//! Per-process counters of documents, behind `automerge_memory_usage()`
//! (see docs/DESIGN.md, "Memory observability"): how many loaded
//! documents are alive, and how many loads ran and how long they took.
//!
//! The bytes those documents take are counted by the extension's global
//! allocator, not here: this crate has no unsafe code, and an allocator
//! needs some.
//!
//! Every counter is an atomic updated with `Relaxed` ordering: a backend
//! has one thread, and the tests of this crate run on several, where the
//! counts stay exact (only their interleaving is unordered).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Documents alive: [`crate::loaded::LoadedDoc`]s and the documents of
/// [`crate::MergeAccumulator`]s.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// `Automerge::load` calls since the process started or [`reset`].
static LOADS: AtomicU64 = AtomicU64::new(0);
/// Their total time, in nanoseconds.
static LOAD_NANOS: AtomicU64 = AtomicU64::new(0);

/// The counters at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// Documents alive that outlive the call that loaded them:
    /// [`crate::loaded::LoadedDoc`]s (expanded values, `merge` results,
    /// the `bytea` cast's) and `merge_agg` states. A document a read loads
    /// and drops within one call is not included.
    pub live_documents: usize,
    /// `Automerge::load` calls: every load of stored bytes, client input
    /// and the save-and-load checks.
    pub loads: u64,
    /// The time those loads took.
    pub load_time: Duration,
}

/// The current counters.
pub fn snapshot() -> Snapshot {
    Snapshot {
        live_documents: LIVE.load(Ordering::Relaxed),
        loads: LOADS.load(Ordering::Relaxed),
        load_time: Duration::from_nanos(LOAD_NANOS.load(Ordering::Relaxed)),
    }
}

/// Start the cumulative counters (loads, load time) over from zero. The
/// number of live documents is a current value and stays.
pub fn reset() {
    LOADS.store(0, Ordering::Relaxed);
    LOAD_NANOS.store(0, Ordering::Relaxed);
}

/// Run a load `f`, counting it and its time (also when it fails or
/// panics: the time is added by a guard that runs while unwinding).
pub(crate) fn timed_load<T>(f: impl FnOnce() -> T) -> T {
    struct Timer(Instant);
    impl Drop for Timer {
        fn drop(&mut self) {
            let nanos = u64::try_from(self.0.elapsed().as_nanos()).unwrap_or(u64::MAX);
            LOADS.fetch_add(1, Ordering::Relaxed);
            LOAD_NANOS.fetch_add(nanos, Ordering::Relaxed);
        }
    }
    let _timer = Timer(Instant::now());
    f()
}

/// A token held by every live document: counts it while it exists.
#[derive(Debug)]
pub(crate) struct Live(());

impl Live {
    pub(crate) fn new() -> Self {
        LIVE.fetch_add(1, Ordering::Relaxed);
        Live(())
    }
}

impl Clone for Live {
    fn clone(&self) -> Self {
        Live::new()
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The counters are per process, and the crate's unit tests run in
    // parallel threads whose documents come and go, so only the monotonic
    // counters are checked here, as lower bounds (tests/stats.rs checks
    // the live count in a test binary of its own).
    #[test]
    fn counts_timed_loads() {
        let before = snapshot();
        let n = timed_load(|| {
            std::thread::sleep(Duration::from_millis(2));
            7
        });
        assert_eq!(n, 7);
        let after = snapshot();
        assert!(after.loads > before.loads);
        assert!(after.load_time >= before.load_time + Duration::from_millis(2));
        // A panicking load is counted too.
        let loads = snapshot().loads;
        let _ = std::panic::catch_unwind(|| timed_load(|| panic!("load failed")));
        assert!(snapshot().loads > loads);
    }
}
