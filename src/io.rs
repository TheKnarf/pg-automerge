//! The `automerge` type itself: text and binary I/O, the type's SQL, and
//! the casts to and from `bytea` and `jsonb`.

use std::ffi::{CStr, CString};

use pg_automerge_core::loaded::{self, LoadedDoc};
use pg_automerge_core::{self as am, Error};
use pgrx::Internal;
use pgrx::prelude::*;

use crate::datum::{AutomergeArg, AutomergeDatum, AutomergeValue, Detoasted};
use crate::error::{OrRaise, raise};
use crate::expanded::new_expanded;
use crate::jsonb::{JsonbBuilder, JsonbDatum};

// The I/O functions are declared by hand in the `automerge_type` block (they
// must exist before `CREATE TYPE`), so pgrx emits no SQL for them.

#[pg_extern(sql = false)]
fn automerge_in(input: &CStr) -> AutomergeDatum {
    let text = input.to_str().unwrap_or_else(|_| {
        raise(Error::InvalidInput(
            "invalid input syntax for type automerge: not valid UTF-8".into(),
        ))
    });
    // A literal too long to fit the load memory limit whatever it holds
    // is refused before its hex is decoded.
    am::budget::check_input_len(text.len().saturating_sub(2) / 2).or_raise();
    AutomergeDatum::from_external(&am::encoding::from_hex_literal(text).or_raise())
}

#[pg_extern(sql = false)]
fn automerge_out(doc: AutomergeArg) -> CString {
    CString::new(am::encoding::to_hex_literal(doc.detoast().stored()))
        .expect("hex output never contains NUL")
}

#[pg_extern(sql = false)]
fn automerge_recv(buf: Internal) -> AutomergeDatum {
    let buf = buf
        .unwrap()
        .expect("recv is strict, so the buffer is never NULL")
        .cast_mut_ptr::<pg_sys::StringInfoData>();
    // SAFETY: Postgres calls a type's receive function with a valid
    // StringInfo holding the message. We consume the rest of it, as
    // receive functions must.
    unsafe {
        let info = &mut *buf;
        let start = info.cursor as usize;
        let len = info.len as usize;
        let bytes = std::slice::from_raw_parts(info.data.cast::<u8>().add(start), len - start);
        info.cursor = info.len;
        AutomergeDatum::from_external(bytes)
    }
}

#[pg_extern(sql = false)]
fn automerge_send(doc: AutomergeArg) -> Vec<u8> {
    match doc.detoast() {
        Detoasted::Flat { bytes, .. } => bytes,
        expanded => expanded.stored().to_vec(),
    }
}

extension_sql!(
    r#"
CREATE TYPE automerge;

CREATE FUNCTION automerge_in(cstring) RETURNS automerge
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'automerge_in_wrapper';
CREATE FUNCTION automerge_out(automerge) RETURNS cstring
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'automerge_out_wrapper';
CREATE FUNCTION automerge_recv(internal) RETURNS automerge
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'automerge_recv_wrapper';
CREATE FUNCTION automerge_send(automerge) RETURNS bytea
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'automerge_send_wrapper';

CREATE TYPE automerge (
    INPUT = automerge_in,
    OUTPUT = automerge_out,
    RECEIVE = automerge_recv,
    SEND = automerge_send,
    INTERNALLENGTH = VARIABLE,
    ALIGNMENT = int4,
    STORAGE = extended
);

COMMENT ON TYPE automerge IS
    'An Automerge CRDT document (uncompressed save format). Implicitly castable to jsonb. New values must fit pg_automerge.max_load_memory.';
COMMENT ON FUNCTION automerge_in(cstring) IS
    'Input function of type automerge: \x followed by the hex of an Automerge save.';
COMMENT ON FUNCTION automerge_out(automerge) IS
    'Output function of type automerge: \x followed by the hex of the stored bytes.';
COMMENT ON FUNCTION automerge_recv(internal) IS
    'Binary input function of type automerge: an Automerge save.';
COMMENT ON FUNCTION automerge_send(automerge) IS
    'Binary output function of type automerge: the stored bytes.';
"#,
    name = "automerge_type",
    creates = [
        Type(AutomergeDatum),
        Type(AutomergeArg),
        Type(AutomergeValue)
    ],
);

// ---------------------------------------------------------------------------
// Casts
// ---------------------------------------------------------------------------

/// `bytea -> automerge`: validates and normalizes. The result is an
/// expanded value holding the document just loaded next to its stored
/// bytes (see "Expanded values"): storing it copies the bytes, and a
/// function reading it (`merge(doc, $1::automerge)`, `excluded.doc` of an
/// upsert) uses the document without loading it again.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_from_bytea(bytes: &[u8]) -> AutomergeValue {
    AutomergeValue::Datum(new_expanded(LoadedDoc::from_external(bytes).or_raise()))
}

/// The current state of the document as jsonb (see the mapping in DESIGN.md).
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_to_jsonb(doc: AutomergeArg) -> JsonbDatum {
    let mut jsonb = JsonbBuilder::default();
    doc.with_input(|input| loaded::write_json(input, &mut jsonb))
        .or_raise();
    jsonb.finish().unwrap_or_else(|| {
        raise(Error::Internal(
            "automerge to jsonb: incomplete result".into(),
        ))
    })
}

extension_sql!(
    r#"
CREATE CAST (bytea AS automerge) WITH FUNCTION automerge_from_bytea(bytea) AS ASSIGNMENT;
-- Same varlena layout: the stored bytes are a valid Automerge save.
CREATE CAST (automerge AS bytea) WITHOUT FUNCTION;
-- The only implicit cast from automerge, so every jsonb operator and
-- function applies to automerge values directly.
CREATE CAST (automerge AS jsonb) WITH FUNCTION automerge_to_jsonb(automerge) AS IMPLICIT;

COMMENT ON FUNCTION automerge_from_bytea(bytea) IS
    'An Automerge save (or change chunks) as an automerge value, validated (within pg_automerge.max_load_memory) and normalized; the bytea to automerge cast.';
COMMENT ON FUNCTION automerge_to_jsonb(automerge) IS
    'The current state of the document as jsonb; the implicit automerge to jsonb cast.';
COMMENT ON CAST (bytea AS automerge) IS
    'Assignment cast: validates (within pg_automerge.max_load_memory) and normalizes an Automerge save.';
COMMENT ON CAST (automerge AS bytea) IS
    'Explicit cast: the stored Automerge bytes (an uncompressed save).';
COMMENT ON CAST (automerge AS jsonb) IS
    'Implicit cast: the current state of the document as jsonb.';
"#,
    name = "automerge_casts",
    requires = ["automerge_type", automerge_from_bytea, automerge_to_jsonb],
);
