//! Introspection without loading: `automerge_heads` and
//! `automerge_contains`.

use pg_automerge_core as am;
use pg_automerge_core::loaded;
use pgrx::prelude::*;

use crate::datum::AutomergeArg;
use crate::error::OrRaise;

/// Current heads as sorted lowercase hex change hashes. Read from the
/// start of the stored value; the document is not loaded.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_heads(doc: AutomergeArg) -> Vec<String> {
    am::heads_to_strings(doc.heads().or_raise())
}

/// Whether every change of `b` is already in `a`, i.e. `merge(a, b)` is a no-op.
///
/// Decided from the two values' heads when possible, or else from their
/// change counts (`b` with at least as many changes as `a` and other heads
/// is not in `a`); both need only a prefix of each value, no load.
/// Otherwise `a` is loaded, `b` never is.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_contains(a: AutomergeArg, b: AutomergeArg) -> bool {
    if a.same_object(&b) {
        return true;
    }
    let heads_b = b.heads().or_raise();
    let heads_a = a.heads().or_raise();
    if let Some(answer) = am::contains_by_heads(&heads_a, &heads_b) {
        return answer;
    }
    // Before loading `a`: `b` with at least as many changes (a newer or a
    // concurrent version) is not in it. An expanded `a` answers for free.
    let by_counts = if a.loaded().is_none() {
        am::contains_by_heads_and_counts(&heads_a, &heads_b, a.change_count(), b.change_count())
    } else {
        None
    };
    match by_counts {
        Some(answer) => answer,
        None => a
            .with_input(|input| loaded::contains_heads(input, &heads_b))
            .or_raise(),
    }
}

/// Whether every change in `changes` (a save or bare change chunks, as for
/// `merge(automerge, bytea)`) is already in `doc`, i.e. whether
/// `merge(doc, changes)` returns `doc` unchanged.
///
/// Decided from `doc`'s heads (read from a prefix) and the chunks' hashes
/// and dependencies when `changes` is bare change chunks that re-send the
/// current heads or build on them, or (for a save that passes Automerge's
/// chunk parse, without reconstructing it) from its header heads when they
/// are heads of `doc`, or from its header's change count when that is at
/// least `doc`'s (not contained; no load); for another single save `doc` is loaded
/// and checked for the save's heads (the save never is); otherwise
/// `doc ++ changes` is loaded once (no save), or for an expanded `doc`
/// each chunk's hash is looked up.
/// Changes with dependencies in neither input are not contained (false).
#[pg_extern(immutable, strict, parallel_safe, name = "automerge_contains")]
fn automerge_contains_changes(a: AutomergeArg, changes: &[u8]) -> bool {
    if changes.is_empty() {
        return true;
    }
    // `a`'s change count is read (from a prefix) only for a save its heads
    // do not decide; an expanded `a` answers the rest for free.
    let heads_a = a.heads().or_raise();
    let count_a = || a.loaded().is_none().then(|| a.change_count()).flatten();
    let decided = am::contains_input_by_header(&heads_a, count_a, changes);
    match decided {
        Some(answer) => answer,
        None => a
            .with_input(|input| loaded::contains_changes(input, changes))
            .or_raise(),
    }
}

extension_sql!(
    r#"
COMMENT ON FUNCTION automerge_heads(automerge) IS
    'Current heads as sorted hex change hashes, read from the stored header without loading the document.';
COMMENT ON FUNCTION automerge_contains(automerge, automerge) IS
    'Whether a already has every change of b, i.e. merge(a, b) adds nothing.';
COMMENT ON FUNCTION automerge_contains(automerge, bytea) IS
    'Whether every change in an Automerge save or change chunks is already in the document, i.e. merge(doc, changes) adds nothing.';
"#,
    name = "automerge_introspect_comments",
    requires = [
        automerge_heads,
        automerge_contains,
        automerge_contains_changes
    ],
);
