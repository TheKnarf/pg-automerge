//! Bounding the memory a load of client bytes can take, before Automerge
//! allocates anything (see
//! docs/src/pages/design/resource-limits.mdx).
//!
//! Automerge input is run-length encoded and deflated, so a few bytes can
//! describe millions of operations, changes or dependencies, and a load
//! allocates in proportion to what the input describes. A failed Rust
//! allocation aborts the backend (the postmaster then restarts every
//! session), so the only defence is to refuse such input up front. This
//! module reads what is cheap to read (chunk headers, column metadata, the
//! run-length encoded columns run by run, never row by row) and computes an
//! upper estimate of the peak memory of loading it, which callers compare
//! with [`limit`] (the `pg_automerge.max_load_memory` setting).
//!
//! The estimate ([`doc_estimate`], [`changes_estimate`]) is linear in what
//! the input describes, with the worst cost per unit measured for Automerge
//! 0.12 (docs/src/pages/design/benchmarks/2026-09-29.mdx,
//! "Input amplification measurements"). The core test
//! `tests/memory_bounds.rs` measures real peaks with a counting allocator
//! and checks that they stay below the estimate; it must be re-run (and the
//! constants revisited) whenever Automerge is upgraded.
//!
//! What is read:
//!
//! - A document chunk: its actors and the change and op columns. Deflated
//!   columns are inflated one at a time, in total at most a tenth of the
//!   limit (an inflated byte is charged 10 bytes, so more than that exceeds
//!   the limit anyway: the scan stops there, [`InputCounts::truncated`]).
//!   Rows of every known column are counted run by run (Automerge sizes
//!   some allocations by a column's length before it checks the columns
//!   against each other, so the largest one counts), group columns are
//!   summed (successors, dependencies), and [`DocCounts::gmax`] is found
//!   by merging the object, key, insert and successor columns run by run.
//!   What rebuilding its changes copies beyond the chunk's own bytes
//!   ([`DocCounts::rebuilt`]: messages and mark names repeated by a run
//!   and actor ids longer than 16 bytes, copied into every rebuilt change
//!   that holds them; [`DocCounts::rebuilt_keys`]: keys repeated by a
//!   run) is computed from the run headers.
//! - A change chunk (compressed or not): its dependencies, actors and op
//!   columns. Its list of other actors is priced per entry, duplicates
//!   included (Automerge keeps every entry); a list longer than the limit
//!   pays for stops the scan ([`InputCounts::truncated`]). What applying
//!   it copies per op ([`ChangeCounts::repeated`]: keys and mark names
//!   repeated by a run) is computed from the run headers.
//! - Column metadata, in both: validated in place, nothing allocated per
//!   entry; entries beyond one per column Automerge writes are priced
//!   ([`DocCounts::extra_columns`]), and a block with more than the limit
//!   pays for stops the scan.
//! - Bundle chunks are not read: [`InputCounts::bundle`], which callers
//!   reject (an experimental Automerge format).
//!
//! The scan never fails. Input it cannot parse is marked
//! [`InputCounts::malformed`], with the counts of everything before the
//! point where it stopped, which bound what Automerge can allocate before
//! it fails on the same bytes (it parses a chunk completely before it
//! reconstructs or applies it); callers then let Automerge reject it with
//! its own message. Column contents are decoded leniently, as Automerge's
//! streaming decoders are: a run that cannot be read ends the column. The
//! fuzz harness checks that input Automerge loads is never marked
//! malformed and that its measured peak stays below the estimate.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io::Read;
use std::sync::OnceLock;

use crate::{Error, Ticker};

const MAGIC: [u8; 4] = [0x85, 0x6f, 0x4a, 0x83];
const DEFLATE_BIT: u64 = 0x08;

const DOCUMENT_CHUNK: u8 = 0;
const CHANGE_CHUNK: u8 = 1;
const COMPRESSED_CHUNK: u8 = 2;
const BUNDLE_CHUNK: u8 = 3;

// Column types (the low three bits of a column spec).
const GROUP: u64 = 0;
const DELTA: u64 = 3;
const BOOLEAN: u64 = 4;
const STRING: u64 = 5;
const VALUE: u64 = 7;

/// Change column specs of a document chunk in the order Automerge 0.12
/// writes them (`change_graph.rs` `ids`), deflate bit clear: actor, seq,
/// max op, time, message, deps (group, then values), extra (metadata, then
/// values).
const DOC_CHANGE_SPECS: [u64; 9] = [0x01, 0x03, 0x13, 0x23, 0x35, 0x40, 0x43, 0x56, 0x57];
/// Op column specs of a document chunk in the order Automerge 0.12 writes
/// them (`op_set2/columns.rs` `ids::ALL_COLUMN_SPECS` sorted): obj (actor,
/// counter), key (actor, counter, string), id (actor, counter), insert,
/// action, value (metadata, values), succ (group, actor, counter), expand,
/// mark name.
const DOC_OP_SPECS: [u64; 16] = [
    0x01, 0x02, 0x11, 0x13, 0x15, 0x21, 0x23, 0x34, 0x42, 0x56, 0x57, 0x80, 0x81, 0x83, 0x94, 0xa5,
];
/// Op column specs of a change chunk (`storage/change/change_op_columns.rs`):
/// obj, key, insert, action, value, pred (group, actor, counter), expand,
/// mark name.
const CHANGE_OP_SPECS: [u64; 14] = [
    0x01, 0x02, 0x11, 0x13, 0x15, 0x34, 0x42, 0x56, 0x57, 0x70, 0x71, 0x73, 0x94, 0xa5,
];

const DEPS_GROUP: u64 = 0x40;
const DEPS_MEMBER: u64 = 0x43;
const SUCC_GROUP: u64 = 0x80;
const SUCC_MEMBERS: [u64; 2] = [0x81, 0x83];
const PRED_GROUP: u64 = 0x70;
const PRED_MEMBERS: [u64; 2] = [0x71, 0x73];
const OBJ_ACTOR: u64 = 0x01;
const OBJ_CTR: u64 = 0x02;
const KEY_ACTOR: u64 = 0x11;
const KEY_CTR: u64 = 0x13;
const KEY_STR: u64 = 0x15;
const INSERT: u64 = 0x34;
/// The change message column of a document chunk.
const MESSAGE: u64 = 0x35;
/// The mark name column (document and change chunks).
const MARK_NAME: u64 = 0xa5;
/// The change actor column of a document chunk.
const CHANGE_ACTOR: u64 = 0x01;
/// The longest actor id Automerge's `ActorId` holds inline (a
/// `TinyVec<[u8; 16]>`): a longer one is a heap copy wherever it is
/// cloned.
const INLINE_ACTOR: usize = 16;

/// Bytes charged per inflated input byte (values and strings are copied
/// into the document, 5-7 bytes each measured).
const PER_BYTE: u64 = 10;
/// Bytes charged per entry of a change's list of other actors, duplicates
/// included, besides its bytes: Automerge parses the list into a vector
/// of 32-byte `ActorId`s that doubles as it fills (`length_prefixed`),
/// while an entry (an empty actor id) takes one input byte. Measured up
/// to 63 per entry; 96 is three `ActorId`s, the old and the new buffer
/// of a growth step both held.
const PER_OTHER_ACTOR: u128 = 100;
/// Bytes charged per byte that a document's rebuilt changes hold beyond
/// the chunk's own bytes ([`DocCounts::rebuilt`]): Automerge rebuilds
/// every change of a document with its own copy of the message, held
/// twice (in the change's bytes and as a `String`), of its actor ids
/// (in its bytes and as `ActorId`s) and of its ops' mark names, and a
/// load whose heads do not match clones every rebuilt change into its
/// error. Measured 4.0 per byte (messages, actor ids), 3.0 (mark names).
const PER_REBUILT: u128 = 5;
/// Bytes charged per byte of the keys that a document's rebuilt changes
/// hold beyond the chunk's own bytes ([`DocCounts::rebuilt_keys`]): held
/// once, in the change's bytes (and again in the error of a load whose
/// heads do not match). Measured 2.0 per byte with the error's clone,
/// 1.6-1.7 for saves written by Automerge whose keys are overwritten many
/// times. (Mark names cost more, 3.0 per byte: they are priced with
/// [`DocCounts::rebuilt`].)
const PER_REBUILT_KEY: u128 = 3;
/// Bytes charged per byte that applying changes copies beyond the
/// chunks' own bytes ([`ChangeCounts::repeated`]): Automerge makes an
/// owned `String` of every op's key and mark name when it imports a
/// change's ops, and the document holds a key literally where other keys
/// come between its rows. Measured 1.0 per byte (one key), 2.0 (mark
/// names), 4.0 (a key held literally by the document; 5.9 with the save
/// of the result that `normalize` makes).
const PER_REPEATED: u128 = 8;
/// Bytes charged per column metadata entry beyond one per column
/// Automerge writes ([`DocCounts::extra_columns`]): its parse keeps every
/// entry of a block in vectors that double as they fill (and copies the
/// list as it checks the layout), while an entry of an empty column takes
/// two input bytes. Measured up to 120 per entry for a document chunk,
/// 168 for a change chunk and 174 for a compressed one, at counts just
/// past a power of two.
const PER_EXTRA_COLUMN: u128 = 200;
/// Bytes charged for any load or apply, whatever it holds: Automerge's
/// fixed structures, which the per-unit costs do not cover for tiny
/// documents.
const FIXED: u128 = 64 << 10;

/// The Automerge version the constants above were measured for. A test
/// (`tests/memory_bounds.rs`) fails when the lock file has another one:
/// an upgrade means re-measuring the cost model first
/// (docs/src/pages/design/resource-limits.mdx, "The estimate").
pub const MEASURED_AUTOMERGE: &str = "0.12.0";
/// The version of hexane, Automerge's column store, the constants were
/// measured with: Automerge 0.12.0 requires `^1.0.0-alpha.5`, which a
/// `cargo update` can move without Automerge changing.
pub const MEASURED_HEXANE: &str = "1.0.0-alpha.5";

/// The default of `pg_automerge.max_load_memory`: 2 GB.
pub const DEFAULT_LIMIT: u64 = 2 << 30;

// ---------------------------------------------------------------------------
// The limit
// ---------------------------------------------------------------------------

/// The registered source of the limit (see [`set_limit_source`]).
static LIMIT_SOURCE: OnceLock<fn() -> Option<u64>> = OnceLock::new();

/// Register the function that returns the current limit in bytes (`None`:
/// no limit). The extension registers the `pg_automerge.max_load_memory`
/// setting. Only the first registration counts; without one the limit is
/// [`DEFAULT_LIMIT`].
pub fn set_limit_source(source: fn() -> Option<u64>) {
    let _ = LIMIT_SOURCE.set(source);
}

/// The current limit on the estimated memory of a load, in bytes (`None`:
/// no limit).
pub fn limit() -> Option<u64> {
    #[cfg(feature = "test-hooks")]
    if let Some(limit) = crate::test_hooks::limit_override() {
        return limit;
    }
    match LIMIT_SOURCE.get() {
        Some(source) => source(),
        None => Some(DEFAULT_LIMIT),
    }
}

