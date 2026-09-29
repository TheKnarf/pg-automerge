//! A counting global allocator for the memory tests: live and peak bytes
//! per thread (the test harness runs tests on parallel threads; Automerge
//! allocates on the calling thread only).
//!
//! Include with `#[path = "common/counting.rs"] mod counting;` in a test
//! binary that wants it; it installs itself as that binary's global
//! allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

pub struct Counting;

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn grew(n: usize) {
    let _ = LIVE.try_with(|live| {
        let now = live.get() + n as isize;
        live.set(now);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
    });
}

fn shrank(n: usize) {
    let _ = LIVE.try_with(|live| live.set(live.get() - n as isize));
}

// SAFETY: delegates to the system allocator; the counters are plain
// thread-local cells without destructors, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        shrank(layout.size());
        unsafe { System.dealloc(p, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, new) };
        if !q.is_null() {
            shrank(layout.size());
            grew(new);
        }
        q
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Run `f`; its result and the peak number of bytes this thread had
/// allocated during it, above what it held when `f` started.
pub fn peak_of<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let start = LIVE.with(Cell::get);
    PEAK.with(|p| p.set(start));
    let result = f();
    let peak = PEAK.with(Cell::get) - start;
    (result, peak.max(0) as u64)
}
