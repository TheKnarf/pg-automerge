//! Building `jsonb` directly from the core JSON walk, without a detour
//! through JSON text.
//!
//! [`JsonbBuilder`] is a [`JsonSink`] that feeds `pushJsonbValue` and
//! finishes with `JsonbValueToJsonb`: the same calls `jsonb_in` makes while
//! it parses text, with the same values, so the result is byte for byte
//! what `jsonb_in` gives for the JSON text of the walk (pg_tests check
//! this). The scalars:
//!
//! - strings and keys: copied into palloc'd memory (the parse state keeps
//!   pointers to them until `JsonbValueToJsonb`), with `jsonb_in`'s length
//!   limit;
//! - `i64`: `int64_to_numeric`, the same numeric `numeric_in` makes of its
//!   decimal digits;
//! - `u64` above `i64::MAX` and floats: `numeric_in` on the text
//!   `serde_json` writes for them (shortest round-trip form for floats),
//!   exactly as `jsonb_in` would parse that text.
//!
//! Postgres ERRORs inside these calls (out of memory, the jsonb size
//! limits) unwind through the walk as pgrx panics, which the core's guard
//! passes on untouched.

use std::ffi::CString;

use pg_automerge_core::json::JsonSink;
use pgrx::callconv::{BoxRet, FcInfo};
use pgrx::datum::Datum;
use pgrx::pgrx_sql_entity_graph::metadata::{
    ArgumentError, ReturnsError, ReturnsRef, SqlMappingRef, SqlTranslatable, TypeOrigin,
};
use pgrx::prelude::*;

use crate::error::{PgError, raise};

/// A `jsonb` result: a datum built by [`JsonbBuilder`].
pub struct JsonbDatum(pg_sys::Datum);

impl IntoDatum for JsonbDatum {
    fn into_datum(self) -> Option<pg_sys::Datum> {
        Some(self.0)
    }

    fn type_oid() -> pg_sys::Oid {
        pg_sys::JSONBOID
    }
}

unsafe impl BoxRet for JsonbDatum {
    unsafe fn box_into<'fcx>(self, fcinfo: &mut FcInfo<'fcx>) -> Datum<'fcx> {
        unsafe { fcinfo.return_optional_datum(self.into_datum()) }
    }
}

unsafe impl SqlTranslatable for JsonbDatum {
    const TYPE_IDENT: &'static str = pgrx::pgrx_resolved_type!(JsonbDatum);
    const TYPE_ORIGIN: TypeOrigin = TypeOrigin::External;
    const ARGUMENT_SQL: Result<SqlMappingRef, ArgumentError> = Ok(SqlMappingRef::literal("jsonb"));
    const RETURN_SQL: Result<ReturnsRef, ReturnsError> =
        Ok(ReturnsRef::One(SqlMappingRef::literal("jsonb")));
}

/// A [`JsonSink`] building a `jsonb` value in the current memory context.
pub struct JsonbBuilder {
    state: *mut pg_sys::JsonbParseState,
    /// Whether each open container is an object (values are then
    /// `WJB_VALUE`s, otherwise `WJB_ELEM`s).
    open: Vec<bool>,
    /// What the last `pushJsonbValue` returned: the finished top-level
    /// container once the last one is closed.
    last: *mut pg_sys::JsonbValue,
}

impl Default for JsonbBuilder {
    fn default() -> Self {
        Self {
            state: std::ptr::null_mut(),
            open: Vec::new(),
            last: std::ptr::null_mut(),
        }
    }
}

/// `jsonb_in`'s limit on the length of a string or key
/// (`JENTRY_OFFLENMASK`).
const MAX_STRING_LEN: usize = 0x0FFF_FFFF;

impl JsonbBuilder {
    /// The finished value (every container closed), or `None`.
    pub fn finish(self) -> Option<JsonbDatum> {
        if !self.open.is_empty() || self.last.is_null() {
            return None;
        }
        // SAFETY: `last` is the complete container pushJsonbValue returned
        // for the final end token; converting it copies everything.
        let jsonb = unsafe { pg_sys::JsonbValueToJsonb(self.last) };
        Some(JsonbDatum(pg_sys::Datum::from(jsonb)))
    }

    fn push(&mut self, token: pg_sys::JsonbIteratorToken::Type, value: *mut pg_sys::JsonbValue) {
        // SAFETY: `state` is null or the parse state built by earlier
        // pushes; `value` is null (container tokens) or a valid scalar
        // whose string data lives in palloc'd memory until the end.
        self.last = unsafe { pg_sys::pushJsonbValue(&raw mut self.state, token, value) };
    }

    fn scalar(&mut self, mut value: pg_sys::JsonbValue) {
        let token = match self.open.last() {
            Some(true) => pg_sys::JsonbIteratorToken::WJB_VALUE,
            _ => pg_sys::JsonbIteratorToken::WJB_ELEM,
        };
        self.push(token, &raw mut value);
    }