// ---------------------------------------------------------------------------
// Counts and estimates
// ---------------------------------------------------------------------------

/// What a document chunk describes (or a loaded document, in the form its
/// save would have).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DocCounts {
    /// Op rows.
    pub ops: u64,
    /// Successor entries (the sum of the succ group column).
    pub succ: u64,
    /// The largest number of successor entries in one (object, key) group
    /// of rows (see [`DocCounts`] in
    /// docs/src/pages/design/resource-limits.mdx: a key overwritten or
    /// deleted many times).
    pub gmax: u64,
    /// Changes.
    pub changes: u64,
    /// Dependency entries (the sum of the deps group column).
    pub deps: u64,
    /// Actors.
    pub actors: u64,
    /// Bytes of the chunk with its columns inflated.
    pub inflated: u64,
    /// Bytes that rebuilding the document's changes copies beyond the
    /// chunk's own bytes: a repeat run of `n` change messages of `len`
    /// bytes is `(n - 1) * len` (every change holds its own copy), the
    /// same for the ops' mark names, and the actor ids longer than 16
    /// bytes that each change holds (its own, and the others its ops refer
    /// to). Computed from the run headers, never expanded.
    pub rebuilt: u64,
    /// The same for the op keys repeated by a run (a rebuilt change holds
    /// them once, in its bytes; priced lower than [`DocCounts::rebuilt`]).
    pub rebuilt_keys: u64,
    /// Column metadata entries beyond one per column Automerge writes (an
    /// empty column's entry takes two input bytes; Automerge keeps every
    /// entry while it parses the chunk).
    pub extra_columns: u64,
}

/// What change chunks describe (or changes about to be applied).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangeCounts {
    /// Op rows.
    pub ops: u64,
    /// Pred entries (the sum of the pred group column).
    pub preds: u64,
    /// Changes.
    pub changes: u64,
    /// Dependency entries.
    pub deps: u64,
    /// Distinct actors (a change's own and its other actors).
    pub actors: u64,
    /// Entries of the changes' lists of other actors, duplicates
    /// included: Automerge keeps one `ActorId` per entry while it parses
    /// a change, and an entry can take a single byte.
    pub other_actor_entries: u64,
    /// Bytes of the chunks' data, inflated.
    pub inflated: u64,
    /// Bytes that applying the changes copies beyond the chunks' own
    /// bytes: a repeat run of `n` keys or mark names of `len` bytes is
    /// `(n - 1) * len` (every op gets its own copy); for changes turned
    /// from a document chunk, its [`DocCounts::rebuilt`].
    pub repeated: u64,
    /// Column metadata entries beyond one per column Automerge writes, as
    /// [`DocCounts::extra_columns`].
    pub extra_columns: u64,
}

impl DocCounts {
    /// Every count added (saturating).
    fn add(&mut self, other: &DocCounts) {
        self.ops = self.ops.saturating_add(other.ops);
        self.succ = self.succ.saturating_add(other.succ);
        self.gmax = self.gmax.saturating_add(other.gmax);
        self.changes = self.changes.saturating_add(other.changes);
        self.deps = self.deps.saturating_add(other.deps);
        self.actors = self.actors.saturating_add(other.actors);
        self.inflated = self.inflated.saturating_add(other.inflated);
        self.rebuilt = self.rebuilt.saturating_add(other.rebuilt);
        self.rebuilt_keys = self.rebuilt_keys.saturating_add(other.rebuilt_keys);
        self.extra_columns = self.extra_columns.saturating_add(other.extra_columns);
    }

    /// An upper bound of the counts of a document with these counts after
    /// the changes `delta` are applied to it: every op and dependency is
    /// added, every pred becomes a successor entry (possibly all on one
    /// key, so it is added to [`DocCounts::gmax`] too), every actor may be
    /// new. The bytes are added as they are (an approximation: the columns
    /// of the merged document may encode less compactly). What rebuilding
    /// the new changes copies is at most their bytes and what applying them
    /// copies ([`DocCounts::rebuilt`]: a rebuilt change is the change as
    /// it was applied).
    pub fn plus_changes(&self, delta: &ChangeCounts) -> DocCounts {
        let mut sum = *self;
        sum.ops = sum.ops.saturating_add(delta.ops);
        sum.succ = sum.succ.saturating_add(delta.preds);
        sum.gmax = sum.gmax.saturating_add(delta.preds);
        sum.changes = sum.changes.saturating_add(delta.changes);
        sum.deps = sum.deps.saturating_add(delta.deps);
        sum.actors = sum.actors.saturating_add(delta.actors);
        sum.inflated = sum.inflated.saturating_add(delta.inflated);
        sum.rebuilt = sum
            .rebuilt
            .saturating_add(delta.inflated)
            .saturating_add(delta.repeated);
        sum
    }

    /// These counts as changes to apply (a document chunk that is not the
    /// first chunk of a load is turned into changes and applied).
    fn as_changes(&self) -> ChangeCounts {
        ChangeCounts {
            ops: self.ops,
            preds: self.succ,
            changes: self.changes,
            deps: self.deps,
            actors: self.actors,
            other_actor_entries: 0,
            inflated: 0,
            // Applying the rebuilt changes copies what rebuilding them did
            // (a bound: keys and mark names are copied per op when
            // applied, and held literally by the result where its other
            // rows come between theirs; messages and actors at most once
            // more).
            repeated: self.rebuilt.saturating_add(self.rebuilt_keys),
            // Its metadata is priced with the chunk; the changes Automerge
            // rebuilds list only the columns it writes.
            extra_columns: 0,
        }
    }
}

impl ChangeCounts {
    /// Every count added (saturating; actors too, so only an upper bound
    /// of the distinct actors).
    pub fn add(&mut self, other: &ChangeCounts) {
        self.ops = self.ops.saturating_add(other.ops);
        self.preds = self.preds.saturating_add(other.preds);
        self.changes = self.changes.saturating_add(other.changes);
        self.deps = self.deps.saturating_add(other.deps);
        self.actors = self.actors.saturating_add(other.actors);
        self.other_actor_entries = self
            .other_actor_entries
            .saturating_add(other.other_actor_entries);
        self.inflated = self.inflated.saturating_add(other.inflated);
        self.repeated = self.repeated.saturating_add(other.repeated);
        self.extra_columns = self.extra_columns.saturating_add(other.extra_columns);
    }
}

/// The document changes are applied to: its numbers of changes and
/// actors, which Automerge's clock cache multiplies with the new ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Base {
    /// Changes of the document.
    pub changes: u64,
    /// Actors of the document.
    pub actors: u64,
}

impl From<&DocCounts> for Base {
    fn from(counts: &DocCounts) -> Self {
        Base {
            changes: counts.changes,
            actors: counts.actors,
        }
    }
}

/// `sum` of `terms`, each a product of `factor` and counts, saturating.
fn sat(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Estimated peak bytes of loading a document chunk with these counts
/// (`Automerge::load` of a save, or turning a later document chunk into
/// changes). Worst measured cost per unit
/// (docs/src/pages/design/resource-limits.mdx): 450 per op
/// (out-of-order ops; appended text costs about 90), 30 per successor, 600
/// per successor pending on one key, 1600 per change plus up to 130 per
/// op or successor for rebuilding the changes, 200 per dependency and per
/// actor, 0.3 per change × actor (Automerge caches a clock every 16
/// changes), 3 per actor for each change with more than about 16 ops, 10
/// per inflated byte, 5 per byte of messages, mark names and actor ids
/// the rebuilt changes copy beyond the chunk's own bytes and 3 per byte
/// of keys, 200 per column metadata entry beyond one per column
/// Automerge writes, and 64 kB whatever the document holds.
pub fn doc_estimate(d: &DocCounts) -> u64 {
    let (ops, succ, gmax, changes, deps, actors, inflated) = (
        u128::from(d.ops),
        u128::from(d.succ),
        u128::from(d.gmax),
        u128::from(d.changes),
        u128::from(d.deps),
        u128::from(d.actors),
        u128::from(d.inflated),
    );
    let rows = ops + succ;
    sat(FIXED
        + 450 * ops
        + 30 * succ
        + 600 * gmax
        + 1600 * changes
        + (130 * rows).min(1600 * changes)
        + 200 * deps
        + 200 * actors
        + 3 * changes * actors / 10
        + 3 * changes.min(rows / 16) * actors
        + u128::from(PER_BYTE) * inflated
        + PER_REBUILT * u128::from(d.rebuilt)
        + PER_REBUILT_KEY * u128::from(d.rebuilt_keys)
        + PER_EXTRA_COLUMN * u128::from(d.extra_columns))
}

/// Estimated peak bytes of parsing and applying changes with these counts
/// to a document with `base`'s changes and actors: 1000 per op, 80 per
/// pred, 2500 per change, 200 per dependency and per actor, 100 per entry
/// of a change's list of other actors (duplicates included), 0.3 per
/// change × actor of the result's clock cache, 10 per inflated byte, 8
/// per byte applying them copies beyond the chunks' own bytes, 200 per
/// column metadata entry beyond one per column Automerge writes, and
/// 64 kB.
pub fn changes_estimate(c: &ChangeCounts, base: Base) -> u64 {
    let (ops, preds, changes, deps, actors, inflated) = (
        u128::from(c.ops),
        u128::from(c.preds),
        u128::from(c.changes),
        u128::from(c.deps),
        u128::from(c.actors),
        u128::from(c.inflated),
    );
    let (base_changes, base_actors) = (u128::from(base.changes), u128::from(base.actors));
    sat(FIXED
        + 1000 * ops
        + 80 * preds
        + 2500 * changes
        + 200 * deps
        + 200 * actors
        + PER_OTHER_ACTOR * u128::from(c.other_actor_entries)
        + 3 * (changes * (base_actors + actors) + base_changes * actors) / 10
        + u128::from(PER_BYTE) * inflated
        + PER_REPEATED * u128::from(c.repeated)
        + PER_EXTRA_COLUMN * u128::from(c.extra_columns))
}

/// What a scan of external input found (see [`scan_input`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputCounts {
    /// The first chunk, when it is a document chunk (loaded as the
    /// document).
    pub first_doc: DocCounts,
    /// Document chunks after the first chunk, summed: Automerge turns them
    /// into changes and applies them (their changes are in
    /// [`InputCounts::changes`] too).
    pub later_docs: DocCounts,
    /// Change chunks, compressed or not, and the changes of later
    /// document chunks.
    pub changes: ChangeCounts,
    /// The scan stopped because inflating deflated data produced more than
    /// a tenth of the limit (each such byte is charged 10 bytes, so the
    /// input exceeds the limit whatever else it describes), or because a
    /// change lists more other actors than the limit pays for (each entry
    /// is charged 110 bytes), or a column metadata block more entries
    /// (each beyond one per column Automerge writes is charged 200 bytes);
    /// the counts are only a lower bound.
    pub truncated: bool,
    /// The input does not parse (framing, lengths, column metadata, a
    /// deflated column that does not inflate); the counts cover what comes
    /// before the point where the scan stopped.
    pub malformed: bool,
    /// The input holds a bundle chunk (not read).
    pub bundle: bool,
    /// The input is exactly one document chunk that passes everything
    /// Automerge's chunk parse checks (see [`crate::header::document_parses`]).
    pub single_doc_parses: bool,
    /// Bytes produced by inflating deflated columns and compressed chunks
    /// (what parsing allocates beyond the input itself).
    pub deflated_out: u64,
    /// Gmax of some document chunk is its upper bound (the chunk's
    /// successor entries), not computed exactly (see [`scan_input`]).
    pub gmax_bounded: bool,
    /// For input that is one document chunk with deflated columns, scanned
    /// by [`scan_input_keep`] to the end: those columns as the scan
    /// inflated them, in order, so that the normalized save can be
    /// compared with the input's inflated form without inflating it again
    /// ([`crate::header::inflated_document_is`]). Held while the input is
    /// loaded: at most the inflated bytes, each charged 10 bytes.
    pub inflated_columns: Option<Vec<Vec<u8>>>,
}

