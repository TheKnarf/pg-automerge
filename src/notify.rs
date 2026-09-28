//! Change notifications: the `automerge_notify()` trigger.

use std::ffi::{CStr, CString};

use pg_automerge_core::{self as am, Error};
use pgrx::prelude::*;

use crate::datum::AutomergeArg;
use crate::error::{OrRaise, fail, raise};

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
    // SAFETY: plain catalog lookups; the name is a NUL-terminated literal.
    let oid = unsafe {
        let nsp = pg_sys::get_func_namespace(fn_oid);
        pg_sys::GetSysCacheOid(
            pg_sys::SysCacheIdentifier::TYPENAMENSP as std::ffi::c_int,
            pg_sys::Anum_pg_type_oid as pg_sys::AttrNumber,
            pg_sys::Datum::from(c"automerge".as_ptr()),
            pg_sys::Datum::from(nsp),
            pg_sys::Datum::from(0),
            pg_sys::Datum::from(0),
        )
    };
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
// 0A000 error instead of an XX000 panic message.
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
                    fail(
                        PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
                        format!(
                            "automerge_notify() can only be called as a trigger; {NOTIFY_USAGE}"
                        ),
                    )
                });
            notify_row(&trigger);
            // The result of an AFTER trigger is ignored.
            pg_sys::Datum::from(0)
        })
    }
}

const NOTIFY_USAGE: &str = "usage: CREATE TRIGGER ... AFTER INSERT OR UPDATE OR DELETE ON table \
     FOR EACH ROW EXECUTE FUNCTION automerge_notify('channel', 'key_column' [, ...])";

