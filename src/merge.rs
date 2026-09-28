//! Merging: `merge(automerge, automerge)`, `merge(automerge, bytea)`, their
//! planner support function, the `||` operators and the `merge_agg`
//! aggregate.

use pg_automerge_core::loaded::{self, MergeOutcome};
use pg_automerge_core::{self as am, Error, MergeAccumulator};
use pgrx::prelude::*;
use pgrx::{Internal, PgMemoryContexts};

use crate::datum::{AutomergeArg, AutomergeValue};
use crate::error::{OrRaise, raise};
use crate::expanded::new_expanded;

/// CRDT merge: `a` plus every change of `b` it lacks. Returns an input
/// unchanged when it already contains the other; otherwise a new document
/// as an expanded value (see "Expanded values"), built in place when `a` is
/// a read-write expanded pointer.
#[pg_extern(immutable, strict, parallel_safe, support = automerge_merge_support)]
fn merge(a: AutomergeArg, b: AutomergeArg) -> AutomergeValue {
    // The same object twice (`doc := merge(doc, doc)`): nothing to do, and
    // no document is borrowed while it could be replaced.
    if a.same_object(&b) {
        return a.unchanged();
    }
    // The heads decide the common no-op cases from a prefix of each value,
    // before anything is detoasted: an unchanged flat input is returned as
    // it arrived (see `AutomergeArg::unchanged`).
    let (heads_a, heads_b) = (a.heads().or_raise(), b.heads().or_raise());
    if am::is_subset(&heads_b, &heads_a) {
        return a.unchanged();
    }
    if am::is_subset(&heads_a, &heads_b) {
        return b.unchanged();
    }
    let doc = {
        let (da, db) = (a.detoast(), b.detoast());
        match loaded::merge(da.input(), db.input()).or_raise() {
            MergeOutcome::Left => return da.into_value(),
            MergeOutcome::Right => return db.into_value(),
            MergeOutcome::New(doc) => doc,
        }
    };
    a.with_result(*doc)
}

/// `merge(automerge, bytea)`: apply an Automerge save or bare change chunks
/// (`save_incremental()` / `save_after()` output, possibly concatenated) on
/// top of the stored document. Returns `a` unchanged when nothing is new.
/// Changes with missing dependencies are rejected (22P02), naming them.
#[pg_extern(immutable, strict, parallel_safe, name = "merge", support = automerge_merge_support)]
fn merge_bytea(a: AutomergeArg, changes: &[u8]) -> AutomergeValue {
    // Nothing new by `a`'s heads (read from a prefix): `a` as it arrived,
    // never detoasted (see `AutomergeArg::unchanged`).
    if am::contains_input_by_heads(&a.heads().or_raise(), changes) == Some(true) {
        return a.unchanged();
    }
    let doc = {
        let da = a.detoast();
        match loaded::merge_changes(da.input(), changes).or_raise() {
            None => return da.into_value(),
            Some(doc) => doc,
        }
    };
    a.with_result(doc)
}

/// Planner support function of both `merge`s: answers PL/pgSQL's
/// `SupportRequestModifyInPlace` for `x := merge(x, ...)` (also written
/// `x := x || ...`) by naming the first argument, so a variable holding an
/// expanded document is passed read-write and merged into in place.
///
/// The two conditions of that request (nodes/supportnodes.h) hold: `merge`
/// never modifies its first argument on failure (the new document is built
/// completely before it replaces the old one), and other references to `x`
/// in the arguments are safe (they arrive read-only and are read before
/// the replacement; `merge(x, x)` returns `x` untouched).
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_merge_support(request: Internal) -> Internal {
    let node = request
        .unwrap()
        .map_or(std::ptr::null_mut(), |d| d.cast_mut_ptr::<pg_sys::Node>());
    // SAFETY: the planner passes a valid support request node.
    let param = unsafe { modify_in_place_param(node) };
    // A non-NULL datum holding the pointer (NULL pointer: no), since fmgr
    // rejects a NULL result from a support function.
    Internal::from(Some(pg_sys::Datum::from(param)))
}

/// For a `SupportRequestModifyInPlace`: the first argument when it is the
/// assignment target's Param, otherwise null (also for other requests).
///
/// # Safety
///
/// `node` must be null or a valid support request node.
pub(crate) unsafe fn modify_in_place_param(node: *mut pg_sys::Node) -> *mut pg_sys::Node {
    // SAFETY: per the contract; the request's args is a List of Nodes.
    unsafe {
        if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_SupportRequestModifyInPlace {
            return std::ptr::null_mut();
        }
        let request = node.cast::<pg_sys::SupportRequestModifyInPlace>();
        let args = (*request).args;
        if args.is_null() || (*args).length < 1 {
            return std::ptr::null_mut();
        }
        let first = (*(*args).elements).ptr_value.cast::<pg_sys::Node>();
        if first.is_null() || (*first).type_ != pg_sys::NodeTag::T_Param {
            return std::ptr::null_mut();
        }
        let param = first.cast::<pg_sys::Param>();
        if (*param).paramkind == pg_sys::ParamKind::PARAM_EXTERN
            && (*param).paramid == (*request).paramid
        {
            first
        } else {
            std::ptr::null_mut()
        }
    }
}