impl InputCounts {
    /// Estimated peak bytes of `Automerge::load` of the input on its own.
    pub fn load_estimate(&self) -> u64 {
        let base = Base::from(&self.first_doc);
        doc_estimate(&self.first_doc)
            .saturating_add(doc_estimate(&self.later_docs))
            .saturating_add(changes_estimate(&self.changes, base))
    }

    /// Estimated peak bytes of loading a document with `base`'s changes
    /// and actors followed by the input (`a ++ changes`), or of parsing
    /// the input's chunks and applying them to that document: every
    /// document chunk of the input is turned into changes, none is the
    /// document. (The document itself is not counted.)
    pub fn apply_estimate(&self, base: Base) -> u64 {
        let mut changes = self.changes;
        changes.add(&self.first_doc.as_changes());
        doc_estimate(&self.first_doc)
            .saturating_add(doc_estimate(&self.later_docs))
            .saturating_add(changes_estimate(&changes, base))
    }

    /// Estimated peak bytes of only parsing the input, as a load does for
    /// a document chunk whose heads the document already has: what
    /// inflating its deflated columns produces, held once (Automerge
    /// copies the chunk with its columns inflated into one buffer; an
    /// uncompressed chunk is parsed in place, for nothing).
    pub fn parse_estimate(&self) -> u64 {
        self.deflated_out
    }

    /// Every chunk of the input as changes applied to a document: change
    /// chunks, and document chunks turned into changes (an upper bound of
    /// what a load of `a ++ input` adds to `a`).
    pub fn as_changes(&self) -> ChangeCounts {
        let mut changes = self.changes;
        changes.add(&self.first_doc.as_changes());
        changes.inflated = self.inflated();
        changes
    }

    /// All inflated bytes read.
    fn inflated(&self) -> u64 {
        self.first_doc
            .inflated
            .saturating_add(self.later_docs.inflated)
            .saturating_add(self.changes.inflated)
    }

    /// The input as the document a load of it on its own would build, in
    /// the form of its counts: the first document chunk plus every change
    /// (an upper bound).
    pub fn as_document(&self) -> DocCounts {
        let mut changes = self.changes;
        changes.inflated = changes.inflated.saturating_add(self.later_docs.inflated);
        self.first_doc.plus_changes(&changes)
    }

    /// The totals shown in an error's DETAIL.
    fn shown(&self) -> Shown {
        Shown {
            ops: self
                .first_doc
                .ops
                .saturating_add(self.later_docs.ops)
                .saturating_add(self.changes.ops),
            changes: self
                .first_doc
                .changes
                .saturating_add(self.later_docs.changes)
                .saturating_add(self.changes.changes),
            actors: self.first_doc.actors.saturating_add(self.changes.actors),
            bytes: self.inflated(),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Which load an estimate was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    /// Loading client input (text or binary input, the `bytea` cast, the
    /// bytes of `merge(automerge, bytea)` / `automerge_contains`).
    Input,
    /// The document client input normalizes to, as it will be stored.
    Normalized,
    /// Applying changes to a document (merging).
    Apply,
    /// The result of a merge, as it will be stored.
    Merged,
}

/// Counts shown in the DETAIL of a limit error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shown {
    /// Operations.
    pub ops: u64,
    /// Changes.
    pub changes: u64,
    /// Actors.
    pub actors: u64,
    /// Bytes, uncompressed.
    pub bytes: u64,
}

impl From<&DocCounts> for Shown {
    fn from(d: &DocCounts) -> Self {
        Shown {
            ops: d.ops,
            changes: d.changes,
            actors: d.actors,
            bytes: d.inflated,
        }
    }
}

impl From<&ChangeCounts> for Shown {
    fn from(c: &ChangeCounts) -> Self {
        Shown {
            ops: c.ops,
            changes: c.changes,
            actors: c.actors,
            bytes: c.inflated,
        }
    }
}

/// An estimate above the limit: [`Error::LoadLimit`] (SQLSTATE 53400).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitError {
    /// What would have been loaded.
    pub kind: LimitKind,
    /// The limit, in bytes.
    pub limit: u64,
    /// The estimate, in bytes.
    pub estimate: u64,
    /// The scan stopped early: the estimate is a lower bound.
    pub at_least: bool,
    /// What the input describes, if it was read.
    pub shown: Option<Shown>,
}

/// `n` bytes for a message: whole MB (rounded up) from 1 MB on, kB below.
fn format_bytes(n: u64) -> String {
    const MB: u64 = 1 << 20;
    if n >= MB {
        format!("{} MB", n.div_ceil(MB))
    } else {
        format!("{} kB", n.div_ceil(1024))
    }
}

/// The limit for a message: in MB when it is a whole number of them (as
/// it is set, in kB, otherwise).
fn format_limit(n: u64) -> String {
    const MB: u64 = 1 << 20;
    if n.is_multiple_of(MB) {
        format!("{} MB", n / MB)
    } else {
        format!("{} kB", n / 1024)
    }
}

impl LimitError {
    /// The primary message.
    pub fn message(&self) -> String {
        let what = match self.kind {
            LimitKind::Input | LimitKind::Normalized => "estimated memory to load automerge input",
            LimitKind::Apply => "estimated memory for applying automerge changes",
            LimitKind::Merged => "estimated memory to load merged automerge document",
        };
        format!(
            "{what} exceeds \"pg_automerge.max_load_memory\" ({})",
            format_limit(self.limit)
        )
    }

    /// The DETAIL: the estimate and what it is made of.
    pub fn detail(&self) -> String {
        let bound = if self.at_least { "at least" } else { "up to" };
        let estimate = format_bytes(self.estimate);
        let subject = match self.kind {
            LimitKind::Input | LimitKind::Merged => "Loading it",
            LimitKind::Normalized => "Loading the document it normalizes to",
            LimitKind::Apply => "Applying them",
        };
        let n = |n: u64, what: &str| {
            if n == 1 {
                format!("1 {what}")
            } else {
                format!("{n} {what}s")
            }
        };
        match self.shown {
            Some(s) => format!(
                "{subject} could take {bound} {estimate} ({}, {}, {}, {} uncompressed).",
                n(s.ops, "operation"),
                n(s.changes, "change"),
                n(s.actors, "actor"),
                n(s.bytes, "byte")
            ),
            None => format!("{subject} could take {bound} {estimate}."),
        }
    }

    /// The HINT.
    pub fn hint() -> &'static str {
        "A superuser can raise \"pg_automerge.max_load_memory\"."
    }
}

fn limit_error(
    kind: LimitKind,
    limit: u64,
    estimate: u64,
    at_least: bool,
    shown: Option<Shown>,
) -> Error {
    Error::LoadLimit(Box::new(LimitError {
        kind,
        limit,
        estimate,
        at_least,
        shown,
    }))
}

/// The error for a bundle chunk in client input.
pub(crate) fn bundle_error() -> Error {
    Error::Unsupported("automerge bundle chunks are not supported".into())
}

/// Reject a bundle chunk in the input (0A000): only bundles, whatever the
/// limit.
pub(crate) fn check_no_bundle(counts: &InputCounts) -> Result<(), Error> {
    if counts.bundle {
        Err(bundle_error())
    } else {
        Ok(())
    }
}

/// `estimate` of `counts`, the scan of `bytes`; when that is over `limit`
/// with Gmax taken at its bound, of an exact rescan instead (the bound is
/// only an upper bound). Returns the estimate and the counts it is of.
fn priced<'c>(
    counts: &'c InputCounts,
    bytes: &[u8],
    limit: u64,
    estimate: impl Fn(&InputCounts) -> u64,
) -> (u64, Cow<'c, InputCounts>) {
    let first = estimate(counts);
    if first <= limit || !counts.gmax_bounded || counts.truncated {
        return (first, Cow::Borrowed(counts));
    }
    let exact = scan_input_exact(bytes, Some(limit));
    (estimate(&exact), Cow::Owned(exact))
}

/// Check that loading the input `bytes` on its own, which the scan found
/// `counts` in, fits `limit` ([`InputCounts::load_estimate`]); also
/// rejects bundles.
pub(crate) fn check_load(
    counts: &InputCounts,
    bytes: &[u8],
    limit: Option<u64>,
) -> Result<(), Error> {
    check_no_bundle(counts)?;
    let Some(limit) = limit else { return Ok(()) };
    let (estimate, counts) = priced(counts, bytes, limit, InputCounts::load_estimate);
    if estimate > limit || counts.truncated {
        return Err(limit_error(
            LimitKind::Input,
            limit,
            estimate.max(limit.saturating_add(1)),
            counts.truncated,
            Some(counts.shown()),
        ));
    }
    Ok(())
}

/// Check that loading the input `bytes` (scanned: `counts`) after a
/// document with `base`'s counts, or applying it to that document, fits
/// `limit` ([`InputCounts::apply_estimate`]); also rejects bundles.
pub(crate) fn check_apply_input(
    counts: &InputCounts,
    bytes: &[u8],
    base: Base,
    limit: Option<u64>,
) -> Result<(), Error> {
    check_no_bundle(counts)?;
    let Some(limit) = limit else { return Ok(()) };
    let (estimate, counts) = priced(counts, bytes, limit, |c| c.apply_estimate(base));
    if estimate > limit || counts.truncated {
        return Err(limit_error(
            LimitKind::Input,
            limit,
            estimate.max(limit.saturating_add(1)),
            counts.truncated,
            Some(counts.shown()),
        ));
    }
    Ok(())
}

/// Check that only parsing the input fits `limit`
/// ([`InputCounts::parse_estimate`]); also rejects bundles. A scan that
/// stopped early (inflation beyond a tenth of the limit) counts as over
/// it: such input is refused however it is used.
pub(crate) fn check_parse(counts: &InputCounts, limit: Option<u64>) -> Result<(), Error> {
    check_no_bundle(counts)?;
    let Some(limit) = limit else { return Ok(()) };
    let estimate = counts.parse_estimate();
    if estimate > limit || counts.truncated {
        return Err(limit_error(
            LimitKind::Input,
            limit,
            estimate.max(limit.saturating_add(1)),
            counts.truncated,
            Some(counts.shown()),
        ));
    }
    Ok(())
}

