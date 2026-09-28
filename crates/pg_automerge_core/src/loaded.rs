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
use std::sync::atomic::{AtomicUsize, Ordering};

use automerge::{Automerge, Change, ChangeHash};

use crate::{
    Applied, Error, apply_changes, contains_changes_by_heads, ensure_complete, guard_for,
    guard_input, guard_stored, has_all, header, is_subset, load_stored_unguarded,
    missing_deps_error, reload_check, stored_heads_unguarded,
};

/// Number of [`LoadedDoc`]s alive in this process (for leak tests).
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// How many [`LoadedDoc`]s exist right now in this process.
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
    pub fn from_stored(bytes: &[u8]) -> Result<Self, Error> {
        guard_stored(|| {
            Ok(Self::new(
                load_stored_unguarded(bytes)?,
                Some(bytes.to_vec()),
                false,
            ))
        })
    }

    /// A document built in memory (its stored bytes are computed when
    /// needed). `unverified`: it contains external changes, see
    /// [`LoadedDoc::stored`].
    pub fn from_doc(doc: Automerge, unverified: bool) -> Result<Self, Error> {
        guard_stored(|| Ok(Self::new(doc, None, unverified)))
    }

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
    /// needed (to store or send the value) rather than at every merge.
    pub fn stored(&self) -> Result<&[u8], Error> {
        if let Some(bytes) = self.stored.get() {
            return Ok(bytes);
        }
        let bytes = guard_for(self.unverified, || {
            let saved = self.doc.save_nocompress();
            if self.unverified {
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
    Stored(&'a [u8]),
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
    pub fn heads(&self) -> Result<Cow<'a, [ChangeHash]>, Error> {
        guard_stored(|| self.heads_unguarded())
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
    Left,
    Right,
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
/// - Changes whose dependencies are neither in `a` nor in `changes`:
///   [`Error::InvalidInput`] naming the missing hashes. Nothing orphaned is
///   ever stored.
/// - Malformed bytes: [`Error::InvalidInput`], including decoder panics.
///
/// Bare uncompressed change chunks (what `save_incremental()` /
/// `save_after()` produce) are parsed one by one (every chunk must parse:
/// strict, like `Automerge::load`) and applied to `a`'s document (loaded,
/// or cloned when `a` is loaded already), the same steps a load of
/// `a ++ changes` takes. Anything else (a save, compressed chunks) is
/// loaded as `a ++ changes`. Either way the result is marked unverified:
/// its stored bytes get the save-and-load check when first requested.
pub fn merge_changes(a: Input<'_>, changes: &[u8]) -> Result<Option<LoadedDoc>, Error> {
    if changes.is_empty() {
        return Ok(None);
    }
    guard_input(|| {
        let heads_a = a.heads_unguarded()?;
        if contains_changes_by_heads(&heads_a, changes) == Some(true) {
            return Ok(None);
        }
        let doc = match header::change_chunks(changes) {
            Some(chunks) => {
                if let Input::Loaded(loaded) = a
                    && chunks.iter().all(|c| has_change(loaded.doc(), &c.hash))
                {
                    return Ok(None);
                }
                let parsed = chunks
                    .iter()
                    .map(|c| {
                        Change::try_from(&changes[c.range.clone()]).map_err(|e| {
                            Error::InvalidInput(format!("invalid automerge changes: {e}"))
                        })
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
                    return Err(missing_deps_error(&missing));
                }
                if is_subset(&doc.get_heads(), &heads_a) {
                    return Ok(None);
                }
                doc
            }
            None => match apply_changes(&a.bytes_for_load(), &heads_a, changes)? {
                Applied::Unchanged => return Ok(None),
                Applied::MissingDeps(missing) => return Err(missing_deps_error(&missing)),
                Applied::Changed(doc) => *doc,
            },
        };
        Ok(Some(LoadedDoc::new(doc, None, true)))
    })
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
/// change with that hash has exactly those bytes); otherwise by loading
/// `a ++ changes` (one load, no save). Changes whose dependencies are in
/// neither input are not in `a`: `false`, where `merge` raises an error.
/// Malformed input on the loading path is [`Error::InvalidInput`].
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
        Ok(matches!(
            apply_changes(&a.bytes_for_load(), &heads_a, changes)?,
            Applied::Unchanged
        ))
    })
}

/// Whether `a` has every change of `b` (so `merge(a, b)` is `a`):
/// `automerge_contains(a, b)`. Decided from the heads when possible,
/// otherwise from `a`'s history (loading a stored `a`).
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
        match a {
            Input::Loaded(doc) => Ok(has_all(doc.doc(), &heads_b)),
            Input::Stored(bytes) => Ok(has_all(&load_stored_unguarded(bytes)?, &heads_b)),
        }
    })
}

/// Whether `a` has every change of the history ending at `heads` (loads a
/// stored `a`).
pub fn contains_heads(a: Input<'_>, heads: &[ChangeHash]) -> Result<bool, Error> {
    with_doc(a, |doc| Ok(has_all(doc, heads)))
}

/// Run `f` on the document of `input`: in place when loaded, otherwise
/// after loading the stored bytes (errors of `f` on stored values are
/// internal, as for every read of a stored value).
pub fn with_doc<T>(
    input: Input<'_>,
    f: impl FnOnce(&Automerge) -> Result<T, Error>,
) -> Result<T, Error> {
    guard_stored(|| match input {
        Input::Loaded(doc) => f(doc.doc()),
        Input::Stored(bytes) => f(&load_stored_unguarded(bytes)?),
    })
}
