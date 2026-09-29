//! Documents kept loaded in memory between calls: the Rust side of the
//! extension's expanded `automerge` values (see docs/DESIGN.md, "Expanded
//! values").
//!
//! A [`LoadedDoc`] is an [`Automerge`] document plus its stored bytes
//! (`save_nocompress()`), computed on first request and cached. It is
//! immutable once built: functions that "modify" a document build a new
//! `LoadedDoc` (from a clone, which is cheap next to a load) and the caller
//! may then replace the old one with it. So a failure part way through a
//! merge can never leave a half-modified document behind, and a cached
//! byte string always belongs to exactly the document next to it.
//!
//! [`Input`] lets the merge and containment functions take either stored
//! bytes or a loaded document, so the flat and the expanded paths run the
//! same code.

use std::borrow::Cow;
use std::cell::OnceCell;
#[cfg(feature = "test-hooks")]
use std::sync::atomic::{AtomicUsize, Ordering};

use automerge::{Automerge, Change, ChangeHash};

use crate::{
    Applied, Error, apply_changes, contains_changes_by_heads, ensure_complete, guard_for,
    guard_input, guard_stored, has_all, header, is_subset, load_bytes, load_stored_unguarded,
    loads_as_saved, normalize_unguarded, reload_check, stored_heads_unguarded,
    verification_enabled,
};

/// Number of [`LoadedDoc`]s alive in this process (for leak tests).
#[cfg(feature = "test-hooks")]
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// How many [`LoadedDoc`]s exist right now in this process (only with the
/// `test-hooks` feature, which the extension's pg_tests enable).
#[cfg(feature = "test-hooks")]
pub fn live_count() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// A loaded document and its lazily computed stored bytes.
pub struct LoadedDoc {
    doc: Automerge,
    /// Current heads, sorted.
    heads: Vec<ChangeHash>,
    /// The canonical stored bytes of `doc` once known. Only ever set to
    /// bytes that load back to `doc`'s heads, and never changed afterwards:
    /// `doc` is immutable, so the cache cannot go stale.
    stored: OnceCell<Vec<u8>>,
    /// Whether `doc` contains changes from external input (a `bytea`)
    /// whose save has not been loaded back yet. Stored bytes of such a
    /// document are checked with a load before they are handed out (the
    /// same safeguard as [`crate::normalize`]).
    unverified: bool,
}

#[cfg(feature = "test-hooks")]
impl Drop for LoadedDoc {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

impl LoadedDoc {
    fn new(doc: Automerge, stored: Option<Vec<u8>>, unverified: bool) -> Self {
        let mut heads = doc.get_heads();
        heads.sort_unstable();
        let cell = OnceCell::new();
        if let Some(bytes) = stored {
            let _ = cell.set(bytes);
        }
        #[cfg(feature = "test-hooks")]
        LIVE.fetch_add(1, Ordering::Relaxed);
        Self {
            doc,
            heads,
            stored: cell,
            unverified,
        }
    }

    /// Load a stored value (validated on its way in); its bytes are kept as
    /// the cached stored bytes.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] if the bytes do not load (a corrupt stored value).
    pub fn from_stored(bytes: &[u8]) -> Result<Self, Error> {
        guard_stored(|| {
            Ok(Self::new(
                load_stored_unguarded(bytes)?,
                Some(bytes.to_vec()),
                false,
            ))
        })
    }

    /// Validate and normalize external bytes (text input, binary receive,
    /// the `bytea` cast) exactly as [`crate::normalize`] does, keeping the
    /// loaded document next to its stored bytes, so the value needs
    /// neither a save to be stored nor a load to be read.
    ///
    /// # Errors
    ///
    /// As [`crate::normalize`].
    pub fn from_external(bytes: &[u8]) -> Result<Self, Error> {
        guard_input(|| {
            let (doc, saved) = normalize_unguarded(bytes)?;
            Ok(Self::new(doc, Some(saved), false))
        })
    }

    /// A document just loaded from external bytes `input` on its own
    /// (unguarded, like the load). Its save is computed right away: when
    /// [`loads_as_saved`] proves that it loads back, the document is
    /// verified and keeps the save as its stored bytes; otherwise it is
    /// unverified (the check runs when the bytes are first needed).
    fn from_loaded_input(doc: Automerge, input: &[u8]) -> Self {
        let saved = doc.save_nocompress();
        if loads_as_saved(input, &saved) {
            Self::new(doc, Some(saved), false)
        } else {
            Self::new(doc, None, true)
        }
    }