/// Check that applying changes with `counts` to a document with `base`'s
/// counts fits `limit` ([`changes_estimate`]).
pub(crate) fn check_changes(
    counts: &ChangeCounts,
    base: Base,
    limit: Option<u64>,
) -> Result<(), Error> {
    let Some(limit) = limit else { return Ok(()) };
    let estimate = changes_estimate(counts, base);
    if estimate > limit {
        return Err(limit_error(
            LimitKind::Apply,
            limit,
            estimate,
            false,
            Some(counts.into()),
        ));
    }
    Ok(())
}

/// The counts of `saved`, a save about to become a stored value, checked
/// against `limit` ([`doc_estimate`]; Gmax exactly when its bound would
/// put it over); `kind` says which document it is.
pub(crate) fn check_saved(
    saved: &[u8],
    kind: LimitKind,
    limit: Option<u64>,
) -> Result<DocCounts, Error> {
    let scanned = scan_input(saved, None);
    let mut counts = scanned.as_document();
    if let Some(bound) = limit
        && doc_estimate(&counts) > bound
        && scanned.gmax_bounded
    {
        counts = scan_doc_exact(saved);
    }
    check_doc(&counts, kind, limit)?;
    Ok(counts)
}

/// Check that loading a document with `counts` fits `limit`
/// ([`doc_estimate`]); `kind` says which document it is.
pub(crate) fn check_doc(
    counts: &DocCounts,
    kind: LimitKind,
    limit: Option<u64>,
) -> Result<(), Error> {
    let Some(limit) = limit else { return Ok(()) };
    let estimate = doc_estimate(counts);
    if estimate > limit {
        return Err(limit_error(
            kind,
            limit,
            estimate,
            false,
            Some(counts.into()),
        ));
    }
    Ok(())
}

/// Check that `len` bytes of client input could fit the limit at all
/// (every byte costs 10 in the estimate): lets the text input function refuse a
/// huge literal before decoding its hex.
///
/// # Errors
///
/// [`Error::LoadLimit`] if `len` bytes alone exceed the limit.
pub fn check_input_len(len: usize) -> Result<(), Error> {
    let Some(limit) = limit() else { return Ok(()) };
    let estimate = (len as u64).saturating_mul(PER_BYTE);
    if estimate > limit {
        return Err(limit_error(LimitKind::Input, limit, estimate, true, None));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A cursor over bytes; every read is bounds-checked (`None` at the end or
/// on an over-long LEB128). `canonical` turns false when a LEB128 read with
/// [`Reader::uleb_c`] is overlong (Automerge rejects those in the places
/// it reads that way).
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    canonical: bool,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader {
            bytes,
            pos: 0,
            canonical: true,
        }
    }

    fn done(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn uleb(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for i in 0..10 {
            let byte = *self.bytes.get(self.pos)?;
            self.pos += 1;
            let bits = u64::from(byte & 0x7f);
            if i == 9 && bits > 1 {
                return None;
            }
            value |= bits << (7 * i);
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    /// [`Self::uleb`], noting an overlong encoding in `canonical`.
    fn uleb_c(&mut self) -> Option<u64> {
        let start = self.pos;
        let value = self.uleb()?;
        if self.pos - start > 1 && self.bytes[self.pos - 1] == 0 {
            self.canonical = false;
        }
        Some(value)
    }

    /// Step over up to `n` LEB128 values without decoding them: each ends
    /// at the first byte with the high bit clear, found eight bytes at a
    /// time. Returns how many were stepped over (fewer only at the end of
    /// the data, where a value without its last byte is not counted).
    /// Unlike [`Self::uleb`] it does not stop at a value longer than ten
    /// bytes, so it can count more values than decoding would, never fewer.
    fn skip_lebs(&mut self, n: u64) -> u64 {
        const HIGH: u64 = 0x8080_8080_8080_8080;
        let rest = &self.bytes[self.pos..];
        let mut left = n;
        let mut i = 0;
        // A whole word is stepped over only when it ends fewer values than
        // are left: its trailing bytes then belong to a value still to
        // step over, not to what follows the run.
        while left > 8 {
            let Some(word) = rest.get(i..i + 8) else {
                break;
            };
            let word = u64::from_le_bytes(word.try_into().unwrap_or([0; 8]));
            let ends = u64::from((!word & HIGH).count_ones());
            if ends >= left {
                break;
            }
            left -= ends;
            i += 8;
        }
        while left > 0 && i < rest.len() {
            if rest[i] & 0x80 == 0 {
                left -= 1;
            }
            i += 1;
        }
        if left > 0 {
            // A value cut off by the end of the data: not counted.
            self.pos = self.bytes.len();
        } else {
            self.pos += i;
        }
        n - left
    }

    /// Step over up to `n` length-prefixed strings; returns how many (a
    /// string that cannot be read ends it, as in [`Runs::value`]).
    fn skip_strings(&mut self, n: u64) -> u64 {
        for done in 0..n {
            if self.skip_string().is_none() {
                return done;
            }
        }
        n
    }

    /// Step over one length-prefixed string; returns its length (`None`:
    /// it cannot be read).
    fn skip_string(&mut self) -> Option<u64> {
        // One-byte lengths (below 128) inline, others decoded.
        let len = match self.bytes.get(self.pos) {
            Some(&b) if b < 0x80 => {
                self.pos += 1;
                usize::from(b)
            }
            _ => usize::try_from(self.uleb()?).ok()?,
        };
        self.take(len)?;
        Some(len as u64)
    }

    fn usize_c(&mut self) -> Option<usize> {
        usize::try_from(self.uleb_c()?).ok()
    }

    fn sleb(&mut self) -> Option<i64> {
        let mut value = 0i64;
        for i in 0..10 {
            let byte = *self.bytes.get(self.pos)?;
            self.pos += 1;
            let bits = i64::from(byte & 0x7f);
            if i == 9 && !(bits == 0 || bits == 0x7f) {
                return None;
            }
            value |= bits << (7 * i);
            if byte & 0x80 == 0 {
                let shift = 7 * (i + 1);
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Some(value);
            }
        }
        None
    }
}

/// A value of a column at some row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Val<'a> {
    Null,
    U(u64),
    I(i128),
    S(&'a [u8]),
    B(bool),
}

/// A stretch of rows of one column: `len` rows whose first value is
/// `first`; for a delta column each further row adds `step`, otherwise
/// every row has the same value.
#[derive(Debug, Clone, Copy)]
struct Seg<'a> {
    len: u64,
    first: Val<'a>,
    step: i128,
}

impl<'a> Seg<'a> {
    /// The value `k` rows into the segment.
    fn at(&self, k: u64) -> Val<'a> {
        match self.first {
            Val::I(v) if self.step != 0 => {
                Val::I(v.wrapping_add(self.step.wrapping_mul(i128::from(k))))
            }
            v => v,
        }
    }
}

/// The segments of a column, run by run: a repeat run is one segment, a
/// literal run one per value, a null run one. Decoding is lenient, like
/// Automerge's streaming decoders: anything that cannot be read ends the
/// column.
struct Runs<'a> {
    r: Reader<'a>,
    ty: u64,
    /// Values left in the current literal run.
    literal: u64,
    /// The running value of a delta column.
    acc: i128,
    /// The next value of a boolean column.
    bool_next: bool,
    ended: bool,
}

impl<'a> Runs<'a> {
    fn new(ty: u64, data: &'a [u8]) -> Self {
        Runs {
            r: Reader::new(data),
            ty,
            literal: 0,
            acc: 0,
            bool_next: false,
            ended: false,
        }
    }

    /// One value of the column's type (not for booleans).
    fn value(&mut self) -> Option<Val<'a>> {
        match self.ty {
            DELTA => Some(Val::I(i128::from(self.r.sleb()?))),
            STRING => {
                let len = usize::try_from(self.r.uleb()?).ok()?;
                Some(Val::S(self.r.take(len)?))
            }
            _ => Some(Val::U(self.r.uleb()?)),
        }
    }

    fn next_seg(&mut self) -> Option<Seg<'a>> {
        if self.ended {
            return None;
        }
        let seg = self.read_seg();
        if seg.is_none() {
            self.ended = true;
        }
        seg
    }

    fn read_seg(&mut self) -> Option<Seg<'a>> {
        if self.ty == BOOLEAN {
            loop {
                if self.r.done() {
                    return None;
                }
                let len = self.r.uleb()?;
                let value = self.bool_next;
                self.bool_next = !value;
                if len > 0 {
                    return Some(Seg {
                        len,
                        first: Val::B(value),
                        step: 0,
                    });
                }
            }
        }
        if self.ty == VALUE {
            return None;
        }
        loop {
            if self.literal > 0 {
                self.literal -= 1;
                let value = self.value()?;
                let first = match value {
                    Val::I(delta) => {
                        self.acc = self.acc.wrapping_add(delta);
                        Val::I(self.acc)
                    }
                    v => v,
                };
                return Some(Seg {
                    len: 1,
                    first,
                    step: 0,
                });
            }
            if self.r.done() {
                return None;
            }
            let n = self.r.sleb()?;
            if n > 0 {
                let len = n.unsigned_abs();
                let value = self.value()?;
                return Some(match value {
                    Val::I(delta) => {
                        let first = self.acc.wrapping_add(delta);
                        self.acc = self.acc.wrapping_add(delta.wrapping_mul(i128::from(len)));
                        Seg {
                            len,
                            first: Val::I(first),
                            step: delta,
                        }
                    }
                    v => Seg {
                        len,
                        first: v,
                        step: 0,
                    },
                });
            } else if n < 0 {
                self.literal = n.unsigned_abs();
            } else {
                let len = self.r.uleb()?;
                if len > 0 {
                    return Some(Seg {
                        len,
                        first: Val::Null,
                        step: 0,
                    });
                }
            }
        }
    }
}

/// Rows of a column, for a group column the sum of its values, and for a
/// string column the bytes its repeat runs expand to beyond the one
/// value each holds.
#[derive(Debug, Clone, Copy, Default)]
struct ColumnStats {
    rows: u64,
    sum: u64,
    /// For each repeat run of `n` strings of `len` bytes, `(n - 1) * len`.
    excess: u64,
}

