//! Postgres-independent Automerge logic for pg_automerge.
//!
//! Everything here is plain Rust and tested with `cargo test -p pg_automerge_core`;
//! the pgrx crate at the repo root is only glue. See docs/DESIGN.md.
//!
//! "Stored bytes" below always means the canonical representation of the SQL
//! `automerge` type: the output of [`Automerge::save_nocompress`] for a
//! document without queued (dependency-less) changes.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use automerge::{Automerge, AutomergeError, ChangeHash, ReadDoc};

pub mod encoding;
pub mod header;
pub mod history;
pub mod json;
pub mod loaded;
pub mod notify;

pub use automerge;
pub use serde_json;

/// Errors surfaced to SQL. The glue maps [`Error::InvalidInput`] and
/// [`Error::MissingDependencies`] to SQLSTATE 22P02
/// (invalid_text_representation), [`Error::InvalidParameter`] to 22023
/// (invalid_parameter_value) and [`Error::Internal`] to XX000, with
/// [`Error::message`] as the message and [`Error::detail`] as the DETAIL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Malformed or unacceptable client input (bytes, text, hashes).
    InvalidInput(String),
    /// A well-formed argument that does not fit the document, e.g. heads
    /// the document does not have.
    InvalidParameter(String),
    /// Changes whose dependencies are in neither the document nor the
    /// input (`merge(automerge, bytea)`): the missing hashes, sorted.
    /// Invalid input, like [`Error::InvalidInput`].
    MissingDependencies(Vec<ChangeHash>),
    /// A broken invariant: a stored value that does not load, or a bug.
    Internal(String),
}

impl Error {
    /// How many missing hashes [`Error::detail`] names.
    const SHOWN: usize = 5;

    /// The primary message: one short line.
    pub fn message(&self) -> String {
        match self {
            Error::InvalidInput(msg) | Error::InvalidParameter(msg) | Error::Internal(msg) => {
                msg.clone()
            }
            Error::MissingDependencies(missing) => {
                let what = if missing.len() == 1 {
                    "dependency"
                } else {
                    "dependencies"
                };
                format!(
                    "invalid automerge changes: missing {} {what} that neither the document nor the input contains",
                    missing.len()
                )
            }
        }
    }

    /// Supporting detail, if any: for [`Error::MissingDependencies`] the
    /// missing hashes (at most five, then a count of the others).
    pub fn detail(&self) -> Option<String> {
        match self {
            Error::MissingDependencies(missing) => {
                Some(format!("Missing changes: {}.", Self::list_missing(missing)))
            }
            _ => None,
        }
    }

    fn list_missing(missing: &[ChangeHash]) -> String {
        let shown: Vec<String> = missing
            .iter()
            .take(Self::SHOWN)
            .map(ToString::to_string)
            .collect();
        let more = if missing.len() > Self::SHOWN {
            format!(" and {} more", missing.len() - Self::SHOWN)
        } else {
            String::new()
        };
        format!("{}{more}", shown.join(", "))
    }
}

impl fmt::Display for Error {
    /// The message, followed by the missing hashes for
    /// [`Error::MissingDependencies`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())?;
        if let Error::MissingDependencies(missing) = self {
            write!(f, ": {}", Self::list_missing(missing))?;
        }
        Ok(())
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
pub(crate) fn guard_input<T>(f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    guard(f, |msg| {
        Error::InvalidInput(format!(
            "invalid automerge document: malformed data ({msg})"
        ))
    })
}

/// `guard` for operations on stored (already validated) values.
pub(crate) fn guard_stored<T>(f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    guard(f, |msg| {
        Error::Internal(format!("automerge failed on a stored value: {msg}"))
    })
}

/// [`guard_input`] when the operation involves a document with unverified
/// external changes (see [`loaded::LoadedDoc`]), whose failures are the
/// client's; [`guard_stored`] otherwise.
pub(crate) fn guard_for<T>(
    unverified: bool,
    f: impl FnOnce() -> Result<T, Error>,
) -> Result<T, Error> {
    if unverified {
        guard_input(f)
    } else {
        guard_stored(f)
    }
}

/// Load bytes arriving from outside (text input, binary recv, `bytea` cast).
///
/// Accepts anything `Automerge::load` accepts: a document chunk, change
/// chunks, compressed or not, concatenated. Rejects input whose changes have
/// missing dependencies, so a stored value never carries orphaned changes.
/// Empty input is the empty document. Unguarded: callers run it inside
/// [`guard_input`].
fn load_external(bytes: &[u8]) -> Result<Automerge, Error> {
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
        let doc = load_external(bytes)?;
        let saved = doc.save_nocompress();
        if saved != bytes {
            let mut heads = doc.get_heads();
            heads.sort_unstable();
            reload_check(&saved, &heads, "invalid automerge document")?;
        }
        Ok(saved)
    })
}