    /// A document built in memory (its stored bytes are computed when
    /// needed). `unverified`: it contains external changes, see
    /// [`LoadedDoc::stored`].
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] if Automerge fails reading the document.
    pub fn from_doc(doc: Automerge, unverified: bool) -> Result<Self, Error> {
        guard_stored(|| Ok(Self::new(doc, None, unverified)))
    }

    /// The Automerge document (read-only: a `LoadedDoc` never changes).
    pub fn doc(&self) -> &Automerge {
        &self.doc
    }

    /// Current heads, sorted.
    pub fn heads(&self) -> &[ChangeHash] {
        &self.heads
    }

    /// The stored bytes if they have been computed already.
    pub fn cached_stored(&self) -> Option<&[u8]> {
        self.stored.get().map(Vec::as_slice)
    }

    /// Whether the stored bytes still need the save-and-load check.
    pub fn is_unverified(&self) -> bool {
        self.unverified && self.stored.get().is_none()
    }

    /// The stored bytes: `save_nocompress()`, computed once and cached. For
    /// a document with unverified external changes the save is loaded back
    /// once and must keep the heads, otherwise [`Error::InvalidInput`]: the
    /// safeguard of [`crate::normalize`], applied when the bytes are first
    /// needed (to store or send the value) rather than at every merge, and
    /// only while [`crate::verification_enabled`].
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] if a document with unverified external changes
    /// does not survive the save and load; [`Error::Internal`] if saving
    /// fails.
    pub fn stored(&self) -> Result<&[u8], Error> {
        if let Some(bytes) = self.stored.get() {
            return Ok(bytes);
        }
        let bytes = guard_for(self.unverified, || {
            let saved = self.doc.save_nocompress();
            if self.unverified && verification_enabled() {
                reload_check(&saved, &self.heads, "invalid automerge changes")?;
            }
            Ok(saved)
        })?;
        Ok(self.stored.get_or_init(|| bytes))
    }
}

/// An `automerge` argument: stored bytes, or a document already loaded.
#[derive(Clone, Copy)]
pub enum Input<'a> {
    /// Stored bytes (the canonical representation, validated on the way
    /// in).
    Stored(&'a [u8]),
    /// A document kept loaded (an expanded value).
    Loaded(&'a LoadedDoc),
}

impl<'a> Input<'a> {
    /// Current heads (unsorted for stored values).
    fn heads_unguarded(&self) -> Result<Cow<'a, [ChangeHash]>, Error> {
        match self {
            Input::Stored(bytes) => Ok(Cow::Owned(stored_heads_unguarded(bytes)?)),
            Input::Loaded(doc) => Ok(Cow::Borrowed(doc.heads())),
        }
    }

    /// Current heads: from the header of a stored value (no load when it
    /// is a single document chunk), from memory for a loaded one.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] if a stored value has to be loaded and does not
    /// load.
    pub fn heads(&self) -> Result<Cow<'a, [ChangeHash]>, Error> {
        guard_stored(|| self.heads_unguarded())
    }

    /// The number of changes: read from the change actor column of a
    /// stored value (`None` when that column cannot be read that way, see
    /// [`header::change_count_from_bytes`]), from the change graph of a
    /// loaded one. Never loads.
    pub fn change_count(&self) -> Option<u64> {
        match self {
            Input::Stored(bytes) => header::change_count_from_bytes(bytes),
            Input::Loaded(doc) => {
                guard_stored(|| Ok(automerge::ReadDoc::stats(doc.doc()).num_changes)).ok()
            }
        }
    }

    /// The same value: identical bytes or the same loaded document.
    fn same(&self, other: &Input<'_>) -> bool {
        match (self, other) {
            (Input::Stored(a), Input::Stored(b)) => a == b,
            (Input::Loaded(a), Input::Loaded(b)) => std::ptr::eq(*a, *b),
            _ => false,
        }
    }

    /// Whether this is a loaded document with unverified external changes.
    pub(crate) fn unverified(&self) -> bool {
        matches!(self, Input::Loaded(doc) if doc.is_unverified())
    }

    /// Stored bytes, or a fresh save (unverified: callers that need a check
    /// load them strictly anyway).
    fn bytes_for_load(&self) -> Cow<'a, [u8]> {
        match self {
            Input::Stored(bytes) => Cow::Borrowed(bytes),
            Input::Loaded(doc) => match doc.cached_stored() {
                Some(bytes) => Cow::Borrowed(bytes),
                None => Cow::Owned(doc.doc().save_nocompress()),
            },
        }
    }
}