/// Rows of a column, for a group column the sum of its values, for a
/// string column what its repeat runs expand to ([`ColumnStats::excess`]):
/// run by run. Literal values are only stepped over: numbers by their LEB128
/// last bytes, eight bytes at a time ([`Reader::skip_lebs`]), strings by
/// their lengths; a group's are summed. Lenient like [`Runs`]: anything
/// that cannot be read ends the column (except an over-long literal
/// number, which is stepped over like any other: more rows, never fewer).
fn column_stats(ty: u64, data: &[u8], ticker: &mut Ticker) -> ColumnStats {
    let mut stats = ColumnStats::default();
    let mut r = Reader::new(data);
    match ty {
        VALUE => return stats,
        BOOLEAN => {
            while !r.done() {
                ticker.tick();
                let Some(n) = r.uleb() else { break };
                stats.rows = stats.rows.saturating_add(n);
            }
            return stats;
        }
        _ => {}
    }
    // Step over `n` values that do not matter; returns how many.
    let skip = |r: &mut Reader<'_>, n: u64| {
        if ty == STRING {
            r.skip_strings(n)
        } else {
            r.skip_lebs(n)
        }
    };
    'runs: while !r.done() {
        ticker.tick();
        // A one-byte positive run length inline, anything else decoded.
        let n = match r.bytes.get(r.pos) {
            Some(&b) if b < 0x40 => {
                r.pos += 1;
                i64::from(b)
            }
            _ => {
                let Some(n) = r.sleb() else { break };
                n
            }
        };
        let len = n.unsigned_abs();
        if n == 0 {
            // A null run.
            let Some(k) = r.uleb() else { break };
            stats.rows = stats.rows.saturating_add(k);
        } else if ty == GROUP {
            // Group sizes, summed.
            let values = if n > 0 { 1 } else { len };
            for _ in 0..values {
                let Some(v) = r.uleb() else { break 'runs };
                let rows = if n > 0 { len } else { 1 };
                stats.rows = stats.rows.saturating_add(rows);
                stats.sum = stats.sum.saturating_add(rows.saturating_mul(v));
            }
        } else if n > 0 {
            // A repeat run: one value.
            if ty == STRING {
                let Some(bytes) = r.skip_string() else { break };
                let extra = (len - 1).saturating_mul(bytes);
                stats.excess = stats.excess.saturating_add(extra);
            } else if skip(&mut r, 1) == 0 {
                break;
            }
            stats.rows = stats.rows.saturating_add(len);
        } else {
            // A literal run.
            let stepped = skip(&mut r, len);
            stats.rows = stats.rows.saturating_add(stepped);
            if stepped < len {
                break;
            }
        }
    }
    stats
}

/// A column of a chunk: its spec (deflate bit clear) and inflated data.
struct Column<'a> {
    spec: u64,
    data: Cow<'a, [u8]>,
}

/// Why a chunk's scan stopped.
enum Stop {
    /// It does not parse.
    Malformed,
    /// The inflated bytes exceed the cap, or a list of other actors or a
    /// column metadata block is longer than the limit pays for.
    Truncated,
}

struct Scanner {
    /// The limit, `None` without one.
    limit: Option<u64>,
    /// The largest number of bytes inflation may still produce (the limit
    /// over [`PER_BYTE`]), `None` without a limit.
    cap: Option<u64>,
    /// Bytes read so far, deflated columns and chunks inflated.
    inflated: u64,
    /// Bytes produced by inflation so far (bounded by `cap`).
    deflated_out: u64,
    /// Distinct actors of change chunks and later document chunks.
    actors: HashSet<Vec<u8>>,
    /// Compute Gmax exactly (otherwise its upper bound, the successors).
    exact_gmax: bool,
    /// Some Gmax was taken at its bound.
    gmax_bounded: bool,
    /// Keep the inflated columns of the next document chunk in `kept`.
    keep: bool,
    /// The inflated columns of the first document chunk (when `keep`).
    kept: Vec<Vec<u8>>,
    ticker: Ticker,
}

impl Scanner {
    fn new(limit: Option<u64>, exact_gmax: bool) -> Self {
        Scanner {
            limit,
            cap: limit.map(|l| l / PER_BYTE),
            inflated: 0,
            deflated_out: 0,
            actors: HashSet::new(),
            exact_gmax,
            gmax_bounded: false,
            keep: false,
            kept: Vec::new(),
            ticker: Ticker::default(),
        }
    }

    /// Inflate a deflated column or compressed chunk, within the cap: an
    /// inflated byte costs [`PER_BYTE`] in every estimate, so more than
    /// the cap exceeds the limit whatever the input describes.
    fn inflate(&mut self, raw: &[u8]) -> Result<Vec<u8>, Stop> {
        let mut out = Vec::new();
        let decoder = flate2::bufread::DeflateDecoder::new(raw);
        let result = match self.cap {
            Some(cap) => {
                let room = cap.saturating_sub(self.deflated_out);
                decoder.take(room.saturating_add(1)).read_to_end(&mut out)
            }
            None => {
                let mut decoder = decoder;
                decoder.read_to_end(&mut out)
            }
        };
        result.map_err(|_| Stop::Malformed)?;
        let n = out.len() as u64;
        self.deflated_out = self.deflated_out.saturating_add(n);
        self.count(n);
        match self.cap {
            Some(cap) if self.deflated_out > cap => Err(Stop::Truncated),
            _ => Ok(out),
        }
    }

    /// Note an actor id (by name: counted once however often it is
    /// listed; a repeat allocates nothing).
    fn actor(&mut self, id: &[u8]) {
        if !self.actors.contains(id) {
            self.actors.insert(id.to_vec());
        }
    }

    /// Count `n` bytes read.
    fn count(&mut self, n: u64) {
        self.inflated = self.inflated.saturating_add(n);
    }

    /// A column metadata block, validated in place (nothing is allocated
    /// per entry: a block can list millions of empty columns). Stops the
    /// scan when its entries beyond `known`'s alone exceed the limit (each
    /// is charged [`PER_EXTRA_COLUMN`]), before they are read.
    fn metadata<'a>(&mut self, r: &mut Reader<'a>, known: &[u64]) -> Result<Meta<'a>, Stop> {
        let count = r.usize_c().ok_or(Stop::Malformed)?;
        // Each entry takes at least two bytes.
        if count > r.bytes.len() / 2 {
            return Err(Stop::Malformed);
        }
        let extra = extra_entries(count, known);
        if self
            .limit
            .is_some_and(|l| u128::from(extra) * PER_EXTRA_COLUMN > u128::from(l))
        {
            return Err(Stop::Truncated);
        }
        let start = r.pos;
        let mut layout = Layout::new(known);
        for _ in 0..count {
            self.ticker.tick();
            let spec = r.uleb_c().ok_or(Stop::Malformed)?;
            if spec > u64::from(u32::MAX) {
                return Err(Stop::Malformed);
            }
            r.usize_c().ok_or(Stop::Malformed)?;
            layout.step(spec & !DEFLATE_BIT);
        }
        Ok(Meta {
            entries: &r.bytes[start..r.pos],
            count,
            extra,
            layout: layout.ok,
        })
    }

    /// Read the data of the columns listed in `meta` from `r`, in order,
    /// inflating deflated ones (`deflate`: the deflate bit is allowed,
    /// document chunks; otherwise it makes the chunk malformed, change
    /// chunks), and hand each to `visit` with its spec (deflate bit clear).
    /// Returns the first column of each spec in `hold` (for the passes
    /// that merge columns) and whether one of those specs is listed again.
    /// With [`Scanner::keep`], every inflated column is kept, in order.
    fn columns<'a>(
        &mut self,
        r: &mut Reader<'a>,
        meta: &Meta<'_>,
        deflate: bool,
        hold: &[u64],
        mut visit: impl FnMut(&mut Self, u64, &[u8]),
    ) -> Result<(Vec<Column<'a>>, bool), Stop> {
        let (mut held, mut repeated) = (Vec::<Column<'a>>::new(), false);
        let mut entries = Reader::new(meta.entries);
        for _ in 0..meta.count {
            self.ticker.tick();
            // Validated by `metadata`.
            let (Some(spec), Some(len)) = (entries.uleb(), entries.usize_c()) else {
                return Err(Stop::Malformed);
            };
            let raw = r.take(len).ok_or(Stop::Malformed)?;
            let data = if spec & DEFLATE_BIT != 0 {
                if !deflate {
                    return Err(Stop::Malformed);
                }
                // The raw bytes were counted with the chunk: count the
                // inflated ones instead.
                self.inflated = self.inflated.saturating_sub(len as u64);
                Cow::Owned(self.inflate(raw)?)
            } else {
                Cow::Borrowed(raw)
            };
            let spec = spec & !DEFLATE_BIT;
            visit(self, spec, &data);
            let first = hold.contains(&spec) && !held.iter().any(|c| c.spec == spec);
            repeated |= hold.contains(&spec) && !first;
            match data {
                Cow::Owned(mut data) if self.keep => {
                    // Held through the load: no spare capacity (what is
                    // held is exactly the inflated bytes, each charged).
                    data.shrink_to_fit();
                    if first {
                        self.kept.push(data.clone());
                        held.push(Column {
                            spec,
                            data: Cow::Owned(data),
                        });
                    } else {
                        self.kept.push(data);
                    }
                }
                data if first => held.push(Column { spec, data }),
                _ => {}
            }
        }
        Ok((held, repeated))
    }

    /// The bytes of long actor ids (`long`: index and length of every
    /// actor longer than [`INLINE_ACTOR`], by index) that the runs of an
    /// actor column refer to: each run's length, at most `cap`, times the
    /// actor's length.
    fn long_actor_bytes(&mut self, long: &[(u64, u64)], data: &[u8], cap: u64) -> u64 {
        let mut sum = 0u64;
        let mut runs = Runs::new(1, data);
        while let Some(seg) = runs.next_seg() {
            self.ticker.tick();
            if let Val::U(idx) = seg.first
                && let Ok(at) = long.binary_search_by_key(&idx, |&(i, _)| i)
            {
                sum = sum.saturating_add(seg.len.min(cap).saturating_mul(long[at].1));
            }
        }
        sum
    }

    /// Scan a document chunk's data. Returns its counts and whether it
    /// passes everything Automerge's chunk parse checks (conservatively,
    /// see [`crate::header::document_parses`]).
    fn doc_chunk(&mut self, data: &[u8], counts: &mut DocCounts) -> Result<bool, Stop> {
        let mut r = Reader::new(data);
        let malformed = || Stop::Malformed;
        let actors = r.usize_c().ok_or_else(malformed)?;
        if actors > data.len() {
            return Err(Stop::Malformed);
        }
        counts.actors = actors as u64;
        // The actors longer than an `ActorId` holds inline: (index, length).
        let mut long = Vec::new();
        for i in 0..actors {
            self.ticker.tick();
            let len = r.usize_c().ok_or_else(malformed)?;
            r.take(len).ok_or_else(malformed)?;
            if len > INLINE_ACTOR {
                long.push((i as u64, len as u64));
            }
        }
        let heads = r.usize_c().ok_or_else(malformed)?;
        r.take(heads.checked_mul(32).ok_or_else(malformed)?)
            .ok_or_else(malformed)?;
        let change_meta = self.metadata(&mut r, &DOC_CHANGE_SPECS)?;
        let op_meta = self.metadata(&mut r, &DOC_OP_SPECS)?;
        let layout = change_meta.layout && op_meta.layout;
        // The rows, groups and repeat runs of the columns, and the long
        // actors they refer to: each change's own (the change actor
        // column), and the other actors its ops refer to (the object and
        // key actor columns, each at most once per change).
        let (mut change_tally, mut op_tally) = (Tally::default(), Tally::default());
        let (mut own, mut referred) = (0u64, 0u64);
        self.columns(&mut r, &change_meta, true, &[], |s, spec, data| {
            change_tally.add(&DOC_CHANGE_BLOCK, spec, data, &mut s.ticker);
            if spec == CHANGE_ACTOR && !long.is_empty() {
                own = own.saturating_add(s.long_actor_bytes(&long, data, u64::MAX));
            }
        })?;
        let changes = change_tally.rows;
        let hold: &[u64] = if self.exact_gmax { &GMAX_SPECS } else { &[] };
        let (op_cols, repeated) = self.columns(&mut r, &op_meta, true, hold, |s, spec, data| {
            op_tally.add(&DOC_OP_BLOCK, spec, data, &mut s.ticker);
            if (spec == OBJ_ACTOR || spec == KEY_ACTOR) && !long.is_empty() {
                let bytes = s.long_actor_bytes(&long, data, changes);
                referred = referred.saturating_add(bytes);
            }
        })?;
        // Only the first chunk's columns are kept.
        self.keep = false;
        // The head indices: absent (older JS saves), or one per head, then
        // the end of the chunk (Automerge rejects leftover data).
        if !r.done() {
            for _ in 0..heads {
                r.uleb_c().ok_or_else(malformed)?;
            }
            if !r.done() {
                return Err(Stop::Malformed);
            }
        }
        let (ops, succ) = (op_tally.rows, op_tally.entries);
        // A change refers to another actor for each successor entry, at
        // most three times (a rebuilt op's pred, or a delete Automerge
        // rebuilds from it: its object, key and pred). It holds every
        // actor once at most, its own included, so its own and the others
        // are at most every long actor.
        let longest = long.iter().map(|&(_, len)| len).max().unwrap_or(0);
        let all = long
            .iter()
            .map(|&(_, len)| len)
            .fold(0, u64::saturating_add);
        let others = referred.saturating_add(succ.saturating_mul(3).saturating_mul(longest));
        let actors_held = own.saturating_add(others).min(changes.saturating_mul(all));
        // Every rebuilt change holds its own message, its actors and its
        // ops' mark names (each costs about as much as a message), and its
        // ops' keys (in its bytes).
        counts.rebuilt = change_tally
            .excess
            .saturating_add(actors_held)
            .saturating_add(op_tally.mark_excess);
        counts.rebuilt_keys = op_tally.excess.saturating_sub(op_tally.mark_excess);
        counts.extra_columns = change_meta.extra.saturating_add(op_meta.extra);
        counts.changes = changes;
        counts.deps = change_tally.entries;
        counts.ops = ops;
        counts.succ = succ;
        counts.gmax = if succ == 0 {
            0
        } else if self.exact_gmax && !repeated {
            gmax(&op_cols, ops, &mut self.ticker)
        } else {
            // At its bound (also for a column listed twice: which of them
            // Automerge reads is not modelled, the bound holds for both).
            self.gmax_bounded |= !self.exact_gmax;
            succ
        };
        Ok(layout && r.canonical)
    }

    /// Scan a change chunk's data (inflated if it was compressed).
    fn change_chunk(&mut self, data: &[u8], counts: &mut ChangeCounts) -> Result<(), Stop> {
        let mut r = Reader::new(data);
        let malformed = || Stop::Malformed;
        let deps = r.usize_c().ok_or_else(malformed)?;
        r.take(deps.checked_mul(32).ok_or_else(malformed)?)
            .ok_or_else(malformed)?;
        let len = r.usize_c().ok_or_else(malformed)?;
        self.actor(r.take(len).ok_or_else(malformed)?);
        r.uleb().ok_or_else(malformed)?; // seq
        r.uleb().ok_or_else(malformed)?; // start op
        r.sleb().ok_or_else(malformed)?; // time
        let len = r.usize_c().ok_or_else(malformed)?;
        r.take(len).ok_or_else(malformed)?; // message
        let others = r.usize_c().ok_or_else(malformed)?;
        if others > data.len() - r.pos {
            return Err(Stop::Malformed);
        }
        // The entries alone (each at least a byte) exceed the limit: stop
        // before stepping through them (millions of one-byte entries
        // inflate from a few kB).
        if self.limit.is_some_and(|l| {
            others as u128 * (PER_OTHER_ACTOR + u128::from(PER_BYTE)) > u128::from(l)
        }) {
            return Err(Stop::Truncated);
        }
        // Every entry is priced, duplicates too (Automerge keeps them
        // all), including those before a point where the list stops
        // parsing (Automerge has allocated for them when it fails).
        for _ in 0..others {
            self.ticker.tick();
            let Some(id) = r.usize_c().and_then(|len| r.take(len)) else {
                return Err(Stop::Malformed);
            };
            counts.other_actor_entries = counts.other_actor_entries.saturating_add(1);
            self.actor(id);
        }
        let meta = self.metadata(&mut r, &CHANGE_OP_SPECS)?;
        let mut tally = Tally::default();
        self.columns(&mut r, &meta, false, &[], |s, spec, data| {
            tally.add(&CHANGE_OP_BLOCK, spec, data, &mut s.ticker);
        })?;
        // Applying the change copies every op's key and mark name.
        counts.repeated = counts.repeated.saturating_add(tally.excess);
        counts.extra_columns = counts.extra_columns.saturating_add(meta.extra);
        counts.ops = counts.ops.saturating_add(tally.rows);
        counts.preds = counts.preds.saturating_add(tally.entries);
        counts.changes = counts.changes.saturating_add(1);
        counts.deps = counts.deps.saturating_add(deps as u64);
        Ok(())
    }
}

