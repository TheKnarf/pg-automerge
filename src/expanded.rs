//! Expanded `automerge` values: loaded documents kept in memory between
//! function calls through Postgres' expanded-object protocol.

use pg_automerge_core::Error;
use pg_automerge_core::loaded::LoadedDoc;
use pgrx::prelude::*;

use crate::error::{OrRaise, raise};

// ---------------------------------------------------------------------------
// Expanded values
// ---------------------------------------------------------------------------
//
// An expanded `automerge` value is a loaded document kept in memory between
// function calls (Postgres' expanded-object protocol, utils/expandeddatum.h),
// so that `merge(merge(a, b), c)`, `doc := merge(doc, x)` in a PL/pgSQL loop
// or `merge(..)::jsonb` do not save and re-load the document at every step.
// See docs/DESIGN.md, "Expanded values".
//
// - The object lives in its own memory context, a child of the context the
//   function was called in; Postgres moves or deletes that context with the
//   value. The Rust document is owned by the object and dropped by a reset
//   callback of that context, so it is freed exactly when the object is.
// - Flattening (storing the value in a tuple, sending it, casting it to
//   bytea) writes the document's stored bytes: `save_nocompress()`, computed
//   once and cached in the `LoadedDoc`, and loaded back once first when the
//   document contains changes from a bytea (the input safeguard). The
//   document of an object is never modified, only replaced as a whole, so the
//   cache cannot go stale.
// - Only a read-write pointer allows replacing the document (in place);
//   functions given a read-only pointer return a new object.

/// Merges that replaced the document of a read-write argument in place (for
/// the pg_tests, which check that PL/pgSQL's in-place paths are taken).
#[cfg(any(test, feature = "pg_test"))]
pub(crate) static IN_PLACE_MERGES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Our expanded object: the standard header, then the document.
#[repr(C)]
pub struct ExpandedAutomerge {
    header: pg_sys::ExpandedObjectHeader,
    /// Owned (`Box::into_raw`); null once the reset callback dropped it.
    doc: *mut LoadedDoc,
    /// Registered on the object's memory context; drops `doc`.
    callback: pg_sys::MemoryContextCallback,
}

static EXPANDED_METHODS: pg_sys::ExpandedObjectMethods = pg_sys::ExpandedObjectMethods {
    get_flat_size: Some(expanded_get_flat_size),
    flatten_into: Some(expanded_flatten_into),
};

/// If `datum` points to one of our expanded objects: the object and whether
/// the pointer is read-write.
///
/// # Safety
///
/// `datum` must be a non-null varlena datum.
pub(crate) unsafe fn expanded_object(
    datum: pg_sys::Datum,
) -> Option<(*mut ExpandedAutomerge, bool)> {
    let ptr = datum.cast_mut_ptr::<pg_sys::varlena>();
    // SAFETY: a varlena; an external (1-byte 0x01 header) one has its tag
    // in the second byte (varattrib_1b_e).
    unsafe {
        if !pgrx::varlena::varatt_is_1b_e(ptr) {
            return None;
        }
        let tag = u32::from(*ptr.cast::<u8>().add(1));
        let read_write = match tag {
            pg_sys::vartag_external::VARTAG_EXPANDED_RW => true,
            pg_sys::vartag_external::VARTAG_EXPANDED_RO => false,
            _ => return None,
        };
        let header = pg_sys::DatumGetEOHP(datum);
        // Only this extension creates expanded automerge values; anything
        // else would be flattened through its own methods by a detoast.
        if !std::ptr::eq((*header).eoh_methods, &EXPANDED_METHODS) {
            return None;
        }
        Some((header.cast::<ExpandedAutomerge>(), read_write))
    }
}

/// The document of a live expanded object.
///
/// # Safety
///
/// `object` must be a live object created by [`new_expanded`], and no
/// document replacement may happen while the returned reference is used.
pub(crate) unsafe fn expanded_doc<'a>(object: *mut ExpandedAutomerge) -> &'a LoadedDoc {
    // SAFETY: per the contract.
    let doc = unsafe { (*object).doc };
    if doc.is_null() {
        raise(Error::Internal(
            "expanded automerge value used after it was freed".into(),
        ));
    }
    // SAFETY: non-null, owned by the object.
    unsafe { &*doc }
}