    /// A string value (for a key or a scalar) with its bytes copied into
    /// palloc'd memory.
    fn string_value(s: &str) -> pg_sys::JsonbValue {
        if s.len() > MAX_STRING_LEN {
            raise(
                PgError::new(
                    PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
                    "string too long to represent as jsonb string",
                )
                .detail(format!(
                    "Due to an implementation restriction, jsonb strings cannot exceed {MAX_STRING_LEN} bytes."
                )),
            );
        }
        // SAFETY: palloc of at least one byte; the string is copied in.
        let ptr = unsafe {
            let ptr = pg_sys::palloc(s.len().max(1)).cast::<u8>();
            std::ptr::copy_nonoverlapping(s.as_ptr(), ptr, s.len());
            ptr
        };
        pg_sys::JsonbValue {
            type_: pg_sys::jbvType::jbvString,
            val: pg_sys::JsonbValue__bindgen_ty_1 {
                string: pg_sys::JsonbValue__bindgen_ty_1__bindgen_ty_1 {
                    len: s.len() as i32,
                    val: ptr.cast(),
                },
            },
        }
    }

    fn numeric_value(numeric: pg_sys::Numeric) -> pg_sys::JsonbValue {
        pg_sys::JsonbValue {
            type_: pg_sys::jbvType::jbvNumeric,
            val: pg_sys::JsonbValue__bindgen_ty_1 { numeric },
        }
    }

    /// `numeric_in(text)`, as `jsonb_in` parses a JSON number.
    fn numeric_from_text(text: &str) -> pg_sys::JsonbValue {
        let text = CString::new(text).expect("a number has no NUL");
        // SAFETY: numeric_in(cstring, oid, int4) with a valid C string.
        let datum = unsafe {
            pgrx::fcinfo::direct_function_call_as_datum(
                pg_sys::numeric_in,
                &[
                    Some(pg_sys::Datum::from(text.as_ptr())),
                    Some(pg_sys::Datum::from(pg_sys::InvalidOid)),
                    Some(pg_sys::Datum::from(-1i32)),
                ],
            )
        }
        .expect("numeric_in never returns NULL");
        Self::numeric_value(datum.cast_mut_ptr())
    }
}

impl JsonSink for JsonbBuilder {
    fn begin_object(&mut self) {
        self.push(
            pg_sys::JsonbIteratorToken::WJB_BEGIN_OBJECT,
            std::ptr::null_mut(),
        );
        self.open.push(true);
    }

    fn end_object(&mut self) {
        self.open.pop();
        self.push(
            pg_sys::JsonbIteratorToken::WJB_END_OBJECT,
            std::ptr::null_mut(),
        );
    }

    fn begin_array(&mut self, _len_hint: usize) {
        // No preallocation hint: jsonb_in gives none either (the result is
        // the same, this only keeps the two paths alike).
        self.push(
            pg_sys::JsonbIteratorToken::WJB_BEGIN_ARRAY,
            std::ptr::null_mut(),
        );
        self.open.push(false);
    }

    fn end_array(&mut self) {
        self.open.pop();
        self.push(
            pg_sys::JsonbIteratorToken::WJB_END_ARRAY,
            std::ptr::null_mut(),
        );
    }

    fn key(&mut self, key: &str) {
        let mut value = Self::string_value(key);
        self.push(pg_sys::JsonbIteratorToken::WJB_KEY, &raw mut value);
    }

    fn string(&mut self, value: &str) {
        self.scalar(Self::string_value(value));
    }

    fn int(&mut self, value: i64) {
        // SAFETY: plain conversion, palloc'd result.
        let numeric = unsafe { pg_sys::int64_to_numeric(value) };
        self.scalar(Self::numeric_value(numeric));
    }

    fn uint(&mut self, value: u64) {
        match i64::try_from(value) {
            Ok(value) => self.int(value),
            Err(_) => self.scalar(Self::numeric_from_text(&value.to_string())),
        }
    }

    fn float(&mut self, value: f64) {
        // The text serde_json writes for it (what the text path parsed).
        let text = pg_automerge_core::serde_json::Number::from_f64(value)
            .expect("the walk only passes finite floats")
            .to_string();
        self.scalar(Self::numeric_from_text(&text));
    }

    fn bool(&mut self, value: bool) {
        self.scalar(pg_sys::JsonbValue {
            type_: pg_sys::jbvType::jbvBool,
            val: pg_sys::JsonbValue__bindgen_ty_1 { boolean: value },
        });
    }

    fn null(&mut self) {
        self.scalar(pg_sys::JsonbValue {
            type_: pg_sys::jbvType::jbvNull,
            // SAFETY: an all-zero union is valid; jbvNull reads none of it.
            val: unsafe { std::mem::zeroed() },
        });
    }
}