fn notify_row(trigger: &pgrx::PgTrigger<'_>) {
    use am::notify::{Column, Event, Op};
    use pgrx::{PgTriggerLevel, PgTriggerOperation, PgTriggerWhen};

    let protocol = PgSqlErrorCode::ERRCODE_E_R_I_E_TRIGGER_PROTOCOL_VIOLATED;
    let tgname = trigger.name().unwrap_or("?").to_string();
    match trigger.when() {
        Ok(PgTriggerWhen::After) => {}
        Ok(when) => fail(
            protocol,
            format!(
                "automerge_notify() must be fired AFTER, not {when} (trigger \"{tgname}\"); {NOTIFY_USAGE}"
            ),
        ),
        Err(e) => raise(Error::Internal(e.to_string())),
    }
    if !matches!(trigger.level(), PgTriggerLevel::Row) {
        fail(
            protocol,
            format!(
                "automerge_notify() must be fired FOR EACH ROW (trigger \"{tgname}\"); {NOTIFY_USAGE}"
            ),
        );
    }
    let op = match trigger.op() {
        Ok(PgTriggerOperation::Insert) => Op::Insert,
        Ok(PgTriggerOperation::Update) => Op::Update,
        Ok(PgTriggerOperation::Delete) => Op::Delete,
        _ => fail(
            protocol,
            format!(
                "automerge_notify() must be fired for INSERT, UPDATE or DELETE (trigger \"{tgname}\")"
            ),
        ),
    };

    let bad_arg = PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE;
    let args = trigger.extra_args().unwrap_or_else(|e| {
        fail(
            bad_arg,
            format!("automerge_notify(): invalid trigger argument: {e}"),
        )
    });
    if args.len() < 2 {
        fail(
            bad_arg,
            format!(
                "automerge_notify() needs a channel and at least one key column (trigger \"{tgname}\"); {NOTIFY_USAGE}"
            ),
        );
    }
    let channel = &args[0];
    if channel.is_empty() || channel.len() >= pg_sys::NAMEDATALEN as usize {
        fail(
            bad_arg,
            format!(
                "automerge_notify(): channel name must be 1 to {} bytes, got {} (trigger \"{tgname}\")",
                pg_sys::NAMEDATALEN - 1,
                channel.len()
            ),
        );
    }

    let td = trigger.trigger_data();
    let tg = trigger.trigger();
    let automerge_oid = automerge_type_in_schema_of(tg.tgfoid);
    let table_name = || {
        let schema = trigger
            .table_schema()
            .unwrap_or_else(|e| raise(Error::Internal(e.to_string())));
        let relname = trigger
            .table_name()
            .unwrap_or_else(|e| raise(Error::Internal(e.to_string())));
        format!(
            "{}.{}",
            pgrx::spi::quote_identifier(&schema),
            pgrx::spi::quote_identifier(&relname)
        )
    };

    // SAFETY: the trigger's relation is open and locked for the call.
    let tupdesc = unsafe { (*td.tg_relation).rd_att };
    let natts = unsafe { (*tupdesc).natts } as usize;
    // The live user columns.
    let attrs: Vec<Attr> = (0..natts)
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
        .collect();
    let is_automerge = |typoid: pg_sys::Oid| {
        // SAFETY: catalog lookup.
        typoid == automerge_oid || unsafe { pg_sys::getBaseType(typoid) } == automerge_oid
    };

    let mut key_cols: Vec<&Attr> = Vec::new();
    for name in &args[1..] {
        let Some(attr) = attrs.iter().find(|a| &a.name == name) else {
            let table = table_name();
            fail(
                PgSqlErrorCode::ERRCODE_UNDEFINED_COLUMN,
                format!(
                    "automerge_notify(): key column \"{name}\" does not exist in table {table} (trigger \"{tgname}\")"
                ),
            );
        };
        if key_cols.iter().any(|a| &a.name == name) {
            fail(
                bad_arg,
                format!(
                    "automerge_notify(): key column \"{name}\" is listed twice (trigger \"{tgname}\")"
                ),
            );
        }
        if is_automerge(attr.typoid) {
            fail(
                bad_arg,
                format!(
                    "automerge_notify(): key column \"{name}\" is an automerge column; name the columns that identify the row (trigger \"{tgname}\")"
                ),
            );
        }
        key_cols.push(attr);
    }

    let (old, new) = match op {
        Op::Insert => (std::ptr::null_mut(), td.tg_trigtuple),
        Op::Update => (td.tg_trigtuple, td.tg_newtuple),
        Op::Delete => (td.tg_trigtuple, std::ptr::null_mut()),
    };
    let get = |tuple: *mut pg_sys::HeapTupleData, attnum: usize| -> Option<pg_sys::Datum> {
        if tuple.is_null() {
            return None;
        }
        // SAFETY: a tuple of this trigger call, described by `tupdesc`;
        // attnum is a live user column.
        unsafe {
            pgrx::heap_getattr_raw(tuple, std::num::NonZeroUsize::new(attnum).unwrap(), tupdesc)
        }
    };
    let key_of = |tuple: *mut pg_sys::HeapTupleData| -> Vec<(String, String)> {
        key_cols
            .iter()
            .map(|a| {
                let value = match get(tuple, a.attnum) {
                    Some(d) => json_text(d, a.typoid),
                    None => "null".to_string(),
                };
                (a.name.clone(), value)
            })
            .collect()
    };

    let mut columns = Vec::new();
    for Attr {
        attnum,
        name,
        typoid,
        ..
    } in &attrs
    {
        if !is_automerge(*typoid) {
            continue;
        }
        let old_datum = get(old, *attnum);
        let new_datum = get(new, *attnum);
        let column = match op {
            Op::Insert => Column {
                name: name.clone(),
                heads: Some(column_heads(new_datum)),
                prev_heads: None,
            },
            Op::Delete => Column {
                name: name.clone(),
                heads: None,
                prev_heads: Some(column_heads(old_datum)),
            },
            Op::Update => {
                if let (Some(a), Some(b)) = (old_datum, new_datum)
                    && same_raw_varlena(a, b)
                {
                    continue;
                }
                let prev = column_heads(old_datum);
                let heads = column_heads(new_datum);
                if prev == heads {
                    continue;
                }
                Column {
                    name: name.clone(),
                    heads: Some(heads),
                    prev_heads: Some(prev),
                }
            }
        };
        columns.push(column);
    }

    // An UPDATE that changed no heads notifies only for a key change;
    // identical raw key values (the usual case) need no JSON.
    if op == Op::Update
        && columns.is_empty()
        && key_cols
            .iter()
            .all(|a| same_raw_datum(get(old, a.attnum), get(new, a.attnum), a))
    {
        return;
    }
    let (key, old_key) = match op {
        Op::Insert => (key_of(new), None),
        Op::Delete => (key_of(old), None),
        Op::Update => {
            let key = key_of(new);
            let old_key = key_of(old);
            (key.clone(), (old_key != key).then_some(old_key))
        }
    };
    if op == Op::Update && columns.is_empty() && old_key.is_none() {
        return; // nothing a listener needs to know about
    }
    let payload = Event {
        table: table_name(),
        op,
        seq: next_notify_seq(),
        key,
        old_key,
        columns,
    }
    .payload(am::notify::MAX_PAYLOAD);
    send_notification(channel, &payload);
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
