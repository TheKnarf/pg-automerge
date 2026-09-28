//! The Rust types of SQL `automerge` values: [`AutomergeDatum`] (a new flat
//! value), [`AutomergeArg`] (an argument, flat or expanded, not detoasted up
//! front, with the prefix reads of the heads fast path) and
//! [`AutomergeValue`] (a result).

use std::borrow::Cow;

use pg_automerge_core::automerge::ChangeHash;
use pg_automerge_core::header::{self, Prefix};
use pg_automerge_core::loaded::{Input, LoadedDoc};
use pg_automerge_core::{self as am, Error};
use pgrx::callconv::{Arg, ArgAbi, BoxRet, FcInfo};
use pgrx::datum::Datum;
use pgrx::pgrx_sql_entity_graph::metadata::{
    ArgumentError, ReturnsError, ReturnsRef, SqlMappingRef, SqlTranslatable, TypeOrigin,
};
use pgrx::prelude::*;

use crate::error::{OrRaise, raise};
use crate::expanded::{
    ExpandedAutomerge, expanded_doc, expanded_object, new_expanded, replace_in_place,
};

/// A new flat value of the SQL `automerge` type, returned by the input
/// functions and the `bytea` cast: canonical stored bytes (the output of
/// `save_nocompress()`).
///
/// Only construct this from bytes that are already validated and normalized
/// (see [`am::normalize`]); the datum is written to disk as-is. Arguments
/// use [`AutomergeArg`], other results [`AutomergeValue`].
pub struct AutomergeDatum(Vec<u8>);

impl AutomergeDatum {
    /// Validate and normalize bytes from any entry path (text input, binary
    /// receive, the `bytea` cast) into a datum.
    pub(crate) fn from_external(bytes: &[u8]) -> Self {
        Self(am::normalize(bytes).or_raise())
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

/// An `automerge` argument, flat or expanded, *not* detoasted up front.
///
/// - `Flat`: an ordinary varlena datum (inline, compressed or a TOAST
///   pointer). For functions that usually need only the heads, which sit in
///   the first few hundred bytes of a stored value (see
///   `pg_automerge_core::header`), [`AutomergeArg::heads`] fetches just a
///   prefix with `pg_detoast_datum_slice`, which for an out-of-line value
///   reads only the TOAST chunks covering it (and for a compressed one
///   decompresses only that far).
/// - `Expanded`: a pointer to one of our expanded objects (see "Expanded
///   values" below), read-write or read-only; the document is used in
///   place, never flattened to be read.
///
/// The datum is only valid for the current call; this type is only ever a
/// function argument and never stored.
pub enum AutomergeArg {
    Flat(pg_sys::Datum),
    Expanded {
        datum: pg_sys::Datum,
        object: *mut ExpandedAutomerge,
        read_write: bool,
    },
}

impl FromDatum for AutomergeArg {
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        _typoid: pg_sys::Oid,
    ) -> Option<Self> {
        if is_null {
            return None;
        }
        // SAFETY: a non-null datum of type automerge (a varlena).
        Some(match unsafe { expanded_object(datum) } {
            Some((object, read_write)) => Self::Expanded {
                datum,
                object,
                read_write,
            },
            None => Self::Flat(datum),
        })
    }
}

unsafe impl<'fcx> ArgAbi<'fcx> for AutomergeArg {
    unsafe fn unbox_arg_unchecked(arg: Arg<'_, 'fcx>) -> Self {
        let index = arg.index();
        unsafe { arg.unbox_arg_using_from_datum() }
            .unwrap_or_else(|| panic!("argument {index} must not be null"))
    }
}

unsafe impl SqlTranslatable for AutomergeArg {
    const TYPE_IDENT: &'static str = pgrx::pgrx_resolved_type!(AutomergeArg);
    const TYPE_ORIGIN: TypeOrigin = TypeOrigin::ThisExtension;
    const ARGUMENT_SQL: Result<SqlMappingRef, ArgumentError> =
        Ok(SqlMappingRef::literal("automerge"));
    const RETURN_SQL: Result<ReturnsRef, ReturnsError> =
        Ok(ReturnsRef::One(SqlMappingRef::literal("automerge")));
}

impl AutomergeArg {
    /// First prefix fetched; covers the heads of documents with dozens of
    /// actors and heads.
    const FIRST_PREFIX: usize = 4096;

