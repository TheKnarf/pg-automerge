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
/// Decided from the two values' heads when possible (no load, and only a
/// prefix of each is detoasted); otherwise `a` is loaded, `b` never is.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_contains(a: AutomergeArg, b: AutomergeArg) -> bool {
    if a.same_object(&b) {
        return true;
    }
    let heads_b = b.heads().or_raise();
    let heads_a = a.heads().or_raise();
    match am::contains_by_heads(&heads_a, &heads_b) {
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
/// current heads or build on them, or from a save's header heads when they
/// are heads of `doc` (no load); for another single save `doc` is loaded
/// and checked for the save's heads (the save never is); otherwise
/// `doc ++ changes` is loaded once (no save), or for an expanded `doc`
/// each chunk's hash is looked up.
/// Changes with dependencies in neither input are not contained (false).
#[pg_extern(immutable, strict, parallel_safe, name = "automerge_contains")]
fn automerge_contains_changes(a: AutomergeArg, changes: &[u8]) -> bool {
    if changes.is_empty() {
        return true;
    }
    match am::contains_input_by_heads(&a.heads().or_raise(), changes) {
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
