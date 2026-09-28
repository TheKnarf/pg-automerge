//! Test instrumentation (only with the `test-hooks` feature, which the
//! extension's pg_tests and the core's own tests enable): observe and force
//! the save-and-load check that keeps unloadable values out of storage
//! (see [`crate::normalize`] and [`crate::loaded::LoadedDoc::stored`]).
//!
//! Per thread: core tests run in parallel threads, a backend has one.

use std::cell::Cell;

thread_local! {
    static RELOAD_CHECKS: Cell<usize> = const { Cell::new(0) };
    static FAIL_RELOAD_CHECK: Cell<bool> = const { Cell::new(false) };
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
