//! History (read-only): the change types, the change set-returning
//! functions, `automerge_get_change`, `automerge_change_count` and
//! `automerge_to_jsonb(automerge, text[])`.

use pg_automerge_core::automerge::ChangeHash;
use pg_automerge_core::header;
use pg_automerge_core::{self as am, Error};
use pgrx::PgTupleDesc;
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::prelude::*;

use crate::datum::AutomergeArg;
use crate::error::{OrRaise, PgError, null_element, raise};
use crate::jsonb::{JsonbBuilder, JsonbDatum};

extension_sql!(
    r#"
-- One change of a document (automerge_changes, automerge_get_change).
CREATE TYPE automerge_change AS (
    hash text,          -- change hash, 64 lowercase hex digits
    actor text,         -- actor id, lowercase hex
    seq bigint,         -- 1, 2, ... per actor
    start_op bigint,    -- counter of the change's first op
    op_count bigint,    -- number of ops (0 for an empty change)
    "time" timestamptz, -- commit time (Unix seconds); NULL when not set
    message text,       -- commit message; NULL when not set
    deps text[],        -- sorted hashes of the changes it depends on
    change bytea        -- the change chunk, loadable by any Automerge
);
COMMENT ON TYPE automerge_change IS 'One change of an automerge document.';

-- The same without the change bytes (automerge_changes_meta).
CREATE TYPE automerge_change_meta AS (
    hash text,
    actor text,
    seq bigint,
    start_op bigint,
    op_count bigint,
    "time" timestamptz,
    message text,
    deps text[]
);
COMMENT ON TYPE automerge_change_meta IS
    'Metadata of one change of an automerge document (no change bytes).';
"#,
    name = "automerge_change_types",
    requires = ["automerge_type"],
);

/// A `text[]` argument of change hashes: no NULL elements (22004), every
/// element 64 hex digits (22P02).
#[track_caller]
fn hashes_arg(name: &str, texts: &[Option<String>]) -> Vec<ChangeHash> {
    let mut hashes = Vec::with_capacity(texts.len());
    for text in texts {
        match text {
            Some(text) => hashes.push(text.as_str()),
            None => null_element(name),
        }
    }
    am::history::parse_hashes(&hashes).or_raise()
}

/// `n` (`what`, e.g. the `seq` of change `hash`) as `bigint`: 22003
/// (numeric_value_out_of_range) when it does not fit, as for any value too
/// large for `bigint`. Automerge keeps these as `u64`, and a value past
/// `i64::MAX` does not make a document invalid.
#[track_caller]
fn to_i64(what: &str, n: u64, hash: Option<&str>) -> i64 {
    match i64::try_from(n) {
        Ok(n) => n,
        Err(_) => {
            let err = PgError::new(
                PgSqlErrorCode::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE,
                format!("automerge {what} {n} is out of range for type bigint"),
            );
            raise(match hash {
                Some(hash) => err.detail(format!("The value is in change {hash}.")),
                None => err,
            })
        }
    }
}

/// The declared result type of the function being called (the composite
/// type of a `SETOF automerge_change` function, say), looked up by OID so
/// that it does not depend on `search_path`.
fn result_type(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Oid {
    // SAFETY: fcinfo and its flinfo are this call's.
    unsafe { pg_sys::get_func_rettype((*(*fcinfo).flinfo).fn_oid) }
}

/// Build an `automerge_change` (with bytes) or `automerge_change_meta`
/// (without) tuple of type `typoid` from a row.
fn change_tuple(
    typoid: pg_sys::Oid,
    row: am::history::ChangeInfo,
) -> pgrx::composite_type!('static, "automerge_change") {
    let time = am::history::pg_timestamptz_micros(row.time)
        .and_then(|micros| pgrx::datum::TimestampWithTimeZone::try_from(micros).ok());
    let seq = to_i64("seq", row.seq, Some(&row.hash));
    let start_op = to_i64("start_op", row.start_op, Some(&row.hash));
    let op_count = to_i64("op_count", row.op_count, Some(&row.hash));
    let mut datums = vec![
        row.hash.into_datum(),
        row.actor.into_datum(),
        seq.into_datum(),
        start_op.into_datum(),
        op_count.into_datum(),
        time.into_datum(),
        row.message.into_datum(),
        row.deps.into_datum(),
    ];
    if let Some(bytes) = row.bytes {
        datums.push(bytes.into_datum());
    }
    let tupdesc = PgTupleDesc::for_composite_type_by_oid(typoid).unwrap_or_else(|| {
        raise(Error::Internal(format!(
            "type {typoid:?} is not a composite type"
        )))
    });
    if tupdesc.len() != datums.len() {
        raise(Error::Internal(format!(
            "result type has {} attributes, expected {}",
            tupdesc.len(),
            datums.len()
        )));
    }
    // SAFETY: the datums are, in order, text, text, int8, int8, int8,
    // timestamptz, text, text[] and (for automerge_change) bytea, the
    // attribute types of the types created in `automerge_change_types`;
    // the count is checked above.
    unsafe { PgHeapTuple::from_datums(tupdesc, datums) }.unwrap_or_else(|e| {
        raise(Error::Internal(format!(
            "could not build a change row: {e}"
        )))
    })
}

/// Every change of `doc` not reachable from `since_heads` (all of them for
/// `'{}'`), in causal order (each change after its dependencies), with the
/// change bytes.
///
/// The rows are computed in full on the first call and returned one by one
/// from a Rust `Vec` owned by the SRF's multi-call memory context (freed on
/// completion and on early termination); no Automerge document outlives
/// the first call. Rebuilding change bytes is the expensive part; use
/// `automerge_changes_meta` when only metadata is needed.
#[pg_extern(immutable, strict, parallel_safe, requires = ["automerge_change_types"])]
fn automerge_changes(
    doc: AutomergeArg,
    since_heads: default!(Vec<Option<String>>, "'{}'"),
    fcinfo: pg_sys::FunctionCallInfo,
) -> SetOfIterator<'static, pgrx::composite_type!('static, "automerge_change")> {
    let since = hashes_arg("since_heads", &since_heads);
    let rows = if doc.nothing_since(&since) {
        Vec::new()
    } else {
        doc.with_input(|input| am::history::changes(input, &since))
            .or_raise()
    };
    let typoid = result_type(fcinfo);
    SetOfIterator::new(rows.into_iter().map(move |row| change_tuple(typoid, row)))
}

