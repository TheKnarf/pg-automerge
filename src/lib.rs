//! pg_automerge: Automerge CRDT documents as a native Postgres type.
//!
//! This crate is the pgrx glue; all Automerge logic lives in
//! `pg_automerge_core`. See docs/DESIGN.md for the SQL surface and semantics.

use std::ffi::{CStr, CString};

use pg_automerge_core::automerge::ChangeHash;
use pg_automerge_core::header::{self, Prefix};
use pg_automerge_core::{self as am, Error, MergeAccumulator};
use pgrx::callconv::{Arg, ArgAbi, BoxRet, FcInfo};
use pgrx::datum::Datum;
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::pgrx_sql_entity_graph::metadata::{
    ArgumentError, ReturnsError, ReturnsRef, SqlMappingRef, SqlTranslatable, TypeOrigin,
};
use pgrx::prelude::*;
use pgrx::{Internal, JsonB, PgMemoryContexts, PgTupleDesc};

::pgrx::pg_module_magic!(name, version);

// ---------------------------------------------------------------------------
// The `automerge` type
// ---------------------------------------------------------------------------

/// A value of the SQL `automerge` type: the canonical stored bytes (the output
/// of `save_nocompress()`), copied out of the (possibly toasted) datum so no
/// reference into Postgres memory outlives the call.
///
/// Only construct this from bytes that are already validated and normalized
/// (see [`am::normalize`]); the datum is written to disk as-is.
pub struct AutomergeDatum(Vec<u8>);

impl AutomergeDatum {
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl FromDatum for AutomergeDatum {
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        typoid: pg_sys::Oid,
    ) -> Option<Self> {
        // Same varlena layout as bytea: detoast and copy the payload.
        unsafe { Vec::<u8>::from_polymorphic_datum(datum, is_null, typoid) }.map(Self)
    }
}

impl IntoDatum for AutomergeDatum {
    fn into_datum(self) -> Option<pg_sys::Datum> {
        self.0.into_datum()
    }

    fn type_oid() -> pg_sys::Oid {
        pgrx::regtypein("automerge")
    }
}

unsafe impl<'fcx> ArgAbi<'fcx> for AutomergeDatum {
    unsafe fn unbox_arg_unchecked(arg: Arg<'_, 'fcx>) -> Self {
        let index = arg.index();
        unsafe { arg.unbox_arg_using_from_datum() }
            .unwrap_or_else(|| panic!("argument {index} must not be null"))
    }
}

unsafe impl BoxRet for AutomergeDatum {
    unsafe fn box_into<'fcx>(self, fcinfo: &mut FcInfo<'fcx>) -> Datum<'fcx> {
        unsafe { fcinfo.return_optional_datum(self.into_datum()) }
    }
}

unsafe impl SqlTranslatable for AutomergeDatum {
    const TYPE_IDENT: &'static str = pgrx::pgrx_resolved_type!(AutomergeDatum);
    // Created by the `automerge_type` extension_sql! block below, so every
    // function using it is ordered after the type in the generated SQL.
    const TYPE_ORIGIN: TypeOrigin = TypeOrigin::ThisExtension;
    const ARGUMENT_SQL: Result<SqlMappingRef, ArgumentError> =
        Ok(SqlMappingRef::literal("automerge"));
    const RETURN_SQL: Result<ReturnsRef, ReturnsError> =
        Ok(ReturnsRef::One(SqlMappingRef::literal("automerge")));
}

/// An `automerge` argument that is *not* detoasted up front.
///
/// For functions that usually need only the heads, which sit in the first
/// few hundred bytes of a stored value (see `pg_automerge_core::header`):
/// [`LazyAutomerge::heads`] fetches just a prefix with
/// `pg_detoast_datum_slice`, which for an out-of-line value reads only the
/// TOAST chunks covering it (and for a compressed one decompresses only that
/// far). The datum is only valid for the current call; this type is only
/// ever a function argument and never stored.
pub struct LazyAutomerge(pg_sys::Datum);

impl FromDatum for LazyAutomerge {
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        _typoid: pg_sys::Oid,
    ) -> Option<Self> {
        (!is_null).then_some(Self(datum))
    }
}

unsafe impl<'fcx> ArgAbi<'fcx> for LazyAutomerge {
    unsafe fn unbox_arg_unchecked(arg: Arg<'_, 'fcx>) -> Self {
        let index = arg.index();
        unsafe { arg.unbox_arg_using_from_datum() }
            .unwrap_or_else(|| panic!("argument {index} must not be null"))
    }
}

unsafe impl SqlTranslatable for LazyAutomerge {
    const TYPE_IDENT: &'static str = pgrx::pgrx_resolved_type!(LazyAutomerge);
    const TYPE_ORIGIN: TypeOrigin = TypeOrigin::ThisExtension;
    const ARGUMENT_SQL: Result<SqlMappingRef, ArgumentError> =
        Ok(SqlMappingRef::literal("automerge"));
    const RETURN_SQL: Result<ReturnsRef, ReturnsError> =
        Ok(ReturnsRef::One(SqlMappingRef::literal("automerge")));
}

impl LazyAutomerge {
    /// First prefix fetched; covers the heads of documents with dozens of
    /// actors and heads.
    const FIRST_PREFIX: usize = 4096;

    /// Length of the stored bytes (without the varlena header), without
    /// detoasting.
    fn len(&self) -> usize {
        // SAFETY: a non-null varlena datum of this call.
        let raw = unsafe { pg_sys::toast_raw_datum_size(self.0) };
        raw.saturating_sub(pg_sys::VARHDRSZ)
    }

    /// The first `n` stored bytes (fewer if the value is shorter).
    fn prefix(&self, n: usize) -> Vec<u8> {
        let count = i32::try_from(n).unwrap_or(i32::MAX);
        // SAFETY: a non-null varlena datum of this call. The slice is a
        // fresh palloc'd, 4-byte-header varlena, copied out and freed.
        unsafe {
            let datum = self.0.cast_mut_ptr::<pg_sys::varlena>();
            let slice = pg_sys::pg_detoast_datum_slice(datum, 0, count);
            let len = pgrx::varlena::varsize_any_exhdr(slice);
            let data = pgrx::varlena::vardata_any(slice).cast::<u8>();
            let bytes = std::slice::from_raw_parts(data, len).to_vec();
            if slice != datum {
                pg_sys::pfree(slice.cast());
            }
            bytes
        }
    }

    /// All stored bytes (detoasted and copied, like [`AutomergeDatum`]).
    fn bytes(&self) -> Vec<u8> {
        // SAFETY: a non-null varlena datum of this call.
        unsafe { Vec::<u8>::from_polymorphic_datum(self.0, false, pg_sys::InvalidOid) }
            .expect("not null")
    }

    /// Run a header parser on as short a prefix as possible (4 kB first,
    /// then growing to what the parser asks for, at least doubling). `None`
    /// if the value is not in the shape the parser expects, so the caller
    /// must load it.
    fn read_prefix<T>(&self, parse: impl Fn(&[u8], usize) -> Prefix<T>) -> Option<T> {
        let total = self.len();
        let mut want = total.min(Self::FIRST_PREFIX);
        loop {
            let prefix = self.prefix(want);
            match parse(&prefix, total) {
                Prefix::Found(value) => return Some(value),
                Prefix::NeedMore(n) if prefix.len() == want && want < total => {
                    want = n.max(want.saturating_mul(2)).min(total);
                }
                Prefix::NeedMore(_) | Prefix::NotSingleDoc => return None,
            }
        }
    }

    /// The heads, read from as short a prefix as possible; a full load only
    /// if the value is not a single document chunk.
    fn heads(&self) -> Result<Vec<ChangeHash>, Error> {
        match self.read_prefix(header::heads_from_prefix) {
            Some(heads) => Ok(heads),
            None => am::stored_heads(&self.bytes()),
        }
    }

    /// Whether the value has nothing that is not already in `since`: every
    /// head of the value is in `since`. Reads only the heads.
    fn nothing_since(&self, since: &[ChangeHash]) -> bool {
        !since.is_empty() && am::is_subset(&self.heads().or_raise(), since)
    }
}

/// Raise a core error as a Postgres ERROR (never returns).
fn raise(err: Error) -> ! {
    let code = match err {
        Error::InvalidInput(_) => PgSqlErrorCode::ERRCODE_INVALID_TEXT_REPRESENTATION,
        Error::InvalidParameter(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        Error::Internal(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
    };
    pgrx::pg_sys::panic::ErrorReport::new(code, err.to_string(), pgrx::function_name!())
        .report(PgLogLevel::ERROR);
    unreachable!("ereport(ERROR) does not return")
}

trait OrRaise<T> {
    fn or_raise(self) -> T;
}

impl<T> OrRaise<T> for Result<T, Error> {
    fn or_raise(self) -> T {
        self.unwrap_or_else(|e| raise(e))
    }
}

/// Validate + normalize bytes from any entry path into a datum.
fn from_external(bytes: &[u8]) -> AutomergeDatum {
    AutomergeDatum(am::normalize(bytes).or_raise())
}

// The I/O functions are declared by hand in the `automerge_type` block (they
// must exist before `CREATE TYPE`), so pgrx emits no SQL for them.

#[pg_extern(sql = false)]
fn automerge_in(input: &CStr) -> AutomergeDatum {
    let text = input.to_str().unwrap_or_else(|_| {
        raise(Error::InvalidInput(
            "invalid input syntax for type automerge: not valid UTF-8".into(),
        ))
    });
    from_external(&am::encoding::from_hex_literal(text).or_raise())
}

#[pg_extern(sql = false)]
fn automerge_out(doc: AutomergeDatum) -> CString {
    CString::new(am::encoding::to_hex_literal(doc.bytes())).expect("hex output never contains NUL")
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
        from_external(bytes)
    }
}

#[pg_extern(sql = false)]
fn automerge_send(doc: AutomergeDatum) -> Vec<u8> {
    doc.0
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
    'An Automerge CRDT document (uncompressed save format). Implicitly castable to jsonb.';
"#,
    name = "automerge_type",
    creates = [Type(AutomergeDatum), Type(LazyAutomerge)],
);

// ---------------------------------------------------------------------------
// Casts
// ---------------------------------------------------------------------------

/// `bytea -> automerge`: validates and normalizes.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_from_bytea(bytes: &[u8]) -> AutomergeDatum {
    from_external(bytes)
}

/// The current state of the document as jsonb (see the mapping in DESIGN.md).
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_to_jsonb(doc: AutomergeDatum) -> JsonB {
    JsonB(am::to_json(doc.bytes()).or_raise())
}

extension_sql!(
    r#"
CREATE CAST (bytea AS automerge) WITH FUNCTION automerge_from_bytea(bytea) AS ASSIGNMENT;
-- Same varlena layout: the stored bytes are a valid Automerge save.
CREATE CAST (automerge AS bytea) WITHOUT FUNCTION;
-- The only implicit cast from automerge, so every jsonb operator and
-- function applies to automerge values directly.
CREATE CAST (automerge AS jsonb) WITH FUNCTION automerge_to_jsonb(automerge) AS IMPLICIT;
"#,
    name = "automerge_casts",
    requires = ["automerge_type", automerge_from_bytea, automerge_to_jsonb],
);

// ---------------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------------

/// CRDT merge: `a` plus every change of `b` it lacks. Returns an input
/// unchanged when it already contains the other.
#[pg_extern(immutable, strict, parallel_safe)]
fn merge(a: AutomergeDatum, b: AutomergeDatum) -> AutomergeDatum {
    match am::merge(a.bytes(), b.bytes()).or_raise() {
        am::Merged::Left => a,
        am::Merged::Right => b,
        am::Merged::New(bytes) => AutomergeDatum(bytes),
    }
}

