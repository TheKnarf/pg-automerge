//! Conversion of the state of an Automerge document (current, or as of given
//! heads) to JSON, following the mapping table in docs/DESIGN.md.
//!
//! The walk ([`write_json_at`]) emits events into a [`JsonSink`]: the
//! extension builds jsonb directly from them, and [`ValueSink`] builds a
//! `serde_json::Value` ([`doc_to_json`], used by tests and tools).

use std::borrow::Cow;
use std::sync::Arc;

use automerge::iter::{DocItem, ListRange, MapRange, Span};
use automerge::{
    Automerge, ChangeHash, ObjId, ObjType, ROOT, ReadDoc, ScalarValue, ScalarValueRef, ValueRef,
};
use serde_json::{Map, Number, Value};

use crate::Error;
use crate::encoding::{base64, iso8601_millis};

/// Maximum object nesting depth converted. The walk below is iterative, but
/// building the result recurses (jsonb's `convertToJsonb`, dropping a
/// `serde_json::Value`), so a hostile document could otherwise exhaust the
/// backend's stack (Postgres' `check_stack_depth` would stop that too, but
/// this cap gives a clear error). Real documents are nowhere near this deep.
pub const MAX_DEPTH: usize = 1000;

/// Receiver of the events of a JSON walk, in document order: a value is a
/// scalar call or a `begin_*` .. `end_*` bracket, and inside an object
/// every value is preceded by its [`JsonSink::key`].
///
/// Strings and keys never contain U+0000 (replaced by U+FFFD), floats are
/// always finite (non-finite ones arrive as [`JsonSink::null`]), and an
/// object may repeat a key only if the document has two keys that differ
/// just in U+0000 vs U+FFFD; the last one wins, as in `jsonb` and in a
/// `serde_json::Map`.
pub trait JsonSink {
    /// Start an object.
    fn begin_object(&mut self);
    /// End the innermost open object.
    fn end_object(&mut self);
    /// Start an array of (at least, and usually exactly) `len_hint` elements.
    fn begin_array(&mut self, len_hint: usize);
    /// End the innermost open array.
    fn end_array(&mut self);
    /// The key of the next value of the innermost open object.
    fn key(&mut self, key: &str);
    /// A string.
    fn string(&mut self, value: &str);
    /// A signed integer (exact).
    fn int(&mut self, value: i64);
    /// An unsigned integer (exact; may exceed `i64::MAX`).
    fn uint(&mut self, value: u64);
    /// A finite float.
    fn float(&mut self, value: f64);
    /// A boolean.
    fn bool(&mut self, value: bool);
    /// `null`.
    fn null(&mut self);
}

/// Convert the current state of `doc` to a JSON object.
///
/// Integers (`Int`, `Uint`, `Counter`) become exact JSON numbers; they never
/// pass through `f64`. For a key with concurrent conflicting values, the
/// value Automerge's `get` returns (the conflict winner) is used.
///
/// # Errors
///
/// [`Error::LimitExceeded`] for a document nested deeper than
/// [`MAX_DEPTH`]; [`Error::Internal`] if Automerge fails to read it.
pub fn doc_to_json(doc: &Automerge) -> Result<Value, Error> {
    doc_to_json_at(doc, None)
}

/// Convert the state of `doc` as of `heads` (`None`: the current state) to a
/// JSON object, with the same mapping as [`doc_to_json`].
///
/// # Errors
///
/// As [`doc_to_json`].
pub fn doc_to_json_at(doc: &Automerge, heads: Option<&[ChangeHash]>) -> Result<Value, Error> {
    let mut sink = ValueSink::default();
    write_json_at(doc, heads, &mut sink)?;
    Ok(sink.into_value().expect("the walk emits one object"))
}

