//! The library's global allocator: the system allocator (`malloc`), plus
//! counters of the bytes this library's Rust code holds, for
//! `automerge_memory_usage()` (see docs/DESIGN.md, "Memory
//! observability").
//!
//! Everything Rust allocates in a backend goes through it: Automerge's
//! documents, the core's buffers, pgrx's own Rust allocations. Postgres'
//! memory (`palloc`) does not; that is what `pg_backend_memory_contexts`
//! shows. A `#[global_allocator]` covers only the Rust code linked into
//! this library, so other Rust extensions in the same backend are not
//! counted.
//!
//! Plain Rust with no pgrx dependency, so a benchmark can install the same
//! allocator (the overhead measurements in docs/DESIGN.md).
//!
//! Counts are the sizes Rust asks for (`Layout::size`), not what `malloc`
//! uses for them (its chunk headers and rounding, and freed memory it
//! keeps for reuse, are not included). A third counter, the high point
//! since the last trim, decides when [`trim_if_freed`] hands that freed
//! memory back to the system (docs/DESIGN.md, "Returning freed memory").

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Bytes currently allocated.
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
/// The most [`ALLOCATED`] has been since the process started or the last
/// [`reset_peak`].
static PEAK: AtomicUsize = AtomicUsize::new(0);
/// The most [`ALLOCATED`] has been since the last trim ([`trim_if_freed`]),
/// internal: unlike [`PEAK`], no SQL function resets it.
static HIGH: AtomicUsize = AtomicUsize::new(0);

/// The counting allocator (install with `#[global_allocator]`).
pub struct Counting;

#[inline]
fn grew(n: usize) {
    // Relaxed: the counters order nothing else. A backend has one thread;
    // under several (the benchmarks) every update is still an atomic
    // read-modify-write, so none is lost.
    let now = ALLOCATED.fetch_add(n, Ordering::Relaxed).wrapping_add(n);
    if now > PEAK.load(Ordering::Relaxed) {
        PEAK.fetch_max(now, Ordering::Relaxed);
    }
    // A plain store: only the trim heuristic reads it, and a backend has
    // one thread (under several, a lost update delays a trim at worst).
    if now > HIGH.load(Ordering::Relaxed) {
        HIGH.store(now, Ordering::Relaxed);
    }
}

#[inline]
fn shrank(n: usize) {
    ALLOCATED.fetch_sub(n, Ordering::Relaxed);
}

// SAFETY: every method delegates to the system allocator with the caller's
// arguments and returns its result unchanged; the counters are atomics in
// statics, which neither allocate nor panic, so the allocator never
// re-enters itself or unwinds.
unsafe impl GlobalAlloc for Counting {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) };
        shrank(layout.size());
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        // On failure the old block is untouched and still counted.
        if !p.is_null() {
            let old = layout.size();
            if new_size >= old {
                grew(new_size - old);
            } else {
                shrank(old - new_size);
            }
        }
        p
    }
}

/// Bytes the library's Rust code holds right now.
#[allow(dead_code)] // unused by the benchmark that includes this file
pub fn allocated() -> usize {
    ALLOCATED.load(Ordering::Relaxed)
}

/// The most bytes it has held since the process started (or the last
/// [`reset_peak`]).
#[allow(dead_code)]
pub fn peak() -> usize {
    PEAK.load(Ordering::Relaxed).max(allocated())
}

/// Start the peak over from the current allocation.
#[allow(dead_code)]
pub fn reset_peak() {
    PEAK.store(allocated(), Ordering::Relaxed);
}

/// Return the memory `malloc` keeps for reuse to the operating system
/// (`malloc_trim(0)`) when the allocation has fallen by at least
/// `threshold` bytes from its high point since the last trim, and start
/// the high point over from the current allocation. Returns whether it
/// trimmed. A no-op (returning false) where the C library has no
/// `malloc_trim` (anything but glibc).
///
/// Call it between operations (the library calls it at the end of each
/// transaction), never from within an allocator call.
#[allow(dead_code)]
pub fn trim_if_freed(threshold: usize) -> bool {
    let now = allocated();
    let high = HIGH.load(Ordering::Relaxed).max(now);
    if high - now < threshold {
        return false;
    }
    HIGH.store(now, Ordering::Relaxed);
    trim()
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim() -> bool {
    unsafe extern "C" {
        /// glibc: release free memory from the top of the heap and, since
        /// glibc 2.8, the free pages inside it (`madvise(MADV_DONTNEED)`)
        /// to the system. Returns 1 if it released any.
        fn malloc_trim(pad: usize) -> core::ffi::c_int;
    }
    // SAFETY: malloc_trim takes no pointer and only works on malloc's own
    // state under its own lock; it is called between allocator calls,
    // never inside one, so it cannot re-enter malloc.
    unsafe { malloc_trim(0) };
    true
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim() -> bool {
    false
}
