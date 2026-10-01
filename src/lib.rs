//! pg_automerge: Automerge CRDT documents as a native Postgres type.
//!
//! This crate is the pgrx glue; all Automerge logic lives in
//! `pg_automerge_core`. See docs/src/pages/reference/ for the SQL surface and
//! semantics.
//!
//! - `datum`: the Rust types of `automerge` arguments and results.
//! - `expanded`: expanded (in-memory) `automerge` values.
//! - `error`: raising Postgres errors.
//! - `jsonb`: building jsonb directly from the core's JSON walk.
//! - `io`: the type's I/O functions, its SQL, and the casts.
//! - `merge`: `merge`, `||` and `merge_agg`.
//! - `introspect`: `automerge_heads` and `automerge_contains`.
//! - `history`: the change types and history functions.
//! - `notify`: the `automerge_notify()` trigger.
//! - `spans`: `automerge_spans`, the structure of a text object.
//! - `alloc`: the counting global allocator, and returning freed memory
//!   to the system at transaction end (`pg_automerge.trim_threshold`).
//! - `memory`: `automerge_memory_usage()` and `automerge_memory_reset()`.

// A cdylib: its docs are for developers (built with
// --document-private-items), so links to private items are fine.
#![allow(rustdoc::private_intra_doc_links)]

mod alloc;
pub mod datum;
mod error;
pub mod expanded;
mod history;
mod introspect;
mod io;
mod jsonb;
mod memory;
mod merge;
mod notify;
mod spans;

::pgrx::pg_module_magic!(name, version);

/// Every Rust allocation of the library goes through the system allocator
/// and is counted, for `automerge_memory_usage()`
/// (docs/src/pages/design/memory-observability.mdx).
#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

/// `pg_automerge.verify_writes`: whether values built from client input
/// are loaded back once before they are stored or sent (see
/// docs/src/pages/reference/automerge-type.mdx, "Invariants", and
/// docs/src/pages/design/expanded-values.mdx, "The deferred verification").
static VERIFY_WRITES: pgrx::GucSetting<bool> = pgrx::GucSetting::<bool>::new(true);

/// The default of `pg_automerge.max_load_memory`, in kB: 2 GB.
const MAX_LOAD_MEMORY_DEFAULT_KB: i32 = 2 * 1024 * 1024;

/// `pg_automerge.max_load_memory`: the largest estimated memory, in kB, a
/// load of client input (or a merge result) may take; -1 for no limit
/// (see docs/src/pages/design/resource-limits.mdx).
static MAX_LOAD_MEMORY: pgrx::GucSetting<i32> =
    pgrx::GucSetting::<i32>::new(MAX_LOAD_MEMORY_DEFAULT_KB);

/// The default of `pg_automerge.trim_threshold`, in kB: 64 MB.
const TRIM_THRESHOLD_DEFAULT_KB: i32 = 64 * 1024;

/// `pg_automerge.trim_threshold`: at the end of a transaction, return the
/// memory `malloc` keeps for reuse to the operating system when the
/// library's Rust allocation fell by at least this many kB from its high
/// point since the last time; -1: never (see
/// docs/src/pages/design/memory-observability.mdx, "Returning freed memory").
static TRIM_THRESHOLD: pgrx::GucSetting<i32> =
    pgrx::GucSetting::<i32>::new(TRIM_THRESHOLD_DEFAULT_KB);