/// Result of [`merge`]: one of the inputs unchanged (it already contains
/// the other), or a new document.
pub enum MergeOutcome {
    /// `a` already contains `b`: the result is `a`.
    Left,
    /// `b` already contains `a`: the result is `b`.
    Right,
    /// A new document with the changes of both.
    New(Box<LoadedDoc>),
}

/// CRDT merge: `a` plus every change of `b` it lacks, on stored or loaded
/// inputs.
///
/// Commutative in state (heads and JSON), not byte for byte: when neither
/// input contains the other, the new document's stored bytes depend on
/// argument order.
///
/// No-op checks, cheapest first: the same value; the heads (headers or
/// memory); the history of a loaded input; the history of stored inputs,
/// larger first (it usually contains the other). The new document is
/// `a`'s document (a clone of it when `a` is loaded) with `b`'s missing
/// changes applied, so its stored bytes do not depend on whether the
/// inputs were stored or loaded.
///
/// # Errors
///
/// [`Error::Internal`] if a stored input does not load or applying the
/// changes fails ([`Error::InvalidInput`] instead when an input holds
/// unverified external changes).
pub fn merge(a: Input<'_>, b: Input<'_>) -> Result<MergeOutcome, Error> {
    if a.same(&b) {
        return Ok(MergeOutcome::Left);
    }
    let unverified = a.unverified() || b.unverified();
    guard_for(unverified, || merge_unguarded(a, b, unverified))
}

fn merge_unguarded(a: Input<'_>, b: Input<'_>, unverified: bool) -> Result<MergeOutcome, Error> {
    let heads_a = a.heads_unguarded()?;
    let heads_b = b.heads_unguarded()?;
    if is_subset(&heads_b, &heads_a) {
        return Ok(MergeOutcome::Left);
    }
    if is_subset(&heads_a, &heads_b) {
        // a ⊊ b by heads, so b contains a (see `contains_by_heads`).
        return Ok(MergeOutcome::Right);
    }
    // Loaded inputs answer for free.
    if let Input::Loaded(doc) = a
        && has_all(doc.doc(), &heads_b)
    {
        return Ok(MergeOutcome::Left);
    }
    if let Input::Loaded(doc) = b
        && has_all(doc.doc(), &heads_a)
    {
        return Ok(MergeOutcome::Right);
    }
    // Load stored inputs, the larger one first (it usually contains the
    // other).
    let mut loaded_a = None;
    let mut loaded_b = None;
    let order = match (a, b) {
        (Input::Stored(x), Input::Stored(y)) if x.len() < y.len() => [false, true],
        _ => [true, false],
    };
    for first_is_a in order {
        let (input, other_heads, slot, wins) = if first_is_a {
            (a, &heads_b, &mut loaded_a, MergeOutcome::Left)
        } else {
            (b, &heads_a, &mut loaded_b, MergeOutcome::Right)
        };
        if let Input::Stored(bytes) = input {
            let doc = load_stored_unguarded(bytes)?;
            if has_all(&doc, other_heads) {
                return Ok(wins);
            }
            *slot = Some(doc);
        }
    }
    let mut target = match (a, loaded_a) {
        (Input::Loaded(doc), _) => doc.doc().clone(),
        (Input::Stored(_), Some(doc)) => doc,
        (Input::Stored(_), None) => unreachable!("stored inputs were loaded above"),
    };
    let other: &Automerge = match (&b, &loaded_b) {
        (Input::Loaded(doc), _) => doc.doc(),
        (Input::Stored(_), Some(doc)) => doc,
        (Input::Stored(_), None) => unreachable!("stored inputs were loaded above"),
    };
    merge_from(&mut target, other)?;
    Ok(MergeOutcome::New(Box::new(LoadedDoc::new(
        target, None, unverified,
    ))))
}

/// Apply every change of `other` that `target` lacks: what
/// `Automerge::merge` does, without needing `other` mutably.
pub(crate) fn merge_from(target: &mut Automerge, other: &Automerge) -> Result<(), Error> {
    let changes = target.get_changes_added(other);
    target
        .apply_changes(changes)
        .map_err(|e| Error::Internal(format!("could not merge automerge documents: {e}")))?;
    ensure_complete(target).map_err(|missing| {
        Error::Internal(format!(
            "merged automerge document is missing {} dependencies (e.g. {})",
            missing.len(),
            missing[0]
        ))
    })
}

