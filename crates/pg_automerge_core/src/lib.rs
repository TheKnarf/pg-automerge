//! Postgres-independent Automerge logic for pg_automerge.
//!
//! Everything here is plain Rust and tested with `cargo test -p pg_automerge_core`;
//! the pgrx crate at the repo root is only glue. See docs/DESIGN.md.
//!
//! "Stored bytes" below always means the canonical representation of the SQL
//! `automerge` type: the output of [`Automerge::save_nocompress`] for a
//! document without queued (dependency-less) changes.

use std::borrow::Cow;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use automerge::{Automerge, AutomergeError, ChangeHash, ReadDoc};

pub mod encoding;
pub mod header;
pub mod json;

pub use automerge;
pub use serde_json;

/// Errors surfaced to SQL. The glue maps [`Error::InvalidInput`] to SQLSTATE
/// 22P02 (invalid_text_representation) and [`Error::Internal`] to XX000.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    InvalidInput(String),
    Internal(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidInput(msg) | Error::Internal(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

/// Run `f`, converting a panic inside it into an error built by `on_panic`
/// from the panic message.
///
/// The automerge crate's decoder is not panic-free: a chunk whose checksum
/// is valid but whose column data is malformed can hit `unwrap`s, index
/// panics and assertions inside `Automerge::load` (and, for a document that
/// did load, possibly later operations on it). pgrx would turn such a panic
/// into an XX000 ERROR anyway, but corrupt client input must be a 22P02
/// "invalid automerge document" error, so every public entry point runs its
/// Automerge calls through this.
///
/// `AssertUnwindSafe` is fine: nothing captured outlives a panic in a usable
/// state that anyone observes. In Postgres the resulting ERROR aborts the
/// statement, which discards e.g. a half-updated `merge_agg` state.
///
/// In a backend, pgrx's panic hook only records the location (no log line),
/// so a caught panic is silent apart from the returned error.
fn guard<T>(
    f: impl FnOnce() -> Result<T, Error>,
    on_panic: impl FnOnce(&str) -> Error,
) -> Result<T, Error> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("unknown panic");
        Err(on_panic(msg))
    })
}

/// `guard` for operations on external input.
fn guard_input<T>(f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    guard(f, |msg| {
        Error::InvalidInput(format!(
            "invalid automerge document: malformed data ({msg})"
        ))
    })
}

/// `guard` for operations on stored (already validated) values.
fn guard_stored<T>(f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    guard(f, |msg| {
        Error::Internal(format!("automerge failed on a stored value: {msg}"))
    })
}

/// Load bytes arriving from outside (text input, binary recv, `bytea` cast).
///
/// Accepts anything `Automerge::load` accepts: a document chunk, change
/// chunks, compressed or not, concatenated. Rejects input whose changes have
/// missing dependencies, so a stored value never carries orphaned changes.
/// Empty input is the empty document.
pub fn load_external(bytes: &[u8]) -> Result<Automerge, Error> {
    guard_input(|| load_external_unguarded(bytes))
}

fn load_external_unguarded(bytes: &[u8]) -> Result<Automerge, Error> {
    let doc = Automerge::load(bytes).map_err(|e| match e {
        AutomergeError::MissingDeps => Error::InvalidInput(
            "invalid automerge document: changes are missing dependencies".into(),
        ),
        e => Error::InvalidInput(format!("invalid automerge document: {e}")),
    })?;
    ensure_complete(&doc).map_err(|missing| {
        Error::InvalidInput(format!(
            "invalid automerge document: changes are missing {} dependencies (e.g. {})",
            missing.len(),
            missing[0]
        ))
    })?;
    Ok(doc)
}

/// Validate and normalize external bytes into stored bytes.
///
/// The save is guarded too: a document that loaded from malformed (but
/// checksummed) input could still trip an assertion when re-encoded.
/// Unless the input already was the canonical encoding, the result is loaded
/// back and must have the same heads. Malformed input with valid checksums
/// can load "successfully" into a document whose re-save does not load
/// (seen in fuzzing: "mismatching heads"); storing that would make the value
/// unreadable forever, so it is rejected here as invalid input instead. This
/// costs a second load on writes of non-canonical (e.g. compressed) saves.
pub fn normalize(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    guard_input(|| {
        let doc = load_external_unguarded(bytes)?;
        let saved = doc.save_nocompress();
        if saved != bytes {
            let reloaded = Automerge::load(&saved).map_err(|e| {
                Error::InvalidInput(format!(
                    "invalid automerge document: does not survive a save and load ({e})"
                ))
            })?;
            if reloaded.get_heads() != doc.get_heads() {
                return Err(Error::InvalidInput(
                    "invalid automerge document: heads change after a save and load".into(),
                ));
            }
        }
        Ok(saved)
    })
}

