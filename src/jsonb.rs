//! Building `jsonb` directly from the core JSON walk, without a detour
//! through JSON text.
//!
//! [`JsonbBuilder`] is a [`JsonSink`] that assembles the in-memory
//! `JsonbValue` tree `jsonb_in` builds while it parses text (the tree
//! `pushJsonbValue` accumulates) and finishes with one `JsonbValueToJsonb`,
//! so the result is byte for byte what `jsonb_in` gives for the JSON text of
//! the walk (pg_tests check this). The tree is assembled in Rust memory
//! rather than through `pushJsonbValue` (one guarded FFI call and a few
//! pallocs per event): element and pair arrays are allocated once per
//! container at its exact size, string bytes go to a chunked arena, and
//! object pairs are sorted and de-duplicated exactly as
//! `uniqueifyJsonbObject` does (by length, then bytes; of equal keys the
//! last one wins). Everything is freed when the builder is dropped, after
//! `JsonbValueToJsonb` copied it. The scalars:
//!
//! - strings and keys: `jsonb_in`'s length limit;
//! - `i64`: `int64_to_numeric`, the same numeric `numeric_in` makes of its
//!   decimal digits;
//! - `u64` above `i64::MAX` and floats: `numeric_in` on the text
//!   `serde_json` writes for them (shortest round-trip form for floats),
//!   exactly as `jsonb_in` would parse that text;
//! - element and pair counts: `pushJsonbValue`'s limits and errors.
//!
//! Postgres ERRORs inside these calls (out of memory, the jsonb size
//! limits) unwind through the walk as pgrx panics, which the core's guard
//! passes on untouched; the Rust memory is freed on the way.

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

/// `jsonb_in`'s limit on the length of a string or key
/// (`JENTRY_OFFLENMASK`).
const MAX_STRING_LEN: usize = 0x0FFF_FFFF;

/// `MaxAllocSize`, which bounds `pushJsonbValue`'s element and pair arrays.
const MAX_ALLOC_SIZE: usize = 0x3FFF_FFFF;

/// `JB_CMASK`: the largest count a jsonb container header holds.
const JB_CMASK: usize = 0x0FFF_FFFF;

/// `pushJsonbValue`'s `JSONB_MAX_ELEMS`.
const MAX_ELEMS: usize = {
    let n = MAX_ALLOC_SIZE / size_of::<pg_sys::JsonbValue>();
    if n < JB_CMASK { n } else { JB_CMASK }
};

/// `pushJsonbValue`'s `JSONB_MAX_PAIRS`.
const MAX_PAIRS: usize = {
    let n = MAX_ALLOC_SIZE / size_of::<pg_sys::JsonbPair>();
    if n < JB_CMASK { n } else { JB_CMASK }
};

/// Size of a string arena chunk (larger strings get a chunk of their own).
const CHUNK: usize = 64 * 1024;

/// Append-only storage for string bytes whose addresses never change: a
/// chunk is never grown past the capacity it was allocated with.
#[derive(Default)]
struct StringArena {
    chunks: Vec<Vec<u8>>,
}

impl StringArena {
    /// A copy of `s` that stays at the same address until the arena is
    /// dropped.
    fn copy(&mut self, s: &str) -> *const u8 {
        let fits = self
            .chunks
            .last()
            .is_some_and(|c| c.capacity() - c.len() >= s.len());
        if !fits {
            self.chunks.push(Vec::with_capacity(s.len().max(CHUNK)));
        }
        let chunk = self.chunks.last_mut().expect("pushed above");
        let start = chunk.len();
        // Within capacity: no reallocation, earlier pointers stay valid.
        chunk.extend_from_slice(s.as_bytes());
        chunk[start..].as_ptr()
    }
}

/// A container being filled.
enum Open {
    /// An object: its pairs so far, and the key of the value that comes
    /// next.
    Object {
        pairs: Vec<pg_sys::JsonbPair>,
        key: Option<pg_sys::JsonbValue>,
    },
    /// An array: its elements so far.
    Array(Vec<pg_sys::JsonbValue>),
}

/// A [`JsonSink`] building a `jsonb` value (in the current memory context,
/// by [`JsonbBuilder::finish`]).
#[derive(Default)]
pub struct JsonbBuilder {
    /// Open containers, innermost last.
    open: Vec<Open>,
    /// The finished top-level container.
    root: Option<pg_sys::JsonbValue>,
    /// The element and pair arrays of finished containers: the tree points
    /// into them (boxed slices do not move when the box does).
    values: Vec<Box<[pg_sys::JsonbValue]>>,
    pairs: Vec<Box<[pg_sys::JsonbPair]>>,
    /// The bytes of every string and key in the tree.
    strings: StringArena,
}

/// The bytes of a `jbvString` value built by [`JsonbBuilder::string_value`].
fn string_bytes(value: &pg_sys::JsonbValue) -> &[u8] {
    // SAFETY: only called on keys, which are jbvString values pointing
    // into the builder's arena for `len` bytes.
    unsafe {
        let s = value.val.string;
        std::slice::from_raw_parts(s.val.cast::<u8>(), s.len as usize)
    }
}

/// `uniqueifyJsonbObject`: sort by key (shorter first, then bytewise), and
/// of equal keys keep the one added last.
fn uniqueify(pairs: &mut Vec<pg_sys::JsonbPair>) {
    if pairs.len() < 2 {
        return;
    }
    pairs.sort_unstable_by(|a, b| {
        let (ka, kb) = (string_bytes(&a.key), string_bytes(&b.key));
        ka.len()
            .cmp(&kb.len())
            .then_with(|| ka.cmp(kb))
            .then_with(|| b.order.cmp(&a.order))
    });
    pairs.dedup_by(|later, earlier| string_bytes(&later.key) == string_bytes(&earlier.key));
}

