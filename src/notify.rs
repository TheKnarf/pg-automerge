//! Change notifications: the `automerge_notify()` trigger.

use std::ffi::{CStr, CString};

use pg_automerge_core::notify::{Column, Event, Op};
use pg_automerge_core::{self as am, Error};
use pgrx::prelude::*;
use pgrx::{PgTrigger, PgTriggerLevel, PgTriggerOperation, PgTriggerWhen};

use crate::datum::AutomergeArg;
use crate::error::{OrRaise, PgError, raise};

unsafe extern "C-unwind" {
    // utils/jsonfuncs.h (exported, not in pgrx's bindings): the machinery
    // behind to_json(anyelement). `JsonTypeCategory` is a C enum (an int).
    // Called only through `pg_guard_ffi_boundary`, since they can ERROR.
    fn json_categorize_type(
        typoid: pg_sys::Oid,
        is_jsonb: bool,
        tcategory: *mut std::ffi::c_int,
        outfuncoid: *mut pg_sys::Oid,
    );
    fn datum_to_json(
        val: pg_sys::Datum,
        tcategory: std::ffi::c_int,
        outfuncoid: pg_sys::Oid,
    ) -> pg_sys::Datum;
}

/// The OID of the `automerge` type, looked up in the schema of the
/// function `fn_oid` (this extension's schema), so it does not depend on
/// `search_path`.
fn automerge_type_in_schema_of(fn_oid: pg_sys::Oid) -> pg_sys::Oid {
    // SAFETY: a catalog lookup of the trigger's own function.
    let oid = crate::datum::automerge_type_in(unsafe { pg_sys::get_func_namespace(fn_oid) });
    if oid == pg_sys::InvalidOid {
        raise(Error::Internal(
            "type automerge not found in the schema of automerge_notify()".into(),
        ));
    }
    oid
}

/// `to_json(value)` as text.
fn json_text(datum: pg_sys::Datum, typoid: pg_sys::Oid) -> String {
    let mut category: std::ffi::c_int = 0;
    let mut outfunc = pg_sys::InvalidOid;
    // SAFETY: a non-null datum of type `typoid` from the trigger tuple; the
    // result is a text datum in the current memory context.
    unsafe {
        let json = pg_sys::ffi::pg_guard_ffi_boundary(|| {
            json_categorize_type(typoid, false, &mut category, &mut outfunc);
            datum_to_json(datum, category, outfunc)
        });
        String::from_datum(json, false).expect("to_json is not null")
    }
}

/// Whether two non-null varlena datums have identical raw representations
/// (the same inline bytes, or the same TOAST pointer), which means the same
/// value. `false` says nothing.
fn same_raw_varlena(a: pg_sys::Datum, b: pg_sys::Datum) -> bool {
    // SAFETY: both are non-null varlena datums of the trigger tuples.
    unsafe {
        let (pa, pb) = (a.cast_mut_ptr::<u8>(), b.cast_mut_ptr::<u8>());
        let (la, lb) = (
            pgrx::varlena::varsize_any(pa.cast()),
            pgrx::varlena::varsize_any(pb.cast()),
        );
        la == lb && std::slice::from_raw_parts(pa, la) == std::slice::from_raw_parts(pb, lb)
    }
}

/// A live user column of the trigger's table.
struct Attr {
    attnum: usize,
    name: String,
    typoid: pg_sys::Oid,
    byval: bool,
    len: i16,
}

/// Whether two values of column `att` are certainly equal because their
/// raw representations are (both NULL, the same by-value datum, the same
/// fixed-length bytes, or the same varlena bytes / TOAST pointer). `false`
/// says nothing.
fn same_raw_datum(a: Option<pg_sys::Datum>, b: Option<pg_sys::Datum>, att: &Attr) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) if att.byval => a == b,
        (Some(a), Some(b)) if att.len > 0 => {
            let n = att.len as usize;
            // SAFETY: by-reference datums of a fixed-length type of `n` bytes.
            unsafe {
                std::slice::from_raw_parts(a.cast_mut_ptr::<u8>(), n)
                    == std::slice::from_raw_parts(b.cast_mut_ptr::<u8>(), n)
            }
        }
        (Some(a), Some(b)) if att.len == -1 => same_raw_varlena(a, b),
        _ => false,
    }
}

