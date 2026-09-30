//! Postgres-independent Automerge logic for pg_automerge.
//!
//! Everything here is plain Rust and tested with `cargo test -p pg_automerge_core`;
//! the pgrx crate at the repo root is only glue. See docs/DESIGN.md.
//!
//! "Stored bytes" below always means the canonical representation of the SQL
//! `automerge` type: the output of [`Automerge::save_nocompress`] for a
//! document without queued (dependency-less) changes.

#![warn(missing_docs)]
// No unsafe code of its own: all of it is in the pgrx glue (docs/DESIGN.md,
// "Unsafe code").
#![forbid(unsafe_code)]

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::OnceLock;

use automerge::{Automerge, AutomergeError, ChangeHash, ReadDoc};

pub mod blocks;
pub mod budget;
pub mod encoding;
pub mod header;
pub mod history;
pub mod json;
pub mod loaded;
pub mod notify;
pub mod spans;

#[cfg(feature = "test-hooks")]
pub mod test_hooks;

pub use automerge;
pub use serde_json;

/// Errors surfaced to SQL. The glue maps [`Error::InvalidInput`] and
/// [`Error::MissingDependencies`] to SQLSTATE 22P02
/// (invalid_text_representation), [`Error::ConflictingChanges`] to 22000
/// (data_exception), [`Error::InvalidParameter`] to 22023
/// (invalid_parameter_value), [`Error::LimitExceeded`] to 54000
/// (program_limit_exceeded), [`Error::LoadLimit`] to 53400
/// (configuration_limit_exceeded), [`Error::Unsupported`] to 0A000
/// (feature_not_supported) and [`Error::Internal`] to XX000, with
/// [`Error::message`] as the message, [`Error::detail`] as the DETAIL and
/// [`Error::hint`] as the HINT.
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
    /// Two histories of one actor: different changes with the same actor
    /// id and sequence number (or two author assignments of one actor),
    /// which Automerge refuses to merge. Each input may be valid on its
    /// own; together they show that two writers shared an actor id. The
    /// primary message, naming the actor and seq.
    ConflictingChanges(String),
    /// A valid document that an output format cannot represent (nesting
    /// deeper than [`json::MAX_DEPTH`] for jsonb).
    LimitExceeded(String),
    /// Client input (or a merge result) whose estimated load exceeds
    /// `pg_automerge.max_load_memory` (see [`budget`]).
    LoadLimit(Box<budget::LimitError>),
    /// Client input in a form the extension does not accept (Automerge's
    /// experimental bundle chunks).
    Unsupported(String),
    /// A broken invariant: a stored value that does not load, or a bug.
    Internal(String),
}

impl Error {
    /// How many missing hashes [`Error::detail`] names.
    const SHOWN: usize = 5;

