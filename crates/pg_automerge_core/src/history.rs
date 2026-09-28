//! Read-only access to the individual changes of a stored document, and to
//! its state as of earlier heads.
//!
//! Automerge keeps a document as an op set plus a change graph (hash, actor,
//! seq, ops range, time, message and deps of every change). The graph alone
//! answers metadata questions ([`changes_meta`], [`change_count`]); the bytes
//! of a change are not stored anywhere and are rebuilt from the op set
//! ([`changes`], [`change`], [`changes_bytes`]), which costs about as much as
//! encoding those changes from scratch, on top of the load.
//!
//! "Since heads" follows `Automerge::get_changes(have_deps)`: the changes
//! that are not ancestors of (or equal to) the given heads. Hashes the
//! document does not have are ignored, so heads from a replica that is
//! ahead of the stored document yield everything that is not an ancestor of
//! the heads the document does know (a superset of what that replica
//! lacks, never less). Automerge computes this set with a per-actor
//! sequence-number clock of the known heads; for histories written by
//! Automerge (every change depends on its actor's previous change) that is
//! exactly the set of non-ancestors, which the tests check against a graph
//! walk.
//!
//! Every result is fully materialized (a `Vec`); nothing borrows the loaded
//! document once a function returns.

use std::collections::{BinaryHeap, HashMap};

use automerge::{Automerge, Change, ChangeHash, ReadDoc};

use crate::json::JsonSink;
use crate::loaded::{Input, with_doc};
use crate::{Error, Ticker, is_subset};

/// One change of a document, as returned by the history functions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeInfo {
    /// Lowercase hex change hash.
    pub hash: String,
    /// Lowercase hex actor id.
    pub actor: String,
    /// Sequence number of the change among its actor's changes (1, 2, ...).
    pub seq: u64,
    /// Counter of the change's first op.
    pub start_op: u64,
    /// Number of ops in the change (0 for an empty change).
    pub op_count: u64,
    /// The change's timestamp as stored: Unix seconds by Automerge's
    /// convention (`CommitOptions::with_time`), 0 when none was set.
    pub time: i64,
    /// The commit message, if any.
    pub message: Option<String>,
    /// Sorted lowercase hex hashes of the changes this one depends on.
    pub deps: Vec<String>,
    /// The change chunk (uncompressed, as `save_after` emits it); `None`
    /// from the metadata-only functions.
    pub bytes: Option<Vec<u8>>,
}

impl ChangeInfo {
    fn from_change(change: &Change) -> Self {
        let mut deps: Vec<String> = change.deps().iter().map(ToString::to_string).collect();
        deps.sort();
        ChangeInfo {
            hash: change.hash().to_string(),
            actor: change.actor_id().to_hex_string(),
            seq: change.seq(),
            start_op: change.start_op().get(),
            op_count: change.len() as u64,
            time: change.timestamp(),
            message: change.message().map(str::to_owned),
            deps,
            bytes: Some(change.raw_bytes().to_vec()),
        }
    }

    fn from_meta(meta: &automerge::ChangeMetadata<'_>) -> Self {
        let mut deps: Vec<String> = meta.deps.iter().map(ToString::to_string).collect();
        deps.sort();
        ChangeInfo {
            hash: meta.hash.to_string(),
            actor: meta.actor.to_hex_string(),
            seq: meta.seq,
            start_op: meta.start_op,
            // start_op = max_op - op_count + 1 in the change graph.
            op_count: (meta.max_op + 1).saturating_sub(meta.start_op),
            time: meta.timestamp,
            message: meta.message.as_deref().map(str::to_owned),
            deps,
            bytes: None,
        }
    }
}