    /// The loaded document of an expanded argument.
    pub(crate) fn loaded(&self) -> Option<&LoadedDoc> {
        match self {
            // SAFETY: the object is alive for the duration of the call (its
            // owner holds it), and nothing mutates it while this shared
            // reference exists: documents are replaced only through
            // `replace_in_place`, which callers use after their last read.
            Self::Expanded { object, .. } => Some(unsafe { expanded_doc(*object) }),
            Self::Flat(_) => None,
        }
    }

    /// Run `f` on this value as a core [`Input`]: the loaded document of an
    /// expanded value, or the detoasted bytes of a flat one.
    pub(crate) fn with_input<T>(&self, f: impl FnOnce(Input<'_>) -> T) -> T {
        match self.loaded() {
            Some(doc) => f(Input::Loaded(doc)),
            None => f(Input::Stored(&self.bytes())),
        }
    }

    /// Length of the stored bytes of a flat value (without the varlena
    /// header), without detoasting.
    pub(crate) fn flat_len(datum: pg_sys::Datum) -> usize {
        // SAFETY: a non-null, non-expanded varlena datum of this call.
        let raw = unsafe { pg_sys::toast_raw_datum_size(datum) };
        raw.saturating_sub(pg_sys::VARHDRSZ)
    }

    /// The first `n` stored bytes of a flat value (fewer if it is shorter).
    pub(crate) fn flat_prefix(datum: pg_sys::Datum, n: usize) -> Vec<u8> {
        let count = i32::try_from(n).unwrap_or(i32::MAX);
        // SAFETY: a non-null, non-expanded varlena datum of this call. The
        // slice is a fresh palloc'd, 4-byte-header varlena, copied out and
        // freed.
        unsafe {
            let datum = datum.cast_mut_ptr::<pg_sys::varlena>();
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

    /// All stored bytes: detoasted and copied for a flat value; for an
    /// expanded one its stored bytes (saved once and cached in the object).
    pub(crate) fn bytes(&self) -> Cow<'_, [u8]> {
        match self {
            // SAFETY: a non-null varlena datum of this call.
            Self::Flat(datum) => Cow::Owned(
                unsafe { Vec::<u8>::from_polymorphic_datum(*datum, false, pg_sys::InvalidOid) }
                    .expect("not null"),
            ),
            Self::Expanded { .. } => {
                Cow::Borrowed(self.loaded().expect("expanded").stored().or_raise())
            }
        }
    }

    /// Run a header parser on as short a prefix of a flat value as possible
    /// (4 kB first, then growing to what the parser asks for, at least
    /// doubling). `None` if the value is not in the shape the parser
    /// expects (or is expanded), so the caller must use the document.
    pub(crate) fn read_prefix<T>(&self, parse: impl Fn(&[u8], usize) -> Prefix<T>) -> Option<T> {
        let Self::Flat(datum) = *self else {
            return None;
        };
        let total = Self::flat_len(datum);
        let mut want = total.min(Self::FIRST_PREFIX);
        loop {
            let prefix = Self::flat_prefix(datum, want);
            match parse(&prefix, total) {
                Prefix::Found(value) => return Some(value),
                Prefix::NeedMore(n) if prefix.len() == want && want < total => {
                    want = n.max(want.saturating_mul(2)).min(total);
                }
                Prefix::NeedMore(_) | Prefix::NotSingleDoc => return None,
            }
        }
    }

    /// The heads: from memory for an expanded value; for a flat one read
    /// from as short a prefix as possible, with a full load only if the
    /// value is not a single document chunk.
    pub(crate) fn heads(&self) -> Result<Vec<ChangeHash>, Error> {
        if let Some(doc) = self.loaded() {
            return Ok(doc.heads().to_vec());
        }
        match self.read_prefix(header::heads_from_prefix) {
            Some(heads) => Ok(heads),
            None => am::stored_heads(&self.bytes()),
        }
    }

    /// Whether the value has nothing that is not already in `since`: every
    /// head of the value is in `since`. Reads only the heads.
    pub(crate) fn nothing_since(&self, since: &[ChangeHash]) -> bool {
        am::history::nothing_since(&self.heads().or_raise(), since)
    }

    /// Whether this argument and `other` are the same expanded object
    /// (through a read-write and a read-only pointer, say).
    pub(crate) fn same_object(&self, other: &AutomergeArg) -> bool {
        match (self, other) {
            (Self::Expanded { object: a, .. }, Self::Expanded { object: b, .. }) => a == b,
            _ => false,
        }
    }

    /// The detoasted bytes of a flat value (`None` for an expanded one), to
    /// be used through [`AutomergeArg::input`] and
    /// [`AutomergeArg::into_value`] so that a flat value is detoasted once.
    pub(crate) fn detoasted(&self) -> Option<Vec<u8>> {
        match self {
            Self::Flat(_) => Some(self.bytes().into_owned()),
            Self::Expanded { .. } => None,
        }
    }

    /// This value as a core [`Input`], given its [`AutomergeArg::detoasted`]
    /// bytes.
    pub(crate) fn input<'a>(&'a self, detoasted: &'a Option<Vec<u8>>) -> Input<'a> {
        match (detoasted, self.loaded()) {
            (Some(bytes), _) => Input::Stored(bytes),
            (None, Some(doc)) => Input::Loaded(doc),
            (None, None) => raise(Error::Internal(
                "flat automerge value was not detoasted".into(),
            )),
        }
    }

    /// [`AutomergeArg::unchanged`], reusing the detoasted bytes.
    pub(crate) fn into_value(self, detoasted: Option<Vec<u8>>) -> AutomergeValue {
        match detoasted {
            Some(bytes) => AutomergeValue::Bytes(bytes),
            None => self.unchanged(),
        }
    }

    /// The result for "this argument, unchanged": the datum itself for an
    /// expanded value (its owner keeps it alive for as long as the result
    /// can be used, as for any argument passed through), a copy of the
    /// bytes for a flat one (as before expanded values existed).
    pub(crate) fn unchanged(&self) -> AutomergeValue {
        match self {
            Self::Expanded { datum, .. } => AutomergeValue::Datum(*datum),
            Self::Flat(_) => AutomergeValue::Bytes(self.bytes().into_owned()),
        }
    }

    /// The result for a new document `doc` computed from this argument: in
    /// place when this is a read-write expanded pointer (the object's
    /// document is replaced, and its pointer returned), otherwise a new
    /// expanded object.
    ///
    /// Replacing is the only mutation of an expanded object; the new
    /// document was built completely beforehand (from a clone), so a
    /// failure never leaves the object half-modified, as PL/pgSQL's
    /// in-place assignment requires.
    pub(crate) fn with_result(&self, doc: LoadedDoc) -> AutomergeValue {
        match self {
            Self::Expanded {
                datum,
                object,
                read_write: true,
            } => {
                // SAFETY: a read-write pointer grants the right to modify the
                // object; no reference into its old document is alive (the
                // caller's reads are done).
                unsafe { replace_in_place(*object, doc) };
                #[cfg(any(test, feature = "pg_test"))]
                crate::expanded::IN_PLACE_MERGES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                AutomergeValue::Datum(*datum)
            }
            _ => AutomergeValue::Datum(new_expanded(doc)),
        }
    }
}

/// A function result of type `automerge`: stored bytes (a new flat
/// varlena) or a datum that is already an `automerge` value (an argument
/// passed through, or a pointer to an expanded object).
pub enum AutomergeValue {
    Bytes(Vec<u8>),
    Datum(pg_sys::Datum),
}

impl IntoDatum for AutomergeValue {
    fn into_datum(self) -> Option<pg_sys::Datum> {
        match self {
            Self::Bytes(bytes) => bytes.into_datum(),
            Self::Datum(datum) => Some(datum),
        }
    }

    fn type_oid() -> pg_sys::Oid {
        pgrx::regtypein("automerge")
    }
}

unsafe impl BoxRet for AutomergeValue {
    unsafe fn box_into<'fcx>(self, fcinfo: &mut FcInfo<'fcx>) -> Datum<'fcx> {
        unsafe { fcinfo.return_optional_datum(self.into_datum()) }
    }
}

unsafe impl SqlTranslatable for AutomergeValue {
    const TYPE_IDENT: &'static str = pgrx::pgrx_resolved_type!(AutomergeValue);
    const TYPE_ORIGIN: TypeOrigin = TypeOrigin::ThisExtension;
    const ARGUMENT_SQL: Result<SqlMappingRef, ArgumentError> =
        Ok(SqlMappingRef::literal("automerge"));
    const RETURN_SQL: Result<ReturnsRef, ReturnsError> =
        Ok(ReturnsRef::One(SqlMappingRef::literal("automerge")));
}