/// Walk the state of `doc` as of `heads` (`None`: the current state) into
/// `sink`: one object, with the mapping of [`doc_to_json`]. Checks for
/// interrupts (see [`crate::set_interrupt_check`]) every
/// [`crate::TICK_EVERY`] steps.
///
/// Two passes: Automerge's document iterator (`ReadDoc::iter_at`) visits
/// every reachable object once, in one sweep over the op set, and the
/// entries are buffered per object (borrowing from `doc`); then they are
/// emitted depth first from the root. Setting up a `map_range` /
/// `list_range` iterator per object ([`write_json_per_object`]) costs about
/// 5 µs per object, which dominated documents made of many small objects
/// (a list of 20,000 small maps: 141 ms per-object, 64 ms in one sweep).
/// Text comes from the sweep's spans: string runs, and U+FFFC for each
/// block marker, exactly the characters `ReadDoc::text` returns (both put
/// U+FFFC for anything in a text that is not a string, and nothing for
/// marks); on one huge text that is about a quarter slower than `text()`.
/// A document with a `Table` object (legacy: automerge 0.12 no longer
/// creates them, but loads them from old saves; the sweep would read one
/// as a list) takes the per-object walk.
///
/// `heads` must all be changes of `doc` (callers check); Automerge ignores
/// unknown ones. Heads equal to the document's current heads take the
/// current-state path inside Automerge.
///
/// On an error the sink has received an incomplete walk.
///
/// # Errors
///
/// As [`doc_to_json`].
pub fn write_json_at<S: JsonSink + ?Sized>(
    doc: &Automerge,
    heads: Option<&[ChangeHash]>,
    sink: &mut S,
) -> Result<(), Error> {
    match Sweep::collect(doc, heads) {
        Some(sweep) => sweep.emit(sink),
        None => write_json_per_object(doc, heads, sink),
    }
}

/// The visible content of every reachable object, from one sweep.
///
/// Laid out for a document of many small objects (a list of 20,000 small
/// maps: 80,000 items), where this bookkeeping cost a third of the walk
/// with a `Vec` of entries per object and a `HashMap` from object id to
/// content: all entries go to one `Vec` (the iterator yields each object's
/// items together, so an object's entries are one range of it), and
/// objects are found by binary search in an index sorted by id rather than
/// by hashing their ids (SipHash of the id's actor bytes, twice per
/// object). The index costs no sort in practice (see [`ObjIndex`]), and a
/// lookup is O(log n) whatever ids a document holds.
struct Sweep<'a> {
    /// Per object, in the order visited.
    objects: Vec<Content>,
    /// The entries of every map and list, each object's in one range.
    entries: Vec<Entry<'a>>,
    /// The characters of every text object.
    texts: Vec<String>,
    index: ObjIndex,
}

#[derive(Clone, Copy)]
enum Content {
    /// A map's or list's entries, in order: `entries[start..end]`.
    Entries { start: usize, end: usize },
    /// A text's characters: `texts[i]`.
    Text(usize),
}

struct Entry<'a> {
    /// The map key; `None` in lists.
    key: Option<Cow<'a, str>>,
    value: EntryValue<'a>,
}

enum EntryValue<'a> {
    Scalar(ScalarValueRef<'a>),
    Object(ObjId, ObjType),
}

/// Object id → position among the visited objects: the ids sorted by
/// `ObjId`'s own order (counter, then actor bytes; consistent with its
/// equality), searched by bisection.
///
/// Automerge's document iterator visits objects in the order of its
/// internal ids (counter, then index in the document's sorted actor
/// table), which is the same order, so the ids arrive sorted and the check
/// in [`ObjIndex::new`] is all it costs; should they ever not, they are
/// sorted there. Holds the iterator's shared per-object ids (`Arc`), not
/// copies.
struct ObjIndex(Vec<(Arc<ObjId>, usize)>);

impl ObjIndex {
    /// The index of `(id, position)` pairs, one per object (ids distinct).
    fn new(mut ids: Vec<(Arc<ObjId>, usize)>) -> Self {
        if !ids.is_sorted_by(|a, b| a.0 < b.0) {
            ids.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        }
        Self(ids)
    }

    fn get(&self, id: &ObjId) -> Option<usize> {
        self.0
            .binary_search_by(|(probe, _)| (**probe).cmp(id))
            .ok()
            .map(|i| self.0[i].1)
    }
}