/// Apply external bytes to a stored or loaded `a`: `merge(automerge,
/// bytea)`.
///
/// `changes` may be anything Automerge can load incrementally: a full save
/// (compressed or not, optionally followed by change chunks), or bare
/// change chunks (`save_incremental()` / `save_after()` output), several
/// of them concatenated. Chunks are applied on top of `a`, so bare changes
/// may depend on changes `a` already has.
///
/// - Empty `changes`, or nothing new: `None` (use `a` as is).
/// - Changes whose dependencies are neither in `a` nor in `changes`: an
///   error naming the missing hashes. Nothing orphaned is ever stored.
///
/// Three paths, by the shape of `changes`:
///
/// - Bare uncompressed change chunks (what `save_incremental()` /
///   `save_after()` produce) are parsed one by one (every chunk must parse:
///   strict, like `Automerge::load`) and applied to `a`'s document
///   (loaded, or cloned when `a` is loaded already), the same steps a load
///   of `a ++ changes` takes.
/// - A save (see `merge_save`) is treated like `merge(a, b)` of two
///   documents: loaded on its own (strictly, and checked like
///   [`crate::normalize`] input), so that when it contains `a`, as a
///   newer save of the same document does, `a` is never loaded and the
///   result is the save itself.
/// - Anything else (compressed change chunks, a save whose trailing
///   changes depend on `a`) is loaded as `a ++ changes`.
///
/// A result with changes from `changes` in it is marked unverified: its
/// stored bytes get the save-and-load check when first requested (unless
/// they are provably the input's own bytes, see [`crate::normalize`]).
///
/// # Errors
///
/// [`Error::MissingDependencies`] for changes whose dependencies are in
/// neither `a` nor `changes`; [`Error::InvalidInput`] for malformed bytes
/// (including decoder panics) and changes Automerge rejects.
pub fn merge_changes(a: Input<'_>, changes: &[u8]) -> Result<Option<LoadedDoc>, Error> {
    if changes.is_empty() {
        return Ok(None);
    }
    guard_input(|| {
        let heads_a = a.heads_unguarded()?;
        if contains_changes_by_heads(&heads_a, changes) == Some(true) {
            return Ok(None);
        }
        if let Some(chunks) = header::change_chunks(changes) {
            return apply_change_chunks(a, &heads_a, changes, &chunks);
        }
        if header::starts_with_document(changes) {
            match merge_save(a, &heads_a, changes)? {
                SaveMerge::Unchanged => return Ok(None),
                SaveMerge::New(doc) => return Ok(Some(*doc)),
                SaveMerge::Concatenate => {}
            }
        }
        match apply_changes(&a.bytes_for_load(), &heads_a, changes)? {
            Applied::Unchanged => Ok(None),
            Applied::MissingDeps(missing) => Err(Error::MissingDependencies(missing)),
            Applied::Changed(doc) => Ok(Some(LoadedDoc::new(*doc, None, true))),
        }
    })
}

