//! pg_automerge: Automerge CRDT documents as a native Postgres type.
//!
//! This crate is the pgrx glue; all Automerge logic lives in
//! `pg_automerge_core`. See docs/DESIGN.md for the SQL surface and semantics.

use std::ffi::{CStr, CString};

use pg_automerge_core::{self as am, Error, MergeAccumulator};
use pgrx::callconv::{Arg, ArgAbi, BoxRet, FcInfo};
use pgrx::datum::Datum;
use pgrx::pgrx_sql_entity_graph::metadata::{
    ArgumentError, ReturnsError, ReturnsRef, SqlMappingRef, SqlTranslatable, TypeOrigin,
};
use pgrx::prelude::*;
use pgrx::{Internal, JsonB, PgMemoryContexts};

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

/// Raise a core error as a Postgres ERROR (never returns).
fn raise(err: Error) -> ! {
    let code = match err {
        Error::InvalidInput(_) => PgSqlErrorCode::ERRCODE_INVALID_TEXT_REPRESENTATION,
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
    creates = [Type(AutomergeDatum)],
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

extension_sql!(
    r#"
CREATE OPERATOR || (
    LEFTARG = automerge,
    RIGHTARG = automerge,
    FUNCTION = merge,
    COMMUTATOR = ||
);
"#,
    name = "automerge_merge_operator",
    requires = ["automerge_type", merge],
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
    acc.finish().map(|bytes| AutomergeDatum(bytes.into_owned()))
}

extension_sql!(
    r#"
CREATE AGGREGATE merge_agg(automerge) (
    SFUNC = merge_agg_trans,
    STYPE = internal,
    FINALFUNC = merge_agg_final,
    PARALLEL = SAFE
);
"#,
    name = "automerge_merge_agg",
    requires = ["automerge_type", merge_agg_trans, merge_agg_final],
);

// ---------------------------------------------------------------------------
// Introspection
// ---------------------------------------------------------------------------

/// Current heads as sorted lowercase hex change hashes.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_heads(doc: AutomergeDatum) -> Vec<String> {
    am::heads(doc.bytes()).or_raise()
}

/// Whether every change of `b` is already in `a`, i.e. `merge(a, b)` is a no-op.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_contains(a: AutomergeDatum, b: AutomergeDatum) -> bool {
    am::contains(a.bytes(), b.bytes()).or_raise()
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
        Spi::connect(|client| {
            client
                .select(&format!("EXPLAIN (COSTS OFF) {sql}"), None, &[])
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