/// Like `automerge_changes`, without the change bytes: answered from the
/// change graph of the loaded document, no change is rebuilt.
#[pg_extern(immutable, strict, parallel_safe, requires = ["automerge_change_types"])]
fn automerge_changes_meta(
    doc: AutomergeArg,
    since_heads: default!(Vec<Option<String>>, "'{}'"),
    fcinfo: pg_sys::FunctionCallInfo,
) -> SetOfIterator<'static, pgrx::composite_type!('static, "automerge_change_meta")> {
    let since = hashes_arg("since_heads", &since_heads);
    let rows = if doc.nothing_since(&since) {
        Vec::new()
    } else {
        doc.with_input(|input| am::history::changes_meta(input, &since))
            .or_raise()
    };
    let typoid = result_type(fcinfo);
    SetOfIterator::new(rows.into_iter().map(move |row| change_tuple(typoid, row)))
}

/// The changes of `doc` not reachable from `since_heads`, as concatenated
/// change chunks in causal order (Automerge's `save_after(since_heads)`):
/// load it with `loadIncremental` / `applyChanges`, or apply it with
/// `merge(automerge, bytea)`. Empty when there is nothing new.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_changes_bytes(
    doc: AutomergeArg,
    since_heads: default!(Vec<Option<String>>, "'{}'"),
) -> Vec<u8> {
    let since = hashes_arg("since_heads", &since_heads);
    if doc.nothing_since(&since) {
        return Vec::new();
    }
    doc.with_input(|input| am::history::changes_bytes(input, &since))
        .or_raise()
}

/// The change with hash `hash` (64 hex digits, either case), with its
/// bytes; NULL if `doc` does not have it.
#[pg_extern(immutable, strict, parallel_safe, requires = ["automerge_change_types"])]
fn automerge_get_change(
    doc: AutomergeArg,
    hash: &str,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<pgrx::composite_type!('static, "automerge_change")> {
    let hash = am::history::parse_hash(hash).or_raise();
    let row = doc
        .with_input(|input| am::history::change(input, &hash))
        .or_raise()?;
    Some(change_tuple(result_type(fcinfo), row))
}

/// Number of changes in the document. Read from the stored header and the
/// change actor column (a prefix of the value) when possible, otherwise from
/// the change graph of the loaded document.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_change_count(doc: AutomergeArg) -> i64 {
    let n = match doc.read_prefix(header::change_count_from_prefix) {
        Some(n) => n,
        None => doc.with_input(am::history::change_count).or_raise(),
    };
    to_i64("change count", n, None)
}

/// The document's state as of `heads` as jsonb. Every head must be a change
/// of the document (22023 otherwise); `'{}'` is the state before any change.
#[pg_extern(immutable, strict, parallel_safe, name = "automerge_to_jsonb")]
fn automerge_to_jsonb_at(doc: AutomergeArg, heads: Vec<Option<String>>) -> JsonbDatum {
    let heads = hashes_arg("heads", &heads);
    let mut jsonb = JsonbBuilder::default();
    doc.with_input(|input| am::history::write_json_at(input, &heads, &mut jsonb))
        .or_raise();
    jsonb.finish().unwrap_or_else(|| {
        raise(Error::Internal(
            "automerge to jsonb: incomplete result".into(),
        ))
    })
}

extension_sql!(
    r#"
COMMENT ON FUNCTION automerge_changes(automerge, text[]) IS
    'Every change not reachable from since_heads (all by default), dependencies first, with its bytes.';
COMMENT ON FUNCTION automerge_changes_meta(automerge, text[]) IS
    'Every change not reachable from since_heads (all by default), dependencies first, without its bytes.';
COMMENT ON FUNCTION automerge_changes_bytes(automerge, text[]) IS
    'The changes not reachable from since_heads as concatenated change chunks (save_after).';
COMMENT ON FUNCTION automerge_get_change(automerge, text) IS
    'The change with the given hash, with its bytes; NULL if the document does not have it.';
COMMENT ON FUNCTION automerge_change_count(automerge) IS 'Number of changes in the document.';
COMMENT ON FUNCTION automerge_to_jsonb(automerge, text[]) IS
    'The state of the document as of the given heads as jsonb (''{}'': before any change).';
"#,
    name = "automerge_history_comments",
    requires = [
        automerge_changes,
        automerge_changes_meta,
        automerge_changes_bytes,
        automerge_get_change,
        automerge_change_count,
        automerge_to_jsonb_at
    ],
);