fn limit_exceeded(what: &str, max: usize) -> ! {
    raise(PgError::new(
        PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
        format!("number of jsonb {what} exceeds the maximum allowed ({max})"),
    ))
}

impl JsonbBuilder {
    /// The finished value (every container closed), or `None`.
    pub fn finish(self) -> Option<JsonbDatum> {
        if !self.open.is_empty() {
            return None;
        }
        let mut root = self.root?;
        // SAFETY: `root` is a complete container whose arrays and strings
        // are owned by `self`, alive until after the call, which copies
        // everything into the new jsonb.
        let jsonb = unsafe { pg_sys::JsonbValueToJsonb(&raw mut root) };
        Some(JsonbDatum(pg_sys::Datum::from(jsonb)))
    }

    /// Add a finished value to the innermost open container (a key goes
    /// to an object as the key of its next pair), or make it the root.
    fn add(&mut self, value: pg_sys::JsonbValue) {
        match self.open.last_mut() {
            Some(Open::Array(elems)) => {
                if elems.len() >= MAX_ELEMS {
                    limit_exceeded("array elements", MAX_ELEMS);
                }
                elems.push(value);
            }
            Some(Open::Object { pairs, key }) => {
                let key = key.take().expect("the walk emits a key before each value");
                pairs.push(pg_sys::JsonbPair {
                    key,
                    value,
                    order: pairs.len() as u32,
                });
            }
            None => self.root = Some(value),
        }
    }

    /// A string value (for a key or a scalar) with its bytes copied into
    /// the arena.
    fn string_value(&mut self, s: &str) -> pg_sys::JsonbValue {
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
        let ptr = self.strings.copy(s);
        pg_sys::JsonbValue {
            type_: pg_sys::jbvType::jbvString,
            val: pg_sys::JsonbValue__bindgen_ty_1 {
                string: pg_sys::JsonbValue__bindgen_ty_1__bindgen_ty_1 {
                    len: s.len() as i32,
                    val: ptr.cast_mut().cast(),
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
        self.open.push(Open::Object {
            pairs: Vec::new(),
            key: None,
        });
    }

    fn end_object(&mut self) {
        let Some(Open::Object { mut pairs, .. }) = self.open.pop() else {
            panic!("end_object without an open object");
        };
        uniqueify(&mut pairs);
        let mut pairs = pairs.into_boxed_slice();
        let value = pg_sys::JsonbValue {
            type_: pg_sys::jbvType::jbvObject,
            val: pg_sys::JsonbValue__bindgen_ty_1 {
                object: pg_sys::JsonbValue__bindgen_ty_1__bindgen_ty_3 {
                    nPairs: pairs.len() as i32,
                    pairs: pairs.as_mut_ptr(),
                },
            },
        };
        self.pairs.push(pairs);
        self.add(value);
    }

    fn begin_array(&mut self, len_hint: usize) {
        self.open
            .push(Open::Array(Vec::with_capacity(len_hint.min(MAX_ELEMS))));
    }

    fn end_array(&mut self) {
        let Some(Open::Array(elems)) = self.open.pop() else {
            panic!("end_array without an open array");
        };
        let mut elems = elems.into_boxed_slice();
        let value = pg_sys::JsonbValue {
            type_: pg_sys::jbvType::jbvArray,
            val: pg_sys::JsonbValue__bindgen_ty_1 {
                array: pg_sys::JsonbValue__bindgen_ty_1__bindgen_ty_2 {
                    nElems: elems.len() as i32,
                    elems: elems.as_mut_ptr(),
                    rawScalar: false,
                },
            },
        };
        self.values.push(elems);
        self.add(value);
    }

    fn key(&mut self, key: &str) {
        let value = self.string_value(key);
        match self.open.last_mut() {
            Some(Open::Object { pairs, key }) => {
                if pairs.len() >= MAX_PAIRS {
                    limit_exceeded("object pairs", MAX_PAIRS);
                }
                *key = Some(value);
            }
            _ => panic!("key outside an object"),
        }
    }

    fn string(&mut self, value: &str) {
        let value = self.string_value(value);
        self.add(value);
    }

    fn int(&mut self, value: i64) {
        // SAFETY: plain conversion, palloc'd result.
        let numeric = unsafe { pg_sys::int64_to_numeric(value) };
        self.add(Self::numeric_value(numeric));
    }

    fn uint(&mut self, value: u64) {
        match i64::try_from(value) {
            Ok(value) => self.int(value),
            Err(_) => self.add(Self::numeric_from_text(&value.to_string())),
        }
    }

    fn float(&mut self, value: f64) {
        // The text serde_json writes for it (what the text path parsed).
        let text = pg_automerge_core::serde_json::Number::from_f64(value)
            .expect("the walk only passes finite floats")
            .to_string();
        self.add(Self::numeric_from_text(&text));
    }

    fn bool(&mut self, value: bool) {
        self.add(pg_sys::JsonbValue {
            type_: pg_sys::jbvType::jbvBool,
            val: pg_sys::JsonbValue__bindgen_ty_1 { boolean: value },
        });
    }

    fn null(&mut self) {
        self.add(pg_sys::JsonbValue {
            type_: pg_sys::jbvType::jbvNull,
            // SAFETY: an all-zero union is valid; jbvNull reads none of it.
            val: unsafe { std::mem::zeroed() },
        });
    }
}