/// A column metadata block, validated ([`Scanner::metadata`]).
struct Meta<'a> {
    /// Its entries' bytes.
    entries: &'a [u8],
    /// How many.
    count: usize,
    /// Entries beyond one per column Automerge writes ([`extra_entries`]).
    extra: u64,
    /// The specs follow [`Layout`]'s rule.
    layout: bool,
}

/// The entries of a metadata block of `count` beyond one per column of
/// `known` (the columns Automerge writes, whose entries the per-op and
/// per-change costs include).
fn extra_entries(count: usize, known: &[u64]) -> u64 {
    count.saturating_sub(known.len()) as u64
}

/// The layout rule Automerge's column parser enforces, conservatively,
/// checked spec by spec: the specs (deflate bit cleared) are a
/// subsequence of the specs Automerge writes, in that order, and every
/// value column comes right after its metadata column (see
/// [`crate::header::document_parses`]).
struct Layout<'k> {
    /// The specs a next one may be (the rest of the known ones).
    rest: std::slice::Iter<'k, u64>,
    prev: Option<u64>,
    ok: bool,
}

impl<'k> Layout<'k> {
    fn new(known: &'k [u64]) -> Self {
        Layout {
            rest: known.iter(),
            prev: None,
            ok: true,
        }
    }

    fn step(&mut self, spec: u64) {
        let value_ok = spec & 0x07 != VALUE || self.prev == Some(spec - 1);
        self.ok = self.ok && value_ok && self.rest.any(|&k| k == spec);
        self.prev = Some(spec);
    }
}

/// Which columns of a metadata block count what (see [`Tally`]).
struct Block {
    /// The columns Automerge writes (the others are not counted).
    known: &'static [u64],
    /// The group column whose values are summed, and its members.
    group: u64,
    members: &'static [u64],
    /// The string columns whose repeat runs are copied.
    strings: &'static [u64],
}

const DOC_CHANGE_BLOCK: Block = Block {
    known: &DOC_CHANGE_SPECS,
    group: DEPS_GROUP,
    members: &[DEPS_MEMBER],
    strings: &[MESSAGE],
};
const DOC_OP_BLOCK: Block = Block {
    known: &DOC_OP_SPECS,
    group: SUCC_GROUP,
    members: &SUCC_MEMBERS,
    strings: &[KEY_STR, MARK_NAME],
};
const CHANGE_OP_BLOCK: Block = Block {
    known: &CHANGE_OP_SPECS,
    group: PRED_GROUP,
    members: &PRED_MEMBERS,
    strings: &[KEY_STR, MARK_NAME],
};

/// The columns [`gmax`] merges.
const GMAX_SPECS: [u64; 7] = [
    OBJ_ACTOR, OBJ_CTR, KEY_ACTOR, KEY_CTR, KEY_STR, INSERT, SUCC_GROUP,
];

/// What the columns of a metadata block add up to, column by column.
#[derive(Debug, Clone, Copy, Default)]
struct Tally {
    /// Rows of the known non-member, non-value columns (the largest).
    rows: u64,
    /// The group's entries: the larger of its sum and its members' rows.
    entries: u64,
    /// What the repeat runs of the string columns expand to, summed.
    excess: u64,
    /// The part of [`Tally::excess`] that is the mark name column's.
    mark_excess: u64,
}

impl Tally {
    fn add(&mut self, block: &Block, spec: u64, data: &[u8], ticker: &mut Ticker) {
        let ty = spec & 7;
        if ty == VALUE || !block.known.contains(&spec) {
            return;
        }
        let stats = column_stats(ty, data, ticker);
        if spec == block.group {
            self.entries = self.entries.max(stats.sum);
        }
        if block.members.contains(&spec) {
            self.entries = self.entries.max(stats.rows);
        } else {
            self.rows = self.rows.max(stats.rows);
        }
        if block.strings.contains(&spec) {
            self.excess = self.excess.saturating_add(stats.excess);
            if spec == MARK_NAME {
                self.mark_excess = self.mark_excess.saturating_add(stats.excess);
            }
        }
    }
}

/// A row's object (actor, counter), key (actor, counter, string) and
/// whether it is an insert.
type RowKey<'a> = ((Val<'a>, Val<'a>), (Val<'a>, Val<'a>, Val<'a>), bool);

/// A column's segments with a position inside the current one; past its
/// end a column reads as null (a missing column too).
struct Cursor<'a> {
    runs: Option<Runs<'a>>,
    seg: Option<Seg<'a>>,
    offset: u64,
}

impl<'a> Cursor<'a> {
    fn new(cols: &'a [Column<'a>], spec: u64) -> Self {
        let mut runs = cols
            .iter()
            .find(|c| c.spec == spec)
            .map(|c| Runs::new(spec & 7, &c.data));
        let seg = runs.as_mut().and_then(Runs::next_seg);
        Cursor {
            runs,
            seg,
            offset: 0,
        }
    }

    /// Rows left in the current segment (`u64::MAX` past the end).
    fn left(&self) -> u64 {
        self.seg.map_or(u64::MAX, |s| s.len - self.offset)
    }

    /// The value `k` rows ahead within the current segment.
    fn at(&self, k: u64) -> Val<'a> {
        self.seg.map_or(Val::Null, |s| s.at(self.offset + k))
    }

    /// Whether the value changes from row to row in the current segment.
    fn varying(&self) -> bool {
        self.seg.is_some_and(|s| s.step != 0)
    }

    fn advance(&mut self, n: u64) {
        let Some(seg) = self.seg else { return };
        self.offset += n;
        if self.offset >= seg.len {
            self.offset = 0;
            self.seg = self.runs.as_mut().and_then(Runs::next_seg);
        }
    }
}