    /// The primary message: one short line.
    pub fn message(&self) -> String {
        match self {
            Error::InvalidInput(msg)
            | Error::InvalidParameter(msg)
            | Error::ConflictingChanges(msg)
            | Error::LimitExceeded(msg)
            | Error::Unsupported(msg)
            | Error::Internal(msg) => msg.clone(),
            Error::LoadLimit(err) => err.message(),
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
    /// missing hashes (at most five, then a count of the others), for
    /// [`Error::ConflictingChanges`] why they cannot be merged, for
    /// [`Error::LoadLimit`] the estimate and what it counts.
    pub fn detail(&self) -> Option<String> {
        match self {
            Error::MissingDependencies(missing) => {
                Some(format!("Missing changes: {}.", Self::list_missing(missing)))
            }
            Error::ConflictingChanges(_) => Some(
                "An actor's changes form one sequence; these inputs hold two different ones, \
                 which cannot be merged."
                    .into(),
            ),
            Error::LoadLimit(err) => Some(err.detail()),
            _ => None,
        }
    }

    /// Advice for the user, if any: for [`Error::ConflictingChanges`] that
    /// every writer needs its own actor id, for [`Error::LoadLimit`] who
    /// can raise the limit.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Error::ConflictingChanges(_) => Some(
                "Each writer must use its own actor id. Automerge picks a random one \
                 for every document instance unless the application sets it.",
            ),
            Error::LoadLimit(_) => Some(budget::LimitError::hint()),
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

/// An error of Automerge as an [`Error`]: a conflict between two
/// histories of one actor (a reused actor id: `DuplicateSeqNumber`,
/// `DuplicateAuthor`) is [`Error::ConflictingChanges`] wherever it
/// arises (a load of concatenated input, applying changes, a merge);
/// anything else is what `other` makes of it.
pub(crate) fn automerge_error(
    e: AutomergeError,
    other: impl FnOnce(AutomergeError) -> Error,
) -> Error {
    match e {
        AutomergeError::DuplicateSeqNumber(seq, actor) => Error::ConflictingChanges(format!(
            "conflicting automerge changes: actor {actor} has two different changes with seq {seq}"
        )),
        AutomergeError::DuplicateAuthor(_, actor, seq) => Error::ConflictingChanges(format!(
            "conflicting automerge changes: actor {actor} is assigned an author again at seq {seq}"
        )),
        e => other(e),
    }
}

/// [`automerge_error`] for external change bytes: `invalid automerge
/// changes: ...` ([`Error::InvalidInput`]) unless it is a conflict.
pub(crate) fn invalid_changes(e: AutomergeError) -> Error {
    automerge_error(e, |e| {
        Error::InvalidInput(format!("invalid automerge changes: {e}"))
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
    let doc = load_bytes(bytes).map_err(|e| {
        automerge_error(e, |e| match e {
            AutomergeError::MissingDeps => Error::InvalidInput(
                "invalid automerge document: changes are missing dependencies".into(),
            ),
            e => Error::InvalidInput(format!("invalid automerge document: {e}")),
        })
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
    guard_input(|| normalize_unguarded(bytes).map(|n| n.saved))
}

/// A document [`normalize`] built: the document, its stored bytes, and
/// their counts (see [`budget`]) when a limit was checked.
pub(crate) struct Normalized {
    pub(crate) doc: Automerge,
    pub(crate) saved: Vec<u8>,
    pub(crate) counts: Option<budget::DocCounts>,
}

/// [`normalize`], keeping the loaded document: the document and its
/// stored bytes (checked as `normalize` checks them). Unguarded.
///
/// Before anything is loaded, the input is scanned and its estimated load
/// checked against [`budget::limit`] ([`Error::LoadLimit`]); bundle chunks
/// are rejected ([`Error::Unsupported`]). The normalized document must fit
/// the limit too, so that every stored value fit it when it was written.
pub(crate) fn normalize_unguarded(bytes: &[u8]) -> Result<Normalized, Error> {
    let limit = budget::limit();
    let mut scanned = match limit {
        Some(_) => {
            let counts = budget::scan_input_keep(bytes, limit);
            budget::check_load(&counts, bytes, limit)?;
            Some(counts)
        }
        None if budget::has_bundle(bytes) => return Err(budget::bundle_error()),
        None => None,
    };
    let doc = load_external(bytes)?;
    let saved = doc.save_nocompress();
    // The scan's inflated columns, dropped once compared.
    let as_saved = loads_as_saved(
        bytes,
        &saved,
        scanned
            .as_mut()
            .and_then(|s| s.inflated_columns.take())
            .as_deref(),
    );
    let counts = match scanned {
        // The input is the save itself (or inflates to it): its counts.
        Some(scanned) if as_saved => Some(scanned.first_doc),
        Some(_) => Some(budget::check_saved(
            &saved,
            budget::LimitKind::Normalized,
            limit,
        )?),
        None => None,
    };
    if !as_saved && verification_enabled() {
        let mut heads = doc.get_heads();
        heads.sort_unstable();
        reload_check(&saved, &heads, "invalid automerge document")?;
    }
    Ok(Normalized { doc, saved, counts })
}

/// Whether loading `saved` (the `save_nocompress()` of a document loaded
/// from `input`) provably parses the bytes the load of `input` already
/// accepted, so the save-and-load check is not needed: `input` is the
/// canonical encoding itself, or one compressed document chunk that
/// inflates to it (see [`normalize`]). `inflated`: the input's deflated
/// columns as the load memory scan already inflated them
/// ([`budget::InputCounts::inflated_columns`]), compared in place instead
/// of inflating the input again.
pub(crate) fn loads_as_saved(input: &[u8], saved: &[u8], inflated: Option<&[Vec<u8>]>) -> bool {
    if saved == input {
        return true;
    }
    match inflated {
        Some(columns) => header::inflated_document_is(input, columns, saved),
        None => {
            #[cfg(feature = "test-hooks")]
            test_hooks::count_reinflation();
            header::inflate_document(input).as_deref() == Some(saved)
        }
    }
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
    let doc = load_bytes(&combined).map_err(invalid_changes)?;
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
/// ([`header::document_parses`], within [`budget::limit`]), is contained (`Some(true)`; what a load
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
        // Only if the chunk parses, within the limit; otherwise the
        // loading path rejects it.
        let limit = budget::limit();
        let counts = budget::scan_input(changes, limit);
        return (counts.single_doc_parses && budget::check_parse(&counts, limit).is_ok())
            .then_some(true);
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
    /// A loaded document.
    Loaded(AccDoc),
}

/// The loaded document of a [`MergeAccumulator`].
struct AccDoc {
    /// The document (boxed: an `Automerge` is large).
    doc: Box<Automerge>,
    /// The stored bytes of the input the document was loaded from, while
    /// nothing has been added to it.
    stored: Option<Vec<u8>>,
    /// Whether it holds unverified external changes (see
    /// [`loaded::LoadedDoc`]); the result then does too.
    unverified: bool,
    /// What its save describes (see [`budget`]), when known.
    counts: Option<budget::DocCounts>,
    /// A save of it made to count it, while nothing has been added.
    save: Option<Vec<u8>>,
}

impl AccDoc {
    /// A copy of a loaded input.
    fn adopt(input: &loaded::LoadedDoc) -> Self {
        AccDoc {
            doc: Box::new(input.doc().clone()),
            stored: input.cached_stored().map(<[u8]>::to_vec),
            unverified: input.is_unverified(),
            counts: input.known_counts(),
            save: None,
        }
    }

    /// A document loaded from the stored input `bytes`.
    fn from_stored(doc: Automerge, bytes: Vec<u8>) -> Self {
        AccDoc {
            doc: Box::new(doc),
            stored: Some(bytes),
            unverified: false,
            counts: None,
            save: None,
        }
    }

    /// What its save describes: known, or a scan of its stored bytes (or
    /// of a save, which is kept).
    fn counts(&mut self) -> budget::DocCounts {
        if let Some(counts) = self.counts {
            return counts;
        }
        let counts = match &self.stored {
            Some(bytes) => budget::scan_doc(bytes),
            None => budget::scan_doc(self.save.get_or_insert_with(|| self.doc.save_nocompress())),
        };
        self.counts = Some(counts);
        counts
    }

    /// Merge `other` into the document, within the limit (the changes to
    /// apply, then the result, see [`loaded::merge_from`] and
    /// [`loaded::result_counts`]).
    fn merge(&mut self, other: &Automerge, other_unverified: bool) -> Result<(), Error> {
        let limit = budget::limit();
        let base = limit.map(|_| self.counts());
        let approx = loaded::merge_from(&mut self.doc, base, other, limit)?;
        self.stored = None;
        self.save = None;
        self.counts = None;
        self.unverified |= other_unverified;
        let (counts, save) = loaded::result_counts(&self.doc, approx, limit)?;
        self.counts = counts;
        self.save = save;
        Ok(())
    }
}

impl AccState {
    fn unverified(&self) -> bool {
        matches!(
            self,
            AccState::Loaded(AccDoc {
                unverified: true,
                ..
            })
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
            AccState::Loaded(acc) => guard_for(acc.unverified, || Ok(has_all(&acc.doc, heads))),
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
            (AccState::Empty, Loaded(input)) => self.state = AccState::Loaded(AccDoc::adopt(input)),
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
                        self.state = AccState::Loaded(AccDoc::from_stored(doc, bytes.to_vec()));
                        return Ok(());
                    }
                    let mut target =
                        AccDoc::from_stored(load_stored_unguarded(first)?, std::mem::take(first));
                    target.merge(&doc, false)?;
                    self.state = AccState::Loaded(target);
                } else {
                    let mut target =
                        AccDoc::from_stored(load_stored_unguarded(first)?, std::mem::take(first));
                    if has_all(&target.doc, &heads) {
                        self.state = AccState::Loaded(target);
                        return Ok(());
                    }
                    target.merge(&load_stored_unguarded(bytes)?, false)?;
                    self.state = AccState::Loaded(target);
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
                    self.state = AccState::Loaded(AccDoc::adopt(input));
                    return Ok(());
                }
                let mut target =
                    AccDoc::from_stored(load_stored_unguarded(first)?, std::mem::take(first));
                if has_all(&target.doc, input.heads()) {
                    self.state = AccState::Loaded(target);
                    return Ok(());
                }
                target.merge(input.doc(), input.is_unverified())?;
                self.state = AccState::Loaded(target);
            }
            (AccState::Loaded(acc), Stored(bytes)) => {
                if acc.stored.as_deref() == Some(bytes) {
                    return Ok(());
                }
                // Read the heads from the header first: an input that adds
                // nothing is then never loaded.
                if has_all(&acc.doc, &stored_heads_unguarded(bytes)?) {
                    return Ok(());
                }
                let other = load_stored_unguarded(bytes)?;
                if has_all(&other, &acc.doc.get_heads()) {
                    self.state = AccState::Loaded(AccDoc::from_stored(other, bytes.to_vec()));
                    return Ok(());
                }
                acc.merge(&other, false)?;
            }
            (AccState::Loaded(acc), Loaded(input)) => {
                if has_all(&acc.doc, input.heads()) {
                    return Ok(());
                }
                if has_all(input.doc(), &acc.doc.get_heads()) {
                    self.state = AccState::Loaded(AccDoc::adopt(input));
                    return Ok(());
                }
                acc.merge(input.doc(), input.is_unverified())?;
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
            | AccState::Loaded(AccDoc {
                stored: Some(bytes),
                ..
            }) => Accumulated::Stored(bytes),
            AccState::Loaded(acc) => {
                let doc = guard_stored(|| Ok(Automerge::clone(&acc.doc)))?;
                Accumulated::Loaded(Box::new(loaded::LoadedDoc::from_parts(
                    doc,
                    acc.unverified,
                    acc.counts,
                    acc.save.clone(),
                )))
            }
        }))
    }
}
