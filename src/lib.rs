//! pg_automerge: Automerge CRDT documents as a native Postgres type.
//!
//! This crate is the pgrx glue; all Automerge logic lives in
//! `pg_automerge_core`. See docs/DESIGN.md for the SQL surface and semantics.
//!
//! - `datum`: the Rust types of `automerge` arguments and results.
//! - `expanded`: expanded (in-memory) `automerge` values.
//! - `error`: raising Postgres errors.
//! - `io`: the type's I/O functions, its SQL, and the casts.
//! - `merge`: `merge`, `||` and `merge_agg`.
//! - `introspect`: `automerge_heads` and `automerge_contains`.
//! - `history`: the change types and history functions.
//! - `notify`: the `automerge_notify()` trigger.

// A cdylib: its docs are for developers (built with
// --document-private-items), so links to private items are fine.
#![allow(rustdoc::private_intra_doc_links)]

pub mod datum;
mod error;
pub mod expanded;
mod history;
mod introspect;
mod io;
mod merge;
mod notify;

::pgrx::pg_module_magic!(name, version);

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
