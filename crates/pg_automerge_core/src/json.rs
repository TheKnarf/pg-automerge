//! Conversion of the state of an Automerge document (current, or as of given
//! heads) to JSON, following the mapping table in docs/DESIGN.md.

use automerge::iter::{ListRange, MapRange};
use automerge::{Automerge, ChangeHash, ObjId, ObjType, ROOT, ReadDoc, ScalarValueRef, ValueRef};
use serde_json::{Map, Number, Value};

use crate::Error;
use crate::encoding::{base64, iso8601_millis};

/// Maximum object nesting depth converted. The walk below is iterative, but
/// serializing and dropping the resulting `serde_json::Value` recurse, so a
/// hostile document could otherwise exhaust the backend's stack. Real
/// documents are nowhere near this deep.
pub const MAX_DEPTH: usize = 1000;

/// Convert the current state of `doc` to a JSON object.
///
/// Integers (`Int`, `Uint`, `Counter`) become exact JSON numbers; they never
/// pass through `f64`. For a key with concurrent conflicting values, the
/// value Automerge's `get` returns (the conflict winner) is used.
///
/// # Errors
///
/// [`Error::Internal`] for a document nested deeper than [`MAX_DEPTH`] or
/// if Automerge fails to read it.
pub fn doc_to_json(doc: &Automerge) -> Result<Value, Error> {
    doc_to_json_at(doc, None)
}

/// Convert the state of `doc` as of `heads` (`None`: the current state) to a
/// JSON object, with the same mapping as [`doc_to_json`].
///
/// `heads` must all be changes of `doc` (callers check); Automerge ignores
/// unknown ones. Every container read with `heads` recomputes Automerge's
/// clock for them, which costs a walk of the change graph back to the
/// nearest cached clock, so historical reads are somewhat slower than
/// current ones. Heads equal to the document's current heads take the
/// current-state path inside Automerge.
///
/// # Errors
///
/// As [`doc_to_json`].
pub fn doc_to_json_at(doc: &Automerge, heads: Option<&[ChangeHash]>) -> Result<Value, Error> {
    // Depth-first walk with an explicit stack of containers being filled, so
    // the native stack does not grow with document depth.
    let mut stack = vec![Frame::new(doc, heads, &ROOT, ObjType::Map, None)];
    loop {
        let top = stack
            .last_mut()
            .expect("the root frame is only popped to return");
        let Some((key, value, id)) = top.next_item() else {
            let done = stack.pop().expect("checked above");
            let (key, value) = done.finish();
            match stack.last_mut() {
                Some(parent) => parent.push(key, value),
                None => return Ok(value),
            }
            continue;
        };
        match (value, id) {
            (ValueRef::Scalar(scalar), _) => top.push(key, scalar_to_json(&scalar)),
            (ValueRef::Object(ObjType::Text), Some(id)) => {
                let text = match heads {
                    Some(heads) => doc.text_at(&id, heads),
                    None => doc.text(&id),
                }
                .map_err(|e| Error::Internal(format!("could not read automerge text: {e}")))?;
                top.push(key, Value::String(sanitize(&text).into_owned()));
            }
            (ValueRef::Object(typ), Some(id)) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(Error::Internal(format!(
                        "automerge document is nested more than {MAX_DEPTH} levels deep"
                    )));
                }
                stack.push(Frame::new(doc, heads, &id, typ, key));
            }
            (ValueRef::Object(_), None) => unreachable!("next_item returns an id for objects"),
        }
    }
}

/// A map or list whose visible entries are being converted.
enum Frame<'a> {
    Map {
        key_in_parent: Option<String>,
        // `map_range` yields one item per visible key: the conflict winner.
        iter: MapRange<'a>,
        out: Map<String, Value>,
    },
    List {
        key_in_parent: Option<String>,
        iter: ListRange<'a>,
        out: Vec<Value>,
    },
}

