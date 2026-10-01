//! The cost of the extension's counting allocator (`src/alloc.rs`,
//! included here as it is) over the system allocator it wraps
//! (`mise run bench-alloc`, release build; docs/DESIGN.md, "Memory
//! observability").
//!
//! The binary's global allocator is a switch between the two (one
//! predictable branch, the same for both), so allocations take the path
//! they take in the extension (`std::alloc::alloc`, the `__rust_alloc`
//! shim, the global allocator), and the variants alternate, so they see
//! the same machine state:
//!
//! - `steady`: allocate and free one block of 16 to 79 bytes, over and
//!   over; the counters' steady state (ALLOCATED's two read-modify-writes,
//!   PEAK's and HIGH's loads).
//! - `growth`: allocate 2 million such blocks without freeing (every one a
//!   new high: ALLOCATED, plus a store to HIGH, and to PEAK through
//!   `fetch_max`), then free them and trim (`trim_if_freed`, as at the end
//!   of a transaction, so the next round starts from a new high point).
//!
//! Env: BENCH_ROUNDS (default 7). Prints the best round of each, in
//! nanoseconds per allocation (allocate and free for `steady`, allocate
//! only for `growth`).
//!
//! Pin it to one CPU for stable numbers: `taskset -c 2 mise run bench-alloc`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

#[path = "../../../src/alloc.rs"]
mod alloc;

/// Whether [`Switch`] counts. Changed only while no block is live that the
/// other variant allocated (a block is freed by the variant that
/// allocated it, or the counters would drift).
static COUNTING: AtomicBool = AtomicBool::new(false);

/// [`alloc::Counting`] while [`COUNTING`] is set, `System` otherwise.
struct Switch;

// SAFETY: every method delegates to one of two sound allocators with the
// caller's arguments; a block is always freed or resized by the one that
// allocated it (see COUNTING).
unsafe impl GlobalAlloc for Switch {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        unsafe {
            if COUNTING.load(Ordering::Relaxed) {
                alloc::Counting.alloc(l)
            } else {
                System.alloc(l)
            }
        }
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe {
            if COUNTING.load(Ordering::Relaxed) {
                alloc::Counting.dealloc(p, l)
            } else {
                System.dealloc(p, l)
            }
        }
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        unsafe {
            if COUNTING.load(Ordering::Relaxed) {
                alloc::Counting.realloc(p, l, n)
            } else {
                System.realloc(p, l, n)
            }
        }
    }
}

#[global_allocator]
static GLOBAL: Switch = Switch;

const STEADY: usize = 20_000_000;
const GROWTH: usize = 2_000_000;

fn layout(i: usize) -> Layout {
    Layout::from_size_align(16 + (i & 63), 8).unwrap()
}

/// Nanoseconds per allocate-and-free.
fn steady() -> f64 {
    let t = Instant::now();
    for i in 0..STEADY {
        let l = layout(i);
        // SAFETY: a non-zero size; the block is freed with its layout.
        unsafe {
            let p = std::alloc::alloc(l);
            assert!(!p.is_null());
            std::alloc::dealloc(black_box(p), l);
        }
    }
    t.elapsed().as_nanos() as f64 / STEADY as f64
}

/// Nanoseconds per allocation while the allocation only grows; the blocks
/// are freed and the heap trimmed afterwards, untimed. `blocks` has room
/// for them all (allocated before, so by `System`).
fn growth(blocks: &mut Vec<*mut u8>) -> f64 {
    blocks.clear();
    let t = Instant::now();
    for i in 0..GROWTH {
        // SAFETY: a non-zero size.
        let p = unsafe { std::alloc::alloc(layout(i)) };
        assert!(!p.is_null());
        blocks.push(p);
    }
    let ns = t.elapsed().as_nanos() as f64 / GROWTH as f64;
    for (i, &p) in blocks.iter().enumerate() {
        // SAFETY: allocated above with this layout, freed once.
        unsafe { std::alloc::dealloc(p, layout(i)) };
    }
    alloc::trim_if_freed(0);
    ns
}

/// `f`, run by the counting allocator.
fn counted(f: impl FnOnce() -> f64) -> f64 {
    COUNTING.store(true, Ordering::Relaxed);
    let ns = f();
    COUNTING.store(false, Ordering::Relaxed);
    ns
}

fn main() {
    let rounds: usize = std::env::var("BENCH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7);
    let mut blocks = Vec::with_capacity(GROWTH);
    let (mut s_sys, mut s_cnt, mut g_sys, mut g_cnt) = (f64::MAX, f64::MAX, f64::MAX, f64::MAX);
    for _ in 0..rounds {
        s_sys = s_sys.min(steady());
        s_cnt = s_cnt.min(counted(steady));
        g_sys = g_sys.min(growth(&mut blocks));
        g_cnt = g_cnt.min(counted(|| growth(&mut blocks)));
    }
    assert_eq!(alloc::allocated(), 0, "every counted block was freed");
    println!("best of {rounds} rounds, ns per allocation");
    println!(
        "steady  System {s_sys:6.2}  Counting {s_cnt:6.2}  (+{:.2})",
        s_cnt - s_sys
    );
    println!(
        "growth  System {g_sys:6.2}  Counting {g_cnt:6.2}  (+{:.2})",
        g_cnt - g_sys
    );
}