/// Parse a change hash given as text: exactly 64 hexadecimal digits, either
/// case. Anything else is [`Error::InvalidInput`].
///
/// # Errors
///
/// [`Error::InvalidInput`] for anything but 64 hex digits.
pub fn parse_hash(text: &str) -> Result<ChangeHash, Error> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        let shown: String = text.chars().take(80).collect();
        let ellipsis = if text.chars().count() > 80 { "..." } else { "" };
        return Err(Error::InvalidInput(format!(
            "invalid automerge change hash \"{shown}{ellipsis}\": expected 64 hexadecimal digits"
        )));
    }
    let nibble = |c: u8| crate::encoding::hex_nibble(c).expect("checked above");
    let mut out = [0u8; 32];
    for (i, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        out[i] = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    Ok(ChangeHash(out))
}

/// Parse a list of change hashes, dropping duplicates. The order of the
/// result is unspecified.
///
/// # Errors
///
/// [`Error::InvalidInput`] if any text is not 64 hex digits.
pub fn parse_hashes<S: AsRef<str>>(texts: &[S]) -> Result<Vec<ChangeHash>, Error> {
    let mut hashes = texts
        .iter()
        .map(|t| parse_hash(t.as_ref()))
        .collect::<Result<Vec<_>, _>>()?;
    hashes.sort_unstable();
    hashes.dedup();
    Ok(hashes)
}

/// Whether a document with heads `heads` has nothing that is not already
/// in `since`: every head is one of `since` (and `since` is not empty, which
/// means "everything"). Then every "since" function returns nothing,
/// without looking at the history.
pub fn nothing_since(heads: &[ChangeHash], since: &[ChangeHash]) -> bool {
    !since.is_empty() && is_subset(heads, since)
}

/// Run `f` on the document of `input`, unless [`nothing_since`] says that
/// there is nothing since `since` (then `T::default()`). The heads of a
/// stored input are read from its header, so that case loads nothing.
fn since<T: Default>(
    input: Input<'_>,
    since: &[ChangeHash],
    f: impl FnOnce(&Automerge) -> Result<T, Error>,
) -> Result<T, Error> {
    if nothing_since(&input.heads()?, since) {
        return Ok(T::default());
    }
    with_doc(input, f)
}

/// Order `changes` causally: every change after all of its deps that are in
/// the list. Among changes whose deps are all placed, the one that came
/// first in the input goes first, so an input that already is in causal
/// order (Automerge's own order, in practice) is returned unchanged.
fn causal_order(changes: Vec<ChangeInfo>) -> Result<Vec<ChangeInfo>, Error> {
    let position: HashMap<&str, usize> = changes
        .iter()
        .enumerate()
        .map(|(i, c)| (c.hash.as_str(), i))
        .collect();
    let mut waiting_on = vec![0usize; changes.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); changes.len()];
    for (i, change) in changes.iter().enumerate() {
        for dep in &change.deps {
            if let Some(&d) = position.get(dep.as_str()) {
                waiting_on[i] += 1;
                dependents[d].push(i);
            }
        }
    }
    drop(position);
    let mut ready: BinaryHeap<std::cmp::Reverse<usize>> = waiting_on
        .iter()
        .enumerate()
        .filter(|(_, n)| **n == 0)
        .map(|(i, _)| std::cmp::Reverse(i))
        .collect();
    let mut order = Vec::with_capacity(changes.len());
    let mut ticker = Ticker::default();
    while let Some(std::cmp::Reverse(i)) = ready.pop() {
        ticker.tick();
        order.push(i);
        for &j in &dependents[i] {
            waiting_on[j] -= 1;
            if waiting_on[j] == 0 {
                ready.push(std::cmp::Reverse(j));
            }
        }
    }
    if order.len() != changes.len() {
        // Change hashes cover their deps, so a cycle cannot exist.
        return Err(Error::Internal(
            "automerge change graph is not acyclic".into(),
        ));
    }
    let mut slots: Vec<Option<ChangeInfo>> = changes.into_iter().map(Some).collect();
    Ok(order
        .into_iter()
        .map(|i| slots[i].take().expect("each index is placed once"))
        .collect())
}

