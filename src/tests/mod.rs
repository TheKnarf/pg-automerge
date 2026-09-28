// The #[pg_test]s, run by `cargo pgrx test` inside a pgrx-managed Postgres.
//
// Included (not declared as modules) into `mod tests` in lib.rs: pgrx runs
// every #[pg_test] as a function of the `tests` schema, which only the
// items of the #[pg_schema] module itself go to. So these files share one
// Rust module (and namespace); this one holds the imports and the helpers
// used across them.

use pg_automerge_core as am;
use pg_automerge_core::automerge::transaction::Transactable;
use pg_automerge_core::automerge::{ActorId, AutoCommit, ObjType, ROOT};
use pg_automerge_core::serde_json::{self, json};
use pgrx::prelude::*;
use pgrx::{JsonB, datum::DatumWithOid};

include!("io.rs");
include!("merge.rs");
include!("history.rs");
include!("notify.rs");
include!("expanded.rs");
include!("hardening.rs");

fn actor(n: u8) -> ActorId {
    ActorId::from([n; 16])
}

fn sample() -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "status", "open").unwrap();
    let title = doc.put_object(ROOT, "title", ObjType::Text).unwrap();
    doc.splice_text(&title, 0, 0, "Groceries").unwrap();
    let items = doc.put_object(ROOT, "items", ObjType::List).unwrap();
    for (i, (name, done)) in [("milk", true), ("eggs", false)].into_iter().enumerate() {
        let item = doc.insert_object(&items, i, ObjType::Map).unwrap();
        doc.put(&item, "name", name).unwrap();
        doc.put(&item, "done", done).unwrap();
    }
    doc
}

fn one<T: FromDatum + IntoDatum>(sql: &str, args: &[DatumWithOid]) -> T {
    Spi::get_one_with_args::<T>(sql, args)
        .unwrap()
        .expect("non-null result")
}

/// The jsonb of stored bytes, computed in Rust.
fn stored_json(bytes: &[u8]) -> serde_json::Value {
    am::loaded::with_doc(am::loaded::Input::Stored(bytes), am::json::doc_to_json).unwrap()
}

/// `merge(a, changes)` of stored bytes (`changes` must add something),
/// computed in Rust: the stored bytes of the result.
fn stored_merge_changes(a: &[u8], changes: &[u8]) -> Vec<u8> {
    let doc = am::loaded::merge_changes(am::loaded::Input::Stored(a), changes)
        .unwrap()
        .expect("the changes add something");
    doc.stored().unwrap().to_vec()
}

/// `merge_agg` of stored values, computed in Rust.
fn stored_merge_all(values: &[Vec<u8>]) -> Vec<u8> {
    let mut acc = am::MergeAccumulator::new();
    for v in values {
        acc.add_input(am::loaded::Input::Stored(v)).unwrap();
    }
    match acc.finish_loaded().unwrap().expect("not empty") {
        am::Accumulated::Stored(bytes) => bytes.to_vec(),
        am::Accumulated::Loaded(doc) => doc.stored().unwrap().to_vec(),
    }
}

/// Run `sql` and return `"SQLSTATE: message"` of the error it raises.
/// Uses a PL/pgSQL handler so the failure is rolled back properly.
fn sql_error(sql: &str) -> String {
    Spi::run(
        "CREATE OR REPLACE FUNCTION pg_temp.sql_error(q text) RETURNS text \
         LANGUAGE plpgsql AS $$ \
         BEGIN EXECUTE q; RETURN 'no error'; \
         EXCEPTION WHEN OTHERS THEN RETURN SQLSTATE || ': ' || SQLERRM; END $$",
    )
    .unwrap();
    one("SELECT pg_temp.sql_error($1)", &[sql.into()])
}

/// The error `sql` raises: [SQLSTATE, message, DETAIL, HINT], "" for an
/// absent DETAIL or HINT.
fn sql_error_report(sql: &str) -> Vec<String> {
    Spi::run(
        "CREATE OR REPLACE FUNCTION pg_temp.sql_error_report(q text) RETURNS text[] \
         LANGUAGE plpgsql AS $$ \
         DECLARE s text; m text; d text; h text; \
         BEGIN EXECUTE q; RETURN NULL; \
         EXCEPTION WHEN OTHERS THEN \
           GET STACKED DIAGNOSTICS s = RETURNED_SQLSTATE, m = MESSAGE_TEXT, \
             d = PG_EXCEPTION_DETAIL, h = PG_EXCEPTION_HINT; \
           RETURN ARRAY[s, m, coalesce(d, ''), coalesce(h, '')]; END $$",
    )
    .unwrap();
    one("SELECT pg_temp.sql_error_report($1)", &[sql.into()])
}

fn explain(sql: &str) -> String {
    explain_with("COSTS OFF", sql)
}

fn explain_with(options: &str, sql: &str) -> String {
    Spi::connect(|client| {
        client
            .select(&format!("EXPLAIN ({options}) {sql}"), None, &[])
            .unwrap()
            .map(|row| row.get::<String>(1).unwrap().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn stored(doc: &mut AutoCommit) -> Vec<u8> {
    doc.document().save_nocompress()
}

/// A document of several megabytes that does not compress well.
fn large_doc() -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let body: String = (0..3_000_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'a' + (state % 26) as u8)
        })
        .collect();
    doc.splice_text(&text, 0, 0, &body).unwrap();
    doc.put(ROOT, "status", "big").unwrap();
    doc
}