/// [`DocCounts::gmax`]: the largest number of successor entries in one
/// group of rows, where a new group starts at the first row, at an insert
/// row, when the object changes, or when the key changes and the previous
/// row was not an insert. Computed run by run: within a stretch of rows
/// where every one of these columns is constant (or the key counter
/// changes by a fixed step), either every row is a group of its own
/// (inserts, or a key that changes every row) or all rows add to the
/// current group. `tests/memory_bounds.rs` checks it against the row by
/// row definition.
fn gmax(cols: &[Column<'_>], rows: u64, ticker: &mut Ticker) -> u64 {
    let mut obj_actor = Cursor::new(cols, OBJ_ACTOR);
    let mut obj_ctr = Cursor::new(cols, OBJ_CTR);
    let mut key_actor = Cursor::new(cols, KEY_ACTOR);
    let mut key_ctr = Cursor::new(cols, KEY_CTR);
    let mut key_str = Cursor::new(cols, KEY_STR);
    let mut insert = Cursor::new(cols, INSERT);
    let mut succ = Cursor::new(cols, SUCC_GROUP);

    let (mut best, mut cur) = (0u64, 0u64);
    // The previous row's object, key, and whether it was an insert.
    let mut prev: Option<RowKey<'_>> = None;
    let mut row = 0u64;
    while row < rows {
        ticker.tick();
        let m = [
            &obj_actor, &obj_ctr, &key_actor, &key_ctr, &key_str, &insert, &succ,
        ]
        .iter()
        .map(|c| c.left())
        .min()
        .unwrap_or(u64::MAX)
        .min(rows - row);
        let obj = (obj_actor.at(0), obj_ctr.at(0));
        let key = (key_actor.at(0), key_ctr.at(0), key_str.at(0));
        let ins = insert.at(0) == Val::B(true);
        let g = match succ.at(0) {
            Val::U(g) => g,
            _ => 0,
        };
        let starts_group = match prev {
            None => true,
            Some((prev_obj, prev_key, prev_ins)) => {
                ins || obj != prev_obj || (key != prev_key && !prev_ins)
            }
        };
        if starts_group {
            cur = 0;
        }
        cur = cur.saturating_add(g);
        best = best.max(cur);
        if m > 1 {
            if ins || key_ctr.varying() {
                cur = g;
            } else {
                cur = cur.saturating_add(g.saturating_mul(m - 1));
            }
            best = best.max(cur);
        }
        prev = Some((obj, (key.0, key_ctr.at(m - 1), key.2), ins));
        for c in [
            &mut obj_actor,
            &mut obj_ctr,
            &mut key_actor,
            &mut key_ctr,
            &mut key_str,
            &mut insert,
            &mut succ,
        ] {
            c.advance(m);
        }
        row += m;
    }
    best
}

/// Scan external input (any sequence of chunks) for what loading it would
/// cost. `limit` bounds the inflation (to a tenth of it, see the module
/// docs); `None` inflates everything. Gmax is taken at its upper bound,
/// the number of successor entries ([`InputCounts::gmax_bounded`]); the
/// checks rescan exactly ([`scan_input_exact`]) when that bound is what
/// puts an estimate over the limit. Never fails and never panics.
pub fn scan_input(bytes: &[u8], limit: Option<u64>) -> InputCounts {
    scan(bytes, limit, false, false)
}

/// [`scan_input`] with Gmax computed exactly (merging the object, key,
/// insert and successor columns run by run).
pub fn scan_input_exact(bytes: &[u8], limit: Option<u64>) -> InputCounts {
    scan(bytes, limit, true, false)
}

/// [`scan_input`], also keeping the inflated columns of input that is one
/// document chunk with deflated columns ([`InputCounts::inflated_columns`]).
pub fn scan_input_keep(bytes: &[u8], limit: Option<u64>) -> InputCounts {
    scan(bytes, limit, false, true)
}

fn scan(bytes: &[u8], limit: Option<u64>, exact_gmax: bool, keep: bool) -> InputCounts {
    let mut scanner = Scanner::new(limit, exact_gmax);
    scanner.keep = keep;
    let mut counts = InputCounts::default();
    let mut r = Reader::new(bytes);
    let mut chunks = 0usize;
    let mut single_doc_parses = false;
    while !r.done() {
        let header = (|| {
            if r.take(4)? != MAGIC {
                return None;
            }
            r.take(4)?; // checksum (Automerge checks it)
            let chunk_type = r.take(1)?[0];
            let len = r.usize_c()?;
            Some((chunk_type, r.take(len)?))
        })();
        let Some((chunk_type, data)) = header else {
            counts.malformed = true;
            break;
        };
        chunks += 1;
        // Only the first chunk's columns are kept.
        scanner.keep &= chunks == 1;
        // Header bytes, and the chunk's data as it is (deflated columns
        // are counted inflated instead when they are inflated).
        scanner.count(data.len() as u64 + 20);
        let result = match chunk_type {
            DOCUMENT_CHUNK => {
                let mut doc = DocCounts::default();
                let inflated_before = scanner.inflated;
                let result = scanner.doc_chunk(data, &mut doc);
                doc.inflated =
                    scanner.inflated.saturating_sub(inflated_before) + data.len() as u64 + 20;
                if chunks == 1 {
                    counts.first_doc = doc;
                    if let Ok(parses) = result {
                        single_doc_parses = parses && r.canonical;
                    }
                } else {
                    counts.later_docs.add(&doc);
                    let mut as_changes = doc.as_changes();
                    // Their actors are counted by name with the changes'.
                    as_changes.actors = 0;
                    counts.changes.add(&as_changes);
                    let mut ar = Reader::new(data);
                    if let Some(n) = ar.uleb() {
                        for _ in 0..n.min(data.len() as u64) {
                            scanner.ticker.tick();
                            let Some(len) = ar.uleb().and_then(|l| usize::try_from(l).ok()) else {
                                break;
                            };
                            let Some(id) = ar.take(len) else { break };
                            scanner.actor(id);
                        }
                    }
                }
                result.map(|_| ())
            }
            CHANGE_CHUNK => scanner.change_chunk(data, &mut counts.changes),
            COMPRESSED_CHUNK => scanner
                .inflate(data)
                .and_then(|inflated| scanner.change_chunk(&inflated, &mut counts.changes)),
            BUNDLE_CHUNK => {
                counts.bundle = true;
                Err(Stop::Malformed)
            }
            _ => Err(Stop::Malformed),
        };
        match result {
            Ok(()) => {}
            Err(Stop::Malformed) => {
                counts.malformed = !counts.bundle;
                break;
            }
            Err(Stop::Truncated) => {
                counts.truncated = true;
                break;
            }
        }
    }
    counts.changes.actors = scanner.actors.len() as u64;
    counts.deflated_out = scanner.deflated_out;
    counts.gmax_bounded = scanner.gmax_bounded;
    // Change chunks' bytes: everything inflated that is not a document's.
    counts.changes.inflated = scanner
        .inflated
        .saturating_sub(counts.first_doc.inflated)
        .saturating_sub(counts.later_docs.inflated);
    counts.single_doc_parses = chunks == 1
        && single_doc_parses
        && !counts.malformed
        && !counts.truncated
        && counts.later_docs == DocCounts::default();
    if chunks == 1 && !counts.malformed && !counts.truncated && !scanner.kept.is_empty() {
        counts.inflated_columns = Some(scanner.kept);
    }
    counts
}

/// Whether `bytes` holds a bundle chunk, from the chunk headers alone
/// (nothing else is checked; for the paths that skip the scan without a
/// limit).
pub fn has_bundle(bytes: &[u8]) -> bool {
    let mut r = Reader::new(bytes);
    while !r.done() {
        let header = (|| {
            if r.take(4)? != MAGIC {
                return None;
            }
            r.take(4)?;
            let chunk_type = r.take(1)?[0];
            let len = usize::try_from(r.uleb()?).ok()?;
            r.take(len)?;
            Some(chunk_type)
        })();
        match header {
            Some(BUNDLE_CHUNK) => return true,
            Some(_) => {}
            None => return false,
        }
    }
    false
}

/// The counts of changes about to be applied, from their chunks
/// (`Change::raw_bytes()`, uncompressed change chunks), with their
/// distinct actors. Never fails; what does not parse is not counted
/// (these are changes Automerge built).
pub fn scan_changes<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> ChangeCounts {
    let mut scanner = Scanner::new(None, true);
    let mut counts = ChangeCounts::default();
    for chunk in chunks {
        let mut r = Reader::new(chunk);
        let data = (|| {
            r.take(8)?;
            let chunk_type = r.take(1)?[0];
            let len = usize::try_from(r.uleb()?).ok()?;
            Some((chunk_type, r.take(len)?))
        })();
        match data {
            Some((CHANGE_CHUNK, data)) => {
                scanner.count(data.len() as u64 + 20);
                let _ = scanner.change_chunk(data, &mut counts);
            }
            Some((COMPRESSED_CHUNK, data)) => {
                scanner.count(20);
                if let Ok(inflated) = scanner.inflate(data) {
                    let _ = scanner.change_chunk(&inflated, &mut counts);
                }
            }
            _ => {}
        }
    }
    counts.actors = scanner.actors.len() as u64;
    counts.inflated = scanner.inflated;
    counts
}

/// The action column of a document chunk's ops.
const ACTION: u64 = 0x42;
/// The action of an op that makes a map (`Action::MakeMap` in Automerge's
/// encoding).
const MAKE_MAP: u64 = 0;

/// Whether `is_text` holds for an object of the document chunk `bytes` in
/// which an op makes a map (the root, a map, is never asked about): what
/// [`crate::blocks`] looks for, text objects whose map elements are
/// blocks. `is_text` gets the object's actor id, its index in the chunk's
/// actor table and its counter, once per object, in chunk order, until it
/// returns `true`. Every op of the chunk counts, deleted and overwritten
/// ones too, so the answer covers every historical state. Run by run: the
/// object and action columns are run-length encoded, so this costs about
/// one step per run, not per op.
///
/// `None` unless `bytes` is exactly one document chunk (columns deflated
/// or not) that scans cleanly; callers then assume the worst.
pub(crate) fn any_map_parent(
    bytes: &[u8],
    mut is_text: impl FnMut(&[u8], usize, u64) -> bool,
) -> Option<bool> {
    let mut r = Reader::new(bytes);
    if r.take(4)? != MAGIC {
        return None;
    }
    r.take(4)?; // checksum
    if r.take(1)?[0] != DOCUMENT_CHUNK {
        return None;
    }
    let len = usize::try_from(r.uleb()?).ok()?;
    let data = r.take(len)?;
    if !r.done() {
        return None;
    }
    let mut r = Reader::new(data);
    let count = usize::try_from(r.uleb()?).ok()?;
    if count > data.len() {
        return None;
    }
    let mut actors = Vec::with_capacity(count);
    for _ in 0..count {
        let len = usize::try_from(r.uleb()?).ok()?;
        actors.push(r.take(len)?);
    }
    let heads = usize::try_from(r.uleb()?).ok()?;
    r.take(heads.checked_mul(32)?)?;
    let mut scanner = Scanner::new(None, false);
    let change_meta = scanner.metadata(&mut r, &DOC_CHANGE_SPECS).ok()?;
    let op_meta = scanner.metadata(&mut r, &DOC_OP_SPECS).ok()?;
    scanner
        .columns(&mut r, &change_meta, true, &[], |_, _, _| {})
        .ok()?;
    let (cols, repeated) = scanner
        .columns(
            &mut r,
            &op_meta,
            true,
            &[OBJ_ACTOR, OBJ_CTR, ACTION],
            |_, _, _| {},
        )
        .ok()?;
    if repeated {
        // Which of two columns with one spec Automerge reads is not
        // modelled.
        return None;
    }
    let mut obj_actor = Cursor::new(&cols, OBJ_ACTOR);
    let mut obj_ctr = Cursor::new(&cols, OBJ_CTR);
    let mut action = Cursor::new(&cols, ACTION);
    // The last object asked about (an object's ops are contiguous).
    let mut asked = None;
    let mut ticker = Ticker::default();
    // The action column has a row for every op: past its end there are no
    // more ops (the other two read as null there).
    while action.seg.is_some() {
        ticker.tick();
        let m = [&obj_actor, &obj_ctr, &action]
            .iter()
            .map(|c| c.left())
            .min()
            .unwrap_or(u64::MAX);
        if action.at(0) == Val::U(MAKE_MAP)
            && let (Val::U(actor), Val::U(ctr)) = (obj_actor.at(0), obj_ctr.at(0))
            && asked != Some((actor, ctr))
        {
            asked = Some((actor, ctr));
            // An actor index outside the table: not what Automerge
            // loaded; assume the worst.
            let Some((index, id)) = usize::try_from(actor)
                .ok()
                .and_then(|i| Some((i, *actors.get(i)?)))
            else {
                return Some(true);
            };
            if is_text(id, index, ctr) {
                return Some(true);
            }
        }
        for c in [&mut obj_actor, &mut obj_ctr, &mut action] {
            c.advance(m);
        }
    }
    Some(false)
}

/// The counts of a stored value (one document chunk, uncompressed): what
/// loading it costs, [`doc_estimate`]. For anything else, the counts of
/// the document a load of it would build ([`InputCounts::as_document`]).
/// Never fails.
/// Gmax is taken at its bound, as in [`scan_input`].
pub fn scan_doc(bytes: &[u8]) -> DocCounts {
    scan_input(bytes, None).as_document()
}

/// [`scan_doc`] with Gmax computed exactly.
pub fn scan_doc_exact(bytes: &[u8]) -> DocCounts {
    scan_input_exact(bytes, None).as_document()
}

/// The largest number of successor entries in one (object, key) group of
/// a document chunk, row by row, by the definition in [`gmax`]: the
/// reference for the tests (slow; `None` if `bytes` is not one document
/// chunk).
#[cfg(feature = "test-hooks")]
pub fn gmax_by_rows(bytes: &[u8]) -> Option<u64> {
    let mut r = Reader::new(bytes);
    if r.take(4)? != MAGIC {
        return None;
    }
    r.take(5)?;
    let len = usize::try_from(r.uleb()?).ok()?;
    let data = r.take(len)?;
    let mut r = Reader::new(data);
    let actors = r.uleb()?;
    for _ in 0..actors {
        let len = usize::try_from(r.uleb()?).ok()?;
        r.take(len)?;
    }
    let heads = usize::try_from(r.uleb()?).ok()?;
    r.take(heads * 32)?;
    let mut scanner = Scanner::new(None, true);
    let change_meta = scanner.metadata(&mut r, &DOC_CHANGE_SPECS).ok()?;
    let op_meta = scanner.metadata(&mut r, &DOC_OP_SPECS).ok()?;
    scanner
        .columns(&mut r, &change_meta, true, &[], |_, _, _| {})
        .ok()?;
    let mut tally = Tally::default();
    let (cols, _) = scanner
        .columns(&mut r, &op_meta, true, &GMAX_SPECS, |s, spec, data| {
            tally.add(&DOC_OP_BLOCK, spec, data, &mut s.ticker);
        })
        .ok()?;
    let rows_usize = usize::try_from(tally.rows).ok()?;
    let expand = |spec: u64| -> Vec<Val<'_>> {
        let mut out = Vec::new();
        if let Some(col) = cols.iter().find(|c| c.spec == spec) {
            let mut runs = Runs::new(spec & 7, &col.data);
            while let Some(seg) = runs.next_seg() {
                for k in 0..seg.len {
                    if out.len() >= rows_usize {
                        break;
                    }
                    out.push(seg.at(k));
                }
            }
        }
        out.resize(rows_usize, Val::Null);
        out
    };
    let (oa, oc, ka, kc, ks, ins, succ) = (
        expand(OBJ_ACTOR),
        expand(OBJ_CTR),
        expand(KEY_ACTOR),
        expand(KEY_CTR),
        expand(KEY_STR),
        expand(INSERT),
        expand(SUCC_GROUP),
    );
    let (mut best, mut cur) = (0u64, 0u64);
    for i in 0..rows_usize {
        let is_ins = ins[i] == Val::B(true);
        let boundary = i == 0
            || is_ins
            || (oa[i], oc[i]) != (oa[i - 1], oc[i - 1])
            || ((ka[i], kc[i], ks[i]) != (ka[i - 1], kc[i - 1], ks[i - 1])
                && ins[i - 1] != Val::B(true));
        if boundary {
            cur = 0;
        }
        cur = cur.saturating_add(match succ[i] {
            Val::U(g) => g,
            _ => 0,
        });
        best = best.max(cur);
    }
    Some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`column_stats`] as it decoded every value before literal values
    /// were stepped over by their last bytes: the reference.
    fn column_stats_decoding(ty: u64, data: &[u8]) -> ColumnStats {
        let mut stats = ColumnStats::default();
        let mut r = Reader::new(data);
        match ty {
            VALUE => return stats,
            BOOLEAN => {
                while !r.done() {
                    let Some(n) = r.uleb() else { break };
                    stats.rows = stats.rows.saturating_add(n);
                }
                return stats;
            }
            _ => {}
        }
        let value = |r: &mut Reader<'_>| -> Option<u64> {
            match ty {
                STRING => {
                    let len = usize::try_from(r.uleb()?).ok()?;
                    r.take(len)?;
                    Some(len as u64)
                }
                DELTA => r.sleb().map(|_| 0),
                GROUP => r.uleb(),
                _ => r.uleb().map(|_| 0),
            }
        };
        'runs: while !r.done() {
            let Some(n) = r.sleb() else { break };
            if n > 0 {
                let Some(v) = value(&mut r) else { break };
                let n = n.unsigned_abs();
                stats.rows = stats.rows.saturating_add(n);
                if ty == STRING {
                    // Expanded, less the one value the run holds.
                    let expanded = n.saturating_mul(v);
                    stats.excess = stats.excess.saturating_add(expanded - v);
                } else {
                    stats.sum = stats.sum.saturating_add(n.saturating_mul(v));
                }
            } else if n < 0 {
                for _ in 0..n.unsigned_abs() {
                    let Some(v) = value(&mut r) else {
                        break 'runs;
                    };
                    stats.rows = stats.rows.saturating_add(1);
                    if ty != STRING {
                        stats.sum = stats.sum.saturating_add(v);
                    }
                }
            } else {
                let Some(k) = r.uleb() else { break };
                stats.rows = stats.rows.saturating_add(k);
            }
        }
        stats
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        /// A number of 0 to 64 bits (small ones more often).
        fn number(&mut self) -> u64 {
            let bits = [1, 6, 7, 8, 13, 14, 20, 32, 63, 64][self.below(10) as usize];
            self.next() >> (64 - bits)
        }
    }

    fn uleb(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn sleb(out: &mut Vec<u8>, mut v: i64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if (v == 0 && byte & 0x40 == 0) || (v == -1 && byte & 0x40 != 0) {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    /// A well-formed column of type `ty`: repeat, literal and null runs.
    fn column(rng: &mut Rng, ty: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let value = |rng: &mut Rng, out: &mut Vec<u8>| match ty {
            STRING => {
                let len = [0, 1, 5, 127, 128, 300][rng.below(6) as usize];
                uleb(out, len);
                out.extend((0..len).map(|i| i as u8));
            }
            DELTA => sleb(out, rng.number() as i64),
            GROUP => uleb(out, rng.below(40)),
            _ => uleb(out, rng.number()),
        };
        for _ in 0..rng.below(60) {
            let len = 1 + [rng.below(3), rng.below(20), rng.number() >> 1][rng.below(3) as usize];
            let len = len.min(if ty == GROUP { 1 << 20 } else { 1 << 40 });
            match rng.below(3) {
                0 => {
                    sleb(&mut out, len as i64);
                    value(rng, &mut out);
                }
                1 => {
                    let len = len.min(40);
                    sleb(&mut out, -(len as i64));
                    for _ in 0..len {
                        value(rng, &mut out);
                    }
                }
                _ => {
                    sleb(&mut out, 0);
                    uleb(&mut out, len);
                }
            }
        }
        out
    }

    #[test]
    fn column_stats_steps_over_literals_like_decoding_them() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut ticker = Ticker::default();
        for i in 0..20_000 {
            let ty = [GROUP, 1, 2, DELTA, BOOLEAN, STRING, 6][rng.below(7) as usize];
            let mut data = column(&mut rng, ty);
            let well_formed = i % 2 == 0;
            if !well_formed {
                // Corrupted: bytes flipped, cut off, or garbage appended.
                match rng.below(3) {
                    0 if !data.is_empty() => {
                        for _ in 0..1 + rng.below(4) {
                            let at = rng.below(data.len() as u64) as usize;
                            data[at] = rng.next() as u8;
                        }
                    }
                    1 => data.truncate(rng.below(data.len() as u64 + 1) as usize),
                    _ => data.extend((0..rng.below(40)).map(|_| rng.next() as u8)),
                }
            }
            let fast = column_stats(ty, &data, &mut ticker);
            let reference = column_stats_decoding(ty, &data);
            if well_formed || ty == GROUP || ty == STRING || ty == BOOLEAN {
                assert_eq!(
                    (fast.rows, fast.sum, fast.excess),
                    (reference.rows, reference.sum, reference.excess),
                    "column {i} of type {ty}: {data:?}"
                );
            } else {
                // Only an over-long number (which decoding stops at) can
                // make it count more; never fewer.
                assert!(fast.rows >= reference.rows, "column {i}: {data:?}");
                assert_eq!(fast.sum, 0);
            }
        }
    }

    #[test]
    fn skip_lebs_stops_after_the_last_value() {
        // Values of 1 to 10 bytes packed back to back, then a marker byte:
        // stepping over all of them lands exactly on the marker, whatever
        // the word boundaries.
        for lens in [&[1usize; 20][..], &[2; 9], &[10, 1, 3, 8, 8, 1, 1, 9, 2]] {
            for pad in 0..9 {
                let mut data = vec![0x7f; pad];
                for &len in lens {
                    data.extend(std::iter::repeat_n(0x80, len - 1));
                    data.push(0x01);
                }
                data.push(0xaa);
                let mut r = Reader::new(&data);
                assert_eq!(r.skip_lebs(pad as u64), pad as u64);
                assert_eq!(r.skip_lebs(lens.len() as u64), lens.len() as u64);
                assert_eq!(r.bytes[r.pos], 0xaa, "{lens:?} after {pad}");
                // Asking for more than there are: the cut-off value is not
                // counted, and the reader is at the end.
                let mut r = Reader::new(&data[pad..]);
                assert_eq!(r.skip_lebs(lens.len() as u64 + 5), lens.len() as u64);
                assert!(r.done());
            }
        }
    }
}