/// Sorted hex heads of an `automerge` column value (`None` for NULL), read
/// from a prefix of the stored value; never a full load of a stored value.
fn column_heads(datum: Option<pg_sys::Datum>) -> am::notify::Heads {
    datum.map(|d| {
        // SAFETY: a non-null automerge datum of the row being processed.
        let arg = unsafe { AutomergeArg::from_polymorphic_datum(d, false, pg_sys::InvalidOid) }
            .expect("not null");
        am::heads_to_strings(arg.heads().or_raise())
    })
}

#[cfg(any(test, feature = "pg_test"))]
thread_local! {
    /// Notifications sent by automerge_notify() in this backend, for the
    /// pg_tests (which run in one transaction, so NOTIFY never delivers).
    pub(crate) static SENT_NOTIFICATIONS: std::cell::RefCell<Vec<(String, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

// AFTER INSERT OR UPDATE OR DELETE ... FOR EACH ROW EXECUTE FUNCTION
// automerge_notify('channel', 'key_column' [, ...]): `NOTIFY channel` with a
// JSON payload naming the row and the new/old heads of its `automerge`
// columns, for INSERT and DELETE always, for UPDATE only when the heads of
// some `automerge` column (or the key) changed. See docs/DESIGN.md,
// "Change notifications".
//
// Written like pgrx's `#[pg_trigger]` expansion (V1 info record, guarded
// entry point, SQL below), except that a call outside a trigger is a clean
// 39P01 error (as Postgres' own C trigger function
// suppress_redundant_updates_trigger raises) instead of an XX000 panic
// message.
extension_sql!(
    r#"
CREATE FUNCTION automerge_notify() RETURNS trigger
    LANGUAGE c AS 'MODULE_PATHNAME', 'automerge_notify_wrapper';
COMMENT ON FUNCTION automerge_notify() IS
    'AFTER INSERT OR UPDATE OR DELETE FOR EACH ROW trigger: automerge_notify(channel, key_column [, ...]) sends NOTIFY channel with the row key and the heads of changed automerge columns.';
"#,
    name = "automerge_notify",
    requires = ["automerge_type"],
);

#[unsafe(no_mangle)]
#[doc(hidden)]
pub extern "C" fn pg_finfo_automerge_notify_wrapper() -> &'static pg_sys::Pg_finfo_record {
    const V1_API: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };
    &V1_API
}

/// # Safety
///
/// Called by Postgres through the fmgr V1 interface.
#[unsafe(no_mangle)]
#[doc(hidden)]
pub unsafe extern "C-unwind" fn automerge_notify_wrapper(
    fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    // SAFETY: the guard turns Rust panics and Postgres errors raised inside
    // into Postgres errors, as for every pgrx function.
    unsafe {
        pg_sys::submodules::panic::pgrx_extern_c_guard(move || {
            // SAFETY: Postgres passes a valid fcinfo; from_fcinfo checks
            // that it carries trigger data.
            let trigger = fcinfo
                .as_ref()
                .and_then(|f| pgrx::PgTrigger::from_fcinfo(f).ok())
                .unwrap_or_else(|| {
                    raise(
                        PgError::new(
                            PgSqlErrorCode::ERRCODE_E_R_I_E_TRIGGER_PROTOCOL_VIOLATED,
                            "automerge_notify() can only be called as a trigger",
                        )
                        .hint(NOTIFY_USAGE),
                    )
                });
            notify_row(&trigger);
            // The result of an AFTER trigger is ignored.
            pg_sys::Datum::from(0)
        })
    }
}

/// The HINT of errors about how the trigger is declared.
const NOTIFY_USAGE: &str = "Declare it as CREATE TRIGGER ... AFTER INSERT OR UPDATE OR DELETE \
     ON table FOR EACH ROW EXECUTE FUNCTION automerge_notify('channel', 'key_column' [, ...]).";

/// One firing of the trigger: validate how it is declared, work out what
/// changed, and notify if a listener needs to know.
fn notify_row(trigger: &PgTrigger<'_>) {
    let op = check_trigger_context(trigger);
    let (channel, key_names) = parse_args(trigger);
    let (keys, automerge_columns) = resolve_columns(trigger, &key_names);
    let rows = Rows::of(trigger, op);
    if let Some(event) = build_event(trigger, op, &rows, &keys, &automerge_columns) {
        send_notification(&channel, &event.payload(am::notify::MAX_PAYLOAD));
    }
}