/// Load already-stored bytes. These were validated on the way in, so failure
/// here means on-disk corruption (or a bug) and is reported as internal.
pub fn load_stored(bytes: &[u8]) -> Result<Automerge, Error> {
    guard_stored(|| load_stored_unguarded(bytes))
}

fn load_stored_unguarded(bytes: &[u8]) -> Result<Automerge, Error> {
    Automerge::load(bytes)
        .map_err(|e| Error::Internal(format!("corrupt stored automerge value: {e}")))
}

/// `Err(missing hashes)` if `doc` holds changes whose dependencies are absent.
fn ensure_complete(doc: &Automerge) -> Result<(), Vec<ChangeHash>> {
    let missing = doc.get_missing_deps(&[]);
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

/// Whether every change in the history ending at `heads` is present in `doc`.
///
/// For a complete document (no queued changes), `get_missing_deps` returns
/// exactly the given heads that `doc` does not have; any change it has also
/// has all of its ancestors.
fn has_all(doc: &Automerge, heads: &[ChangeHash]) -> bool {
    doc.get_missing_deps(heads).is_empty()
}

/// Heads of a stored value: read from the document chunk header when the
/// value is a single document chunk (always, for values this extension
/// stored), otherwise by loading it. See [`header`].
fn stored_heads_unguarded(bytes: &[u8]) -> Result<Vec<ChangeHash>, Error> {
    match header::heads_from_bytes(bytes) {
        Some(heads) => Ok(heads),
        None => Ok(load_stored_unguarded(bytes)?.get_heads()),
    }
}

/// Heads of a stored value (unsorted), without loading it when possible.
pub fn stored_heads(bytes: &[u8]) -> Result<Vec<ChangeHash>, Error> {
    guard_stored(|| stored_heads_unguarded(bytes))
}

/// Whether every hash in `sub` is in `sup`.
fn is_subset(sub: &[ChangeHash], sup: &[ChangeHash]) -> bool {
    if sub.len() * sup.len() <= 256 {
        return sub.iter().all(|h| sup.contains(h));
    }
    let mut sup = sup.to_vec();
    sup.sort_unstable();
    sub.iter().all(|h| sup.binary_search(h).is_ok())
}

/// Decide "does the history with heads `a` contain the one with heads `b`"
/// from the heads alone, when that is possible:
///
/// - `b ⊆ a`: yes (every head of `b` is a change of `a`, and a complete
///   document has all ancestors of its changes).
/// - `a ⊊ b`: no. A head `h` of `b` that is not a head of `a` has no
///   successor in `b`; if `a` had it, it would be an ancestor of some head
///   of `a`, which is also in `b`, so `h` would have a successor in `b`.
/// - otherwise: unknown, the history of `a` must be consulted.
pub fn contains_by_heads(a: &[ChangeHash], b: &[ChangeHash]) -> Option<bool> {
    if is_subset(b, a) {
        Some(true)
    } else if is_subset(a, b) {
        Some(false)
    } else {
        None
    }
}

/// Which bytes `merge` produced. `Left`/`Right` mean one input already
/// contained the other and is returned verbatim without a re-save.
#[derive(Debug, PartialEq, Eq)]
pub enum Merged {
    Left,
    Right,
    New(Vec<u8>),
}

impl Merged {
    pub fn into_bytes<'a>(self, a: &'a [u8], b: &'a [u8]) -> Cow<'a, [u8]> {
        match self {
            Merged::Left => Cow::Borrowed(a),
            Merged::Right => Cow::Borrowed(b),
            Merged::New(bytes) => Cow::Owned(bytes),
        }
    }
}

/// CRDT merge of two stored values.
///
/// Commutative in state (heads and jsonb), not byte for byte: when neither
/// input contains the other, the re-saved bytes depend on argument order.
///
/// No-op checks, cheapest first: identical bytes; heads read from the
/// headers (no load); the history of the larger input (one load); the
/// history of the other.
pub fn merge(a: &[u8], b: &[u8]) -> Result<Merged, Error> {
    // Byte-identical stored values are the same document.
    if a == b {
        return Ok(Merged::Left);
    }
    guard_stored(|| merge_unguarded(a, b))
}

fn merge_unguarded(a: &[u8], b: &[u8]) -> Result<Merged, Error> {
    let heads_a = stored_heads_unguarded(a)?;
    let heads_b = stored_heads_unguarded(b)?;
    if is_subset(&heads_b, &heads_a) {
        return Ok(Merged::Left);
    }
    if is_subset(&heads_a, &heads_b) {
        // a ⊊ b by heads, so b contains a (see `contains_by_heads`).
        return Ok(Merged::Right);
    }
    // Load the larger value first: a document that contains the other has
    // a superset of its changes and ops, so it is usually the larger one,
    // and the containment check then needs only that one load. (Only a
    // guess: correctness does not depend on the order.)
    let a_first = a.len() >= b.len();
    let (first, second) = if a_first { (a, b) } else { (b, a) };
    let (first_heads, second_heads) = if a_first {
        (&heads_a, &heads_b)
    } else {
        (&heads_b, &heads_a)
    };
    let (first_wins, second_wins) = if a_first {
        (Merged::Left, Merged::Right)
    } else {
        (Merged::Right, Merged::Left)
    };
    let doc_first = load_stored_unguarded(first)?;
    if has_all(&doc_first, second_heads) {
        return Ok(first_wins);
    }
    let doc_second = load_stored_unguarded(second)?;
    if has_all(&doc_second, first_heads) {
        return Ok(second_wins);
    }
    let (mut doc_a, mut doc_b) = if a_first {
        (doc_first, doc_second)
    } else {
        (doc_second, doc_first)
    };
    merge_into(&mut doc_a, &mut doc_b)?;
    Ok(Merged::New(doc_a.save_nocompress()))
}

fn merge_into(target: &mut Automerge, other: &mut Automerge) -> Result<(), Error> {
    target
        .merge(other)
        .map_err(|e| Error::Internal(format!("could not merge automerge documents: {e}")))?;
    ensure_complete(target).map_err(|missing| {
        Error::Internal(format!(
            "merged automerge document is missing {} dependencies (e.g. {})",
            missing.len(),
            missing[0]
        ))
    })
}

/// Apply external bytes to a stored value: `merge(automerge, bytea)`.
///
/// `changes` may be anything Automerge can load incrementally: a full save
/// (compressed or not, optionally followed by change chunks), or bare change
/// chunks (`save_incremental()` / `save_after()` output), several of them
/// concatenated. Chunks are applied on top of `a`, so bare changes may
/// depend on changes `a` already has.
///
/// - Empty `changes`, or nothing new: `None` (use `a` as is, no re-save).
/// - Changes whose dependencies are neither in `a` nor in `changes`:
///   [`Error::InvalidInput`] naming the missing hashes. Nothing orphaned is
///   ever stored.
/// - Malformed bytes: [`Error::InvalidInput`], including decoder panics.
/// - Otherwise `Some` normalized result, which (as in [`normalize`]) is
///   loaded back once and must keep its heads.
pub fn merge_changes(a: &[u8], changes: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    if changes.is_empty() {
        return Ok(None);
    }
    guard_input(|| merge_changes_unguarded(a, changes))
}

fn merge_changes_unguarded(a: &[u8], changes: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    let heads_a = stored_heads_unguarded(a)?;
    // `a` is a document chunk, so loading `a ++ changes` is exactly
    // "load_incremental(changes) onto a", except that it is strict: a chunk
    // that fails to parse, or has a bad checksum, fails the whole load
    // (`load_incremental` would log it and carry on with what it parsed),
    // and changes with missing dependencies stay queued where we can see
    // them instead of being silently dropped by a later save.
    let mut combined = Vec::with_capacity(a.len() + changes.len());
    combined.extend_from_slice(a);
    combined.extend_from_slice(changes);
    let doc = Automerge::load(&combined)
        .map_err(|e| Error::InvalidInput(format!("invalid automerge changes: {e}")))?;
    drop(combined);
    ensure_complete(&doc).map_err(|mut missing| {
        missing.sort();
        const SHOWN: usize = 5;
        let shown: Vec<String> = missing.iter().take(SHOWN).map(|h| h.to_string()).collect();
        let more = if missing.len() > SHOWN {
            format!(" and {} more", missing.len() - SHOWN)
        } else {
            String::new()
        };
        let what = if missing.len() == 1 { "dependency" } else { "dependencies" };
        Error::InvalidInput(format!(
            "invalid automerge changes: missing {} {what} that neither the document nor the input contains: {}{more}",
            missing.len(),
            shown.join(", ")
        ))
    })?;
    let mut heads = doc.get_heads();
    heads.sort();
    let mut sorted_a = heads_a;
    sorted_a.sort();
    if heads == sorted_a {
        return Ok(None);
    }
    let saved = doc.save_nocompress();
    // Same safeguard as `normalize`: malformed but checksummed input can
    // load into a document whose save does not load back.
    let reloaded = Automerge::load(&saved).map_err(|e| {
        Error::InvalidInput(format!(
            "invalid automerge changes: result does not survive a save and load ({e})"
        ))
    })?;
    if reloaded.get_heads() != heads {
        return Err(Error::InvalidInput(
            "invalid automerge changes: heads change after a save and load".into(),
        ));
    }
    Ok(Some(saved))
}

/// Current heads as sorted lowercase hex change hashes.
pub fn heads(bytes: &[u8]) -> Result<Vec<String>, Error> {
    Ok(heads_to_strings(stored_heads(bytes)?))
}

/// Sorted lowercase hex strings of `heads`.
pub fn heads_to_strings(heads: Vec<ChangeHash>) -> Vec<String> {
    let mut heads: Vec<String> = heads.iter().map(ToString::to_string).collect();
    heads.sort();
    heads
}

/// Whether every change of `b` is already in `a` (so `merge(a, b)` is `a`).
pub fn contains(a: &[u8], b: &[u8]) -> Result<bool, Error> {
    if a == b {
        return Ok(true);
    }
    let heads_a = stored_heads(a)?;
    let heads_b = stored_heads(b)?;
    match contains_by_heads(&heads_a, &heads_b) {
        Some(answer) => Ok(answer),
        None => contains_loaded(a, &heads_b),
    }
}

/// Whether the stored value `a` has every change of the history ending at
/// `heads_b`. Loads `a`.
pub fn contains_loaded(a: &[u8], heads_b: &[ChangeHash]) -> Result<bool, Error> {
    guard_stored(|| Ok(has_all(&load_stored_unguarded(a)?, heads_b)))
}

/// Stored bytes to JSON.
pub fn to_json(bytes: &[u8]) -> Result<serde_json::Value, Error> {
    guard_stored(|| json::doc_to_json(&load_stored_unguarded(bytes)?))
}

/// Running state of the `merge_agg` aggregate. Each input is loaded once and
/// its missing changes applied; the result is saved once at the end.
#[derive(Default)]
pub struct MergeAccumulator {
    doc: Option<Automerge>,
    /// Bytes of the first input while no later input has added anything, so
    /// the common "all rows are the same or older" case needs no re-save.
    unchanged_first: Option<Vec<u8>>,
}

impl MergeAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, bytes: &[u8]) -> Result<(), Error> {
        guard_stored(|| self.add_unguarded(bytes))
    }

    fn add_unguarded(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let Some(doc) = self.doc.as_mut() else {
            self.doc = Some(load_stored_unguarded(bytes)?);
            self.unchanged_first = Some(bytes.to_vec());
            return Ok(());
        };
        if self.unchanged_first.as_deref() == Some(bytes) {
            return Ok(());
        }
        // Read the heads from the header first: an input that adds nothing
        // is then never loaded.
        if has_all(doc, &stored_heads_unguarded(bytes)?) {
            return Ok(());
        }
        let mut other = load_stored_unguarded(bytes)?;
        merge_into(doc, &mut other)?;
        self.unchanged_first = None;
        Ok(())
    }

    /// The merged stored bytes, or `None` if nothing was added.
    pub fn finish(&self) -> Result<Option<Cow<'_, [u8]>>, Error> {
        match (&self.unchanged_first, &self.doc) {
            (Some(bytes), _) => Ok(Some(Cow::Borrowed(bytes))),
            (None, Some(doc)) => guard_stored(|| Ok(Some(Cow::Owned(doc.save_nocompress())))),
            (None, None) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use automerge::transaction::Transactable;
    use automerge::{ActorId, AutoCommit, ROOT};
    use serde_json::json;

    fn actor(n: u8) -> ActorId {
        ActorId::from([n; 16])
    }

    /// A base document and two forks with concurrent edits, all as stored bytes.
    fn forked() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut base = AutoCommit::new().with_actor(actor(1));
        base.put(ROOT, "title", "base").unwrap();
        base.put(ROOT, "n", 1i64).unwrap();
        let mut a = base.fork().with_actor(actor(2));
        let mut b = base.fork().with_actor(actor(3));
        a.put(ROOT, "a", true).unwrap();
        a.put(ROOT, "title", "from a").unwrap();
        b.put(ROOT, "b", true).unwrap();
        b.delete(ROOT, "n").unwrap();
        (
            normalize(&base.save()).unwrap(),
            normalize(&a.save()).unwrap(),
            normalize(&b.save()).unwrap(),
        )
    }

    #[test]
    fn normalize_is_idempotent_and_uncompressed() {
        let (_, a, _) = forked();
        assert_eq!(normalize(&a).unwrap(), a);
        let doc = load_stored(&a).unwrap();
        assert_eq!(doc.save_nocompress(), a);
    }

    #[test]
    fn normalize_accepts_compressed_and_incremental_saves() {
        let mut doc = AutoCommit::new().with_actor(actor(1));
        doc.put(ROOT, "x", 1i64).unwrap();
        let first = doc.save();
        let heads = doc.get_heads();
        doc.put(ROOT, "y", "long enough to maybe compress ".repeat(20))
            .unwrap();
        let mut bytes = first.clone();
        bytes.extend(doc.save_after(&heads));
        let expected = doc.document().save_nocompress();
        assert_eq!(normalize(&bytes).unwrap(), expected);
        assert_eq!(normalize(&doc.save()).unwrap(), expected);
        // Changes alone (no document chunk) load too.
        assert_eq!(normalize(&doc.save_after(&[])).unwrap(), expected);
    }

    #[test]
    fn normalize_rejects_garbage_and_orphans() {
        assert!(matches!(
            normalize(b"not automerge"),
            Err(Error::InvalidInput(_))
        ));
        let mut doc = AutoCommit::new();
        doc.put(ROOT, "x", 1i64).unwrap();
        let heads = doc.get_heads();
        doc.put(ROOT, "y", 2i64).unwrap();
        // A change whose parent is absent.
        let orphan = doc.save_after(&heads);
        let err = normalize(&orphan).unwrap_err();
        assert!(
            matches!(err, Error::InvalidInput(ref m) if m.contains("depend")),
            "{err:?}"
        );
        // Same, trailing a document chunk that lacks the parent.
        let mut other = AutoCommit::new();
        other.put(ROOT, "z", 1i64).unwrap();
        let mut bytes = other.save();
        bytes.extend(orphan);
        assert!(matches!(normalize(&bytes), Err(Error::InvalidInput(_))));
    }

    #[test]
    fn empty_input_is_empty_document() {
        let bytes = normalize(&[]).unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(to_json(&bytes).unwrap(), json!({}));
        assert!(heads(&bytes).unwrap().is_empty());
    }

    #[test]
    fn merge_is_commutative_and_idempotent() {
        let (base, a, b) = forked();
        let ab = merge(&a, &b).unwrap().into_bytes(&a, &b).into_owned();
        let ba = merge(&b, &a).unwrap().into_bytes(&b, &a).into_owned();
        assert_eq!(heads(&ab).unwrap(), heads(&ba).unwrap());
        assert_eq!(heads(&ab).unwrap().len(), 2);
        let json = to_json(&ab).unwrap();
        assert_eq!(json, to_json(&ba).unwrap());
        assert_eq!(json, json!({ "title": "from a", "a": true, "b": true }));

        assert_eq!(merge(&ab, &ab).unwrap(), Merged::Left);
        assert_eq!(merge(&ab, &a).unwrap(), Merged::Left);
        assert_eq!(merge(&base, &ab).unwrap(), Merged::Right);
        assert!(contains(&ab, &a).unwrap());
        assert!(contains(&ab, &base).unwrap());
        assert!(!contains(&a, &b).unwrap());
        assert!(!contains(&base, &a).unwrap());
    }

    #[test]
    fn accumulator() {
        let (base, a, b) = forked();
        let mut acc = MergeAccumulator::new();
        assert!(acc.finish().unwrap().is_none());
        acc.add(&base).unwrap();
        acc.add(&base).unwrap();
        assert_eq!(acc.finish().unwrap().unwrap().as_ref(), base.as_slice());
        acc.add(&a).unwrap();
        acc.add(&b).unwrap();
        let merged = acc.finish().unwrap().unwrap().into_owned();
        let expected = merge(&a, &b).unwrap().into_bytes(&a, &b).into_owned();
        assert_eq!(heads(&merged).unwrap(), heads(&expected).unwrap());
        assert_eq!(to_json(&merged).unwrap(), to_json(&expected).unwrap());
    }
}