impl<'a> Frame<'a> {
    /// `key_in_parent` is the map key this container is stored under, or
    /// `None` for list elements and the root.
    fn new(
        doc: &'a Automerge,
        heads: Option<&[ChangeHash]>,
        obj: &ObjId,
        typ: ObjType,
        key_in_parent: Option<String>,
    ) -> Self {
        match typ {
            ObjType::List => {
                let (iter, len) = match heads {
                    // No length_at: that would be a second clock computation.
                    Some(heads) => (doc.list_range_at(obj, .., heads), 0),
                    None => (doc.list_range(obj, ..), doc.length(obj)),
                };
                Frame::List {
                    key_in_parent,
                    iter,
                    out: Vec::with_capacity(len),
                }
            }
            // Text never gets a frame; it is converted in one go via `text()`.
            ObjType::Map | ObjType::Table | ObjType::Text => Frame::Map {
                key_in_parent,
                iter: match heads {
                    Some(heads) => doc.map_range_at(obj, .., heads),
                    None => doc.map_range(obj, ..),
                },
                out: Map::new(),
            },
        }
    }

    /// The next entry: its map key (maps only), value, and object id when the
    /// value is an object (building an id is not free, so scalars skip it).
    fn next_item(&mut self) -> Option<(Option<String>, ValueRef<'a>, Option<ObjId>)> {
        fn id_if_object(value: &ValueRef<'_>, id: impl FnOnce() -> ObjId) -> Option<ObjId> {
            matches!(value, ValueRef::Object(_)).then(id)
        }
        match self {
            Frame::Map { iter, .. } => iter.next().map(|item| {
                let id = id_if_object(&item.value, || item.id());
                (Some(sanitize(&item.key).into_owned()), item.value, id)
            }),
            Frame::List { iter, .. } => iter.next().map(|item| {
                let id = id_if_object(&item.value, || item.id());
                (None, item.value, id)
            }),
        }
    }

    fn push(&mut self, key: Option<String>, value: Value) {
        match self {
            Frame::Map { out, .. } => {
                out.insert(key.unwrap_or_default(), value);
            }
            Frame::List { out, .. } => out.push(value),
        }
    }

    fn finish(self) -> (Option<String>, Value) {
        match self {
            Frame::Map {
                key_in_parent, out, ..
            } => (key_in_parent, Value::Object(out)),
            Frame::List {
                key_in_parent, out, ..
            } => (key_in_parent, Value::Array(out)),
        }
    }
}

/// Map a scalar per the DESIGN.md table.
pub fn scalar_to_json(scalar: &ScalarValueRef<'_>) -> Value {
    match scalar {
        ScalarValueRef::Str(s) => Value::String(sanitize(s).into_owned()),
        ScalarValueRef::Int(i) | ScalarValueRef::Counter(i) => Value::Number(Number::from(*i)),
        ScalarValueRef::Uint(u) => Value::Number(Number::from(*u)),
        // jsonb has no NaN / Infinity; `from_f64` returns None for those.
        ScalarValueRef::F64(f) => Number::from_f64(*f).map_or(Value::Null, Value::Number),
        ScalarValueRef::Boolean(b) => Value::Bool(*b),
        ScalarValueRef::Timestamp(ms) => Value::String(iso8601_millis(*ms)),
        ScalarValueRef::Bytes(bytes) => Value::String(base64(bytes)),
        ScalarValueRef::Null | ScalarValueRef::Unknown { .. } => Value::Null,
    }
}