/// The trigger's name, for error messages.
fn trigger_name(trigger: &PgTrigger<'_>) -> String {
    trigger.name().unwrap_or("?").to_string()
}

/// The trigger's table as `schema.table`, quoted as needed.
fn table_name(trigger: &PgTrigger<'_>) -> String {
    let schema = trigger.table_schema().unwrap_or_else(|e| {
        raise(Error::Internal(format!(
            "automerge_notify(): could not read the trigger data: {e}"
        )))
    });
    let relname = trigger.table_name().unwrap_or_else(|e| {
        raise(Error::Internal(format!(
            "automerge_notify(): could not read the trigger data: {e}"
        )))
    });
    format!(
        "{}.{}",
        pgrx::spi::quote_identifier(&schema),
        pgrx::spi::quote_identifier(&relname)
    )
}

/// Check that the trigger fires AFTER, FOR EACH ROW, for INSERT, UPDATE or
/// DELETE (39P01 otherwise), and return the operation.
fn check_trigger_context(trigger: &PgTrigger<'_>) -> Op {
    let protocol = |message: String| {
        PgError::new(
            PgSqlErrorCode::ERRCODE_E_R_I_E_TRIGGER_PROTOCOL_VIOLATED,
            message,
        )
    };
    match trigger.when() {
        Ok(PgTriggerWhen::After) => {}
        Ok(when) => raise(
            protocol(format!(
                "automerge_notify() must be fired AFTER, not {when} (trigger \"{}\")",
                trigger_name(trigger)
            ))
            .hint(NOTIFY_USAGE),
        ),
        Err(e) => raise(Error::Internal(format!(
            "automerge_notify(): could not read the trigger data: {e}"
        ))),
    }
    if !matches!(trigger.level(), PgTriggerLevel::Row) {
        raise(
            protocol(format!(
                "automerge_notify() must be fired FOR EACH ROW (trigger \"{}\")",
                trigger_name(trigger)
            ))
            .hint(NOTIFY_USAGE),
        );
    }
    match trigger.op() {
        Ok(PgTriggerOperation::Insert) => Op::Insert,
        Ok(PgTriggerOperation::Update) => Op::Update,
        Ok(PgTriggerOperation::Delete) => Op::Delete,
        _ => raise(protocol(format!(
            "automerge_notify() must be fired for INSERT, UPDATE or DELETE (trigger \"{}\")",
            trigger_name(trigger)
        ))),
    }
}

/// The trigger's arguments: the channel (1 to NAMEDATALEN - 1 bytes) and
/// at least one key column name (22023 otherwise).
fn parse_args(trigger: &PgTrigger<'_>) -> (String, Vec<String>) {
    let bad_arg =
        |message: String| PgError::new(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, message);
    let mut args = trigger.extra_args().unwrap_or_else(|e| {
        raise(bad_arg(format!(
            "automerge_notify(): invalid trigger argument: {e}"
        )))
    });
    if args.len() < 2 {
        raise(
            bad_arg(format!(
                "automerge_notify() needs a channel and at least one key column (trigger \"{}\")",
                trigger_name(trigger)
            ))
            .hint(NOTIFY_USAGE),
        );
    }
    let keys = args.split_off(1);
    let channel = args.pop().expect("two or more arguments");
    if channel.is_empty() || channel.len() >= pg_sys::NAMEDATALEN as usize {
        raise(bad_arg(format!(
            "automerge_notify(): channel name must be 1 to {} bytes, got {} (trigger \"{}\")",
            pg_sys::NAMEDATALEN - 1,
            channel.len(),
            trigger_name(trigger)
        )));
    }
    (channel, keys)
}