/// The safeguard of [`normalize`] for a document built from external
/// input: its save `saved` must load back and have the same `heads`
/// (sorted). Otherwise [`Error::InvalidInput`], with messages starting with
/// `what`.
pub(crate) fn reload_check(saved: &[u8], heads: &[ChangeHash], what: &str) -> Result<(), Error> {
    let reloaded = Automerge::load(saved).map_err(|e| {
        Error::InvalidInput(format!("{what}: does not survive a save and load ({e})"))
    })?;
    let mut reloaded_heads = reloaded.get_heads();
    reloaded_heads.sort_unstable();
    if reloaded_heads != heads {
        return Err(Error::InvalidInput(format!(
            "{what}: heads change after a save and load"
        )));
    }
    Ok(())
}

/// Load already-stored bytes (unguarded). These were validated on the way
/// in, so failure here means on-disk corruption (or a bug) and is reported
/// as internal.
pub(crate) fn load_stored_unguarded(bytes: &[u8]) -> Result<Automerge, Error> {
    Automerge::load(bytes)
        .map_err(|e| Error::Internal(format!("corrupt stored automerge value: {e}")))
}

/// `Err(missing hashes)` if `doc` holds changes whose dependencies are absent.
pub(crate) fn ensure_complete(doc: &Automerge) -> Result<(), Vec<ChangeHash>> {
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
pub(crate) fn has_all(doc: &Automerge, heads: &[ChangeHash]) -> bool {
    doc.get_missing_deps(heads).is_empty()
}

/// Heads of a stored value: read from the document chunk header when the
/// value is a single document chunk (always, for values this extension
/// stored), otherwise by loading it. See [`header`].
pub(crate) fn stored_heads_unguarded(bytes: &[u8]) -> Result<Vec<ChangeHash>, Error> {
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
pub fn is_subset(sub: &[ChangeHash], sup: &[ChangeHash]) -> bool {
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

/// Result of loading `a ++ changes` ([`apply_changes`]).
pub(crate) enum Applied {
    /// Nothing new: the heads are those of `a`.
    Unchanged,
    /// Changes whose dependencies are in neither input (sorted).
    MissingDeps(Vec<ChangeHash>),
    /// New changes were applied.
    Changed(Box<Automerge>),
}

/// Load `a ++ changes` strictly (the path of `merge(automerge, bytea)`
/// for input that is not bare change chunks, see [`loaded::merge_changes`]).
pub(crate) fn apply_changes(
    a: &[u8],
    heads_a: &[ChangeHash],
    changes: &[u8],
) -> Result<Applied, Error> {
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
    if let Err(mut missing) = ensure_complete(&doc) {
        missing.sort();
        return Ok(Applied::MissingDeps(missing));
    }
    let mut heads = doc.get_heads();
    heads.sort();
    let mut sorted_a = heads_a.to_vec();
    sorted_a.sort();
    if heads == sorted_a {
        Ok(Applied::Unchanged)
    } else {
        Ok(Applied::Changed(Box::new(doc)))
    }
}

/// Decide `contains_changes(a, changes)` from `a`'s heads and the framing
/// of `changes` alone, without loading anything; `None` if that is not
/// possible.
///
/// Only when `changes` is bare, uncompressed change chunks (see
/// [`header::change_chunks`]). A stored document is complete (it has every
/// ancestor of its changes), so for each change `c` of the input:
///
/// - `hash(c)` is a head of `a`: `a` has `c`.
/// - some dependency of `c` is a head of `a`, or a change of the input
///   already known to be missing from `a`: `a` does not have `c` (a head
///   has no successor in `a`, and a missing change has no successor in
///   `a` either).
/// - otherwise unknown.
///
/// Any change known to be missing gives `Some(false)`; all known present
/// gives `Some(true)` (also for empty input); otherwise `None`. The common
/// cases are both decided: a re-send of the changes that made the current
/// heads (true), and new changes made on top of the current heads (false).
/// Only framing, checksums and dependency lists are checked here, so
/// `false` does not promise that `merge(a, changes)` will accept the input.
pub fn contains_changes_by_heads(heads_a: &[ChangeHash], changes: &[u8]) -> Option<bool> {
    let chunks = header::change_chunks(changes)?;
    let mut missing: Vec<ChangeHash> = Vec::new();
    let mut unknown = false;
    for chunk in &chunks {
        if heads_a.contains(&chunk.hash) {
            continue;
        }
        if chunk
            .deps
            .iter()
            .any(|d| heads_a.contains(d) || missing.contains(d))
        {
            missing.push(chunk.hash);
        } else {
            unknown = true;
        }
    }
    if !missing.is_empty() {
        Some(false)
    } else if unknown {
        None
    } else {
        Some(true)
    }
}

/// Sorted lowercase hex strings of `heads`.
pub fn heads_to_strings(heads: Vec<ChangeHash>) -> Vec<String> {
    let mut heads: Vec<String> = heads.iter().map(ToString::to_string).collect();
    heads.sort();
    heads
}

/// Running state of the `merge_agg` aggregate. Each input is loaded once and
/// its missing changes applied; the result is saved once at the end.
#[derive(Default)]
pub struct MergeAccumulator {
    doc: Option<Automerge>,
    /// Bytes of the first input while no later input has added anything, so
    /// the common "all rows are the same or older" case needs no re-save.
    unchanged_first: Option<Vec<u8>>,
    /// Whether an input was a loaded document with unverified external
    /// changes (see [`loaded::LoadedDoc`]); the result then is too.
    unverified: bool,
}

/// Result of [`MergeAccumulator::finish_loaded`].
pub enum Accumulated<'a> {
    /// Stored bytes of an input that already contained all the others.
    Stored(&'a [u8]),
    /// A new document (a copy of the state, which stays usable).
    Loaded(Box<loaded::LoadedDoc>),
}

impl MergeAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a stored value or a loaded document.
    pub fn add_input(&mut self, input: loaded::Input<'_>) -> Result<(), Error> {
        guard_for(input.unverified(), || self.add_unguarded(input))
    }

    fn add_unguarded(&mut self, input: loaded::Input<'_>) -> Result<(), Error> {
        let Some(doc) = self.doc.as_mut() else {
            match input {
                loaded::Input::Stored(bytes) => {
                    self.doc = Some(load_stored_unguarded(bytes)?);
                    self.unchanged_first = Some(bytes.to_vec());
                }
                loaded::Input::Loaded(loaded) => {
                    self.doc = Some(loaded.doc().clone());
                    self.unchanged_first = loaded.cached_stored().map(<[u8]>::to_vec);
                    self.unverified = loaded.is_unverified();
                }
            }
            return Ok(());
        };
        match input {
            loaded::Input::Stored(bytes) => {
                if self.unchanged_first.as_deref() == Some(bytes) {
                    return Ok(());
                }
                // Read the heads from the header first: an input that adds
                // nothing is then never loaded.
                if has_all(doc, &stored_heads_unguarded(bytes)?) {
                    return Ok(());
                }
                loaded::merge_from(doc, &load_stored_unguarded(bytes)?)?;
            }
            loaded::Input::Loaded(loaded) => {
                if has_all(doc, loaded.heads()) {
                    return Ok(());
                }
                loaded::merge_from(doc, loaded.doc())?;
                self.unverified |= loaded.is_unverified();
            }
        }
        self.unchanged_first = None;
        Ok(())
    }

    /// The result without saving it: the first input's stored bytes when
    /// nothing was added to it, otherwise a copy of the merged document
    /// (the state stays usable, since a final function may run more than
    /// once). `None` if nothing was added.
    pub fn finish_loaded(&self) -> Result<Option<Accumulated<'_>>, Error> {
        match (&self.unchanged_first, &self.doc) {
            (Some(bytes), _) => Ok(Some(Accumulated::Stored(bytes))),
            (None, Some(doc)) => Ok(Some(Accumulated::Loaded(Box::new(
                loaded::LoadedDoc::from_doc(doc.clone(), self.unverified)?,
            )))),
            (None, None) => Ok(None),
        }
    }
}