/// Replace the document of an expanded object (the old one is dropped).
///
/// # Safety
///
/// `object` must be live, reached through a read-write pointer, and no
/// reference into its current document may be alive.
pub(crate) unsafe fn replace_in_place(object: *mut ExpandedAutomerge, doc: LoadedDoc) {
    // SAFETY: per the contract.
    unsafe {
        let old = (*object).doc;
        if old.is_null() {
            raise(Error::Internal(
                "expanded automerge value used after it was freed".into(),
            ));
        }
        // Assigning drops the old document; the cached stored bytes go
        // with it.
        *old = doc;
    }
}

/// A new expanded object holding `doc`, in a memory context that is a child
/// of the current one; returns its read-write pointer (as functions must
/// for new expanded objects).
pub(crate) fn new_expanded(doc: LoadedDoc) -> pg_sys::Datum {
    // SAFETY: standard expanded-object setup (see array's
    // expand_array()). Allocation errors before the document is moved into
    // the object unwind normally and drop `doc`; after that, nothing can
    // fail before the callback that frees it is registered.
    unsafe {
        let context = pg_sys::AllocSetContextCreateInternal(
            pg_sys::CurrentMemoryContext,
            c"automerge expanded document".as_ptr(),
            pg_sys::ALLOCSET_SMALL_MINSIZE as usize,
            pg_sys::ALLOCSET_SMALL_INITSIZE as usize,
            pg_sys::ALLOCSET_SMALL_MAXSIZE as usize,
        );
        let object = pg_sys::MemoryContextAllocZero(context, size_of::<ExpandedAutomerge>())
            .cast::<ExpandedAutomerge>();
        pg_sys::EOH_init_header(&raw mut (*object).header, &EXPANDED_METHODS, context);
        (*object).doc = Box::into_raw(Box::new(doc));
        (*object).callback.func = Some(expanded_drop_doc);
        (*object).callback.arg = object.cast();
        pg_sys::MemoryContextRegisterResetCallback(context, &raw mut (*object).callback);
        pg_sys::Datum::from((*object).header.eoh_rw_ptr.as_mut_ptr())
    }
}

/// Reset callback of an object's memory context: drop the document.
#[pg_guard]
unsafe extern "C-unwind" fn expanded_drop_doc(arg: *mut std::ffi::c_void) {
    let object = arg.cast::<ExpandedAutomerge>();
    // SAFETY: registered by new_expanded with the object as argument; runs
    // once, before the context's memory is freed.
    unsafe {
        let doc = std::mem::replace(&mut (*object).doc, std::ptr::null_mut());
        if !doc.is_null() {
            drop(Box::from_raw(doc));
        }
    }
}

/// `get_flat_size` method: computes (and caches) the stored bytes. This is
/// where a document with changes from a bytea gets its save-and-load check,
/// so a value that would not load back is an ERROR here and never stored.
#[pg_guard]
unsafe extern "C-unwind" fn expanded_get_flat_size(
    header: *mut pg_sys::ExpandedObjectHeader,
) -> usize {
    // SAFETY: Postgres calls the method with one of our objects.
    let doc = unsafe { expanded_doc(header.cast()) };
    doc.stored().or_raise().len() + pg_sys::VARHDRSZ
}

/// `flatten_into` method: the cached stored bytes as a 4-byte-header
/// varlena, in the space sized by the preceding `get_flat_size` call.
#[pg_guard]
unsafe extern "C-unwind" fn expanded_flatten_into(
    header: *mut pg_sys::ExpandedObjectHeader,
    result: *mut std::ffi::c_void,
    allocated_size: usize,
) {
    // SAFETY: Postgres calls the method with one of our objects and
    // `allocated_size` bytes at `result`.
    unsafe {
        let doc = expanded_doc(header.cast());
        let bytes = doc.stored().or_raise();
        let size = bytes.len() + pg_sys::VARHDRSZ;
        if size != allocated_size {
            raise(Error::Internal(format!(
                "expanded automerge value: flat size changed from {allocated_size} to {size}"
            )));
        }
        let out = result.cast::<u8>();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.add(pg_sys::VARHDRSZ), bytes.len());
        pgrx::varlena::set_varsize_4b(result.cast(), size as i32);
    }
}