extension_sql!(
    r#"
CREATE OPERATOR || (
    LEFTARG = automerge,
    RIGHTARG = automerge,
    FUNCTION = merge,
    COMMUTATOR = ||
);
-- No commutator: there is no bytea || automerge.
CREATE OPERATOR || (
    LEFTARG = automerge,
    RIGHTARG = bytea,
    FUNCTION = merge
);

COMMENT ON FUNCTION merge(automerge, automerge) IS
    'CRDT merge: a plus every change of b it lacks. Commutative and idempotent in state.';
COMMENT ON FUNCTION merge(automerge, bytea) IS
    'Apply an Automerge save or change chunks (save_incremental / save_after output) on top of the document.';
COMMENT ON FUNCTION automerge_merge_support(internal) IS
    'Planner support function of merge: lets PL/pgSQL merge into a variable in place.';
COMMENT ON OPERATOR || (automerge, automerge) IS 'merge(automerge, automerge): CRDT merge.';
COMMENT ON OPERATOR || (automerge, bytea) IS
    'merge(automerge, bytea): apply an Automerge save or change chunks.';
"#,
    name = "automerge_merge_operator",
    requires = [
        "automerge_type",
        merge,
        merge_bytea,
        automerge_merge_support
    ],
);

/// Transition function of `merge_agg`. The state is a [`MergeAccumulator`]
/// owned by the aggregate's memory context and dropped when it is reset.
#[pg_extern(immutable, parallel_safe)]
fn merge_agg_trans(
    mut state: Internal,
    value: Option<AutomergeArg>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Internal {
    let Some(value) = value else { return state };
    // Each input can take a while to load and merge, and none of that work
    // checks for interrupts; check between inputs so a cancel or
    // statement_timeout takes effect mid-aggregate. No Rust state is
    // borrowed yet, so unwinding out of here is harmless.
    pg_sys::check_for_interrupts!();
    // SAFETY: the state is only ever created below, as a MergeAccumulator.
    let acc = match unsafe { state.get_mut::<MergeAccumulator>() } {
        Some(acc) => acc,
        None => {
            let mut agg_context: pg_sys::MemoryContext = std::ptr::null_mut();
            // SAFETY: fcinfo is this call's; AggCheckCallContext only reads it.
            if unsafe { pg_sys::AggCheckCallContext(fcinfo, &mut agg_context) } == 0 {
                raise(Error::Internal(
                    "merge_agg_trans called in non-aggregate context".into(),
                ));
            }
            let ptr =
                PgMemoryContexts::For(agg_context).leak_and_drop_on_delete(MergeAccumulator::new());
            state = Internal::from(Some(pg_sys::Datum::from(ptr)));
            // SAFETY: just initialized with a MergeAccumulator.
            unsafe { state.get_mut::<MergeAccumulator>() }.expect("just initialized")
        }
    };
    // An input the state already has (by its heads, read from a prefix of
    // a flat value) is never detoasted, let alone loaded.
    if acc.has_heads(&value.heads().or_raise()).or_raise() {
        return state;
    }
    value.with_input(|input| acc.add_input(input)).or_raise();
    state
}

/// Final function of `merge_agg`; NULL if every input was NULL.
#[pg_extern(immutable, parallel_safe)]
fn merge_agg_final(state: Internal) -> Option<AutomergeValue> {
    // SAFETY: the state is only ever created by merge_agg_trans.
    let acc = unsafe { state.get::<MergeAccumulator>() }?;
    Some(match acc.finish_loaded().or_raise()? {
        am::Accumulated::Stored(bytes) => AutomergeValue::Bytes(bytes.to_vec()),
        // A copy of the state as an expanded value: saved only if it is
        // stored or sent, and `merge_agg(doc)::jsonb` needs no re-load.
        am::Accumulated::Loaded(doc) => AutomergeValue::Datum(new_expanded(*doc)),
    })
}

extension_sql!(
    r#"
CREATE AGGREGATE merge_agg(automerge) (
    SFUNC = merge_agg_trans,
    STYPE = internal,
    FINALFUNC = merge_agg_final,
    -- The state is a fully loaded document in the Rust heap, invisible to
    -- Postgres memory accounting and not spillable by HashAgg (no
    -- serialfunc). Declare a size that is realistic for non-trivial
    -- documents (the default estimate for internal is ~8kB) so the planner
    -- prefers sorted grouping over large per-group hash tables.
    SSPACE = 1048576,
    PARALLEL = SAFE
);

COMMENT ON AGGREGATE merge_agg(automerge) IS 'CRDT merge of all non-null inputs.';
COMMENT ON FUNCTION merge_agg_trans(internal, automerge) IS 'Transition function of merge_agg.';
COMMENT ON FUNCTION merge_agg_final(internal) IS 'Final function of merge_agg.';
"#,
    name = "automerge_merge_agg",
    requires = ["automerge_type", merge_agg_trans, merge_agg_final],
);