impl<'a> Sweep<'a> {
    /// `None` if the document has a `Table` object.
    fn collect(doc: &'a Automerge, heads: Option<&[ChangeHash]>) -> Option<Self> {
        let mut ticker = crate::Ticker::default();
        let mut objects = Vec::new();
        let mut entries = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        let mut ids = Vec::new();
        let mut current: Option<Arc<ObjId>> = None;
        for item in doc.iter_at(ROOT, heads) {
            ticker.tick();
            // The iterator yields each object's items together, and hands
            // out one shared id per object.
            if current.as_ref().is_none_or(|c| !Arc::ptr_eq(c, &item.obj)) {
                objects.push(match item.item {
                    DocItem::Text(_) => {
                        texts.push(String::new());
                        Content::Text(texts.len() - 1)
                    }
                    DocItem::Map(_) | DocItem::List(_) => Content::Entries {
                        start: entries.len(),
                        end: entries.len(),
                    },
                });
                ids.push((Arc::clone(&item.obj), objects.len() - 1));
                current = Some(item.obj);
            }
            let (key, value, id) = match item.item {
                DocItem::Map(m) => {
                    let id = matches!(m.value, ValueRef::Object(_)).then(|| m.id());
                    (Some(m.key), m.value, id)
                }
                DocItem::List(l) => {
                    let id = matches!(l.value, ValueRef::Object(_)).then(|| l.id());
                    (None, l.value, id)
                }
                DocItem::Text(span) => {
                    if let (Some(Content::Text(_)), Some(text)) = (objects.last(), texts.last_mut())
                    {
                        match span {
                            Span::Text { text: run, .. } => text.push_str(&run),
                            Span::Block(_) => text.push('\u{FFFC}'),
                        }
                    }
                    continue;
                }
            };
            let value = match (value, id) {
                (ValueRef::Scalar(scalar), _) => EntryValue::Scalar(scalar),
                (ValueRef::Object(ObjType::Table), _) => return None,
                (ValueRef::Object(typ), Some(id)) => EntryValue::Object(id, typ),
                (ValueRef::Object(_), None) => unreachable!("objects get an id above"),
            };
            if let Some(Content::Entries { end, .. }) = objects.last_mut() {
                entries.push(Entry { key, value });
                *end = entries.len();
            }
        }
        Some(Sweep {
            objects,
            entries,
            texts,
            index: ObjIndex::new(ids),
        })
    }

    fn content(&self, id: &ObjId) -> Option<Content> {
        self.index.get(id).map(|i| self.objects[i])
    }

    /// The entries of a map or list (none if it has no visible entries).
    fn entries(&self, id: &ObjId) -> &[Entry<'a>] {
        match self.content(id) {
            Some(Content::Entries { start, end }) => &self.entries[start..end],
            _ => &[],
        }
    }

    fn emit<S: JsonSink + ?Sized>(&self, sink: &mut S) -> Result<(), Error> {
        let mut ticker = crate::Ticker::default();
        sink.begin_object();
        // Open containers: their remaining entries, and whether a map.
        let mut stack = vec![(self.entries(&ROOT).iter(), true)];
        while let Some((iter, is_map)) = stack.last_mut() {
            ticker.tick();
            let is_map = *is_map;
            let Some(entry) = iter.next() else {
                stack.pop();
                if is_map {
                    sink.end_object();
                } else {
                    sink.end_array();
                }
                continue;
            };
            if let Some(key) = &entry.key {
                sink.key(&sanitize(key));
            }
            match &entry.value {
                EntryValue::Scalar(scalar) => write_scalar(scalar, sink),
                EntryValue::Object(id, ObjType::Text) => match self.content(id) {
                    Some(Content::Text(i)) => sink.string(&sanitize(&self.texts[i])),
                    _ => sink.string(""),
                },
                EntryValue::Object(id, typ) => {
                    if stack.len() >= MAX_DEPTH {
                        return Err(Error::LimitExceeded(format!(
                            "automerge document is nested more than {MAX_DEPTH} levels deep"
                        )));
                    }
                    let children = self.entries(id);
                    let is_map = *typ != ObjType::List;
                    if is_map {
                        sink.begin_object();
                    } else {
                        sink.begin_array(children.len());
                    }
                    stack.push((children.iter(), is_map));
                }
            }
        }
        Ok(())
    }
}