/// The key columns named by the trigger (each must exist, be named once
/// and not be an `automerge` column), and the table's `automerge` columns
/// (of type `automerge` or a domain over it).
fn resolve_columns(trigger: &PgTrigger<'_>, key_names: &[String]) -> (Vec<Attr>, Vec<Attr>) {
    let automerge_oid = automerge_type_in_schema_of(trigger.trigger().tgfoid);
    let is_automerge = |typoid: pg_sys::Oid| {
        // SAFETY: catalog lookup.
        typoid == automerge_oid || unsafe { pg_sys::getBaseType(typoid) } == automerge_oid
    };
    let bad_key = |code: PgSqlErrorCode, what: String, hint: Option<&str>| -> ! {
        let err = PgError::new(
            code,
            format!(
                "automerge_notify(): key column {what} (trigger \"{}\")",
                trigger_name(trigger)
            ),
        );
        raise(match hint {
            Some(hint) => err.hint(hint),
            None => err,
        })
    };

    let (mut automerge, mut others): (Vec<Attr>, Vec<Attr>) = live_columns(trigger)
        .into_iter()
        .partition(|a| is_automerge(a.typoid));
    let mut keys: Vec<Attr> = Vec::with_capacity(key_names.len());
    for name in key_names {
        if keys.iter().any(|a| &a.name == name) {
            bad_key(
                PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
                format!("\"{name}\" is listed twice"),
                None,
            );
        }
        if let Some(i) = others.iter().position(|a| &a.name == name) {
            keys.push(others.swap_remove(i));
        } else if automerge.iter().any(|a| &a.name == name) {
            bad_key(
                PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
                format!("\"{name}\" is an automerge column"),
                Some(
                    "Name the columns that identify the row, such as its primary key; \
                     the automerge columns are reported by their heads.",
                ),
            );
        } else {
            bad_key(
                PgSqlErrorCode::ERRCODE_UNDEFINED_COLUMN,
                format!("\"{name}\" does not exist in table {}", table_name(trigger)),
                None,
            );
        }
    }
    automerge.sort_by_key(|a| a.attnum);
    (keys, automerge)
}

