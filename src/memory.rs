//! Memory observability: `automerge_memory_usage()` and
//! `automerge_memory_reset()`, reading the counting global allocator
//! ([`crate::alloc`]) and the core's document counters
//! ([`pg_automerge_core::stats`]). See
//! docs/src/pages/design/memory-observability.mdx.

use pg_automerge_core::stats;
use pgrx::prelude::*;

use crate::alloc;

/// Bytes as `bigint` (saturating; a backend cannot hold 2^63 bytes).
fn to_i64(n: impl TryInto<i64>) -> i64 {
    n.try_into().unwrap_or(i64::MAX)
}

/// The Rust heap of this backend's pg_automerge (which Postgres' memory
/// contexts do not show) and its documents, as one row.
///
/// The counters are read before anything of the call is allocated. Only
/// this backend's are visible: each backend has its own heap, and nothing
/// is shared (see docs/src/pages/operations/monitoring.mdx for
/// sampling them from every connection).
#[pg_extern(volatile, parallel_restricted)]
fn automerge_memory_usage() -> TableIterator<
    'static,
    (
        name!(allocated_bytes, i64),
        name!(peak_allocated_bytes, i64),
        name!(live_documents, i64),
        name!(loads, i64),
        name!(load_time, f64),
    ),
> {
    let allocated = alloc::allocated();
    let peak = alloc::peak();
    let docs = stats::snapshot();
    TableIterator::once((
        to_i64(allocated),
        to_i64(peak),
        to_i64(docs.live_documents),
        to_i64(docs.loads),
        docs.load_time.as_secs_f64() * 1000.0,
    ))
}

/// Start `peak_allocated_bytes` over from the current allocation, and
/// `loads` and `load_time` from zero, in this backend.
#[pg_extern(volatile, parallel_restricted)]
fn automerge_memory_reset() {
    alloc::reset_peak();
    stats::reset();
}

extension_sql!(
    r#"
COMMENT ON FUNCTION automerge_memory_usage() IS
    'Memory of pg_automerge in this backend, outside Postgres memory contexts: bytes its Rust code holds now and at most (since the backend started or automerge_memory_reset()), loaded documents alive, Automerge loads and their total time in milliseconds.';
COMMENT ON FUNCTION automerge_memory_reset() IS
    'Start the peak of automerge_memory_usage() over from the current allocation, and its load count and load time from zero, in this backend.';
"#,
    name = "automerge_memory_comments",
    requires = [automerge_memory_usage, automerge_memory_reset],
);