/// [`write_json_at`] one object at a time: a `map_range` / `list_range`
/// iterator per object, `text()` per text. The same output; slower on many
/// small objects, faster on one huge text. Used for documents with `Table`
/// objects (and by tests, as the reference).
///
/// # Errors
///
/// As [`doc_to_json`].
pub fn write_json_per_object<S: JsonSink + ?Sized>(
    doc: &Automerge,
    heads: Option<&[ChangeHash]>,
    sink: &mut S,
) -> Result<(), Error> {
    write_object(doc, heads, &ROOT, ObjType::Map, MAX_DEPTH, sink)
}

/// The per-object walk of [`write_json_per_object`] from the map or list
/// `obj` of type `typ` (a `Table` is walked as a map; not `Text`): one value
/// with the mapping of [`doc_to_json`], at most `max_depth` containers deep
/// (`obj` itself is the first).
///
/// # Errors
///
/// [`Error::LimitExceeded`] for nesting deeper than `max_depth`;
/// [`Error::Internal`] if Automerge fails to read a text.
pub(crate) fn write_object<S: JsonSink + ?Sized>(
    doc: &Automerge,
    heads: Option<&[ChangeHash]>,
    obj: &ObjId,
    typ: ObjType,
    max_depth: usize,
    sink: &mut S,
) -> Result<(), Error> {
    // Depth-first walk with an explicit stack of the containers being
    // read, so the native stack does not grow with document depth. Every
    // container read with `heads` recomputes Automerge's clock for them,
    // which costs a walk of the change graph back to the nearest cached
    // clock.
    let mut ticker = crate::Ticker::default();
    let mut stack = vec![Frame::new(doc, heads, obj, typ, sink)];
    while let Some(top) = stack.last_mut() {
        ticker.tick();
        let Some((value, id)) = top.next_item(sink) else {
            match stack.pop().expect("not empty") {
                Frame::Map(_) => sink.end_object(),
                Frame::List(_) => sink.end_array(),
            }
            continue;
        };
        match (value, id) {
            (ValueRef::Scalar(scalar), _) => write_scalar(&scalar, sink),
            (ValueRef::Object(ObjType::Text), Some(id)) => {
                let text = match heads {
                    Some(heads) => doc.text_at(&id, heads),
                    None => doc.text(&id),
                }
                .map_err(|e| Error::Internal(format!("could not read automerge text: {e}")))?;
                sink.string(&sanitize(&text));
            }
            (ValueRef::Object(typ), Some(id)) => {
                if stack.len() >= max_depth {
                    return Err(Error::LimitExceeded(format!(
                        "automerge document is nested more than {max_depth} levels deep"
                    )));
                }
                stack.push(Frame::new(doc, heads, &id, typ, sink));
            }
            (ValueRef::Object(_), None) => unreachable!("next_item returns an id for objects"),
        }
    }
    Ok(())
}