/// Metadata of every change not reachable from `since` (all changes when
/// `since` is empty), in causal order, from the change graph: no change is
/// rebuilt. `bytes` is `None` in every row.
///
/// # Errors
///
/// [`Error::Internal`] if a stored `input` does not load.
pub fn changes_meta(input: Input<'_>, since: &[ChangeHash]) -> Result<Vec<ChangeInfo>, Error> {
    self::since(input, since, |doc| {
        let mut ticker = Ticker::default();
        let rows = doc
            .get_changes_meta(since)
            .iter()
            .map(|meta| {
                ticker.tick();
                ChangeInfo::from_meta(meta)
            })
            .collect();
        causal_order(rows)
    })
}

/// Like [`changes_meta`], with each change's bytes (rebuilt from the op set).
///
/// # Errors
///
/// [`Error::Internal`] if a stored `input` does not load.
pub fn changes(input: Input<'_>, since: &[ChangeHash]) -> Result<Vec<ChangeInfo>, Error> {
    self::since(input, since, |doc| changes_of(doc, since))
}

fn changes_of(doc: &Automerge, since: &[ChangeHash]) -> Result<Vec<ChangeInfo>, Error> {
    let mut ticker = Ticker::default();
    let rows = doc
        .get_changes(since)
        .iter()
        .map(|change| {
            ticker.tick();
            ChangeInfo::from_change(change)
        })
        .collect();
    causal_order(rows)
}

/// The changes not reachable from `since`, as concatenated change chunks in
/// causal order (`save_after(since)` content): what a replica at `since`
/// needs, loadable with `load_incremental` / `apply_changes`, or with
/// `merge(automerge, bytea)`. Empty when there is nothing new.
///
/// # Errors
///
/// [`Error::Internal`] if a stored `input` does not load.
pub fn changes_bytes(input: Input<'_>, since: &[ChangeHash]) -> Result<Vec<u8>, Error> {
    self::since(input, since, |doc| {
        let rows = changes_of(doc, since)?;
        let mut out = Vec::with_capacity(
            rows.iter()
                .map(|r| r.bytes.as_ref().map_or(0, Vec::len))
                .sum(),
        );
        for row in rows {
            out.extend(row.bytes.expect("changes_of() fills bytes"));
        }
        Ok(out)
    })
}

/// The change with hash `hash`, with its bytes, or `None` if the document
/// does not have it.
///
/// # Errors
///
/// [`Error::Internal`] if a stored `input` does not load.
pub fn change(input: Input<'_>, hash: &ChangeHash) -> Result<Option<ChangeInfo>, Error> {
    with_doc(input, |doc| {
        if doc.get_change_meta_by_hash(hash).is_none() {
            return Ok(None);
        }
        Ok(doc
            .get_change_by_hash(hash)
            .as_ref()
            .map(ChangeInfo::from_change))
    })
}

/// Number of changes in the document. For a stored input read from the
/// header and the change actor column when possible (see
/// [`header::change_count_from_bytes`](crate::header::change_count_from_bytes)),
/// otherwise from the change graph of the (loaded) document.
///
/// # Errors
///
/// [`Error::Internal`] if a stored `input` has to be loaded and does not
/// load.
pub fn change_count(input: Input<'_>) -> Result<u64, Error> {
    if let Input::Stored(bytes) = input
        && let Some(n) = crate::header::change_count_from_bytes(bytes)
    {
        return Ok(n);
    }
    with_doc(input, |doc| Ok(doc.stats().num_changes))
}

/// The document's state as of `heads` as JSON (see [`crate::json`]).
///
/// Every hash in `heads` must be a change of the document. Empty `heads`
/// is the state before any change: `{}`.
///
/// # Errors
///
/// [`Error::InvalidParameter`] naming the first missing head (sorted);
/// [`Error::Internal`] if a stored `input` does not load.
pub fn to_json_at(input: Input<'_>, heads: &[ChangeHash]) -> Result<serde_json::Value, Error> {
    let mut sink = crate::json::ValueSink::default();
    write_json_at(input, heads, &mut sink)?;
    Ok(sink.into_value().expect("the walk emits one object"))
}

