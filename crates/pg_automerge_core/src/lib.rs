//! Postgres-independent Automerge logic for pg_automerge.
//!
//! Everything here is plain Rust and tested with `cargo test -p pg_automerge_core`;
//! the pgrx crate at the repo root is only glue. See docs/DESIGN.md.
//!
//! "Stored bytes" below always means the canonical representation of the SQL
//! `automerge` type: the output of [`Automerge::save_nocompress`] for a
//! document without queued (dependency-less) changes.

#![warn(missing_docs)]

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::OnceLock;

use automerge::{Automerge, AutomergeError, ChangeHash, ReadDoc};

pub mod encoding;
pub mod header;
pub mod history;
pub mod json;
pub mod loaded;
pub mod notify;

#[cfg(feature = "test-hooks")]
pub mod test_hooks;

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
/// Only panics with a string payload (`panic!` with a message, which is
/// what Rust code such as automerge raises) are converted. Any other payload
/// is passed on with `resume_unwind`: in a backend that is a Postgres ERROR
/// raised inside `f` (a query cancel reached through
/// [`set_interrupt_check`], or an error from a Postgres function a
/// [`json::JsonSink`] calls), which pgrx carries as a panic with its own
/// payload type and must reach pgrx's boundary unchanged, not be
/// relabelled 22P02/XX000.
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
        let msg = match payload.downcast_ref::<&str>() {
            Some(msg) => *msg,
            None => match payload.downcast_ref::<String>() {
                Some(msg) => msg.as_str(),
                None => resume_unwind(payload),
            },
        };
        Err(on_panic(msg))
    })
}

/// The interrupt check of the embedding process (see
/// [`set_interrupt_check`]).
static INTERRUPT_CHECK: OnceLock<fn()> = OnceLock::new();

/// Register a function that the long-running loops of this crate (the
/// JSON walk, building history rows) call every [`TICK_EVERY`] steps. The
/// extension registers Postgres' `CHECK_FOR_INTERRUPTS()`, so a cancel or
/// `statement_timeout` takes effect between steps; it raises by panicking
/// with a non-string payload, which the panic guard around Automerge calls
/// passes through. Only the first registration counts. Single Automerge
/// calls (a load, a save, a text read) cannot be interrupted.
pub fn set_interrupt_check(check: fn()) {
    let _ = INTERRUPT_CHECK.set(check);
}

/// How many steps pass between interrupt checks.
pub const TICK_EVERY: u32 = 1024;

/// Counts loop steps and runs the interrupt check every [`TICK_EVERY`].
#[derive(Default)]
pub(crate) struct Ticker(u32);