/// A map or list whose visible entries are being walked.
enum Frame<'a> {
    // `map_range` yields one item per visible key: the conflict winner.
    Map(MapRange<'a>),
    List(ListRange<'a>),
}

impl<'a> Frame<'a> {
    /// Open the container `obj` of type `typ` (and tell the sink).
    fn new<S: JsonSink + ?Sized>(
        doc: &'a Automerge,
        heads: Option<&[ChangeHash]>,
        obj: &ObjId,
        typ: ObjType,
        sink: &mut S,
    ) -> Self {
        match typ {
            ObjType::List => {
                let (iter, len) = match heads {
                    // No length_at: that would be a second clock computation.
                    Some(heads) => (doc.list_range_at(obj, .., heads), 0),
                    None => (doc.list_range(obj, ..), doc.length(obj)),
                };
                sink.begin_array(len);
                Frame::List(iter)
            }
            // Text never gets a frame; it is converted in one go via `text()`.
            ObjType::Map | ObjType::Table | ObjType::Text => {
                sink.begin_object();
                Frame::Map(match heads {
                    Some(heads) => doc.map_range_at(obj, .., heads),
                    None => doc.map_range(obj, ..),
                })
            }
        }
    }

    /// The next entry (its key already passed to the sink, for maps): its
    /// value, and its object id when the value is an object (building an id
    /// is not free, so scalars skip it).
    fn next_item<S: JsonSink + ?Sized>(
        &mut self,
        sink: &mut S,
    ) -> Option<(ValueRef<'a>, Option<ObjId>)> {
        fn id_if_object(value: &ValueRef<'_>, id: impl FnOnce() -> ObjId) -> Option<ObjId> {
            matches!(value, ValueRef::Object(_)).then(id)
        }
        match self {
            Frame::Map(iter) => iter.next().map(|item| {
                sink.key(&sanitize(&item.key));
                let id = id_if_object(&item.value, || item.id());
                (item.value, id)
            }),
            Frame::List(iter) => iter.next().map(|item| {
                let id = id_if_object(&item.value, || item.id());
                (item.value, id)
            }),
        }
    }
}

/// Emit a scalar per the DESIGN.md table.
pub fn write_scalar<S: JsonSink + ?Sized>(scalar: &ScalarValueRef<'_>, sink: &mut S) {
    match scalar {
        ScalarValueRef::Str(s) => sink.string(&sanitize(s)),
        ScalarValueRef::Int(i) | ScalarValueRef::Counter(i) => sink.int(*i),
        ScalarValueRef::Uint(u) => sink.uint(*u),
        // jsonb has no NaN / Infinity.
        ScalarValueRef::F64(f) if f.is_finite() => sink.float(*f),
        ScalarValueRef::F64(_) => sink.null(),
        ScalarValueRef::Boolean(b) => sink.bool(*b),
        ScalarValueRef::Timestamp(ms) => sink.string(&iso8601_millis(*ms)),
        ScalarValueRef::Bytes(bytes) => sink.string(&base64(bytes)),
        ScalarValueRef::Null | ScalarValueRef::Unknown { .. } => sink.null(),
    }
}

/// [`write_scalar`] for an owned [`ScalarValue`] (mark values): the same
/// mapping; a counter is its current value.
pub fn write_owned_scalar<S: JsonSink + ?Sized>(scalar: &ScalarValue, sink: &mut S) {
    match scalar {
        ScalarValue::Str(s) => sink.string(&sanitize(s)),
        ScalarValue::Int(i) => sink.int(*i),
        ScalarValue::Counter(c) => sink.int(i64::from(c)),
        ScalarValue::Uint(u) => sink.uint(*u),
        ScalarValue::F64(f) if f.is_finite() => sink.float(*f),
        ScalarValue::F64(_) => sink.null(),
        ScalarValue::Boolean(b) => sink.bool(*b),
        ScalarValue::Timestamp(ms) => sink.string(&iso8601_millis(*ms)),
        ScalarValue::Bytes(bytes) => sink.string(&base64(bytes)),
        ScalarValue::Null | ScalarValue::Unknown { .. } => sink.null(),
    }
}

/// Map a scalar per the DESIGN.md table, as a `serde_json::Value`.
pub fn scalar_to_json(scalar: &ScalarValueRef<'_>) -> Value {
    let mut sink = ValueSink::default();
    write_scalar(scalar, &mut sink);
    sink.into_value().expect("one scalar was written")
}

/// A [`JsonSink`] building a `serde_json::Value`.
#[derive(Default)]
pub struct ValueSink {
    /// Open containers, innermost last; an object with the key of its next
    /// value.
    open: Vec<Open>,
    done: Option<Value>,
}

enum Open {
    Object(Map<String, Value>, Option<String>),
    Array(Vec<Value>),
}

impl ValueSink {
    /// The value written, once complete.
    pub fn into_value(self) -> Option<Value> {
        if self.open.is_empty() {
            self.done
        } else {
            None
        }
    }

    fn put(&mut self, value: Value) {
        match self.open.last_mut() {
            Some(Open::Object(map, key)) => {
                map.insert(key.take().unwrap_or_default(), value);
            }
            Some(Open::Array(items)) => items.push(value),
            None => self.done = Some(value),
        }
    }
}

impl JsonSink for ValueSink {
    fn begin_object(&mut self) {
        self.open.push(Open::Object(Map::new(), None));
    }
    fn end_object(&mut self) {
        if let Some(Open::Object(map, _)) = self.open.pop() {
            self.put(Value::Object(map));
        }
    }
    fn begin_array(&mut self, len_hint: usize) {
        self.open.push(Open::Array(Vec::with_capacity(len_hint)));
    }
    fn end_array(&mut self) {
        if let Some(Open::Array(items)) = self.open.pop() {
            self.put(Value::Array(items));
        }
    }
    fn key(&mut self, key: &str) {
        if let Some(Open::Object(_, pending)) = self.open.last_mut() {
            *pending = Some(key.to_owned());
        }
    }
    fn string(&mut self, value: &str) {
        self.put(Value::String(value.to_owned()));
    }
    fn int(&mut self, value: i64) {
        self.put(Value::Number(Number::from(value)));
    }
    fn uint(&mut self, value: u64) {
        self.put(Value::Number(Number::from(value)));
    }
    fn float(&mut self, value: f64) {
        self.put(Number::from_f64(value).map_or(Value::Null, Value::Number));
    }
    fn bool(&mut self, value: bool) {
        self.put(Value::Bool(value));
    }
    fn null(&mut self) {
        self.put(Value::Null);
    }
}

/// Postgres text (and therefore jsonb) cannot contain U+0000, and `jsonb_in`
/// rejects the `\u0000` escape. Replace it with U+FFFD so that one stray NUL
/// does not make the whole document unreadable as jsonb.
pub(crate) fn sanitize(s: &str) -> std::borrow::Cow<'_, str> {
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

    /// The object index finds every id and only those, whether the ids
    /// arrive sorted (as from Automerge's iterator) or not, including ids
    /// that share a counter and differ only in the actor, and ids whose
    /// actor-table index disagrees with the actor order.
    #[test]
    fn object_index_finds_exactly_its_ids() {
        let actors: Vec<ActorId> = [[0x30u8; 16], [0x10; 16], [0x20; 16]]
            .into_iter()
            .map(ActorId::from)
            .collect();
        let mut ids = vec![ObjId::Root];
        for counter in [7u64, 1, 900, 3, 7_000_000_000] {
            for (i, actor) in actors.iter().enumerate() {
                ids.push(ObjId::Id(counter, actor.clone(), i));
            }
        }
        let absent = [
            ObjId::Id(2, actors[0].clone(), 0),
            ObjId::Id(7, ActorId::from([0x40u8; 16]), 3),
            ObjId::Id(u64::MAX, actors[1].clone(), 1),
        ];
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        for round in 0..20 {
            let mut order: Vec<usize> = (0..ids.len()).collect();
            if round == 0 {
                order.sort_by(|&a, &b| ids[a].cmp(&ids[b]));
            } else {
                for i in (1..order.len()).rev() {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    order.swap(i, (rng % (i as u64 + 1)) as usize);
                }
            }
            let index = ObjIndex::new(
                order
                    .iter()
                    .map(|&i| (Arc::new(ids[i].clone()), i * 10))
                    .collect(),
            );
            for (i, id) in ids.iter().enumerate() {
                assert_eq!(index.get(id), Some(i * 10), "{id:?}");
            }
            for id in &absent {
                assert_eq!(index.get(id), None, "{id:?}");
            }
        }
        assert_eq!(ObjIndex::new(Vec::new()).get(&ObjId::Root), None);
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
            Err(Error::LimitExceeded(_))
        ));
    }
}
