//! `automerge_spans`: the structure of a text object (text runs with their
//! marks, and blocks) as jsonb, in the shape of the Automerge JavaScript
//! API's `spans()`. See docs/DESIGN.md, "Rich text spans".

use pg_automerge_core::{self as am, Error};
use pgrx::prelude::*;

use crate::datum::AutomergeArg;
use crate::error::{OrRaise, raise};
use crate::history::hashes_arg;
use crate::jsonb::{JsonbBuilder, JsonbDatum};

/// The spans of the text object at `path` (as for jsonb's `#>`), as of
/// `heads` (`None`: the current state). NULL when nothing is at `path` or
/// `path` has a NULL element.
fn spans(
    doc: &AutomergeArg,
    path: &[Option<String>],
    heads: Option<&[am::automerge::ChangeHash]>,
) -> Option<JsonbDatum> {
    let path: Vec<&str> = path
        .iter()
        .map(|step| step.as_deref())
        .collect::<Option<_>>()?;
    let mut jsonb = JsonbBuilder::default();
    let found = doc
        .with_input(|input| am::spans::write_spans(input, &path, heads, &mut jsonb))
        .or_raise();
    if !found {
        return None;
    }
    Some(jsonb.finish().unwrap_or_else(|| {
        raise(Error::Internal(
            "automerge spans to jsonb: incomplete result".into(),
        ))
    }))
}

/// The structure of the text object at `path` (map keys, and list indices
/// as integers, from the root; `'{}'` is the root): a jsonb array of
/// `{"type": "text", "value": ..., "marks": {...}}` runs (no `"marks"` when
/// the run has none) and `{"type": "block", "value": {...}}` blocks, as
/// Automerge's JavaScript `spans()` returns them. NULL when nothing is at
/// `path`; 22023 when the value there is not a text object.
#[pg_extern(immutable, strict, parallel_safe)]
fn automerge_spans(doc: AutomergeArg, path: Vec<Option<String>>) -> Option<JsonbDatum> {
    spans(&doc, &path, None)
}

/// `automerge_spans(doc, path)` as of `heads` (the same heads as
/// `automerge_to_jsonb(doc, heads)`: every head must be a change of the
/// document, 22023 otherwise; `'{}'` is the state before any change).
#[pg_extern(immutable, strict, parallel_safe, name = "automerge_spans")]
fn automerge_spans_at(
    doc: AutomergeArg,
    path: Vec<Option<String>>,
    heads: Vec<Option<String>>,
) -> Option<JsonbDatum> {
    let heads = hashes_arg("heads", &heads);
    spans(&doc, &path, Some(&heads))
}

extension_sql!(
    r#"
COMMENT ON FUNCTION automerge_spans(automerge, text[]) IS
    'The text object at path (as for #>) as jsonb spans: text runs with their marks, and blocks, as Automerge''s JavaScript spans() returns them; NULL if nothing is at path.';
COMMENT ON FUNCTION automerge_spans(automerge, text[], text[]) IS
    'automerge_spans(doc, path) as of the given heads (''{}'': before any change).';
"#,
    name = "automerge_spans_comments",
    requires = [automerge_spans, automerge_spans_at],
);
