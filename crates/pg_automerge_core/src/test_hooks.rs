//! Test instrumentation (only with the `test-hooks` feature, which the
//! extension's pg_tests and the core's own tests enable): observe and force
//! the save-and-load check that keeps unloadable values out of storage
//! (see [`crate::normalize`] and [`crate::loaded::LoadedDoc::stored`]),
//! switch it off, count document loads and re-inflations of input, and
//! override the load memory limit ([`crate::budget::limit`]).
//!
//! Per thread: core tests run in parallel threads, a backend has one.

use std::cell::Cell;

thread_local! {
    static RELOAD_CHECKS: Cell<usize> = const { Cell::new(0) };
    static FAIL_RELOAD_CHECK: Cell<bool> = const { Cell::new(false) };
    static VERIFICATION: Cell<Option<bool>> = const { Cell::new(None) };
    static LOADS: Cell<usize> = const { Cell::new(0) };
    static REINFLATIONS: Cell<usize> = const { Cell::new(0) };
    static LIMIT: Cell<Option<Option<u64>>> = const { Cell::new(None) };
}

/// Override [`crate::budget::limit`] on this thread: `Some(Some(bytes))`
/// a limit, `Some(None)` no limit, `None` the registered source again.
pub fn set_limit(limit: Option<Option<u64>>) {
    LIMIT.with(|l| l.set(limit));
}

pub(crate) fn limit_override() -> Option<Option<u64>> {
    LIMIT.with(Cell::get)
}

/// How many times this thread has run `Automerge::load` (every load the
/// crate does: stored values, external input, save-and-load checks).
pub fn loads() -> usize {
    LOADS.with(Cell::get)
}

pub(crate) fn count_load() {
    LOADS.with(|n| n.set(n.get() + 1));
}

/// How many times this thread has inflated input again to compare it
/// with its normalized save (`loads_as_saved` without the scan's inflated
/// columns).
pub fn reinflations() -> usize {
    REINFLATIONS.with(Cell::get)
}

pub(crate) fn count_reinflation() {
    REINFLATIONS.with(|n| n.set(n.get() + 1));
}

/// Override [`crate::verification_enabled`] on this thread (`None`: use
/// the registered switch again).
pub fn set_verification(on: Option<bool>) {
    VERIFICATION.with(|v| v.set(on));
}

pub(crate) fn verification_override() -> Option<bool> {
    VERIFICATION.with(Cell::get)
}

/// How many save-and-load checks this thread has run (including forced
/// failures).
pub fn reload_checks() -> usize {
    RELOAD_CHECKS.with(Cell::get)
}

/// Make every following save-and-load check on this thread fail (`true`)
/// as if the save did not load back, or behave normally again (`false`).
pub fn set_fail_reload_check(fail: bool) {
    FAIL_RELOAD_CHECK.with(|f| f.set(fail));
}

/// Called at the start of every check: counts it, and whether to fail it.
pub(crate) fn reload_check_hook() -> bool {
    RELOAD_CHECKS.with(|n| n.set(n.get() + 1));
    FAIL_RELOAD_CHECK.with(Cell::get)
}

/// `raw` deflated as Automerge deflates columns and chunks (for tests
/// that build compressed input, e.g. a deflate bomb).
pub fn deflate(raw: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(raw).expect("writing to a Vec");
    encoder.finish().expect("writing to a Vec")
}