impl Ticker {
    #[inline]
    pub(crate) fn tick(&mut self) {
        self.0 = self.0.wrapping_add(1);
        if self.0.is_multiple_of(TICK_EVERY)
            && let Some(check) = INTERRUPT_CHECK.get()
        {
            check();
        }
    }
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
    let doc = load_bytes(bytes).map_err(|e| match e {
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
///
/// The result is loaded back and must have the same heads (when
/// verification is on, see [`verification_enabled`]), unless loading it
/// provably parses the very bytes the first load already accepted (see
/// `loads_as_saved`):
///
/// - the input already was the canonical encoding (`saved == bytes`), or
/// - the input is one document chunk with deflated columns (a compressed
///   `save()`) whose inflated form ([`header::inflate_document`], which
///   inflates exactly as Automerge's loader does) is byte for byte
///   `saved`: Automerge inflates a compressed chunk before reading it, so
///   the first load read those same uncompressed bytes.
///
/// Malformed input with valid checksums can load "successfully" into a
/// document whose re-save does not load (seen in fuzzing: "mismatching
/// heads"); storing that would make the value unreadable forever, so it is
/// rejected here as invalid input instead. Such input never takes the
/// shortcut: its re-save differs from what was loaded. Anything else (bare
/// change chunks, a document plus trailing changes, a document encoded
/// differently, e.g. by another Automerge implementation) costs this
/// second load.
///
/// # Errors
///
/// [`Error::InvalidInput`] for anything that is not a loadable Automerge
/// save or change sequence (including decoder panics), for changes with
/// missing dependencies, and for input that does not survive a save and
/// load.
pub fn normalize(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    guard_input(|| normalize_unguarded(bytes).map(|(_, saved)| saved))
}

/// [`normalize`], keeping the loaded document: the document and its
/// stored bytes (checked as `normalize` checks them). Unguarded.
pub(crate) fn normalize_unguarded(bytes: &[u8]) -> Result<(Automerge, Vec<u8>), Error> {
    let doc = load_external(bytes)?;
    let saved = doc.save_nocompress();
    if !loads_as_saved(bytes, &saved) && verification_enabled() {
        let mut heads = doc.get_heads();
        heads.sort_unstable();
        reload_check(&saved, &heads, "invalid automerge document")?;
    }
    Ok((doc, saved))
}

/// Whether loading `saved` (the `save_nocompress()` of a document loaded
/// from `input`) provably parses the bytes the load of `input` already
/// accepted, so the save-and-load check is not needed: `input` is the
/// canonical encoding itself, or one compressed document chunk that
/// inflates to it (see [`normalize`]).
pub(crate) fn loads_as_saved(input: &[u8], saved: &[u8]) -> bool {
    saved == input || header::inflate_document(input).as_deref() == Some(saved)
}

/// The process-wide switch of the save-and-load check (see
/// [`set_verification_check`]).
static VERIFICATION_CHECK: OnceLock<fn() -> bool> = OnceLock::new();

/// Register the function that says whether the save-and-load check of
/// values built from external input is on (the extension registers the
/// `pg_automerge.verify_writes` setting). Only the first registration
/// counts; without one the check is always on.
pub fn set_verification_check(check: fn() -> bool) {
    let _ = VERIFICATION_CHECK.set(check);
}

/// Whether the save-and-load check of values built from external input
/// runs: [`normalize`] and the deferred check of
/// [`loaded::LoadedDoc::stored`]. On unless the registered switch (see
/// [`set_verification_check`]) says otherwise. With the check off, input
/// that loads but whose normalized save does not load back (malformed
/// input with valid checksums, seen only in fuzzing) is stored as is and
/// fails when it is next loaded.
pub fn verification_enabled() -> bool {
    #[cfg(feature = "test-hooks")]
    if let Some(on) = test_hooks::verification_override() {
        return on;
    }
    VERIFICATION_CHECK.get().is_none_or(|check| check())
}

/// `Automerge::load`, counted for the tests (see `test_hooks::loads`,
/// feature `test-hooks`).
pub(crate) fn load_bytes(bytes: &[u8]) -> Result<Automerge, AutomergeError> {
    #[cfg(feature = "test-hooks")]
    test_hooks::count_load();
    Automerge::load(bytes)
}

/// The safeguard of [`normalize`] for a document built from external
/// input: its save `saved` must load back and have the same `heads`
/// (sorted). Otherwise [`Error::InvalidInput`], with messages starting with
/// `what`.
pub(crate) fn reload_check(saved: &[u8], heads: &[ChangeHash], what: &str) -> Result<(), Error> {
    #[cfg(feature = "test-hooks")]
    if test_hooks::reload_check_hook() {
        return Err(Error::InvalidInput(format!(
            "{what}: does not survive a save and load (forced by a test hook)"
        )));
    }
    let reloaded = load_bytes(saved).map_err(|e| {
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
    load_bytes(bytes).map_err(|e| Error::Internal(format!("corrupt stored automerge value: {e}")))
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
///
/// # Errors
///
/// [`Error::Internal`] if the value has to be loaded and does not load.
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

/// [`contains_by_heads`], also deciding from the numbers of changes when
/// both are known: if the heads cannot say and `b` has at least as many
/// changes as `a`, `a` does not contain `b`. (Containment is `changes(b) ⊆
/// changes(a)`; with `|b| >= |a|` that makes the two sets equal, and equal
/// histories have equal heads, which the heads would have shown.) This
/// decides `contains(older, newer)` and two concurrent versions with the
/// same number of changes without a load. The counts come from the change
/// actor column ([`header::change_count_from_prefix`]), a small prefix of
/// a stored value.
pub fn contains_by_heads_and_counts(
    a: &[ChangeHash],
    b: &[ChangeHash],
    count_a: Option<u64>,
    count_b: Option<u64>,
) -> Option<bool> {
    contains_by_heads(a, b).or(match (count_a, count_b) {
        (Some(count_a), Some(count_b)) if count_b >= count_a => Some(false),
        _ => None,
    })
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
    let doc = load_bytes(&combined)
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

/// [`contains_changes_by_heads`], extended to a save: input that is
/// exactly one document chunk with a valid checksum whose header heads are
/// all heads of `a`, and that passes Automerge's chunk parse
/// ([`header::document_parses`]), is contained (`Some(true)`; what a load
/// of `a ++ changes` decides, see [`loaded::merge_changes`]). One that
/// does not parse is left to the loading path, which rejects it. Needs only `a`'s heads, so callers can
/// answer before they fetch or load `a`.
pub fn contains_input_by_heads(heads_a: &[ChangeHash], changes: &[u8]) -> Option<bool> {
    contains_input_by_header(heads_a, || None, changes)
}

/// [`contains_input_by_heads`], also deciding a save from change counts
/// when `count_a` gives the number of changes of `a` (called only for a
/// save the heads do not decide): a save whose
/// header heads are not all heads of `a` and whose header lists at least
/// as many changes as `a` has is not contained (`Some(false)`; see
/// [`contains_by_heads_and_counts`]: a document chunk that loads has
/// exactly the changes of its heads' history, so `a` having them all would
/// make the histories equal). This decides "is this newer or concurrent
/// save already in the stored document" without loading it. Only the
/// header of the save is read for this answer: a save whose header
/// disagrees with its content, or that does not parse, is not loadable,
/// and `merge` rejects it (so, as for bare change chunks, `false` does not
/// promise that `merge` accepts the input).
pub fn contains_input_by_header(
    heads_a: &[ChangeHash],
    count_a: impl FnOnce() -> Option<u64>,
    changes: &[u8],
) -> Option<bool> {
    if changes.is_empty() {
        return Some(true);
    }
    if let Some(answer) = contains_changes_by_heads(heads_a, changes) {
        return Some(answer);
    }
    let chunk = header::document_chunk(changes)?;
    if is_subset(&chunk.heads, heads_a) {
        // Only if the chunk parses; otherwise the loading path rejects it.
        return header::document_parses(changes).then_some(true);
    }
    match (chunk.change_count, count_a()) {
        (Some(count), Some(count_a)) if count >= count_a => Some(false),
        _ => None,
    }
}

/// Sorted lowercase hex strings of `heads`.
pub fn heads_to_strings(heads: Vec<ChangeHash>) -> Vec<String> {
    let mut heads: Vec<String> = heads.iter().map(ToString::to_string).collect();
    heads.sort();
    heads
}

/// Running state of the `merge_agg` aggregate.
///
/// Documents are loaded only when a merge needs them, and the result is
/// saved at most once:
///
/// - The first stored input is kept as bytes with its header heads, not
///   loaded. A group of one row, or of rows that this value contains or is
///   contained in by their heads (identical values, say), loads nothing.
/// - When a later input cannot be decided by the heads, the larger of the
///   two is loaded first (it usually contains the other, as a newer version
///   of a document contains an older one), and only if it does not is the
///   other loaded and merged into the first input's document.
/// - Once a document is loaded, later inputs are checked against it by
///   their (header) heads, so inputs that add nothing are never loaded. An
///   input that is loaded and contains the whole state replaces it (no
///   merge); otherwise its missing changes are applied.
///
/// While the state equals one input's stored value, that value is the
/// result (no save). Like `merge`, the result is the same document state
/// for any input order, not always the same bytes.
///
/// After an error the accumulator may hold a partial state; in Postgres
/// the error aborts the statement, which discards it.
#[derive(Default)]
pub struct MergeAccumulator {
    state: AccState,
}

#[derive(Default)]
enum AccState {
    /// Nothing added yet.
    #[default]
    Empty,
    /// One stored value, not loaded: its bytes and sorted header heads.
    Pending {
        bytes: Vec<u8>,
        heads: Vec<ChangeHash>,
    },
    /// A loaded document (boxed: an `Automerge` is large).
    Loaded {
        doc: Box<Automerge>,
        /// The stored bytes of the input the document was loaded from,
        /// while nothing has been added to it.
        stored: Option<Vec<u8>>,
        /// Whether it holds unverified external changes (see
        /// [`loaded::LoadedDoc`]); the result then does too.
        unverified: bool,
    },
}

impl AccState {
    /// A copy of a loaded input as the whole state.
    fn adopt(input: &loaded::LoadedDoc) -> Self {
        AccState::Loaded {
            doc: Box::new(input.doc().clone()),
            stored: input.cached_stored().map(<[u8]>::to_vec),
            unverified: input.is_unverified(),
        }
    }

    /// A document loaded from the stored input `bytes`.
    fn from_stored(doc: Automerge, bytes: Vec<u8>) -> Self {
        AccState::Loaded {
            doc: Box::new(doc),
            stored: Some(bytes),
            unverified: false,
        }
    }

    /// A document built by merging.
    fn merged(doc: Automerge, unverified: bool) -> Self {
        AccState::Loaded {
            doc: Box::new(doc),
            stored: None,
            unverified,
        }
    }

    fn unverified(&self) -> bool {
        matches!(
            self,
            AccState::Loaded {
                unverified: true,
                ..
            }
        )
    }
}

/// Result of [`MergeAccumulator::finish_loaded`].
pub enum Accumulated<'a> {
    /// Stored bytes of an input that already contained all the others.
    Stored(&'a [u8]),
    /// A new document (a copy of the state, which stays usable).
    Loaded(Box<loaded::LoadedDoc>),
}

/// Sorted heads of a stored value (from its header when possible).
fn sorted_stored_heads(bytes: &[u8]) -> Result<Vec<ChangeHash>, Error> {
    let mut heads = stored_heads_unguarded(bytes)?;
    heads.sort_unstable();
    Ok(heads)
}

impl MergeAccumulator {
    /// An empty accumulator (nothing added yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether something was added and the state already has every change
    /// of the history ending at `heads`, so adding a value with those heads
    /// would change nothing. Lets a caller skip reading (detoasting) such a
    /// value: only its heads are needed.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] if Automerge fails reading the state
    /// ([`Error::InvalidInput`] when it holds unverified external changes).
    pub fn has_heads(&self, heads: &[ChangeHash]) -> Result<bool, Error> {
        match &self.state {
            AccState::Empty => Ok(false),
            AccState::Pending { heads: have, .. } => Ok(is_subset(heads, have)),
            AccState::Loaded {
                doc, unverified, ..
            } => guard_for(*unverified, || Ok(has_all(doc, heads))),
        }
    }

    /// Add a stored value or a loaded document.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] if a stored value does not load or the merge fails
    /// ([`Error::InvalidInput`] instead when the input or the state holds
    /// unverified external changes).
    pub fn add_input(&mut self, input: loaded::Input<'_>) -> Result<(), Error> {
        let unverified = input.unverified() || self.state.unverified();
        guard_for(unverified, || self.add_unguarded(input))
    }

    fn add_unguarded(&mut self, input: loaded::Input<'_>) -> Result<(), Error> {
        use loaded::Input::{Loaded, Stored};
        match (&mut self.state, input) {
            (AccState::Empty, Stored(bytes)) => {
                self.state = AccState::Pending {
                    bytes: bytes.to_vec(),
                    heads: sorted_stored_heads(bytes)?,
                };
            }
            (AccState::Empty, Loaded(input)) => self.state = AccState::adopt(input),
            (
                AccState::Pending {
                    bytes: first,
                    heads: first_heads,
                },
                Stored(bytes),
            ) => {
                if first.as_slice() == bytes {
                    return Ok(());
                }
                let heads = sorted_stored_heads(bytes)?;
                if is_subset(&heads, first_heads) {
                    return Ok(());
                }
                if is_subset(first_heads, &heads) {
                    // The input contains the first value (see
                    // `contains_by_heads`).
                    self.state = AccState::Pending {
                        bytes: bytes.to_vec(),
                        heads,
                    };
                    return Ok(());
                }
                if bytes.len() > first.len() {
                    let doc = load_stored_unguarded(bytes)?;
                    if has_all(&doc, first_heads) {
                        self.state = AccState::from_stored(doc, bytes.to_vec());
                        return Ok(());
                    }
                    let mut target = load_stored_unguarded(first)?;
                    loaded::merge_from(&mut target, &doc)?;
                    self.state = AccState::merged(target, false);
                } else {
                    let mut target = load_stored_unguarded(first)?;
                    if has_all(&target, &heads) {
                        self.state = AccState::from_stored(target, std::mem::take(first));
                        return Ok(());
                    }
                    loaded::merge_from(&mut target, &load_stored_unguarded(bytes)?)?;
                    self.state = AccState::merged(target, false);
                }
            }
            (
                AccState::Pending {
                    bytes: first,
                    heads: first_heads,
                },
                Loaded(input),
            ) => {
                if is_subset(input.heads(), first_heads) {
                    return Ok(());
                }
                if has_all(input.doc(), first_heads) {
                    self.state = AccState::adopt(input);
                    return Ok(());
                }
                let mut target = load_stored_unguarded(first)?;
                if has_all(&target, input.heads()) {
                    self.state = AccState::from_stored(target, std::mem::take(first));
                    return Ok(());
                }
                loaded::merge_from(&mut target, input.doc())?;
                self.state = AccState::merged(target, input.is_unverified());
            }
            (AccState::Loaded { doc, stored, .. }, Stored(bytes)) => {
                if stored.as_deref() == Some(bytes) {
                    return Ok(());
                }
                // Read the heads from the header first: an input that adds
                // nothing is then never loaded.
                if has_all(doc, &stored_heads_unguarded(bytes)?) {
                    return Ok(());
                }
                let other = load_stored_unguarded(bytes)?;
                if has_all(&other, &doc.get_heads()) {
                    self.state = AccState::from_stored(other, bytes.to_vec());
                    return Ok(());
                }
                loaded::merge_from(doc, &other)?;
                *stored = None;
            }
            (
                AccState::Loaded {
                    doc,
                    stored,
                    unverified,
                },
                Loaded(input),
            ) => {
                if has_all(doc, input.heads()) {
                    return Ok(());
                }
                if has_all(input.doc(), &doc.get_heads()) {
                    self.state = AccState::adopt(input);
                    return Ok(());
                }
                loaded::merge_from(doc, input.doc())?;
                *stored = None;
                *unverified |= input.is_unverified();
            }
        }
        Ok(())
    }

    /// The result without saving it: the stored bytes of the input the
    /// state equals, if any, otherwise a copy of the merged document (the
    /// state stays usable, since a final function may run more than
    /// once). `None` if nothing was added.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] if copying the merged document fails.
    pub fn finish_loaded(&self) -> Result<Option<Accumulated<'_>>, Error> {
        Ok(Some(match &self.state {
            AccState::Empty => return Ok(None),
            AccState::Pending { bytes, .. }
            | AccState::Loaded {
                stored: Some(bytes),
                ..
            } => Accumulated::Stored(bytes),
            AccState::Loaded {
                doc,
                stored: None,
                unverified,
            } => Accumulated::Loaded(Box::new(loaded::LoadedDoc::from_doc(
                Automerge::clone(doc),
                *unverified,
            )?)),
        }))
    }
}