/// `merge(automerge, bytea)`: apply an Automerge save or bare change chunks
/// (`save_incremental()` / `save_after()` output, possibly concatenated) on
/// top of the stored document. Returns `a` unchanged when nothing is new.
/// Changes with missing dependencies are rejected (22P02), naming them.
#[pg_extern(immutable, strict, parallel_safe, name = "merge")]
fn merge_bytea(a: AutomergeDatum, changes: &[u8]) -> AutomergeDatum {
    match am::merge_changes(a.bytes(), changes).or_raise() {
        None => a,
        Some(bytes) => AutomergeDatum(bytes),
    }
}

extension_sql!(
    r#"
CREATE OPERATOR || (
    LEFTARG = automerge,
    RIGHTARG = automerge,
    FUNCTION = merge,
    COMMUTATOR = ||
);
-- No commutator: there is no bytea || automerge.
CREATE OPERATOR || (
    LEFTARG = automerge,
    RIGHTARG = bytea,
    FUNCTION = merge
);
"#,
    name = "automerge_merge_operator",
    requires = ["automerge_type", merge, merge_bytea],
);

/// Transition function of `merge_agg`. The state is a [`MergeAccumulator`]
/// owned by the aggregate's memory context and dropped when it is reset.
#[pg_extern(immutable, parallel_safe)]
fn merge_agg_trans(
    mut state: Internal,
    value: Option<AutomergeDatum>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Internal {
    let Some(value) = value else { return state };
    // Each input can take a while to load and merge, and none of that work
    // checks for interrupts; check between inputs so a cancel or
    // statement_timeout takes effect mid-aggregate. No Rust state is
    // borrowed yet, so unwinding out of here is harmless.
    pg_sys::check_for_interrupts!();
    // SAFETY: the state is only ever created below, as a MergeAccumulator.
    let acc = match unsafe { state.get_mut::<MergeAccumulator>() } {
        Some(acc) => acc,
        None => {
            let mut agg_context: pg_sys::MemoryContext = std::ptr::null_mut();
            // SAFETY: fcinfo is this call's; AggCheckCallContext only reads it.
            if unsafe { pg_sys::AggCheckCallContext(fcinfo, &mut agg_context) } == 0 {
                raise(Error::Internal(
                    "merge_agg_trans called in non-aggregate context".into(),
                ));
            }
            let ptr =
                PgMemoryContexts::For(agg_context).leak_and_drop_on_delete(MergeAccumulator::new());
            state = Internal::from(Some(pg_sys::Datum::from(ptr)));
            // SAFETY: just initialized with a MergeAccumulator.
            unsafe { state.get_mut::<MergeAccumulator>() }.expect("just initialized")
        }
    };
    acc.add(value.bytes()).or_raise();
    state
}

/// Final function of `merge_agg`; NULL if every input was NULL.
#[pg_extern(immutable, parallel_safe)]
fn merge_agg_final(state: Internal) -> Option<AutomergeDatum> {
    // SAFETY: the state is only ever created by merge_agg_trans.
    let acc = unsafe { state.get::<MergeAccumulator>() }?;
    acc.finish()
        .or_raise()
        .map(|bytes| AutomergeDatum(bytes.into_owned()))
}

extension_sql!(
    r#"
CREATE AGGREGATE merge_agg(automerge) (
    SFUNC = merge_agg_trans,
    STYPE = internal,
    FINALFUNC = merge_agg_final,
    -- The state is a fully loaded document in the Rust heap, invisible to
    -- Postgres memory accounting and not spillable by HashAgg (no
    -- serialfunc). Declare a size that is realistic for non-trivial
    -- documents (the default estimate for internal is ~8kB) so the planner
    -- prefers sorted grouping over large per-group hash tables.
    SSPACE = 1048576,
    PARALLEL = SAFE
);
"#,
    name = "automerge_merge_agg",
    requires = ["automerge_type", merge_agg_trans, merge_agg_final],
);

// ---------------------------------------------------------------------------
// Introspection
// ---------------------------------------------------------------------------

/// Current heads as sorted lowercase hex change hashes. Read from the
/// start of the stored value; the document is not loaded.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_heads(doc: LazyAutomerge) -> Vec<String> {
    am::heads_to_strings(doc.heads().or_raise())
}

/// Whether every change of `b` is already in `a`, i.e. `merge(a, b)` is a no-op.
///
/// Decided from the two values' heads when possible (no load, and only a
/// prefix of each is detoasted); otherwise `a` is loaded, `b` never is.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_contains(a: LazyAutomerge, b: LazyAutomerge) -> bool {
    let heads_b = b.heads().or_raise();
    let heads_a = a.heads().or_raise();
    match am::contains_by_heads(&heads_a, &heads_b) {
        Some(answer) => answer,
        None => am::contains_loaded(&a.bytes(), &heads_b).or_raise(),
    }
}

// ---------------------------------------------------------------------------
// History: individual changes and past states (read-only)
// ---------------------------------------------------------------------------

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
fn hashes_arg(name: &str, texts: &[Option<String>]) -> Vec<ChangeHash> {
    let texts: Vec<&str> = texts
        .iter()
        .map(|t| {
            t.as_deref().unwrap_or_else(|| {
                pgrx::pg_sys::panic::ErrorReport::new(
                    PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                    format!("{name} must not contain NULL"),
                    pgrx::function_name!(),
                )
                .report(PgLogLevel::ERROR);
                unreachable!("ereport(ERROR) does not return")
            })
        })
        .collect();
    am::history::parse_hashes(&texts).or_raise()
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or_else(|_| raise(Error::Internal(format!("{n} exceeds bigint"))))
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
    let mut datums = vec![
        row.hash.into_datum(),
        row.actor.into_datum(),
        to_i64(row.seq).into_datum(),
        to_i64(row.start_op).into_datum(),
        to_i64(row.op_count).into_datum(),
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
    doc: LazyAutomerge,
    since_heads: default!(Vec<Option<String>>, "'{}'"),
    fcinfo: pg_sys::FunctionCallInfo,
) -> SetOfIterator<'static, pgrx::composite_type!('static, "automerge_change")> {
    let since = hashes_arg("since_heads", &since_heads);
    let rows = if doc.nothing_since(&since) {
        Vec::new()
    } else {
        am::history::changes(&doc.bytes(), &since).or_raise()
    };
    let typoid = result_type(fcinfo);
    SetOfIterator::new(rows.into_iter().map(move |row| change_tuple(typoid, row)))
}