/// [`to_json_at`] into a [`JsonSink`].
///
/// # Errors
///
/// As [`to_json_at`]; the sink has then received an incomplete walk (or
/// nothing).
pub fn write_json_at<S: JsonSink + ?Sized>(
    input: Input<'_>,
    heads: &[ChangeHash],
    sink: &mut S,
) -> Result<(), Error> {
    with_doc(input, |doc| {
        check_heads(doc, heads)?;
        let mut current = doc.get_heads();
        current.sort_unstable();
        let mut wanted = heads.to_vec();
        wanted.sort_unstable();
        wanted.dedup();
        if wanted == current {
            return crate::json::write_json_at(doc, None, sink);
        }
        crate::json::write_json_at(doc, Some(&wanted), sink)
    })
}

fn check_heads(doc: &Automerge, heads: &[ChangeHash]) -> Result<(), Error> {
    // For a complete document, the missing deps of `heads` are exactly the
    // hashes of `heads` the document does not have.
    let mut missing = doc.get_missing_deps(heads);
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort_unstable();
    let more = if missing.len() > 1 {
        format!(" (and {} more)", missing.len() - 1)
    } else {
        String::new()
    };
    Err(Error::InvalidParameter(format!(
        "automerge document does not contain change {}{more}",
        missing[0]
    )))
}

/// Microseconds since 2000-01-01 (Postgres `timestamptz`) for an Automerge
/// change time in Unix seconds, or `None` for 0 (no time set) and for times
/// Postgres cannot represent.
pub fn pg_timestamptz_micros(unix_seconds: i64) -> Option<i64> {
    // Postgres: MIN_TIMESTAMP <= t < END_TIMESTAMP (datatype/timestamp.h).
    const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
    const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;
    const PG_EPOCH_UNIX_SECONDS: i64 = 946_684_800;
    if unix_seconds == 0 {
        return None;
    }
    let micros = unix_seconds
        .checked_sub(PG_EPOCH_UNIX_SECONDS)?
        .checked_mul(1_000_000)?;
    (MIN_TIMESTAMP..END_TIMESTAMP)
        .contains(&micros)
        .then_some(micros)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_parse_strictly() {
        let lower = "ab".repeat(32);
        let h = parse_hash(&lower).unwrap();
        assert_eq!(h.to_string(), lower);
        assert_eq!(parse_hash(&"AB".repeat(32)).unwrap(), h);
        assert_eq!(parse_hash(&"aB".repeat(32)).unwrap(), h);
        for bad in [
            String::new(),
            "ab".repeat(31),
            "ab".repeat(33),
            format!("{}g", "a".repeat(63)),
            format!("\\x{}", "ab".repeat(31)),
            format!(" {}", "a".repeat(63)),
            // Non-ASCII with the right byte length.
            format!("{}é", "a".repeat(62)),
        ] {
            let err = parse_hash(&bad).unwrap_err();
            assert!(
                matches!(&err, Error::InvalidInput(m) if m.contains("64 hexadecimal digits")),
                "{bad:?}: {err:?}"
            );
        }
        let long = "z".repeat(1000);
        let Error::InvalidInput(msg) = parse_hash(&long).unwrap_err() else {
            panic!()
        };
        assert!(msg.len() < 200, "{msg}");
        assert_eq!(
            parse_hashes(&[lower.clone(), lower.to_uppercase()]).unwrap(),
            vec![h]
        );
    }

    #[test]
    fn timestamps_map_to_postgres() {
        assert_eq!(pg_timestamptz_micros(0), None);
        assert_eq!(pg_timestamptz_micros(946_684_800), Some(0));
        assert_eq!(pg_timestamptz_micros(946_684_801), Some(1_000_000));
        assert_eq!(pg_timestamptz_micros(-1), Some(-946_684_801_000_000));
        assert_eq!(pg_timestamptz_micros(i64::MAX), None);
        assert_eq!(pg_timestamptz_micros(i64::MIN), None);
        // 294276 AD is past Postgres' range; milliseconds mistaken for
        // seconds (year ~57000) are still representable.
        assert_eq!(pg_timestamptz_micros(9_224_318_016_000), None);
        assert!(pg_timestamptz_micros(1_700_000_000_000).is_some());
    }
}