/// The live (not dropped) user columns of the trigger's table.
fn live_columns(trigger: &PgTrigger<'_>) -> Vec<Attr> {
    // SAFETY: the trigger's relation is open and locked for the call.
    let tupdesc = unsafe { (*trigger.trigger_data().tg_relation).rd_att };
    let natts = unsafe { (*tupdesc).natts } as usize;
    (0..natts)
        .filter_map(|i| {
            // SAFETY: i < natts.
            let att = unsafe { &*pg_sys::TupleDescAttr(tupdesc, i as i32) };
            if att.attisdropped {
                return None;
            }
            // SAFETY: NameData is NUL-terminated.
            let name = unsafe { CStr::from_ptr(att.attname.data.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            Some(Attr {
                attnum: i + 1,
                name,
                typoid: att.atttypid,
                byval: att.attbyval,
                len: att.attlen,
            })
        })
        .collect()
}

/// The old and new row of the firing (null where the operation has none).
struct Rows {
    old: *mut pg_sys::HeapTupleData,
    new: *mut pg_sys::HeapTupleData,
    tupdesc: pg_sys::TupleDesc,
}

impl Rows {
    fn of(trigger: &PgTrigger<'_>, op: Op) -> Self {
        let td = trigger.trigger_data();
        let (old, new) = match op {
            Op::Insert => (std::ptr::null_mut(), td.tg_trigtuple),
            Op::Update => (td.tg_trigtuple, td.tg_newtuple),
            Op::Delete => (td.tg_trigtuple, std::ptr::null_mut()),
        };
        // SAFETY: the trigger's relation is open and locked for the call.
        let tupdesc = unsafe { (*td.tg_relation).rd_att };
        Self { old, new, tupdesc }
    }

    /// Column `attnum` of `tuple` (`None` for NULL or no tuple).
    fn get(&self, tuple: *mut pg_sys::HeapTupleData, attnum: usize) -> Option<pg_sys::Datum> {
        if tuple.is_null() {
            return None;
        }
        // SAFETY: a tuple of this trigger call, described by `tupdesc`;
        // attnum is a live user column.
        unsafe {
            pgrx::heap_getattr_raw(
                tuple,
                std::num::NonZeroUsize::new(attnum).expect("attnum >= 1"),
                self.tupdesc,
            )
        }
    }

    fn old_value(&self, attnum: usize) -> Option<pg_sys::Datum> {
        self.get(self.old, attnum)
    }

    fn new_value(&self, attnum: usize) -> Option<pg_sys::Datum> {
        self.get(self.new, attnum)
    }
}

/// The key of a row as (column, JSON value) pairs.
fn key_of(keys: &[Attr], value: impl Fn(usize) -> Option<pg_sys::Datum>) -> Vec<(String, String)> {
    keys.iter()
        .map(|a| {
            let json = match value(a.attnum) {
                Some(d) => json_text(d, a.typoid),
                None => "null".to_string(),
            };
            (a.name.clone(), json)
        })
        .collect()
}

/// The `automerge` columns to report: every one for INSERT (new heads) and
/// DELETE (previous heads); for UPDATE those whose heads changed (identical
/// raw values are skipped without reading them).
fn changed_columns(op: Op, rows: &Rows, automerge_columns: &[Attr]) -> Vec<Column> {
    automerge_columns
        .iter()
        .filter_map(|Attr { attnum, name, .. }| {
            let (old, new) = (rows.old_value(*attnum), rows.new_value(*attnum));
            let name = name.clone();
            Some(match op {
                Op::Insert => Column {
                    name,
                    heads: Some(column_heads(new)),
                    prev_heads: None,
                },
                Op::Delete => Column {
                    name,
                    heads: None,
                    prev_heads: Some(column_heads(old)),
                },
                Op::Update => {
                    if let (Some(a), Some(b)) = (old, new)
                        && same_raw_varlena(a, b)
                    {
                        return None;
                    }
                    let prev = column_heads(old);
                    let heads = column_heads(new);
                    if prev == heads {
                        return None;
                    }
                    Column {
                        name,
                        heads: Some(heads),
                        prev_heads: Some(prev),
                    }
                }
            })
        })
        .collect()
}

/// The event to send, or `None` for an UPDATE that changed neither the
/// heads of an `automerge` column nor the key: nothing a listener needs to
/// know about.
fn build_event(
    trigger: &PgTrigger<'_>,
    op: Op,
    rows: &Rows,
    keys: &[Attr],
    automerge_columns: &[Attr],
) -> Option<Event> {
    let columns = changed_columns(op, rows, automerge_columns);
    // Identical raw key values (the usual case) need no JSON.
    if op == Op::Update
        && columns.is_empty()
        && keys
            .iter()
            .all(|a| same_raw_datum(rows.old_value(a.attnum), rows.new_value(a.attnum), a))
    {
        return None;
    }
    let (key, old_key) = match op {
        Op::Insert => (key_of(keys, |n| rows.new_value(n)), None),
        Op::Delete => (key_of(keys, |n| rows.old_value(n)), None),
        Op::Update => {
            let key = key_of(keys, |n| rows.new_value(n));
            let old_key = key_of(keys, |n| rows.old_value(n));
            let changed = old_key != key;
            (key, changed.then_some(old_key))
        }
    };
    if op == Op::Update && columns.is_empty() && old_key.is_none() {
        return None;
    }
    Some(Event {
        table: table_name(trigger),
        op,
        seq: next_notify_seq(),
        key,
        old_key,
        columns,
    })
}

/// Per-backend notification counter (each backend is its own process).
/// Every payload carries a fresh number, so no two notifications of one
/// transaction are identical: `Async_Notify` silently drops a notification
/// whose channel and payload equal an earlier one of the same transaction,
/// which would lose e.g. the second INSERT of INSERT, DELETE, INSERT.
static NOTIFY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_notify_seq() -> u64 {
    NOTIFY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

fn send_notification(channel: &str, payload: &str) {
    let to_c = |s: &str| {
        CString::new(s)
            .unwrap_or_else(|_| raise(Error::Internal("NUL byte in notification".into())))
    };
    let (c_channel, c_payload) = (to_c(channel), to_c(payload));
    // SAFETY: NUL-terminated strings, lengths validated above / by
    // Event::payload; Async_Notify copies them.
    unsafe { pg_sys::Async_Notify(c_channel.as_ptr(), c_payload.as_ptr()) };
    #[cfg(any(test, feature = "pg_test"))]
    SENT_NOTIFICATIONS.with(|s| {
        s.borrow_mut()
            .push((channel.to_string(), payload.to_string()))
    });
}