/// Postgres text (and therefore jsonb) cannot contain U+0000, and `jsonb_in`
/// rejects the `\u0000` escape. Replace it with U+FFFD so that one stray NUL
/// does not make the whole document unreadable as jsonb.
fn sanitize(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains('\0') {
        s.replace('\0', "\u{FFFD}").into()
    } else {
        s.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use automerge::transaction::Transactable;
    use automerge::{ActorId, AutoCommit, ScalarValue};
    use serde_json::json;

    fn to_json(doc: &mut AutoCommit) -> Value {
        doc_to_json(doc.document()).unwrap()
    }

    #[test]
    fn empty_document_is_empty_object() {
        assert_eq!(doc_to_json(&Automerge::new()).unwrap(), json!({}));
    }

    #[test]
    fn scalars() {
        let mut doc = AutoCommit::new();
        doc.put(ROOT, "str", "hello").unwrap();
        doc.put(ROOT, "int", -42i64).unwrap();
        doc.put(ROOT, "uint", 7u64).unwrap();
        doc.put(ROOT, "f64", 1.5f64).unwrap();
        doc.put(ROOT, "t", true).unwrap();
        doc.put(ROOT, "f", false).unwrap();
        doc.put(ROOT, "null", ScalarValue::Null).unwrap();
        assert_eq!(
            to_json(&mut doc),
            json!({
                "str": "hello", "int": -42, "uint": 7, "f64": 1.5,
                "t": true, "f": false, "null": null
            })
        );
    }

    #[test]
    fn integers_are_exact() {
        let mut doc = AutoCommit::new();
        doc.put(ROOT, "umax", u64::MAX).unwrap();
        doc.put(ROOT, "imin", i64::MIN).unwrap();
        doc.put(ROOT, "imax", i64::MAX).unwrap();
        doc.put(ROOT, "big", ScalarValue::Uint(9_007_199_254_740_993))
            .unwrap(); // 2^53 + 1
        let json = to_json(&mut doc);
        let text = serde_json::to_string(&json).unwrap();
        assert!(text.contains("\"umax\":18446744073709551615"), "{text}");
        assert!(text.contains("\"imin\":-9223372036854775808"), "{text}");
        assert!(text.contains("\"imax\":9223372036854775807"), "{text}");
        assert!(text.contains("\"big\":9007199254740993"), "{text}");
    }

    #[test]
    fn non_finite_floats_become_null() {
        let mut doc = AutoCommit::new();
        doc.put(ROOT, "nan", f64::NAN).unwrap();
        doc.put(ROOT, "inf", f64::INFINITY).unwrap();
        doc.put(ROOT, "ninf", f64::NEG_INFINITY).unwrap();
        doc.put(ROOT, "neg_zero", -0.0f64).unwrap();
        let json = to_json(&mut doc);
        assert_eq!(json["nan"], Value::Null);
        assert_eq!(json["inf"], Value::Null);
        assert_eq!(json["ninf"], Value::Null);
        assert_eq!(json["neg_zero"].as_f64(), Some(0.0));
    }

    #[test]
    fn timestamps_and_bytes() {
        let mut doc = AutoCommit::new();
        doc.put(ROOT, "ts", ScalarValue::Timestamp(1_704_164_645_678))
            .unwrap();
        doc.put(ROOT, "epoch", ScalarValue::Timestamp(0)).unwrap();
        doc.put(ROOT, "bytes", ScalarValue::Bytes(b"foobar!".to_vec()))
            .unwrap();
        doc.put(ROOT, "empty", ScalarValue::Bytes(vec![])).unwrap();
        assert_eq!(
            to_json(&mut doc),
            json!({
                "ts": "2024-01-02T03:04:05.678Z",
                "epoch": "1970-01-01T00:00:00.000Z",
                "bytes": "Zm9vYmFyIQ==",
                "empty": ""
            })
        );
    }

    #[test]
    fn unknown_scalar_is_null() {
        let unknown = ScalarValueRef::Unknown {
            type_code: 42,
            bytes: vec![1, 2].into(),
        };
        assert_eq!(scalar_to_json(&unknown), Value::Null);
    }

    #[test]
    fn counters_report_current_value() {
        let mut doc = AutoCommit::new();
        doc.put(ROOT, "c", ScalarValue::counter(10)).unwrap();
        doc.increment(ROOT, "c", 5).unwrap();
        doc.increment(ROOT, "c", -2).unwrap();
        assert_eq!(to_json(&mut doc), json!({ "c": 13 }));

        // Concurrent increments on two forks both count after merging.
        let mut other = doc.fork();
        doc.increment(ROOT, "c", 1).unwrap();
        other.increment(ROOT, "c", 100).unwrap();
        doc.merge(&mut other).unwrap();
        assert_eq!(to_json(&mut doc), json!({ "c": 114 }));
    }

    #[test]
    fn text_objects_are_strings() {
        let mut doc = AutoCommit::new();
        let text = doc.put_object(ROOT, "title", ObjType::Text).unwrap();
        doc.splice_text(&text, 0, 0, "Hello world").unwrap();
        doc.splice_text(&text, 5, 6, ", Automerge").unwrap();
        assert_eq!(to_json(&mut doc), json!({ "title": "Hello, Automerge" }));
    }

    #[test]
    fn text_block_markers_follow_text() {
        let mut doc = AutoCommit::new();
        let text = doc.put_object(ROOT, "body", ObjType::Text).unwrap();
        doc.splice_text(&text, 0, 0, "ab").unwrap();
        doc.split_block(&text, 1).unwrap();
        let expected = doc.text(&text).unwrap();
        assert_eq!(expected.chars().count(), 3, "{expected:?}");
        assert_eq!(to_json(&mut doc), json!({ "body": expected }));
    }

    #[test]
    fn nested_lists_and_maps() {
        let mut doc = AutoCommit::new();
        let items = doc.put_object(ROOT, "items", ObjType::List).unwrap();
        for (i, title) in ["a", "b"].into_iter().enumerate() {
            let item = doc.insert_object(&items, i, ObjType::Map).unwrap();
            doc.put(&item, "title", title).unwrap();
            doc.put(&item, "done", i == 0).unwrap();
            let tags = doc.put_object(&item, "tags", ObjType::List).unwrap();
            doc.insert(&tags, 0, "x").unwrap();
            let nested = doc.insert_object(&tags, 1, ObjType::List).unwrap();
            doc.insert(&nested, 0, 1i64).unwrap();
        }
        let empty_list = doc.put_object(ROOT, "empty_list", ObjType::List).unwrap();
        let _ = empty_list;
        doc.put_object(ROOT, "empty_map", ObjType::Map).unwrap();
        assert_eq!(
            to_json(&mut doc),
            json!({
                "items": [
                    { "title": "a", "done": true, "tags": ["x", [1]] },
                    { "title": "b", "done": false, "tags": ["x", [1]] }
                ],
                "empty_list": [],
                "empty_map": {}
            })
        );
    }

    #[test]
    fn conflicts_resolve_to_automerge_winner() {
        let mut a = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
        a.put(ROOT, "k", "base").unwrap();
        let mut b = a.fork().with_actor(ActorId::from([2u8; 16]));
        a.put(ROOT, "k", "from a").unwrap();
        b.put(ROOT, "k", "from b").unwrap();
        a.merge(&mut b).unwrap();
        b.merge(&mut a).unwrap();

        // Both sides agree, and they agree with what `get` returns.
        let winner = match a.get(ROOT, "k").unwrap().unwrap().0 {
            automerge::Value::Scalar(s) => s.into_owned(),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(a.get_all(ROOT, "k").unwrap().len(), 2);
        let ScalarValue::Str(winner) = winner else {
            panic!("unexpected {winner:?}")
        };
        let expected = json!({ "k": winner.as_str() });
        assert_eq!(to_json(&mut a), expected);
        assert_eq!(to_json(&mut b), expected);
        // The higher actor id wins in Automerge's ordering.
        assert_eq!(expected, json!({ "k": "from b" }));
    }

    #[test]
    fn conflict_between_object_and_scalar() {
        let mut a = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
        let mut b = AutoCommit::new().with_actor(ActorId::from([2u8; 16]));
        a.put(ROOT, "k", 1i64).unwrap();
        let obj = b.put_object(ROOT, "k", ObjType::Map).unwrap();
        b.put(&obj, "nested", true).unwrap();
        a.merge(&mut b).unwrap();
        assert_eq!(to_json(&mut a), json!({ "k": { "nested": true } }));
    }

    #[test]
    fn deleted_keys_and_elements_are_absent() {
        let mut doc = AutoCommit::new();
        doc.put(ROOT, "gone", 1i64).unwrap();
        doc.put(ROOT, "kept", 2i64).unwrap();
        doc.delete(ROOT, "gone").unwrap();
        let list = doc.put_object(ROOT, "l", ObjType::List).unwrap();
        doc.insert(&list, 0, "a").unwrap();
        doc.insert(&list, 1, "b").unwrap();
        doc.delete(&list, 0).unwrap();
        assert_eq!(to_json(&mut doc), json!({ "kept": 2, "l": ["b"] }));
    }

    #[test]
    fn nul_characters_are_replaced() {
        let mut doc = AutoCommit::new();
        doc.put(ROOT, "a\0b", "c\0d").unwrap();
        assert_eq!(to_json(&mut doc), json!({ "a\u{FFFD}b": "c\u{FFFD}d" }));
    }

    #[test]
    fn excessive_nesting_is_an_error_not_a_crash() {
        let mut doc = AutoCommit::new();
        let mut obj = ROOT;
        for _ in 0..MAX_DEPTH + 1 {
            obj = doc.put_object(&obj, "x", ObjType::Map).unwrap();
        }
        assert!(matches!(
            doc_to_json(doc.document()),
            Err(Error::Internal(_))
        ));
    }
}