/// [`merge_changes`] for bare change chunks (unguarded).
fn apply_change_chunks(
    a: Input<'_>,
    heads_a: &[ChangeHash],
    changes: &[u8],
    chunks: &[header::ChangeChunk],
) -> Result<Option<LoadedDoc>, Error> {
    if let Input::Loaded(loaded) = a
        && chunks.iter().all(|c| has_change(loaded.doc(), &c.hash))
    {
        return Ok(None);
    }
    let parsed = chunks
        .iter()
        .map(|c| {
            Change::try_from(&changes[c.range.clone()])
                .map_err(|e| Error::InvalidInput(format!("invalid automerge changes: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut doc = match a {
        Input::Stored(bytes) => load_stored_unguarded(bytes)?,
        Input::Loaded(loaded) => loaded.doc().clone(),
    };
    doc.apply_changes(parsed)
        .map_err(|e| Error::InvalidInput(format!("invalid automerge changes: {e}")))?;
    if let Err(mut missing) = ensure_complete(&doc) {
        missing.sort();
        return Err(Error::MissingDependencies(missing));
    }
    if is_subset(&doc.get_heads(), heads_a) {
        return Ok(None);
    }
    Ok(Some(LoadedDoc::new(doc, None, true)))
}

/// Result of [`merge_save`].
enum SaveMerge {
    /// `a` already has everything.
    Unchanged,
    /// The result.
    New(Box<LoadedDoc>),
    /// The save's trailing changes depend on `a`: load `a ++ changes`.
    Concatenate,
}

/// [`merge_changes`] for input starting with a document chunk (a save),
/// like `merge(a, b)` of two documents, cheapest checks first (unguarded):
///
/// 1. Exactly one document chunk with a valid checksum whose header heads
///    are all in `a` (heads of `a`, or changes of a loaded `a`): `a`
///    unchanged, nothing loaded. This is what a load of `a ++ changes`
///    decides from the same header (Automerge skips such a chunk).
/// 2. If the header says the save has no more changes than a stored `a`
///    (an older save, probably contained in `a`), `a` is loaded first and
///    checked for those heads, again as that load would.
/// 3. The input is loaded on its own, strictly; trailing change chunks
///    whose dependencies are missing from it (they may be in `a`) send it
///    to the concatenating path. If it contains `a` (by the heads, or by
///    its history), it is the result: its save is its stored bytes, and
///    needs no check when it is the input's own encoding (a canonical or
///    compressed save, see [`crate::loads_as_saved`]).
/// 4. Otherwise `a`'s document (loaded, or a clone) gets the input's
///    missing changes.
///
/// So a newer full save of the document costs one load (of the save) and
/// no check.
fn merge_save(a: Input<'_>, heads_a: &[ChangeHash], changes: &[u8]) -> Result<SaveMerge, Error> {
    let chunk = header::document_chunk(changes);
    if let Some(chunk) = &chunk {
        if is_subset(&chunk.heads, heads_a) {
            return Ok(SaveMerge::Unchanged);
        }
        if let Input::Loaded(loaded) = a
            && has_all(loaded.doc(), &chunk.heads)
        {
            return Ok(SaveMerge::Unchanged);
        }
    }
    let mut doc_a = None;
    if let (Input::Stored(bytes), Some(chunk)) = (a, &chunk)
        && let (Some(count), Some(count_a)) =
            (chunk.change_count, header::change_count_from_bytes(bytes))
        && count <= count_a
    {
        let doc = load_stored_unguarded(bytes)?;
        if has_all(&doc, &chunk.heads) {
            return Ok(SaveMerge::Unchanged);
        }
        doc_a = Some(doc);
    }
    let b = load_bytes(changes)
        .map_err(|e| Error::InvalidInput(format!("invalid automerge changes: {e}")))?;
    if ensure_complete(&b).is_err() {
        return Ok(SaveMerge::Concatenate);
    }
    let mut heads_b = b.get_heads();
    heads_b.sort_unstable();
    if is_subset(&heads_b, heads_a) {
        return Ok(SaveMerge::Unchanged);
    }
    if is_subset(heads_a, &heads_b) || has_all(&b, heads_a) {
        return Ok(SaveMerge::New(Box::new(LoadedDoc::from_loaded_input(
            b, changes,
        ))));
    }
    let mut target = match (doc_a, a) {
        (Some(doc), _) => doc,
        (None, Input::Stored(bytes)) => load_stored_unguarded(bytes)?,
        (None, Input::Loaded(loaded)) => {
            if has_all(loaded.doc(), &heads_b) {
                return Ok(SaveMerge::Unchanged);
            }
            loaded.doc().clone()
        }
    };
    if has_all(&target, &heads_b) {
        return Ok(SaveMerge::Unchanged);
    }
    let added = target.get_changes_added(&b);
    target
        .apply_changes(added)
        .map_err(|e| Error::InvalidInput(format!("invalid automerge changes: {e}")))?;
    Ok(SaveMerge::New(Box::new(LoadedDoc::new(target, None, true))))
}

fn has_change(doc: &Automerge, hash: &ChangeHash) -> bool {
    doc.get_change_meta_by_hash(hash).is_some()
}

/// Whether `a` already has every change in `changes` (external bytes, as
/// for [`merge_changes`]), i.e. whether `merge(a, changes)` would return
/// `a` unchanged: `automerge_contains(a, changes bytea)`.
///
/// Decided by [`crate::contains_changes_by_heads`] when possible; for a
/// loaded `a` and bare change chunks by a lookup of each chunk's hash (a
/// change with that hash has exactly those bytes); for a single document
/// chunk (a save) by whether `a` has the heads in its header (loading a
/// stored `a`, never the save: what a load of `a ++ changes` decides from
/// the same header); otherwise by loading `a ++ changes` (one load, no
/// save). Changes whose dependencies are in neither input are not in `a`:
/// `false`, where `merge` raises an error.
///
/// # Errors
///
/// [`Error::InvalidInput`] for malformed `changes` on the loading path;
/// [`Error::Internal`] if a stored `a` has to be loaded and does not load.
pub fn contains_changes(a: Input<'_>, changes: &[u8]) -> Result<bool, Error> {
    if changes.is_empty() {
        return Ok(true);
    }
    guard_input(|| {
        let heads_a = a.heads_unguarded()?;
        if let Some(answer) = contains_changes_by_heads(&heads_a, changes) {
            return Ok(answer);
        }
        if let (Input::Loaded(loaded), Some(chunks)) = (a, header::change_chunks(changes)) {
            return Ok(chunks.iter().all(|c| has_change(loaded.doc(), &c.hash)));
        }
        // One document chunk (a save): whether `a` has its heads, as a
        // load of `a ++ changes` decides it, without reading the save; a
        // save listing at least as many changes as a stored `a` has (and
        // other heads) is not in it (see `contains_input_by_header`).
        if let Some(chunk) = header::document_chunk(changes) {
            if let (Input::Stored(_), Some(count_a), Some(count)) =
                (a, a.change_count(), chunk.change_count)
                && count >= count_a
                && !is_subset(&chunk.heads, &heads_a)
            {
                return Ok(false);
            }
            return Ok(is_subset(&chunk.heads, &heads_a)
                || match a {
                    Input::Loaded(loaded) => has_all(loaded.doc(), &chunk.heads),
                    Input::Stored(bytes) => has_all(&load_stored_unguarded(bytes)?, &chunk.heads),
                });
        }
        Ok(matches!(
            apply_changes(&a.bytes_for_load(), &heads_a, changes)?,
            Applied::Unchanged
        ))
    })
}

/// Whether `a` has every change of `b` (so `merge(a, b)` is `a`):
/// `automerge_contains(a, b)`. Decided from the heads when possible,
/// otherwise from `a`'s history (loading a stored `a`).
///
/// # Errors
///
/// [`Error::Internal`] if a stored `a` has to be loaded and does not load.
pub fn contains(a: Input<'_>, b: Input<'_>) -> Result<bool, Error> {
    if a.same(&b) {
        return Ok(true);
    }
    guard_stored(|| {
        let heads_a = a.heads_unguarded()?;
        let heads_b = b.heads_unguarded()?;
        if let Some(answer) = crate::contains_by_heads(&heads_a, &heads_b) {
            return Ok(answer);
        }
        // A stored `a` would have to be loaded: first see whether the
        // change counts decide.
        if let Input::Stored(_) = a
            && let Some(answer) = crate::contains_by_heads_and_counts(
                &heads_a,
                &heads_b,
                a.change_count(),
                b.change_count(),
            )
        {
            return Ok(answer);
        }
        match a {
            Input::Loaded(doc) => Ok(has_all(doc.doc(), &heads_b)),
            Input::Stored(bytes) => Ok(has_all(&load_stored_unguarded(bytes)?, &heads_b)),
        }
    })
}

/// Whether `a` has every change of the history ending at `heads` (loads a
/// stored `a`).
///
/// # Errors
///
/// [`Error::Internal`] if a stored `a` does not load.
pub fn contains_heads(a: Input<'_>, heads: &[ChangeHash]) -> Result<bool, Error> {
    with_doc(a, |doc| Ok(has_all(doc, heads)))
}

/// Run `f` on the document of `input`: in place when loaded, otherwise
/// after loading the stored bytes (errors of `f` on stored values are
/// internal, as for every read of a stored value).
///
/// # Errors
///
/// [`Error::Internal`] if a stored `input` does not load, and the errors of
/// `f`; a panic inside is [`Error::Internal`].
pub fn with_doc<T>(
    input: Input<'_>,
    f: impl FnOnce(&Automerge) -> Result<T, Error>,
) -> Result<T, Error> {
    guard_stored(|| match input {
        Input::Loaded(doc) => f(doc.doc()),
        Input::Stored(bytes) => f(&load_stored_unguarded(bytes)?),
    })
}