/// Library initialization: let the core's long loops (the jsonb walk,
/// history rows, the input scan) honour query cancel and
/// `statement_timeout`, define the `pg_automerge.verify_writes`,
/// `pg_automerge.max_load_memory` and `pg_automerge.trim_threshold`
/// settings, register the transaction callback that trims, and reserve the
/// `pg_automerge` prefix.
#[pgrx::pg_guard]
pub extern "C-unwind" fn _PG_init() {
    pg_automerge_core::set_interrupt_check(check_for_interrupts);
    pgrx::GucRegistry::define_bool_guc(
        c"pg_automerge.verify_writes",
        c"Load back the normalized save of values built from client input before storing them.",
        c"On (the default), a value built from client bytes (input, the bytea cast, merge with a \
          bytea) is loaded back once before it is stored or sent, unless it provably is the input's \
          own encoding, so malformed input that loads but whose save does not is rejected (22P02) \
          instead of stored. Off skips that load, which halves the cost of such writes, at the \
          risk of storing a value that fails when it is next read. Superuser-only.",
        &VERIFY_WRITES,
        pgrx::GucContext::Suset,
        pgrx::GucFlags::default(),
    );
    pg_automerge_core::set_verification_check(verify_writes);
    pgrx::GucRegistry::define_int_guc(
        c"pg_automerge.max_load_memory",
        c"Largest estimated memory a load of client input or a merge result may take.",
        c"Automerge input is compressed and run-length encoded, so a few bytes can describe a \
          document that takes gigabytes to load, and a failed allocation restarts the server. \
          Every value built from client bytes (input, the bytea cast, merge with a bytea) and \
          every merge result is priced from its headers before Automerge allocates, and \
          rejected (53400) when the estimate exceeds this. Loading stored values is never \
          limited. -1 means no limit. Superuser-only.",
        &MAX_LOAD_MEMORY,
        -1,
        i32::MAX,
        pgrx::GucContext::Suset,
        pgrx::GucFlags::UNIT_KB,
    );
    pg_automerge_core::budget::set_limit_source(max_load_memory);
    pgrx::GucRegistry::define_int_guc(
        c"pg_automerge.trim_threshold",
        c"Return freed memory to the operating system at transaction end once this much was freed.",
        c"Loaded documents live in the backend's malloc heap, which keeps freed memory for reuse, \
          so after one large document a backend would hold that much for the rest of its life. \
          At the end of a transaction in which pg_automerge's allocation fell by at least this \
          much from its high point, malloc_trim(0) returns the free memory to the operating \
          system (0: whenever anything was freed). -1 never trims. Superuser-only.",
        &TRIM_THRESHOLD,
        -1,
        i32::MAX,
        pgrx::GucContext::Suset,
        pgrx::GucFlags::UNIT_KB,
    );
    // SAFETY: a callback with the signature Postgres expects, registered
    // once per backend (_PG_init runs once); it touches no Postgres state.
    unsafe {
        pgrx::pg_sys::RegisterXactCallback(Some(trim_at_transaction_end), std::ptr::null_mut());
    }
    // Reserve the prefix: a misspelled pg_automerge.* setting is an error
    // from now on (42602), and placeholders already set (from
    // postgresql.conf, ALTER SYSTEM/ROLE/DATABASE or SET before the library
    // was loaded) are removed with a WARNING, instead of being accepted
    // silently while the real setting keeps its default.
    // SAFETY: a NUL-terminated literal; Postgres copies it.
    unsafe { pgrx::pg_sys::MarkGUCPrefixReserved(c"pg_automerge".as_ptr()) };
}

/// Transaction callback: at the end of every top-level transaction (and
/// in parallel workers), trim `malloc`'s free memory when the library's
/// allocation fell by `pg_automerge.trim_threshold` from its high point
/// ([`alloc::trim_if_freed`]). It reads a setting and calls the C library,
/// nothing that can raise an error, so it is safe at abort too.
#[pgrx::pg_guard]
unsafe extern "C-unwind" fn trim_at_transaction_end(
    event: pgrx::pg_sys::XactEvent::Type,
    _arg: *mut std::ffi::c_void,
) {
    use pgrx::pg_sys::XactEvent::{
        XACT_EVENT_ABORT, XACT_EVENT_COMMIT, XACT_EVENT_PARALLEL_ABORT, XACT_EVENT_PARALLEL_COMMIT,
        XACT_EVENT_PREPARE,
    };
    if !matches!(
        event,
        XACT_EVENT_COMMIT
            | XACT_EVENT_ABORT
            | XACT_EVENT_PARALLEL_COMMIT
            | XACT_EVENT_PARALLEL_ABORT
            | XACT_EVENT_PREPARE
    ) {
        return;
    }
    if let Ok(kb) = usize::try_from(TRIM_THRESHOLD.get()) {
        alloc::trim_if_freed(kb.saturating_mul(1024));
    }
}

/// The current value of `pg_automerge.verify_writes`.
fn verify_writes() -> bool {
    VERIFY_WRITES.get()
}

/// The current value of `pg_automerge.max_load_memory`, in bytes (`None`:
/// no limit).
fn max_load_memory() -> Option<u64> {
    u64::try_from(MAX_LOAD_MEMORY.get())
        .ok()
        .map(|kb| kb.saturating_mul(1024))
}

/// `CHECK_FOR_INTERRUPTS()`. An interrupt raises an ERROR, which unwinds
/// as a pgrx panic that the core's guard passes on untouched.
fn check_for_interrupts() {
    pgrx::pg_sys::check_for_interrupts!();
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    include!("tests/mod.rs");
}

/// Hooks of the `cargo pgrx test` framework (it calls these by path, so the
/// module must sit at the crate root).
#[cfg(test)]
pub mod pg_test {
    /// One-off setup before the test Postgres starts: nothing needed.
    pub fn setup(_options: Vec<&str>) {}

    /// Extra postgresql.conf settings for the test Postgres: none.
    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