/// Like `automerge_changes`, without the change bytes: answered from the
/// change graph of the loaded document, no change is rebuilt.
#[pg_extern(immutable, strict, parallel_safe, requires = ["automerge_change_types"])]
fn automerge_changes_meta(
    doc: LazyAutomerge,
    since_heads: default!(Vec<Option<String>>, "'{}'"),
    fcinfo: pg_sys::FunctionCallInfo,
) -> SetOfIterator<'static, pgrx::composite_type!('static, "automerge_change_meta")> {
    let since = hashes_arg("since_heads", &since_heads);
    let rows = if doc.nothing_since(&since) {
        Vec::new()
    } else {
        am::history::changes_meta(&doc.bytes(), &since).or_raise()
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
    doc: LazyAutomerge,
    since_heads: default!(Vec<Option<String>>, "'{}'"),
) -> Vec<u8> {
    let since = hashes_arg("since_heads", &since_heads);
    if doc.nothing_since(&since) {
        return Vec::new();
    }
    am::history::changes_bytes(&doc.bytes(), &since).or_raise()
}

/// The change with hash `hash` (64 hex digits, either case), with its
/// bytes; NULL if `doc` does not have it.
#[pg_extern(immutable, strict, parallel_safe, requires = ["automerge_change_types"])]
fn automerge_get_change(
    doc: AutomergeDatum,
    hash: &str,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<pgrx::composite_type!('static, "automerge_change")> {
    let hash = am::history::parse_hash(hash).or_raise();
    let row = am::history::change(doc.bytes(), &hash).or_raise()?;
    Some(change_tuple(result_type(fcinfo), row))
}

/// Number of changes in the document. Read from the stored header and the
/// change actor column (a prefix of the value) when possible, otherwise from
/// the change graph of the loaded document.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_change_count(doc: LazyAutomerge) -> i64 {
    let n = match doc.read_prefix(header::change_count_from_prefix) {
        Some(n) => n,
        None => am::history::change_count_loaded(&doc.bytes()).or_raise(),
    };
    to_i64(n)
}

/// The document's state as of `heads` as jsonb. Every head must be a change
/// of the document (22023 otherwise); `'{}'` is the state before any change.
#[pg_extern(immutable, strict, parallel_safe, name = "automerge_to_jsonb")]
fn automerge_to_jsonb_at(doc: AutomergeDatum, heads: Vec<Option<String>>) -> JsonB {
    let heads = hashes_arg("heads", &heads);
    JsonB(am::history::to_json_at(doc.bytes(), &heads).or_raise())
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pg_automerge_core::automerge::transaction::Transactable;
    use pg_automerge_core::automerge::{ActorId, AutoCommit, ObjType, ROOT};
    use pgrx::prelude::*;
    use pgrx::{JsonB, datum::DatumWithOid};
    use serde_json::json;

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

    #[pg_test]
    fn text_io_round_trip() {
        let bytes = sample().save();
        let normalized = sample().document().save_nocompress();
        let text: String = one("SELECT $1::automerge::text", &[bytes.clone().into()]);
        assert!(text.starts_with("\\x"));
        assert_eq!(
            text,
            pg_automerge_core::encoding::to_hex_literal(&normalized)
        );
        // Text output reads back to the same value.
        let again: String = one("SELECT $1::text::automerge::text", &[text.clone().into()]);
        assert_eq!(again, text);
        // Uppercase hex is accepted too.
        let upper = format!("\\x{}", text[2..].to_uppercase());
        let again: String = one("SELECT $1::text::automerge::text", &[upper.into()]);
        assert_eq!(again, text);
    }

    #[pg_test]
    fn binary_send_recv_round_trip() {
        let bytes = sample().save();
        let normalized = sample().document().save_nocompress();
        let sent: Vec<u8> = one(
            "SELECT automerge_send($1::automerge)",
            &[bytes.clone().into()],
        );
        assert_eq!(sent, normalized);
        // recv is exercised through a binary COPY round trip.
        Spi::run("CREATE TEMP TABLE recv_src (d automerge)").unwrap();
        Spi::run_with_args("INSERT INTO recv_src VALUES ($1)", &[bytes.into()]).unwrap();
        Spi::run(
            "CREATE TEMP TABLE recv_dst AS SELECT d FROM recv_src WITH NO DATA; \
             COPY recv_src TO '/tmp/pg_automerge_recv_test.bin' WITH (FORMAT binary); \
             COPY recv_dst FROM '/tmp/pg_automerge_recv_test.bin' WITH (FORMAT binary);",
        )
        .unwrap();
        let back: Vec<u8> = one("SELECT d::bytea FROM recv_dst", &[]);
        assert_eq!(back, normalized);
    }

    #[pg_test]
    fn casts() {
        let bytes = sample().save();
        let normalized = sample().document().save_nocompress();
        // bytea -> automerge normalizes; automerge -> bytea returns stored bytes.
        let stored: Vec<u8> = one("SELECT $1::automerge::bytea", &[bytes.clone().into()]);
        assert_eq!(stored, normalized);
        // The bytea cast is assignment-level: a bytea parameter can be
        // inserted directly.
        Spi::run("CREATE TEMP TABLE cast_docs (doc automerge)").unwrap();
        Spi::run_with_args("INSERT INTO cast_docs VALUES ($1)", &[bytes.clone().into()]).unwrap();
        // ... but not implicit (no silent bytea -> automerge in expressions).
        let implicit = Spi::get_one_with_args::<bool>(
            "SELECT castcontext = 'a' FROM pg_cast \
             WHERE castsource = 'bytea'::regtype AND casttarget = 'automerge'::regtype",
            &[],
        );
        assert_eq!(implicit.unwrap(), Some(true));
        let explicit_only = one::<bool>(
            "SELECT castcontext = 'e' FROM pg_cast \
             WHERE castsource = 'automerge'::regtype AND casttarget = 'bytea'::regtype",
            &[],
        );
        assert!(explicit_only);
        // automerge -> jsonb is implicit and its function immutable.
        let (context, volatility) = Spi::get_two::<String, String>(
            "SELECT c.castcontext::text, p.provolatile::text FROM pg_cast c \
             JOIN pg_proc p ON p.oid = c.castfunc \
             WHERE castsource = 'automerge'::regtype AND casttarget = 'jsonb'::regtype",
        )
        .unwrap();
        assert_eq!(context.as_deref(), Some("i"));
        assert_eq!(volatility.as_deref(), Some("i"));

        let json: JsonB = one("SELECT doc::jsonb FROM cast_docs", &[]);
        assert_eq!(
            json.0,
            json!({
                "status": "open",
                "title": "Groceries",
                "items": [{ "name": "milk", "done": true }, { "name": "eggs", "done": false }]
            })
        );
        // Implicit: assignable to a jsonb variable/column without a cast.
        Spi::run("CREATE TEMP TABLE cast_json (j jsonb)").unwrap();
        Spi::run("INSERT INTO cast_json SELECT doc FROM cast_docs").unwrap();
        let count: i64 = one(
            "SELECT count(*) FROM cast_json WHERE j->>'status' = 'open'",
            &[],
        );
        assert_eq!(count, 1);
    }

    #[pg_test]
    fn jsonb_operators_on_automerge_column() {
        Spi::run("CREATE TEMP TABLE docs (id int, doc automerge)").unwrap();
        Spi::run_with_args("INSERT INTO docs VALUES (1, $1)", &[sample().save().into()]).unwrap();
        let mut other = AutoCommit::new();
        other.put(ROOT, "status", "closed").unwrap();
        Spi::run_with_args("INSERT INTO docs VALUES (2, $1)", &[other.save().into()]).unwrap();

        let title: String = one("SELECT doc->>'title' FROM docs WHERE id = 1", &[]);
        assert_eq!(title, "Groceries");
        let first: JsonB = one("SELECT doc->'items'->0 FROM docs WHERE id = 1", &[]);
        assert_eq!(first.0, json!({ "name": "milk", "done": true }));
        let id: i32 = one(
            "SELECT id FROM docs WHERE doc @> '{\"status\": \"closed\"}'",
            &[],
        );
        assert_eq!(id, 2);
        let exists: bool = one("SELECT doc ? 'items' FROM docs WHERE id = 1", &[]);
        assert!(exists);
        let open: String = one(
            "SELECT jsonb_path_query(doc, '$.items[*] ? (@.done == false).name')::text \
             FROM docs WHERE id = 1",
            &[],
        );
        assert_eq!(open, "\"eggs\"");
        let path: String = one(
            "SELECT doc #>> '{items,1,name}' FROM docs WHERE id = 1",
            &[],
        );
        assert_eq!(path, "eggs");
    }

    #[pg_test]
    fn merge_commutative_and_idempotent() {
        let mut base = AutoCommit::new().with_actor(actor(1));
        base.put(ROOT, "title", "base").unwrap();
        let list = base.put_object(ROOT, "list", ObjType::List).unwrap();
        base.insert(&list, 0, "x").unwrap();
        let mut a = base.fork().with_actor(actor(2));
        let mut b = base.fork().with_actor(actor(3));
        a.put(ROOT, "title", "from a").unwrap();
        a.insert(&list, 1, "a").unwrap();
        b.put(ROOT, "b", 1i64).unwrap();
        b.insert(&list, 1, "b").unwrap();

        let args = [a.save().into(), b.save().into(), base.save().into()];
        let q = |expr: &str| {
            format!(
                "WITH v(a, b, base) AS (SELECT $1::automerge, $2::automerge, $3::automerge) SELECT {expr} FROM v"
            )
        };

        // Unqualified merge(...) works despite MERGE being a keyword.
        let same_heads: bool = one(
            &q("automerge_heads(merge(a, b)) = automerge_heads(merge(b, a))"),
            &args,
        );
        assert!(same_heads);
        let same_json: bool = one(&q("merge(a, b)::jsonb = merge(b, a)::jsonb"), &args);
        assert!(same_json);
        let op: bool = one(&q("(a || b)::jsonb = merge(a, b)::jsonb"), &args);
        assert!(op);
        let n_heads: i32 = one(&q("cardinality(automerge_heads(merge(a, b)))"), &args);
        assert_eq!(n_heads, 2);

        let json: JsonB = one(&q("merge(a, b)::jsonb"), &args);
        a.merge(&mut b).unwrap();
        assert_eq!(json.0["title"], "from a");
        assert_eq!(json.0["b"], 1);
        // Same state as merging in Rust.
        let merged = pg_automerge_core::normalize(&a.save()).unwrap();
        assert_eq!(json.0, pg_automerge_core::to_json(&merged).unwrap());
        assert_eq!(json.0["list"].as_array().unwrap().len(), 3);

        // Idempotent, and merging an ancestor is a byte-for-byte no-op.
        let idem: bool = one(
            &q("merge(merge(a, b), b)::bytea = merge(a, b)::bytea"),
            &args,
        );
        assert!(idem);
        let self_merge: bool = one(&q("merge(a, a)::bytea = a::bytea"), &args);
        assert!(self_merge);
        let ancestor: bool = one(&q("merge(a, base)::bytea = a::bytea"), &args);
        assert!(ancestor);
        let ancestor_left: bool = one(&q("merge(base, a)::bytea = a::bytea"), &args);
        assert!(ancestor_left);
    }

    #[pg_test]
    fn merge_in_update_statement() {
        let mut base = AutoCommit::new().with_actor(actor(1));
        base.put(ROOT, "n", 0i64).unwrap();
        let mut a = base.fork().with_actor(actor(2));
        let mut b = base.fork().with_actor(actor(3));
        a.put(ROOT, "a", "yes").unwrap();
        b.put(ROOT, "b", "yes").unwrap();
        Spi::run("CREATE TEMP TABLE upd (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
        Spi::run_with_args("INSERT INTO upd VALUES (1, $1)", &[base.save().into()]).unwrap();
        Spi::run_with_args(
            "UPDATE upd SET doc = merge(doc, $1::automerge) WHERE id = 1",
            &[a.save().into()],
        )
        .unwrap();
        Spi::run_with_args(
            "INSERT INTO upd VALUES (1, $1) ON CONFLICT (id) DO UPDATE SET doc = merge(upd.doc, EXCLUDED.doc)",
            &[b.save().into()],
        )
        .unwrap();
        let json: JsonB = one("SELECT doc::jsonb FROM upd", &[]);
        assert_eq!(json.0, json!({ "n": 0, "a": "yes", "b": "yes" }));
    }

    #[pg_test]
    fn merge_agg_merges_all_rows() {
        let mut base = AutoCommit::new().with_actor(actor(1));
        base.put(ROOT, "base", true).unwrap();
        Spi::run("CREATE TEMP TABLE agg (doc automerge)").unwrap();
        Spi::run_with_args("INSERT INTO agg VALUES ($1), (NULL)", &[base.save().into()]).unwrap();
        for i in 2..6u8 {
            let mut fork = base.fork().with_actor(actor(i));
            fork.put(ROOT, format!("k{i}"), i64::from(i)).unwrap();
            Spi::run_with_args("INSERT INTO agg VALUES ($1)", &[fork.save().into()]).unwrap();
        }
        let json: JsonB = one("SELECT merge_agg(doc)::jsonb FROM agg", &[]);
        assert_eq!(
            json.0,
            json!({ "base": true, "k2": 2, "k3": 3, "k4": 4, "k5": 5 })
        );
        let heads: i32 = one(
            "SELECT cardinality(automerge_heads(merge_agg(doc))) FROM agg",
            &[],
        );
        assert_eq!(heads, 4);
        // Order-independent.
        let same: bool = one(
            "SELECT (SELECT merge_agg(doc ORDER BY doc::text) FROM agg)::bytea IS NOT NULL \
               AND (SELECT automerge_heads(merge_agg(doc ORDER BY doc::text DESC)) FROM agg) \
                 = (SELECT automerge_heads(merge_agg(doc ORDER BY doc::text)) FROM agg)",
            &[],
        );
        assert!(same);
        // All-NULL / empty input gives NULL.
        let none =
            Spi::get_one::<Vec<u8>>("SELECT merge_agg(doc)::bytea FROM agg WHERE doc IS NULL");
        assert_eq!(none.unwrap(), None);
        // Grouped use.
        let groups: i64 = one(
            "SELECT count(*) FROM (SELECT merge_agg(doc) FROM agg GROUP BY doc IS NULL) s",
            &[],
        );
        assert_eq!(groups, 2);
    }

    #[pg_test]
    fn heads_and_contains() {
        let mut a = AutoCommit::new().with_actor(actor(1));
        a.put(ROOT, "x", 1i64).unwrap();
        let old = a.save();
        a.put(ROOT, "x", 2i64).unwrap();
        let expected: Vec<String> = a.get_heads().iter().map(ToString::to_string).collect();
        let heads: Vec<String> = one("SELECT automerge_heads($1::automerge)", &[a.save().into()]);
        assert_eq!(heads, expected);
        assert_eq!(heads[0].len(), 64);
        assert_eq!(heads[0], heads[0].to_lowercase());

        let args = [a.save().into(), old.into()];
        let c: bool = one(
            "SELECT automerge_contains($1::automerge, $2::automerge)",
            &args,
        );
        assert!(c);
        let c: bool = one(
            "SELECT automerge_contains($2::automerge, $1::automerge)",
            &args,
        );
        assert!(!c);
        let empty: Vec<String> = one("SELECT automerge_heads(''::bytea::automerge)", &[]);
        assert!(empty.is_empty());
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

    #[pg_test]
    fn invalid_input_rejected_with_clear_errors() {
        for (sql, expected) in [
            (
                "SELECT '{\"a\": 1}'::automerge",
                "22P02: invalid input syntax for type automerge: expected \"\\x\" followed by hex digits",
            ),
            (
                "SELECT '\\x0'::automerge",
                "22P02: invalid input syntax for type automerge: odd number of hex digits",
            ),
            (
                "SELECT '\\xzz'::automerge",
                "22P02: invalid input syntax for type automerge: invalid hex digit 'z'",
            ),
        ] {
            assert_eq!(sql_error(sql), expected, "{sql}");
        }
        for sql in [
            "SELECT '\\x0102030405'::automerge",
            "SELECT '\\x0102030405'::bytea::automerge",
            // Automerge magic bytes, then nothing.
            "SELECT '\\x856f4a83'::bytea::automerge",
        ] {
            let err = sql_error(sql);
            assert!(
                err.starts_with("22P02: invalid automerge document: "),
                "{sql}: {err}"
            );
        }
    }

    #[pg_test]
    fn orphaned_changes_rejected() {
        let mut doc = AutoCommit::new();
        doc.put(ROOT, "x", 1i64).unwrap();
        let heads = doc.get_heads();
        doc.put(ROOT, "y", 2i64).unwrap();
        let orphan = pg_automerge_core::encoding::to_hex_literal(&doc.save_after(&heads));
        assert_eq!(
            sql_error(&format!("SELECT '{orphan}'::bytea::automerge")),
            "22P02: invalid automerge document: changes are missing dependencies"
        );
    }

    #[pg_test]
    fn expression_index_and_generated_column() {
        Spi::run(
            "CREATE TEMP TABLE idx_docs (id int PRIMARY KEY, doc automerge NOT NULL, \
             data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED)",
        )
        .unwrap();
        Spi::run("CREATE INDEX idx_docs_expr ON idx_docs USING gin ((doc::jsonb))").unwrap();
        Spi::run("CREATE INDEX idx_docs_data ON idx_docs USING gin (data jsonb_path_ops)").unwrap();
        for i in 0..200i32 {
            let mut doc = AutoCommit::new();
            doc.put(ROOT, "status", if i == 42 { "rare" } else { "common" })
                .unwrap();
            doc.put(ROOT, "i", i as i64).unwrap();
            Spi::run_with_args(
                "INSERT INTO idx_docs (id, doc) VALUES ($1, $2)",
                &[i.into(), doc.save().into()],
            )
            .unwrap();
        }
        Spi::run("ANALYZE idx_docs").unwrap();
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();

        let generated: i64 = one(
            "SELECT (data->>'i')::bigint FROM idx_docs WHERE id = 7",
            &[],
        );
        assert_eq!(generated, 7);

        let plan = explain("SELECT id FROM idx_docs WHERE doc::jsonb @> '{\"status\": \"rare\"}'");
        assert!(plan.contains("idx_docs_expr"), "{plan}");
        let id: i32 = one(
            "SELECT id FROM idx_docs WHERE doc::jsonb @> '{\"status\": \"rare\"}'",
            &[],
        );
        assert_eq!(id, 42);

        let plan = explain("SELECT id FROM idx_docs WHERE data @> '{\"status\": \"rare\"}'");
        assert!(plan.contains("idx_docs_data"), "{plan}");

        // Merging into a row keeps the generated column in sync.
        let mut extra = AutoCommit::new();
        extra.put(ROOT, "extra", true).unwrap();
        Spi::run_with_args(
            "UPDATE idx_docs SET doc = doc || $1::automerge WHERE id = 42",
            &[extra.save().into()],
        )
        .unwrap();
        let extra_flag: bool = one(
            "SELECT (data->>'extra')::bool FROM idx_docs WHERE id = 42",
            &[],
        );
        assert!(extra_flag);
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

    // -----------------------------------------------------------------------
    // Edge cases (see also crates/pg_automerge_core/tests/edge_cases.rs)
    // -----------------------------------------------------------------------

    fn stored(doc: &mut AutoCommit) -> Vec<u8> {
        doc.document().save_nocompress()
    }

    #[pg_test]
    fn empty_and_delete_only_documents() {
        let empty_bytes = pg_automerge_core::normalize(&[]).unwrap();
        for sql in [
            "SELECT ''::bytea::automerge::bytea",
            "SELECT '\\x'::automerge::bytea",
            "SELECT $1::automerge::bytea",
        ] {
            let bytes: Vec<u8> = one(sql, &[AutoCommit::new().save().into()]);
            assert_eq!(bytes, empty_bytes, "{sql}");
        }
        let json: JsonB = one("SELECT ''::bytea::automerge::jsonb", &[]);
        assert_eq!(json.0, json!({}));

        let mut doc = AutoCommit::new().with_actor(actor(1));
        doc.put(ROOT, "gone", 1i64).unwrap();
        doc.delete(ROOT, "gone").unwrap();
        let args = [doc.save().into()];
        let json: JsonB = one("SELECT $1::automerge::jsonb", &args);
        assert_eq!(json.0, json!({}));
        let n: i32 = one("SELECT cardinality(automerge_heads($1::automerge))", &args);
        assert_eq!(n, 1);
        // Merging with the empty document is a no-op in both directions.
        let same: bool = one(
            "SELECT merge($1::automerge, ''::bytea::automerge)::bytea = $1::automerge::bytea \
               AND merge(''::bytea::automerge, $1::automerge)::bytea = $1::automerge::bytea",
            &args,
        );
        assert!(same);
    }

    #[pg_test]
    fn null_handling() {
        let mut doc = AutoCommit::new();
        doc.put(ROOT, "x", 1i64).unwrap();
        let args = [doc.save().into()];
        for expr in [
            "merge(NULL::automerge, $1::automerge)",
            "merge($1::automerge, NULL::automerge)",
            "$1::automerge || NULL::automerge",
            "automerge_heads(NULL::automerge)",
            "automerge_contains($1::automerge, NULL)",
            "automerge_contains(NULL, $1::automerge)",
            "NULL::bytea::automerge",
            "NULL::automerge::jsonb",
            "NULL::automerge::bytea",
            "(NULL::automerge)->'x'",
            "(SELECT merge_agg(d) FROM (VALUES (NULL::automerge), (NULL)) v(d))",
            "(SELECT merge_agg(d) FROM (SELECT $1::automerge WHERE false) v(d))",
        ] {
            let is_null: bool = one(&format!("SELECT ({expr}) IS NULL"), &args);
            assert!(is_null, "{expr} should be NULL");
        }
        // NULL inputs are skipped, not poisoning the aggregate.
        let json: JsonB = one(
            "SELECT merge_agg(d)::jsonb FROM (VALUES (NULL::automerge), ($1::automerge), (NULL)) v(d)",
            &args,
        );
        assert_eq!(json.0, json!({ "x": 1 }));
    }

    #[pg_test]
    fn merge_agg_as_window_function() {
        Spi::run("CREATE TEMP TABLE win (id int, doc automerge)").unwrap();
        let mut base = AutoCommit::new().with_actor(actor(1));
        for i in 1..=4u8 {
            let mut fork = base.fork().with_actor(actor(i + 1));
            fork.put(ROOT, format!("k{i}"), i64::from(i)).unwrap();
            Spi::run_with_args(
                "INSERT INTO win VALUES ($1, $2)",
                &[i32::from(i).into(), fork.save().into()],
            )
            .unwrap();
        }
        // Running merge: the final function is called repeatedly on a state
        // that keeps growing.
        let keys: Vec<i64> = Spi::connect(|client| {
            client
                .select(
                    "SELECT (SELECT count(*) FROM jsonb_object_keys(m)) FROM \
                       (SELECT id, merge_agg(doc) OVER (ORDER BY id)::jsonb FROM win) s(id, m) \
                     ORDER BY id",
                    None,
                    &[],
                )
                .unwrap()
                .map(|row| row.get::<i64>(1).unwrap().unwrap())
                .collect()
        });
        assert_eq!(keys, vec![1, 2, 3, 4]);
        // Sliding frame: Postgres restarts the aggregate (resetting its
        // memory context) for every row, since there is no inverse function.
        let sliding: Vec<JsonB> = Spi::connect(|client| {
            client
                .select(
                    "SELECT merge_agg(doc) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)::jsonb \
                     FROM win ORDER BY id",
                    None,
                    &[],
                )
                .unwrap()
                .map(|row| row.get::<JsonB>(1).unwrap().unwrap())
                .collect()
        });
        let sliding: Vec<_> = sliding.into_iter().map(|j| j.0).collect();
        assert_eq!(
            sliding,
            vec![
                json!({ "k1": 1 }),
                json!({ "k1": 1, "k2": 2 }),
                json!({ "k2": 2, "k3": 3 }),
                json!({ "k3": 3, "k4": 4 }),
            ]
        );
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

    #[pg_test]
    fn large_document_is_toasted_and_round_trips() {
        let mut doc = large_doc();
        let normalized = stored(&mut doc);
        assert!(normalized.len() > 2_000_000, "{}", normalized.len());
        Spi::run("CREATE TEMP TABLE big (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
        // Compressed save in, normalized bytes stored.
        Spi::run_with_args("INSERT INTO big VALUES (1, $1)", &[doc.save().into()]).unwrap();

        // Stored out of line in the TOAST table.
        let toast_bytes: i64 = one(
            "SELECT pg_relation_size(reltoastrelid) FROM pg_class WHERE oid = 'big'::regclass",
            &[],
        );
        assert!(toast_bytes > 1_000_000, "toast size {toast_bytes}");
        let back: Vec<u8> = one("SELECT doc::bytea FROM big", &[]);
        assert_eq!(back, normalized);
        let text_round_trip: bool = one(
            "SELECT doc::text::automerge::bytea = doc::bytea FROM big",
            &[],
        );
        assert!(text_round_trip);

        let mut fork = doc.fork().with_actor(actor(2));
        doc.put(ROOT, "a", true).unwrap();
        fork.put(ROOT, "b", true).unwrap();
        Spi::run_with_args(
            "UPDATE big SET doc = merge(doc, $1::automerge)",
            &[doc.save().into()],
        )
        .unwrap();
        Spi::run_with_args(
            "UPDATE big SET doc = doc || $1::automerge",
            &[fork.save().into()],
        )
        .unwrap();
        let (a, b) =
            Spi::get_two::<bool, bool>("SELECT (doc->>'a')::bool, (doc->>'b')::bool FROM big")
                .unwrap();
        assert_eq!((a, b), (Some(true), Some(true)));
        let len: i32 = one("SELECT length(doc->>'text') FROM big", &[]);
        assert_eq!(len, 3_000_000);
        doc.merge(&mut fork).unwrap();
        let expected: Vec<String> = {
            let mut h: Vec<String> = doc.get_heads().iter().map(ToString::to_string).collect();
            h.sort();
            h
        };
        let heads: Vec<String> = one("SELECT automerge_heads(doc) FROM big", &[]);
        assert_eq!(heads, expected);
    }

    #[pg_test]
    fn scalar_edge_cases_through_jsonb() {
        use pg_automerge_core::automerge::ScalarValue;
        let mut doc = AutoCommit::new();
        doc.put(ROOT, "imin", i64::MIN).unwrap();
        doc.put(ROOT, "imax", i64::MAX).unwrap();
        doc.put(ROOT, "umax", u64::MAX).unwrap();
        doc.put(ROOT, "neg_zero", -0.0f64).unwrap();
        doc.put(ROOT, "nan", f64::NAN).unwrap();
        doc.put(ROOT, "inf", f64::INFINITY).unwrap();
        doc.put(ROOT, "fmax", f64::MAX).unwrap();
        doc.put(ROOT, "subnormal", 5e-324f64).unwrap();
        doc.put(ROOT, "tenth", 0.1f64).unwrap();
        doc.put(ROOT, "counter", ScalarValue::counter(i64::MAX))
            .unwrap();
        doc.increment(ROOT, "counter", 1).unwrap();
        doc.put(ROOT, "before_epoch", ScalarValue::Timestamp(-1))
            .unwrap();
        doc.put(
            ROOT,
            "year_minus_1",
            ScalarValue::Timestamp(-62_167_219_200_001),
        )
        .unwrap();
        doc.put(ROOT, "bytes", ScalarValue::Bytes(vec![0, 1, 0xfe, 0xff]))
            .unwrap();
        doc.put(ROOT, "😀", "emoji key").unwrap();
        doc.put(ROOT, "nul\0key", "nul\0value").unwrap();
        let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
        doc.splice_text(&text, 0, 0, "a👩‍👩‍👧‍👦b").unwrap();
        let args = [doc.save().into()];
        for (expr, expected) in [
            ("d->'imin' = '-9223372036854775808'", true),
            ("d->'imax' = '9223372036854775807'", true),
            ("d->'umax' = '18446744073709551615'", true),
            ("(d->>'umax')::numeric = 18446744073709551615", true),
            ("d->'neg_zero' = '0'", true),
            ("jsonb_typeof(d->'nan') = 'null'", true),
            ("jsonb_typeof(d->'inf') = 'null'", true),
            ("(d->>'fmax')::float8 = 1.7976931348623157e308", true),
            ("(d->>'subnormal')::float8 = 5e-324", true),
            ("d->>'tenth' = '0.1'", true),
            // Wraps like the release build of Automerge does.
            ("d->'counter' = '-9223372036854775808'", true),
            ("d->>'before_epoch' = '1969-12-31T23:59:59.999Z'", true),
            (
                "(d->>'before_epoch')::timestamptz = '1969-12-31 23:59:59.999+00'",
                true,
            ),
            ("d->>'year_minus_1' = '-000001-12-31T23:59:59.999Z'", true),
            ("decode(d->>'bytes', 'base64') = '\\x0001feff'::bytea", true),
            ("d->>'😀' = 'emoji key'", true),
            ("d->>'nul\u{FFFD}key' = 'nul\u{FFFD}value'", true),
            ("d->>'text' = 'a👩‍👩‍👧‍👦b'", true),
            ("d ? 'nul'", false),
        ] {
            let got: bool = one(
                &format!("SELECT {expr} FROM (SELECT $1::automerge::jsonb) v(d)"),
                &args,
            );
            assert_eq!(got, expected, "{expr}");
        }
    }

    #[pg_test]
    fn deep_nesting_limit() {
        let nest = |depth: usize| {
            let mut doc = AutoCommit::new();
            let mut obj = ROOT;
            for _ in 0..depth {
                obj = doc.put_object(&obj, "k", ObjType::Map).unwrap();
            }
            doc.save()
        };
        let max = pg_automerge_core::json::MAX_DEPTH;
        // The deepest document still accepted converts and is queryable.
        let depth: i32 = one(
            "WITH RECURSIVE r(j, n) AS (SELECT $1::automerge::jsonb, 0 \
               UNION ALL SELECT j->'k', n + 1 FROM r WHERE j ? 'k') SELECT max(n) FROM r",
            &[nest(max - 1).into()],
        );
        assert_eq!(depth as usize, max - 1);
        let hex = pg_automerge_core::encoding::to_hex_literal(&nest(max));
        let err = sql_error(&format!("SELECT '{hex}'::automerge::jsonb"));
        assert_eq!(
            err,
            format!("XX000: automerge document is nested more than {max} levels deep")
        );
        // Storing and merging it is still fine; only the jsonb view fails.
        let ok: bool = one(
            "SELECT cardinality(automerge_heads(merge($1::automerge, ''::bytea::automerge))) = 1",
            &[nest(max).into()],
        );
        assert!(ok);
    }

    #[pg_test]
    fn incremental_and_compressed_input_is_normalized() {
        let mut doc = AutoCommit::new().with_actor(actor(1));
        doc.put(ROOT, "v", 0i64).unwrap();
        let mut bytes = doc.save();
        for i in 1..=3i64 {
            doc.put(ROOT, "v", i).unwrap();
            doc.put(ROOT, format!("pad{i}"), "x".repeat(400)).unwrap();
            bytes.extend(doc.save_incremental());
        }
        let compressed = doc.save();
        let normalized = stored(&mut doc);
        assert!(compressed.len() < normalized.len());
        for input in [bytes, compressed, normalized.clone()] {
            let got: Vec<u8> = one("SELECT $1::automerge::bytea", &[input.into()]);
            assert_eq!(got, normalized);
        }
        // An incremental chunk on its own lacks its base and is rejected.
        let heads = doc.get_heads();
        doc.put(ROOT, "v", 4i64).unwrap();
        let hex = pg_automerge_core::encoding::to_hex_literal(&doc.save_after(&heads));
        let err = sql_error(&format!("SELECT '{hex}'::bytea::automerge"));
        assert!(err.starts_with("22P02: "), "{err}");
    }

    #[pg_test]
    fn text_output_round_trips_exactly() {
        // What pg_dump / COPY rely on: text out -> text in is the identity.
        let mut a = AutoCommit::new().with_actor(actor(7));
        a.put(ROOT, "k", "😀").unwrap();
        let mut b = a.fork().with_actor(actor(8));
        a.put(ROOT, "k", "a").unwrap();
        b.put(ROOT, "k", "b").unwrap();
        Spi::run("CREATE TEMP TABLE dump_src (id int, doc automerge)").unwrap();
        for (i, bytes) in [a.save(), b.save(), Vec::new(), large_doc().save()]
            .into_iter()
            .enumerate()
        {
            Spi::run_with_args(
                "INSERT INTO dump_src VALUES ($1, $2)",
                &[(i as i32).into(), bytes.into()],
            )
            .unwrap();
        }
        Spi::run("INSERT INTO dump_src SELECT 10, merge_agg(doc) FROM dump_src").unwrap();
        let all_same: bool = one(
            "SELECT bool_and(doc::text::automerge::bytea = doc::bytea \
                AND automerge_heads(doc::text::automerge) = automerge_heads(doc)) FROM dump_src",
            &[],
        );
        assert!(all_same);
        // Through COPY's text format, as pg_dump does.
        Spi::run(
            "CREATE TEMP TABLE dump_dst (LIKE dump_src); \
             COPY dump_src TO '/tmp/pg_automerge_copy_test.txt'; \
             COPY dump_dst FROM '/tmp/pg_automerge_copy_test.txt';",
        )
        .unwrap();
        let mismatches: i64 = one(
            "SELECT count(*) FROM dump_src s FULL JOIN dump_dst d USING (id) \
             WHERE s.doc::bytea IS DISTINCT FROM d.doc::bytea",
            &[],
        );
        assert_eq!(mismatches, 0);
    }

    #[pg_test]
    fn garbage_input_is_a_clean_error() {
        let mut doc = AutoCommit::new().with_actor(actor(1));
        doc.put(ROOT, "s", "hello").unwrap();
        let l = doc.put_object(ROOT, "l", ObjType::List).unwrap();
        doc.insert(&l, 0, 1i64).unwrap();
        let save = doc.save();
        // Every truncation and a bit flip at every position: either a valid
        // document or 22P02, never another error or a crashed backend.
        let mut inputs: Vec<Vec<u8>> = (1..save.len()).map(|n| save[..n].to_vec()).collect();
        inputs.extend((0..save.len()).map(|i| {
            let mut b = save.clone();
            b[i] ^= 0x55;
            b
        }));
        inputs.push([save.as_slice(), b"trailing"].concat());
        inputs.push(vec![0; 64]);
        inputs.push(vec![0xff; 64]);
        for input in inputs {
            let hex = pg_automerge_core::encoding::to_hex_literal(&input);
            let result = sql_error(&format!("SELECT '{hex}'::bytea::automerge::jsonb"));
            assert!(
                result == "no error" || result.starts_with("22P02: invalid automerge document"),
                "{hex}: {result}"
            );
        }
    }

    #[pg_test]
    fn comparison_operators_compare_jsonb_not_history() {
        // Documented in README/DESIGN: there is no automerge equality, so
        // `=` resolves through the implicit cast to jsonb equality of the
        // current state. Same content, different histories: equal as jsonb,
        // different heads.
        let mut a = AutoCommit::new().with_actor(actor(1));
        let mut b = AutoCommit::new().with_actor(actor(2));
        a.put(ROOT, "x", 1i64).unwrap();
        b.put(ROOT, "x", 1i64).unwrap();
        let args = [a.save().into(), b.save().into()];
        assert!(one::<bool>("SELECT $1::automerge = $2::automerge", &args));
        assert!(!one::<bool>(
            "SELECT automerge_heads($1::automerge) = automerge_heads($2::automerge)",
            &args
        ));
        assert_eq!(
            sql_error("SELECT DISTINCT '\\x'::automerge"),
            "42883: could not identify an equality operator for type automerge"
        );
        // merge is commutative in heads and jsonb.
        let mut c = a.fork().with_actor(actor(3));
        c.put(ROOT, "y", 2i64).unwrap();
        a.put(ROOT, "z", 3i64).unwrap();
        let args = [a.save().into(), c.save().into()];
        assert!(one::<bool>(
            "SELECT automerge_heads(merge($1::automerge, $2::automerge)) = automerge_heads(merge($2::automerge, $1::automerge)) \
               AND merge($1::automerge, $2::automerge)::jsonb = merge($2::automerge, $1::automerge)::jsonb",
            &args
        ));
    }

    #[pg_test]
    fn merge_agg_declares_realistic_state_size() {
        // The Rust-heap state is not spillable; a realistic aggtransspace
        // keeps the planner from assuming ~8kB per HashAgg group.
        let space: i32 = one(
            "SELECT aggtransspace FROM pg_aggregate WHERE aggfnoid = 'merge_agg'::regproc",
            &[],
        );
        assert_eq!(space, 1_048_576);
    }

    #[pg_test]
    fn checksummed_garbage_is_a_clean_error() {
        // Mutations of a chunk body with the checksum recomputed get past
        // Automerge's integrity check into its column decoders, which panic
        // on some malformed data. That must still be 22P02, not XX000.
        let mut doc = AutoCommit::new().with_actor(actor(1));
        doc.put(ROOT, "s", "hello").unwrap();
        doc.put(ROOT, "n", -5i64).unwrap();
        let l = doc.put_object(ROOT, "l", ObjType::List).unwrap();
        doc.insert(&l, 0, 1.5f64).unwrap();
        let t = doc.put_object(ROOT, "t", ObjType::Text).unwrap();
        doc.splice_text(&t, 0, 0, "text").unwrap();
        let save = doc.save_nocompress();
        // One uncompressed document chunk: magic (4), checksum (4), type (1),
        // uleb128 length, data.
        let (typ, data) = {
            let (mut len, mut n) = (0usize, 0);
            while save[9 + n] & 0x80 != 0 {
                len |= ((save[9 + n] & 0x7f) as usize) << (7 * n);
                n += 1;
            }
            len |= (save[9 + n] as usize) << (7 * n);
            (save[8], save[10 + n..10 + n + len].to_vec())
        };
        // The chunk with a valid checksum for `data`, computed in SQL:
        // sha256(type || uleb len || data)[..4].
        let check = "SELECT pg_temp.sql_error(format('SELECT %L::bytea::automerge::jsonb', \
                     '\\x856f4a83'::bytea || substr(sha256($1), 1, 4) || $1))";
        sql_error("SELECT 1"); // creates pg_temp.sql_error
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut decoder_panics = 0;
        for _ in 0..1500 {
            let mut data = data.clone();
            for _ in 0..1 + rand() % 2 {
                let i = (rand() % data.len() as u64) as usize;
                data[i] = rand() as u8;
            }
            let mut chunk = vec![typ];
            let mut len = data.len();
            while len >= 0x80 {
                chunk.push((len as u8 & 0x7f) | 0x80);
                len >>= 7;
            }
            chunk.push(len as u8);
            chunk.extend(&data);
            let result: String = one(check, &[chunk.into()]);
            assert!(
                result == "no error" || result.starts_with("22P02: invalid automerge document"),
                "{result}"
            );
            if result.contains("malformed data") {
                decoder_panics += 1;
            }
        }
        assert!(decoder_panics > 0, "no decoder panic provoked");
    }

    #[pg_test]
    fn reused_actor_id_is_a_clean_merge_error() {
        // Two different histories that both claim (actor 1, seq 1): a
        // backend bug, e.g. a hard-coded actor id. Automerge refuses to
        // merge them; that must surface as an ERROR, not a crash.
        let mut a = AutoCommit::new().with_actor(actor(1));
        let mut b = AutoCommit::new().with_actor(actor(1));
        a.put(ROOT, "x", 1i64).unwrap();
        b.put(ROOT, "x", 2i64).unwrap();
        let (a, b) = (
            pg_automerge_core::encoding::to_hex_literal(&a.save()),
            pg_automerge_core::encoding::to_hex_literal(&b.save()),
        );
        for sql in [
            format!("SELECT merge('{a}'::bytea::automerge, '{b}'::bytea::automerge)"),
            format!("SELECT merge_agg(d) FROM (VALUES ('{a}'::bytea::automerge), ('{b}')) v(d)"),
            // Concatenated saves go through Automerge::load instead.
            format!("SELECT ('{a}'::bytea || '{b}'::bytea)::automerge"),
        ] {
            let err = sql_error(&sql);
            assert!(err.contains("duplicate seq 1"), "{sql}: {err}");
        }
    }

    #[pg_test]
    fn merge_unrelated_documents_in_sql() {
        let mut a = AutoCommit::new().with_actor(actor(1));
        let mut b = AutoCommit::new().with_actor(actor(2));
        a.put(ROOT, "same", "a").unwrap();
        a.put(ROOT, "only_a", 1i64).unwrap();
        b.put(ROOT, "same", "b").unwrap();
        b.put(ROOT, "only_b", 2i64).unwrap();
        let args = [a.save().into(), b.save().into()];
        let json: JsonB = one("SELECT merge($1::automerge, $2::automerge)::jsonb", &args);
        assert_eq!(json.0, json!({ "same": "b", "only_a": 1, "only_b": 2 }));
        let symmetric: bool = one(
            "SELECT automerge_heads($1::automerge || $2::automerge) \
                  = automerge_heads($2::automerge || $1::automerge)",
            &args,
        );
        assert!(symmetric);
    }

    // -----------------------------------------------------------------------
    // merge(automerge, bytea) and the heads fast path
    // -----------------------------------------------------------------------

    /// The extension functions and operators a one-column view over `expr`
    /// depends on, as `regprocedure` / `regoperator` text, sorted, followed
    /// by `=> <result type>`. This is what the parser resolved `expr` to.
    /// (Built-in objects are pinned and have no pg_depend entries, so e.g.
    /// jsonb's `||` shows up only through the result type.)
    fn resolved(expr: &str) -> Vec<String> {
        Spi::run("DROP VIEW IF EXISTS resolve_v").unwrap();
        Spi::run(&format!(
            "CREATE TEMP VIEW resolve_v AS SELECT {expr} AS x FROM resolve_t"
        ))
        .unwrap();
        let mut deps: Vec<String> = one(
            "SELECT coalesce(array_agg(x ORDER BY x), '{}') FROM ( \
               SELECT coalesce(p.oid::regprocedure::text, o.oid::regoperator::text) AS x \
             FROM pg_depend d JOIN pg_rewrite r ON d.classid = 'pg_rewrite'::regclass AND d.objid = r.oid \
             LEFT JOIN pg_proc p ON d.refclassid = 'pg_proc'::regclass AND p.oid = d.refobjid \
             LEFT JOIN pg_operator o ON d.refclassid = 'pg_operator'::regclass AND o.oid = d.refobjid \
             WHERE r.ev_class = 'resolve_v'::regclass AND (p.oid IS NOT NULL OR o.oid IS NOT NULL)) s",
            &[],
        );
        let typ: String = one(
            "SELECT atttypid::regtype::text FROM pg_attribute \
             WHERE attrelid = 'resolve_v'::regclass AND attname = 'x'",
            &[],
        );
        deps.push(format!("=> {typ}"));
        deps
    }

    #[pg_test]
    fn merge_overloads_resolve_unambiguously() {
        Spi::run("CREATE TEMP TABLE resolve_t (d automerge, b bytea)").unwrap();
        let mm = "merge(automerge,automerge)";
        let mb = "merge(automerge,bytea)";
        let op_mm = "||(automerge,automerge)";
        let op_mb = "||(automerge,bytea)";
        let am = "=> automerge";
        for (expr, expected) in [
            ("d || d", vec![op_mm, am]),
            ("d || b", vec![op_mb, am]),
            ("d || '\\x'::bytea", vec![op_mb, am]),
            // An untyped literal takes the other operand's type.
            ("d || '\\x'", vec![op_mm, am]),
            // jsonb || jsonb (built in, so only the cast is listed).
            (
                "d::jsonb || '{\"a\": 1}'",
                vec!["automerge_to_jsonb(automerge)", "=> jsonb"],
            ),
            ("merge(d, d)", vec![mm, am]),
            ("merge(d, b)", vec![mb, am]),
            ("merge(d, '\\x'::bytea)", vec![mb, am]),
            // Unknown literal / NULL: both candidates accept it, and it is
            // assumed to have the known argument's type (automerge).
            ("merge(d, '\\x')", vec![mm, am]),
            ("merge(d, NULL)", vec![mm, am]),
        ] {
            assert_eq!(resolved(expr), expected, "{expr}");
        }
        // A typed bytea parameter (what drivers send) picks the bytea
        // overload without a cast; an untyped one is inferred as automerge.
        Spi::run("PREPARE typed_p(bytea) AS SELECT merge(d, $1), d || $1 FROM resolve_t").unwrap();
        Spi::run("PREPARE untyped_p AS SELECT merge(d, $1) FROM resolve_t").unwrap();
        let types: String = one(
            "SELECT string_agg(name || '=' || parameter_types::text, ' ' ORDER BY name) \
             FROM pg_prepared_statements WHERE name IN ('typed_p', 'untyped_p')",
            &[],
        );
        assert_eq!(types, "typed_p={bytea} untyped_p={automerge}");
        let plan = explain_with("VERBOSE, COSTS OFF", "EXECUTE typed_p('\\x')");
        assert!(plan.contains("merge(d, '\\x'::bytea)"), "{plan}");
        assert!(plan.contains("(d || '\\x'::bytea)"), "{plan}");
        Spi::run("DEALLOCATE typed_p; DEALLOCATE untyped_p").unwrap();
        // Labels are truthful and match merge(automerge, automerge).
        let labels: String = one(
            "SELECT string_agg(p.oid::regprocedure || ':' || provolatile::text || proisstrict::text || proparallel::text, ' ' ORDER BY p.oid::regprocedure::text) \
             FROM pg_proc p WHERE proname = 'merge'",
            &[],
        );
        assert_eq!(
            labels,
            "merge(automerge,automerge):itrues merge(automerge,bytea):itrues"
        );
    }

    #[pg_test]
    fn merge_bytea_persists_incremental_changes() {
        let mut doc = AutoCommit::new().with_actor(actor(1));
        doc.put(ROOT, "n", 0i64).unwrap();
        Spi::run("CREATE TEMP TABLE inc (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
        Spi::run_with_args("INSERT INTO inc VALUES (1, $1)", &[doc.save().into()]).unwrap();
        doc.save_incremental();

        // One chunk per update, bound as a typed bytea parameter (Vec<u8>),
        // no cast in the SQL.
        let update = "UPDATE inc SET doc = merge(doc, $1) WHERE id = 1";
        for i in 1..=3i64 {
            doc.put(ROOT, "n", i).unwrap();
            doc.put(ROOT, format!("k{i}"), i).unwrap();
            Spi::run_with_args(update, &[doc.save_incremental().into()]).unwrap();
        }
        // Several chunks concatenated, through a PREPAREd statement with a
        // bytea parameter, and through the operator.
        Spi::run("PREPARE persist(bytea) AS UPDATE inc SET doc = merge(doc, $1) WHERE id = 1")
            .unwrap();
        let mut chunks = Vec::new();
        for i in 4..=5i64 {
            doc.put(ROOT, "n", i).unwrap();
            doc.commit();
            chunks.extend(doc.save_incremental());
        }
        let hex = pg_automerge_core::encoding::to_hex_literal(&chunks);
        Spi::run(&format!("EXECUTE persist('{hex}')")).unwrap();
        Spi::run("DEALLOCATE persist").unwrap();
        doc.put(ROOT, "via", "operator").unwrap();
        Spi::run_with_args(
            "UPDATE inc SET doc = doc || $1 WHERE id = 1",
            &[doc.save_incremental().into()],
        )
        .unwrap();

        // The stored value is exactly the normalized full document.
        let stored_bytes: Vec<u8> = one("SELECT doc::bytea FROM inc", &[]);
        assert_eq!(stored_bytes, doc.document().save_nocompress());
        let json: JsonB = one("SELECT doc::jsonb FROM inc", &[]);
        assert_eq!(
            json.0,
            json!({ "n": 5, "k1": 1, "k2": 2, "k3": 3, "via": "operator" })
        );

        // No-ops return the input bytes: empty, already-applied chunks, the
        // full save, NULL is NULL.
        let full = doc.save();
        let noop: bool = one(
            "SELECT merge(doc, ''::bytea)::bytea = doc::bytea \
                AND merge(doc, $1)::bytea = doc::bytea \
                AND (doc || $1)::bytea = doc::bytea \
                AND merge(doc, NULL::bytea) IS NULL FROM inc",
            &[full.into()],
        );
        assert!(noop);
        // A full save of a concurrent fork merges like merge(a, b).
        let mut fork = doc.fork().with_actor(actor(2));
        fork.put(ROOT, "fork", true).unwrap();
        let fork_save = fork.save();
        let same: bool = one(
            "SELECT automerge_heads(merge(doc, $1)) = automerge_heads(merge(doc, $1::automerge)) \
                AND merge(doc, $1)::jsonb = merge(doc, $1::automerge)::jsonb FROM inc",
            &[fork_save.into()],
        );
        assert!(same);
    }

    #[pg_test]
    fn merge_bytea_rejects_bad_input() {
        let mut doc = AutoCommit::new().with_actor(actor(1));
        doc.put(ROOT, "x", 1i64).unwrap();
        let base = doc.save();
        let base_hex = pg_automerge_core::encoding::to_hex_literal(&base);
        doc.put(ROOT, "y", 2i64).unwrap();
        let skipped = doc.get_heads()[0].to_string();
        doc.save_incremental();
        doc.put(ROOT, "z", 3i64).unwrap();
        let orphan = pg_automerge_core::encoding::to_hex_literal(&doc.save_incremental());

        let err = sql_error(&format!(
            "SELECT merge('{base_hex}'::bytea::automerge, '{orphan}'::bytea)"
        ));
        assert_eq!(
            err,
            format!(
                "22P02: invalid automerge changes: missing 1 dependency that neither the document nor the input contains: {skipped}"
            )
        );
        for bad in ["\\x0102", "\\x856f4a83", "\\xdeadbeefdeadbeefdeadbeef"] {
            let err = sql_error(&format!(
                "SELECT merge('{base_hex}'::bytea::automerge, '{bad}'::bytea)"
            ));
            assert!(
                err.starts_with("22P02: invalid automerge changes: "),
                "{bad}: {err}"
            );
        }
        // A good chunk followed by garbage fails as a whole.
        let err = sql_error(&format!(
            "SELECT merge('\\x'::automerge, '{base_hex}'::bytea || '\\x00'::bytea)"
        ));
        assert!(err.starts_with("22P02: "), "{err}");
        // An orphan passed as a literal without ::bytea resolves to the
        // automerge overload and is rejected by the automerge input function.
        let err = sql_error(&format!(
            "SELECT merge('{base_hex}'::bytea::automerge, '{orphan}')"
        ));
        assert_eq!(
            err,
            "22P02: invalid automerge document: changes are missing dependencies"
        );
    }

    #[pg_test]
    fn heads_fast_path_matches_rust_for_every_storage_form() {
        Spi::run("CREATE TEMP TABLE hf (id int PRIMARY KEY, doc automerge NOT NULL, heads text[])")
            .unwrap();
        // Tiny (inline, short varlena header), compressible (inline
        // compressed), large compressible (external compressed), large
        // incompressible (external uncompressed), many heads, empty.
        let mut cases: Vec<AutoCommit> = Vec::new();
        // Distinct actors throughout: the pairwise merges below would fail
        // on two histories claiming the same (actor, seq).
        let mut tiny = AutoCommit::new().with_actor(actor(11));
        tiny.put(ROOT, "x", 1i64).unwrap();
        cases.push(tiny);
        for (n, len) in [(12, 6_000usize), (13, 400_000)] {
            let mut d = AutoCommit::new().with_actor(actor(n));
            d.put(ROOT, "pad", "abcdefgh".repeat(len / 8)).unwrap();
            cases.push(d);
        }
        // 150 concurrent forks of one base: 150 heads, 150 actors.
        let mut base = AutoCommit::new().with_actor(actor(14));
        base.put(ROOT, "base", true).unwrap();
        let mut wide = base.fork().with_actor(actor(15));
        for i in 0..150u8 {
            let mut id = [0xee; 16];
            id[0] = i;
            let mut f = base.fork().with_actor(ActorId::from(id));
            f.put(ROOT, format!("k{i}"), i64::from(i)).unwrap();
            wide.merge(&mut f).unwrap();
        }
        cases.push(wide);
        cases.push(AutoCommit::new());
        for (i, mut d) in cases.into_iter().enumerate() {
            let mut expected: Vec<String> = d.get_heads().iter().map(ToString::to_string).collect();
            expected.sort();
            Spi::run_with_args(
                "INSERT INTO hf VALUES ($1, $2, $3)",
                &[(i as i32).into(), d.save().into(), expected.into()],
            )
            .unwrap();
        }
        Spi::run_with_args(
            "INSERT INTO hf VALUES (100, $1, $2)",
            &[
                large_doc().save().into(),
                {
                    let mut h: Vec<String> = large_doc()
                        .get_heads()
                        .iter()
                        .map(ToString::to_string)
                        .collect();
                    h.sort();
                    h
                }
                .into(),
            ],
        )
        .unwrap();
        let bad: i64 = one(
            "SELECT count(*) FROM hf WHERE automerge_heads(doc) IS DISTINCT FROM heads",
            &[],
        );
        assert_eq!(bad, 0);
        // The storage forms above really occur.
        let forms: String = one(
            "SELECT string_agg(DISTINCT CASE \
                 WHEN pg_column_size(doc) < octet_length(doc::bytea) THEN 'compressed' \
                 ELSE 'plain' END, ',') FROM hf",
            &[],
        );
        assert_eq!(forms, "compressed,plain");
        let n_heads: i32 = one(
            "SELECT cardinality(automerge_heads(doc)) FROM hf WHERE id = 3",
            &[],
        );
        assert_eq!(n_heads, 150);
        // contains agrees with the definition (a merge would be a no-op),
        // over all pairs, using heads-only and loading decisions.
        let disagree: i64 = one(
            "SELECT count(*) FROM hf a, hf b \
             WHERE automerge_contains(a.doc, b.doc) \
                   IS DISTINCT FROM (automerge_heads(merge(a.doc, b.doc)) = automerge_heads(a.doc))",
            &[],
        );
        assert_eq!(disagree, 0);
        let true_pairs: i64 = one(
            "SELECT count(*) FROM hf a, hf b WHERE automerge_contains(a.doc, b.doc)",
            &[],
        );
        // Everything contains itself (6) and the empty document (5 more).
        assert_eq!(true_pairs, 11);
    }

    // -- history -----------------------------------------------------------

    /// base -> (a: two changes, one with message and time) and
    /// (b: one change) concurrently, merged. Returns (merged, base, a, b).
    fn history_docs() -> (AutoCommit, AutoCommit, AutoCommit, AutoCommit) {
        use pg_automerge_core::automerge::transaction::CommitOptions;
        let mut base = AutoCommit::new().with_actor(actor(1));
        base.put(ROOT, "title", "base").unwrap();
        base.commit_with(
            CommitOptions::default()
                .with_message("create")
                .with_time(1_700_000_000),
        );
        let mut a = base.fork().with_actor(actor(2));
        a.put(ROOT, "title", "from a").unwrap();
        a.commit();
        a.put(ROOT, "a", 1i64).unwrap();
        a.commit_with(CommitOptions::default().with_message("second a"));
        let mut b = base.fork().with_actor(actor(3));
        b.put(ROOT, "b", true).unwrap();
        b.commit();
        let mut merged = a.fork().with_actor(actor(4));
        merged.merge(&mut b.fork()).unwrap();
        (merged, base, a, b)
    }

    fn heads_of(doc: &mut AutoCommit) -> Vec<String> {
        let mut h: Vec<String> = doc.get_heads().iter().map(ToString::to_string).collect();
        h.sort();
        h
    }

    #[pg_test]
    fn changes_rows_order_and_columns() {
        let (mut merged, mut base, _, _) = history_docs();
        Spi::run("CREATE TEMP TABLE hist (id int, doc automerge)").unwrap();
        Spi::run_with_args("INSERT INTO hist VALUES (1, $1)", &[merged.save().into()]).unwrap();
        let n: i64 = one("SELECT count(*) FROM hist, automerge_changes(doc)", &[]);
        assert_eq!(n, 4);
        // Causal order: every dep of a row appears on an earlier row.
        let bad: i64 = one(
            "WITH c AS (SELECT x.* FROM hist, automerge_changes(doc) WITH ORDINALITY x(hash, actor, seq, start_op, op_count, time, message, deps, change, o)) \
             SELECT count(*) FROM c, unnest(c.deps) d \
             WHERE NOT EXISTS (SELECT 1 FROM c e WHERE e.hash = d AND e.o < c.o)",
            &[],
        );
        assert_eq!(bad, 0);
        // First row: the base change, with its time and message.
        let (hash, actor_hex) = Spi::get_two::<String, String>(
            "SELECT hash, actor FROM hist, automerge_changes(doc) LIMIT 1",
        )
        .unwrap();
        assert_eq!(hash.unwrap(), heads_of(&mut base)[0]);
        assert_eq!(actor_hex.unwrap(), "01".repeat(16));
        let first: bool = one(
            "SELECT seq = 1 AND start_op = 1 AND op_count = 1 \
                    AND time = to_timestamp(1700000000) AND message = 'create' \
                    AND deps = '{}' AND length(change) > 0 \
             FROM hist, automerge_changes(doc) LIMIT 1",
            &[],
        );
        assert!(first);
        // Unset time and message are NULL.
        let nulls: i64 = one(
            "SELECT count(*) FROM hist, automerge_changes(doc) WHERE time IS NULL",
            &[],
        );
        assert_eq!(nulls, 3);
        let messages: Vec<String> = one(
            "SELECT array_agg(message ORDER BY message) FROM hist, automerge_changes(doc) WHERE message IS NOT NULL",
            &[],
        );
        assert_eq!(messages, vec!["create", "second a"]);
        // Metadata-only rows equal the full rows without the bytes.
        let same: bool = one(
            "SELECT array_agg(ROW(c.hash, c.actor, c.seq, c.start_op, c.op_count, c.time, c.message, c.deps)::automerge_change_meta) \
                  = (SELECT array_agg(m) FROM hist, automerge_changes_meta(doc) m) \
             FROM hist, automerge_changes(doc) c",
            &[],
        );
        assert!(same);
        // Each row's bytes are exactly that change: loading them all (in
        // order) rebuilds the document.
        let rebuilt: bool = one(
            "SELECT automerge_heads(string_agg(change, ''::bytea)::automerge) \
                  = (SELECT automerge_heads(doc) FROM hist) \
             FROM hist, automerge_changes(doc)",
            &[],
        );
        assert!(rebuilt);
        let concat: bool = one(
            "SELECT string_agg(change, ''::bytea) = (SELECT automerge_changes_bytes(doc) FROM hist) \
             FROM hist, automerge_changes(doc)",
            &[],
        );
        assert!(concat);
        // Column types of the result.
        let types: String = one(
            "SELECT string_agg(format_type(atttypid, atttypmod), ',' ORDER BY attnum) \
             FROM pg_attribute WHERE attrelid = 'automerge_change'::regclass AND attnum > 0",
            &[],
        );
        assert_eq!(
            types,
            "text,text,bigint,bigint,bigint,timestamp with time zone,text,text[],bytea"
        );
    }

    #[pg_test]
    fn changes_since_heads() {
        let (mut merged, mut base, mut a, mut b) = history_docs();
        let args = [
            merged.save().into(),
            heads_of(&mut base).into(),
            heads_of(&mut a).into(),
            heads_of(&mut b).into(),
            heads_of(&mut merged).into(),
        ];
        let q = |expr: &str| {
            format!(
                "WITH v(doc, base, a, b, cur) AS (SELECT $1::automerge, $2::text[], $3::text[], $4::text[], $5::text[]) SELECT {expr} FROM v"
            )
        };
        let count = |since: &str| -> i64 {
            one(
                &q(&format!(
                    "(SELECT count(*) FROM automerge_changes_meta(doc, {since}))"
                )),
                &args,
            )
        };
        assert_eq!(count("'{}'"), 4);
        assert_eq!(count("base"), 3);
        assert_eq!(count("a"), 1);
        assert_eq!(count("b"), 2);
        assert_eq!(count("a || b"), 0);
        assert_eq!(count("cur"), 0);
        // Unknown hashes are ignored (a replica ahead of the stored doc).
        assert_eq!(count("base || repeat('ab', 32)"), 3);
        assert_eq!(count("ARRAY[repeat('ab', 32)]"), 4);
        // Uppercase is accepted.
        assert_eq!(count("ARRAY[upper(a[1])]"), 1);
        let since_b: String = one(
            &q("(SELECT string_agg(hash, ',') FROM automerge_changes(doc, b))"),
            &args,
        );
        let a_changes: String = one(
            &q(
                "(SELECT string_agg(hash, ',') FROM automerge_changes(doc) c WHERE c.actor = repeat('02', 16))",
            ),
            &args,
        );
        assert_eq!(since_b, a_changes);
        let empty: Vec<u8> = one(&q("automerge_changes_bytes(doc, cur)"), &args);
        assert!(empty.is_empty());
        // Bad hashes: 22P02; NULL elements: 22004.
        for since in [
            "ARRAY['abc']",
            "ARRAY[repeat('g', 64)]",
            "ARRAY[repeat('ab', 33)]",
        ] {
            let err = sql_error(&format!(
                "SELECT count(*) FROM automerge_changes(''::bytea::automerge, {since})"
            ));
            assert!(
                err.starts_with("22P02: invalid automerge change hash"),
                "{err}"
            );
        }
        let err =
            sql_error("SELECT automerge_changes_bytes(''::bytea::automerge, ARRAY[NULL]::text[])");
        assert_eq!(err, "22004: since_heads must not contain NULL");
        // STRICT: NULL arguments give NULL / no rows.
        let null = Spi::get_one::<Vec<u8>>("SELECT automerge_changes_bytes(NULL, '{}')");
        assert_eq!(null.unwrap(), None);
        let rows: i64 = one("SELECT count(*) FROM automerge_changes(NULL)", &[]);
        assert_eq!(rows, 0);
    }

    #[pg_test]
    fn changes_bytes_round_trip_through_merge() {
        let (mut merged, mut base, mut a, _) = history_docs();
        Spi::run("CREATE TEMP TABLE rt (id text PRIMARY KEY, doc automerge NOT NULL)").unwrap();
        Spi::run_with_args(
            "INSERT INTO rt VALUES ('full', $1), ('base', $2), ('a', $3)",
            &[merged.save().into(), base.save().into(), a.save().into()],
        )
        .unwrap();
        for replica in ["base", "a"] {
            let ok: bool = one(
                "SELECT automerge_heads(m) = automerge_heads(f.doc) AND m::jsonb = f.doc::jsonb \
                 FROM rt f, rt r, \
                      LATERAL (SELECT merge(r.doc, automerge_changes_bytes(f.doc, automerge_heads(r.doc)))) x(m) \
                 WHERE f.id = 'full' AND r.id = $1",
                &[replica.into()],
            );
            assert!(ok, "{replica}");
        }
        // The delta is only what the replica lacks.
        let n: i64 = one(
            "SELECT count(*) FROM rt f, rt r, automerge_changes(f.doc, automerge_heads(r.doc)) \
             WHERE f.id = 'full' AND r.id = 'a'",
            &[],
        );
        assert_eq!(n, 1);
        // In an UPDATE: bring a stored replica up to date.
        Spi::run(
            "UPDATE rt r SET doc = merge(r.doc, automerge_changes_bytes(f.doc, automerge_heads(r.doc))) \
             FROM rt f WHERE f.id = 'full' AND r.id = 'base'",
        )
        .unwrap();
        let heads: Vec<String> = one("SELECT automerge_heads(doc) FROM rt WHERE id = 'base'", &[]);
        assert_eq!(heads, heads_of(&mut merged));
        // Loadable by Automerge itself, on top of the replica.
        let delta: Vec<u8> = one(
            "SELECT automerge_changes_bytes(f.doc, automerge_heads(r.doc)) FROM rt f, rt r \
             WHERE f.id = 'full' AND r.id = 'a'",
            &[],
        );
        let mut replica = a.fork();
        replica.load_incremental(&delta).unwrap();
        assert_eq!(heads_of(&mut replica), heads_of(&mut merged));
    }

    #[pg_test]
    fn get_change_by_hash() {
        let (mut merged, mut base, _, _) = history_docs();
        let base_head = heads_of(&mut base)[0].clone();
        let args = [merged.save().into(), base_head.clone().into()];
        let msg: String = one(
            "SELECT (automerge_get_change($1::automerge, $2)).message",
            &args,
        );
        assert_eq!(msg, "create");
        let same: bool = one(
            "SELECT automerge_get_change($1::automerge, upper($2)) \
                  = (SELECT c FROM automerge_changes($1::automerge) c WHERE c.hash = $2)",
            &args,
        );
        assert!(same);
        let change: Vec<u8> = one(
            "SELECT (automerge_get_change($1::automerge, $2)).change",
            &args,
        );
        let loaded = pg_automerge_core::automerge::Change::from_bytes(change).unwrap();
        assert_eq!(loaded.hash().to_string(), base_head);
        let missing: bool = one(
            "SELECT automerge_get_change($1::automerge, repeat('00', 32)) IS NULL",
            &args[..1],
        );
        assert!(missing);
        let err = sql_error(&format!(
            "SELECT automerge_get_change('{}'::automerge, 'nope')",
            pg_automerge_core::encoding::to_hex_literal(&merged.save())
        ));
        assert_eq!(
            err,
            "22P02: invalid automerge change hash \"nope\": expected 64 hexadecimal digits"
        );
    }

    #[pg_test]
    fn to_jsonb_at_heads() {
        let (mut merged, mut base, mut a, mut b) = history_docs();
        let args = [
            merged.save().into(),
            heads_of(&mut base).into(),
            heads_of(&mut a).into(),
            heads_of(&mut b).into(),
        ];
        let q = |heads: &str| -> JsonB {
            one(
                &format!(
                    "SELECT automerge_to_jsonb($1::automerge, {heads}) FROM (SELECT $2::text[], $3::text[], $4::text[]) v(base, a, b)"
                ),
                &args,
            )
        };
        assert_eq!(q("base").0, json!({"title": "base"}));
        assert_eq!(q("a").0, json!({"title": "from a", "a": 1}));
        assert_eq!(q("b").0, json!({"title": "base", "b": true}));
        assert_eq!(q("a || b").0, json!({"title": "from a", "a": 1, "b": true}));
        assert_eq!(q("'{}'").0, json!({}));
        // The current heads give the current state.
        let current: bool = one(
            "SELECT automerge_to_jsonb($1::automerge, automerge_heads($1::automerge)) = $1::automerge::jsonb",
            &args[..1],
        );
        assert!(current);
        // Every change's state, via the change list.
        let n: i64 = one(
            "SELECT count(DISTINCT automerge_to_jsonb($1::automerge, ARRAY[hash])) FROM automerge_changes_meta($1::automerge)",
            &args[..1],
        );
        assert_eq!(n, 4);
        let hex = pg_automerge_core::encoding::to_hex_literal(&merged.save());
        let unknown = "cd".repeat(32);
        let err = sql_error(&format!(
            "SELECT automerge_to_jsonb('{hex}'::automerge, ARRAY['{unknown}'])"
        ));
        assert_eq!(
            err,
            format!("22023: automerge document does not contain change {unknown}")
        );
        let err = sql_error(&format!(
            "SELECT automerge_to_jsonb('{hex}'::automerge, ARRAY['x'])"
        ));
        assert!(err.starts_with("22P02: "), "{err}");
        let err = sql_error(&format!(
            "SELECT automerge_to_jsonb('{hex}'::automerge, '{{NULL}}'::text[])"
        ));
        assert_eq!(err, "22004: heads must not contain NULL");
        // The one-argument cast function is unaffected.
        let cast: bool = one(
            "SELECT automerge_to_jsonb($1::automerge) = $1::automerge::jsonb",
            &args[..1],
        );
        assert!(cast);
    }

    #[pg_test]
    fn change_count_for_every_storage_form() {
        Spi::run("CREATE TEMP TABLE cc (id int, doc automerge NOT NULL)").unwrap();
        let (mut merged, _, _, _) = history_docs();
        let mut many = AutoCommit::new();
        for i in 0..300u16 {
            many.set_actor(actor((i % 7) as u8 + 1));
            many.put(ROOT, format!("k{i}"), i64::from(i)).unwrap();
            many.commit();
        }
        let mut big = AutoCommit::new().with_actor(actor(9));
        big.put(ROOT, "pad", "abcdefgh".repeat(50_000)).unwrap();
        big.commit();
        big.put(ROOT, "more", 1i64).unwrap();
        for (i, bytes) in [
            merged.save(),
            many.save(),
            big.save(),
            AutoCommit::new().save(),
            large_doc().save(),
        ]
        .into_iter()
        .enumerate()
        {
            Spi::run_with_args(
                "INSERT INTO cc VALUES ($1, $2)",
                &[(i as i32).into(), bytes.into()],
            )
            .unwrap();
        }
        let counts: Vec<i64> = one(
            "SELECT array_agg(automerge_change_count(doc) ORDER BY id) FROM cc",
            &[],
        );
        // large_doc() is one change: AutoCommit commits once, on save.
        assert_eq!(counts, vec![4, 300, 2, 0, 1]);
        let disagree: i64 = one(
            "SELECT count(*) FROM cc \
             WHERE automerge_change_count(doc) <> (SELECT count(*) FROM automerge_changes_meta(doc))",
            &[],
        );
        assert_eq!(disagree, 0);
    }

    /// SRFs stopped early (LIMIT on a target-list SRF, closed cursors) and
    /// SRFs failing mid-statement leave nothing behind.
    #[pg_test]
    fn changes_srf_early_termination() {
        let (mut merged, _, _, _) = history_docs();
        Spi::run("CREATE TEMP TABLE et (doc automerge)").unwrap();
        Spi::run_with_args("INSERT INTO et VALUES ($1)", &[merged.save().into()]).unwrap();
        for _ in 0..50 {
            let h: String = one("SELECT (automerge_changes(doc)).hash FROM et LIMIT 1", &[]);
            assert_eq!(h.len(), 64);
            let h: String = one(
                "SELECT (automerge_changes_meta(doc, '{}')).hash FROM et LIMIT 1",
                &[],
            );
            assert_eq!(h.len(), 64);
        }
        Spi::run(
            "DO $$ DECLARE c refcursor; r record; BEGIN \
               FOR i IN 1..20 LOOP \
                 OPEN c FOR SELECT (automerge_changes(doc)).* FROM et; \
                 FETCH c INTO r; \
                 CLOSE c; \
               END LOOP; END $$",
        )
        .unwrap();
        // An error from a later argument after earlier rows were produced.
        let err =
            sql_error("SELECT (automerge_changes(doc)).hash, 1 / (random() * 0)::int FROM et");
        assert!(err.starts_with("22012: "), "{err}");
        let n: i64 = one("SELECT count(*) FROM et, automerge_changes(doc)", &[]);
        assert_eq!(n, 4);
    }

    #[pg_test]
    fn history_functions_are_labelled_and_search_path_safe() {
        let wrong: Vec<String> = one(
            "SELECT coalesce(array_agg(p.oid::regprocedure::text), '{}') FROM pg_proc p \
             WHERE p.proname IN ('automerge_changes', 'automerge_changes_meta', 'automerge_changes_bytes', \
                                 'automerge_get_change', 'automerge_change_count', 'automerge_to_jsonb') \
               AND NOT (p.provolatile = 'i' AND p.proisstrict AND p.proparallel = 's')",
            &[],
        );
        assert!(wrong.is_empty(), "{wrong:?}");
        let n: i64 = one(
            "SELECT count(*) FROM pg_proc WHERE proname IN ('automerge_changes', 'automerge_changes_meta', \
             'automerge_changes_bytes', 'automerge_get_change', 'automerge_change_count', 'automerge_to_jsonb')",
            &[],
        );
        assert_eq!(n, 7);
        // Result rows are built from the function's declared type, not a
        // lookup by name, so they work with the extension off search_path.
        let (mut merged, _, _, _) = history_docs();
        let schema: String = one(
            "SELECT n.nspname::text FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
             WHERE t.typname = 'automerge_change'",
            &[],
        );
        Spi::run("SET LOCAL search_path TO pg_catalog").unwrap();
        let n: i64 = one(
            &format!(
                "SELECT count(*) FROM {schema}.automerge_changes($1::bytea::{schema}.automerge) c \
                 WHERE (c).change IS NOT NULL"
            ),
            &[merged.save().into()],
        );
        Spi::run("RESET search_path").unwrap();
        assert_eq!(n, 4);
    }
}

/// This module is required by `cargo pgrx test` invocations.
/// It must be visible at the root of your extension crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {
        // perform one-off initialization when the pg_test framework starts
    }

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        // return any postgresql.conf settings that are required for your tests
        vec![]
    }
}
