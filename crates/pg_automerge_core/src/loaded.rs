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
use std::cell::{OnceCell, RefCell};
#[cfg(feature = "test-hooks")]
use std::sync::atomic::{AtomicUsize, Ordering};

use automerge::{Automerge, Change, ChangeHash};

use crate::budget::{self, Base, DocCounts, InputCounts, LimitKind};
use crate::{
    Applied, Error, apply_changes, contains_changes_by_heads, ensure_complete, guard_for,
    guard_input, guard_stored, has_all, header, invalid_changes, is_subset, load_bytes,
    load_stored_unguarded, loads_as_saved, normalize_unguarded, reload_check,
    stored_heads_unguarded, verification_enabled,
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
    /// What the document's save describes (see [`budget`]), once known:
    /// exact when scanned from a save, an upper bound for a merge result.
    counts: OnceCell<DocCounts>,
    /// A save of `doc` made to count it exactly, which [`LoadedDoc::stored`]
    /// uses instead of saving again (still with the check of an
    /// unverified document).
    save: RefCell<Option<Vec<u8>>>,
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
            counts: OnceCell::new(),
            save: RefCell::new(None),
        }
    }

    /// Known counts of the document (`None`: computed when needed).
    fn with_counts(self, counts: Option<DocCounts>) -> Self {
        if let Some(counts) = counts {
            let _ = self.counts.set(counts);
        }
        self
    }

    /// A save of the document made while counting it: the stored bytes of
    /// a verified document, otherwise kept for [`LoadedDoc::stored`].
    fn with_save(self, save: Option<Vec<u8>>) -> Self {
        if let Some(save) = save {
            if self.unverified {
                *self.save.borrow_mut() = Some(save);
            } else {
                let _ = self.stored.set(save);
            }
        }
        self
    }

    /// What the document's save describes (see [`budget`]): known, or a
    /// scan of its stored bytes (saving it first if they are not computed
    /// yet; that save is kept). Unguarded.
    pub(crate) fn counts(&self) -> DocCounts {
        *self.counts.get_or_init(|| {
            if let Some(bytes) = self.stored.get() {
                return budget::scan_doc(bytes);
            }
            let mut save = self.save.borrow_mut();
            let saved = save.get_or_insert_with(|| self.doc.save_nocompress());
            budget::scan_doc(saved)
        })
    }

    /// The counts, if already known.
    pub(crate) fn known_counts(&self) -> Option<DocCounts> {
        self.counts.get().copied()
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
            let normalized = normalize_unguarded(bytes)?;
            Ok(Self::new(normalized.doc, Some(normalized.saved), false)
                .with_counts(normalized.counts))
        })
    }

    /// A document just loaded from external bytes `input` on its own
    /// (unguarded, like the load), whose scan found `scanned`. Its save is
    /// computed right away: when [`loads_as_saved`] proves that it loads
    /// back, the document is verified and keeps the save as its stored
    /// bytes; otherwise it is unverified (the check runs when the bytes
    /// are first needed). Either way the save must fit the limit.
    fn from_loaded_input(
        doc: Automerge,
        input: &[u8],
        scanned: &InputCounts,
        inflated: Option<Vec<Vec<u8>>>,
        limit: Option<u64>,
    ) -> Result<Self, Error> {
        let saved = doc.save_nocompress();
        let as_saved = loads_as_saved(input, &saved, inflated.as_deref());
        drop(inflated);
        let counts = match limit {
            Some(_) if as_saved => Some(scanned.first_doc),
            Some(_) => Some(budget::check_saved(&saved, LimitKind::Normalized, limit)?),
            None => None,
        };
        Ok(if as_saved {
            Self::new(doc, Some(saved), false)
        } else {
            Self::new(doc, None, true).with_save(Some(saved))
        }
        .with_counts(counts))
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

    /// [`LoadedDoc::from_doc`] with the document's counts and a save of
    /// it, when known (see [`budget`]).
    pub(crate) fn from_parts(
        doc: Automerge,
        unverified: bool,
        counts: Option<DocCounts>,
        save: Option<Vec<u8>>,
    ) -> Self {
        Self::new(doc, None, unverified)
            .with_counts(counts)
            .with_save(save)
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
            let saved = self
                .save
                .borrow_mut()
                .take()
                .unwrap_or_else(|| self.doc.save_nocompress());
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

    /// What the document's save describes (see [`budget`]): a scan of
    /// stored bytes, or a loaded document's counts. Unguarded.
    fn counts(&self) -> DocCounts {
        match self {
            Input::Stored(bytes) => budget::scan_doc(bytes),
            Input::Loaded(doc) => doc.counts(),
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
    let limit = budget::limit();
    let base = limit.map(|_| a.counts());
    let approx = merge_from(&mut target, base, other, limit)?;
    let (counts, save) = result_counts(&target, approx, limit)?;
    Ok(MergeOutcome::New(Box::new(
        LoadedDoc::new(target, None, unverified)
            .with_counts(counts)
            .with_save(save),
    )))
}

/// Apply every change of `other` that `target` lacks: what
/// `Automerge::merge` does, without needing `other` mutably.
///
/// With a `limit`, the changes are first counted from their chunks and
/// the estimate of applying them to `target` (whose counts are `base`) is
/// checked ([`Error::LoadLimit`]); the result is then an upper bound of
/// the merged document's counts, for [`result_counts`]. `None` without a
/// limit.
pub(crate) fn merge_from(
    target: &mut Automerge,
    base: Option<DocCounts>,
    other: &Automerge,
    limit: Option<u64>,
) -> Result<Option<DocCounts>, Error> {
    let approx = apply_added(target, base, other, limit, |e| {
        crate::automerge_error(e, |e| {
            Error::Internal(format!("could not merge automerge documents: {e}"))
        })
    })?;
    ensure_complete(target).map_err(|missing| {
        Error::Internal(format!(
            "merged automerge document is missing {} dependencies (e.g. {})",
            missing.len(),
            missing[0]
        ))
    })?;
    Ok(approx)
}

/// [`merge_from`] without the completeness check, with the error mapping
/// of the caller.
fn apply_added(
    target: &mut Automerge,
    base: Option<DocCounts>,
    other: &Automerge,
    limit: Option<u64>,
    map_err: impl FnOnce(automerge::AutomergeError) -> Error,
) -> Result<Option<DocCounts>, Error> {
    let changes = target.get_changes_added(other);
    let approx = match (limit, base) {
        (Some(_), Some(base)) => {
            let delta = budget::scan_changes(changes.iter().map(Change::raw_bytes));
            budget::check_changes(&delta, Base::from(&base), limit)?;
            Some(base.plus_changes(&delta))
        }
        _ => None,
    };
    target.apply_changes(changes).map_err(map_err)?;
    Ok(approx)
}

/// The counts of a document built by merging, from `approx`, an upper
/// bound of them (`None` without a limit): kept when their estimate fits
/// `limit`; otherwise the document is saved and counted exactly, and
/// rejected ([`Error::LoadLimit`], a merged document) if that does not fit
/// either. Returns the counts and the save, if one was made.
pub(crate) fn result_counts(
    doc: &Automerge,
    approx: Option<DocCounts>,
    limit: Option<u64>,
) -> Result<(Option<DocCounts>, Option<Vec<u8>>), Error> {
    let (Some(bound), Some(approx)) = (limit, approx) else {
        return Ok((None, None));
    };
    if budget::doc_estimate(&approx) <= bound {
        return Ok((Some(approx), None));
    }
    let saved = doc.save_nocompress();
    let exact = budget::check_saved(&saved, LimitKind::Merged, limit)?;
    Ok((Some(exact), Some(saved)))
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
        let limit = budget::limit();
        let mut scanned = budget::scan_input_keep(changes, limit);
        budget::check_no_bundle(&scanned)?;
        if let Some(chunks) = header::change_chunks(changes) {
            return apply_change_chunks(a, &heads_a, changes, &chunks, &scanned, limit);
        }
        if header::starts_with_document(changes) {
            match merge_save(a, &heads_a, changes, &mut scanned, limit)? {
                SaveMerge::Unchanged => return Ok(None),
                SaveMerge::New(doc) => return Ok(Some(*doc)),
                SaveMerge::Concatenate => {}
            }
        }
        let base = limit.map(|_| a.counts());
        if let Some(base) = &base {
            budget::check_apply_input(&scanned, changes, Base::from(base), limit)?;
        }
        match apply_changes(&a.bytes_for_load(), &heads_a, changes)? {
            Applied::Unchanged => Ok(None),
            Applied::MissingDeps(missing) => Err(Error::MissingDependencies(missing)),
            Applied::Changed(doc) => {
                let approx = base.map(|base| base.plus_changes(&scanned.as_changes()));
                let (counts, save) = result_counts(&doc, approx, limit)?;
                Ok(Some(
                    LoadedDoc::new(*doc, None, true)
                        .with_counts(counts)
                        .with_save(save),
                ))
            }
        }
    })
}

/// [`merge_changes`] for bare change chunks (unguarded).
fn apply_change_chunks(
    a: Input<'_>,
    heads_a: &[ChangeHash],
    changes: &[u8],
    chunks: &[header::ChangeChunk],
    scanned: &InputCounts,
    limit: Option<u64>,
) -> Result<Option<LoadedDoc>, Error> {
    if let Input::Loaded(loaded) = a
        && chunks.iter().all(|c| has_change(loaded.doc(), &c.hash))
    {
        return Ok(None);
    }
    let base = limit.map(|_| a.counts());
    if let Some(base) = &base {
        budget::check_apply_input(scanned, changes, Base::from(base), limit)?;
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
    doc.apply_changes(parsed).map_err(invalid_changes)?;
    if let Err(mut missing) = ensure_complete(&doc) {
        missing.sort();
        return Err(Error::MissingDependencies(missing));
    }
    if is_subset(&doc.get_heads(), heads_a) {
        return Ok(None);
    }
    let approx = base.map(|base| base.plus_changes(&scanned.as_changes()));
    let (counts, save) = result_counts(&doc, approx, limit)?;
    Ok(Some(
        LoadedDoc::new(doc, None, true)
            .with_counts(counts)
            .with_save(save),
    ))
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
///    are all in `a` (heads of `a`, or changes of a loaded `a`) and that
///    passes Automerge's chunk parse ([`header::document_parses`]): `a`
///    unchanged, nothing loaded. This is what a load of `a ++ changes`
///    decides: it parses such a chunk and skips only reconstructing its
///    changes (see [`header::document_chunk`]). A chunk that does not
///    parse goes on to step 3, whose load rejects it.
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
fn merge_save(
    a: Input<'_>,
    heads_a: &[ChangeHash],
    changes: &[u8],
    scanned: &mut InputCounts,
    limit: Option<u64>,
) -> Result<SaveMerge, Error> {
    let chunk = header::document_chunk(changes);
    // Whether the chunk parses (from the scan), and parsing it fits the
    // limit, checked only before a header answer.
    let parses = || -> Result<bool, Error> {
        if scanned.single_doc_parses {
            budget::check_parse(scanned, limit)?;
        }
        Ok(scanned.single_doc_parses)
    };
    if let Some(chunk) = &chunk {
        let known = is_subset(&chunk.heads, heads_a)
            || matches!(a, Input::Loaded(loaded) if has_all(loaded.doc(), &chunk.heads));
        if known && parses()? {
            return Ok(SaveMerge::Unchanged);
        }
    }
    let mut doc_a = None;
    if let (Input::Stored(bytes), Some(chunk)) = (a, &chunk)
        && let (Some(count), Some(count_a)) =
            (chunk.change_count, header::change_count_from_bytes(bytes))
        && count <= count_a
        && parses()?
    {
        let doc = load_stored_unguarded(bytes)?;
        if has_all(&doc, &chunk.heads) {
            return Ok(SaveMerge::Unchanged);
        }
        doc_a = Some(doc);
    }
    budget::check_load(scanned, changes, limit)?;
    let b = load_bytes(changes).map_err(invalid_changes)?;
    // The scan's inflated columns (of a compressed save), held through
    // the load for the comparison of `from_loaded_input`; dropped on every
    // other path.
    let inflated = scanned.inflated_columns.take();
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
            b, changes, scanned, inflated, limit,
        )?)));
    }
    drop(inflated);
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
    let base = limit.map(|_| a.counts());
    let approx = apply_added(&mut target, base, &b, limit, invalid_changes)?;
    let (counts, save) = result_counts(&target, approx, limit)?;
    Ok(SaveMerge::New(Box::new(
        LoadedDoc::new(target, None, true)
            .with_counts(counts)
            .with_save(save),
    )))
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
/// chunk (a save) by its header's change count (`false` when it lists at
/// least as many changes as a stored `a` has, and other heads), else, if
/// it passes Automerge's chunk parse, by whether `a` has the heads in its
/// header (loading a stored `a`, never the save: what a load of
/// `a ++ changes` decides once the chunk parses, see
/// [`header::document_chunk`]); otherwise by loading
/// `a ++ changes` (one load, no
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
        let limit = budget::limit();
        let scanned = budget::scan_input(changes, limit);
        budget::check_no_bundle(&scanned)?;
        // One document chunk (a save): whether `a` has its heads, as a
        // load of `a ++ changes` decides it once the chunk parses, without
        // reconstructing the save; a
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
            // A save that does not parse is left to the load below, which
            // rejects it.
            if scanned.single_doc_parses {
                budget::check_parse(&scanned, limit)?;
                return Ok(is_subset(&chunk.heads, &heads_a)
                    || match a {
                        Input::Loaded(loaded) => has_all(loaded.doc(), &chunk.heads),
                        Input::Stored(bytes) => {
                            has_all(&load_stored_unguarded(bytes)?, &chunk.heads)
                        }
                    });
            }
        }
        if limit.is_some() {
            budget::check_apply_input(&scanned, changes, Base::from(&a.counts()), limit)?;
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
