//! `spans::write_spans` (SQL `automerge_spans`) against the spans Automerge
//! itself produces (`ReadDoc::spans` / `spans_at`, rendered in the shape of
//! the JavaScript API's `spans()`), plus the exact JSON for the common
//! shapes, path resolution, historical heads and the block depth guard.

use automerge::iter::Span;
use automerge::marks::{ExpandMark, Mark};
use automerge::transaction::Transactable;
use automerge::{
    ActorId, AutoCommit, Automerge, ChangeHash, LoadOptions, ObjId, ObjType, ROOT, ReadDoc,
    ScalarValue, TextEncoding, hydrate,
};
use pg_automerge_core::Error;
use pg_automerge_core::json::{ValueSink, write_owned_scalar};
use pg_automerge_core::loaded::{Input, LoadedDoc};
use pg_automerge_core::spans::{MAX_BLOCK_DEPTH, format_path, parse_index, spans_to_json};
use serde_json::{Value, json};

mod common;

use common::Rng;

fn actor(n: u8) -> ActorId {
    ActorId::from([n; 16])
}

fn scalar(s: &ScalarValue) -> Value {
    let mut sink = ValueSink::default();
    write_owned_scalar(s, &mut sink);
    sink.into_value().unwrap()
}

fn hydrated(v: &hydrate::Value) -> Value {
    match v {
        hydrate::Value::Scalar(s) => scalar(s),
        hydrate::Value::Map(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.replace('\0', "\u{FFFD}"), hydrated(&v.value)))
                .collect(),
        ),
        hydrate::Value::List(l) => Value::Array(l.iter().map(|v| hydrated(&v.value)).collect()),
        hydrate::Value::Text(t) => Value::String(t.to_string()),
    }
}

/// What Automerge's own `spans()` / `spans_at()` says, in the JS API's
/// shape (as automerge-wasm's `export_span` builds it): text runs with
/// their marks (omitted when none), blocks with their hydrated map.
fn reference(doc: &Automerge, text: &ObjId, heads: Option<&[ChangeHash]>) -> Value {
    let spans = match heads {
        Some(h) => doc.spans_at(text, h),
        None => doc.spans(text),
    }
    .unwrap();
    Value::Array(
        spans
            .map(|span| match span {
                Span::Text { text, marks } => {
                    let mut o = json!({"type": "text", "value": text});
                    if let Some(m) = marks.filter(|m| m.num_marks() > 0) {
                        o["marks"] = Value::Object(
                            m.iter()
                                .map(|(k, v)| (k.replace('\0', "\u{FFFD}"), scalar(v)))
                                .collect(),
                        );
                    }
                    o
                }
                Span::Block(map) => {
                    json!({"type": "block", "value": hydrated(&hydrate::Value::Map(map))})
                }
            })
            .collect(),
    )
}

/// `automerge_spans` of `doc` saved and stored.
fn spans(doc: &mut AutoCommit, path: &[&str]) -> Result<Option<Value>, Error> {
    let bytes = doc.save_nocompress();
    spans_to_json(Input::Stored(&bytes), path, None)
}

fn spans_at(
    doc: &mut AutoCommit,
    path: &[&str],
    heads: &[ChangeHash],
) -> Result<Option<Value>, Error> {
    let bytes = doc.save_nocompress();
    spans_to_json(Input::Stored(&bytes), path, Some(heads))
}

/// Our result for `path` equals Automerge's own spans of `text`.
#[track_caller]
fn assert_matches_reference(doc: &mut AutoCommit, path: &[&str], text: &ObjId) -> Value {
    let ours = spans(doc, path).unwrap().expect("a text at the path");
    assert_eq!(ours, reference(doc.document(), text, None));
    ours
}

fn with_text(s: &str) -> (AutoCommit, ObjId) {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.splice_text(&text, 0, 0, s).unwrap();
    (doc, text)
}

fn mark(
    doc: &mut AutoCommit,
    text: &ObjId,
    name: &str,
    value: impl Into<ScalarValue>,
    range: (usize, usize),
    expand: ExpandMark,
) {
    doc.mark(
        text,
        Mark::new(name.into(), value, range.0, range.1),
        expand,
    )
    .unwrap();
}

/// A block with the fields editors (automerge-prosemirror) use.
fn block(doc: &mut AutoCommit, text: &ObjId, at: usize, typ: &str, parents: &[&str]) -> ObjId {
    let b = doc.split_block(text, at).unwrap();
    doc.put(&b, "type", typ).unwrap();
    let p = doc.put_object(&b, "parents", ObjType::List).unwrap();
    for (i, parent) in parents.iter().enumerate() {
        doc.insert(&p, i, *parent).unwrap();
    }
    doc.put_object(&b, "attrs", ObjType::Map).unwrap();
    doc.put(&b, "isEmbed", false).unwrap();
    b
}

#[test]
fn plain_and_empty_text() {
    let (mut doc, text) = with_text("Hello, world");
    let out = assert_matches_reference(&mut doc, &["text"], &text);
    assert_eq!(out, json!([{"type": "text", "value": "Hello, world"}]));

    let (mut doc, text) = with_text("");
    let out = assert_matches_reference(&mut doc, &["text"], &text);
    assert_eq!(out, json!([]));
    // Everything deleted: empty again.
    let (mut doc, text) = with_text("gone");
    doc.splice_text(&text, 0, 4, "").unwrap();
    assert_eq!(
        assert_matches_reference(&mut doc, &["text"], &text),
        json!([])
    );
}

#[test]
fn overlapping_marks_split_runs() {
    let (mut doc, text) = with_text("The quick brown fox");
    mark(&mut doc, &text, "bold", true, (4, 15), ExpandMark::After);
    mark(&mut doc, &text, "italic", true, (10, 19), ExpandMark::After);
    mark(
        &mut doc,
        &text,
        "link",
        "https://example.com",
        (16, 19),
        ExpandMark::None,
    );
    let out = assert_matches_reference(&mut doc, &["text"], &text);
    assert_eq!(
        out,
        json!([
            {"type": "text", "value": "The "},
            {"type": "text", "value": "quick ", "marks": {"bold": true}},
            {"type": "text", "value": "brown", "marks": {"bold": true, "italic": true}},
            {"type": "text", "value": " ", "marks": {"italic": true}},
            {"type": "text", "value": "fox", "marks": {"italic": true, "link": "https://example.com"}},
        ])
    );
}

#[test]
fn mark_values_use_the_jsonb_mapping() {
    let (mut doc, text) = with_text("abcdefghij");
    let e = ExpandMark::None;
    mark(&mut doc, &text, "int", -42i64, (0, 1), e);
    mark(&mut doc, &text, "uint", u64::MAX, (1, 2), e);
    mark(&mut doc, &text, "float", 1.5f64, (2, 3), e);
    mark(&mut doc, &text, "nan", f64::NAN, (3, 4), e);
    mark(
        &mut doc,
        &text,
        "ts",
        ScalarValue::Timestamp(1_704_164_645_678),
        (4, 5),
        e,
    );
    mark(
        &mut doc,
        &text,
        "bytes",
        ScalarValue::Bytes(b"foobar!".to_vec()),
        (5, 6),
        e,
    );
    mark(&mut doc, &text, "no", false, (6, 7), e);
    mark(&mut doc, &text, "nul\0name", "a\0b", (7, 8), e);
    let out = assert_matches_reference(&mut doc, &["text"], &text);
    let marks: Vec<Value> = out
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.get("marks").cloned().unwrap_or(Value::Null))
        .collect();
    assert_eq!(
        marks,
        vec![
            json!({"int": -42}),
            json!({"uint": u64::MAX}),
            json!({"float": 1.5}),
            json!({"nan": null}),
            json!({"ts": "2024-01-02T03:04:05.678Z"}),
            json!({"bytes": "Zm9vYmFyIQ=="}),
            json!({"no": false}),
            json!({"nul\u{FFFD}name": "a\u{FFFD}b"}),
            Value::Null,
        ]
    );
    // The last run ("ij") has no marks, so no "marks" key at all.
    assert_eq!(out[8], json!({"type": "text", "value": "ij"}));
}

#[test]
fn expanding_marks_at_boundaries() {
    let (mut doc, text) = with_text("aaa bbb ccc");
    mark(&mut doc, &text, "after", true, (0, 3), ExpandMark::After);
    mark(&mut doc, &text, "none", true, (4, 7), ExpandMark::None);
    mark(&mut doc, &text, "both", true, (8, 11), ExpandMark::Both);
    // At the end of each mark, and before the start of the last.
    doc.splice_text(&text, 11, 0, "C").unwrap();
    doc.splice_text(&text, 8, 0, "<").unwrap();
    doc.splice_text(&text, 7, 0, "B").unwrap();
    doc.splice_text(&text, 3, 0, "A").unwrap();
    let out = assert_matches_reference(&mut doc, &["text"], &text);
    assert_eq!(
        out,
        json!([
            {"type": "text", "value": "aaaA", "marks": {"after": true}},
            {"type": "text", "value": " "},
            {"type": "text", "value": "bbb", "marks": {"none": true}},
            {"type": "text", "value": "B "},
            {"type": "text", "value": "<cccC", "marks": {"both": true}},
        ])
    );
}

#[test]
fn removed_marks_are_omitted() {
    let (mut doc, text) = with_text("hello world");
    mark(&mut doc, &text, "bold", true, (0, 11), ExpandMark::After);
    mark(&mut doc, &text, "em", true, (0, 5), ExpandMark::After);
    // unmark writes a mark with a null value over the range.
    doc.unmark(&text, "bold", 3, 8, ExpandMark::None).unwrap();
    // A mark set to null outright is a removal too.
    mark(
        &mut doc,
        &text,
        "em",
        ScalarValue::Null,
        (0, 2),
        ExpandMark::None,
    );
    let out = assert_matches_reference(&mut doc, &["text"], &text);
    assert_eq!(
        out,
        json!([
            {"type": "text", "value": "he", "marks": {"bold": true}},
            {"type": "text", "value": "l", "marks": {"bold": true, "em": true}},
            {"type": "text", "value": "lo", "marks": {"em": true}},
            {"type": "text", "value": " wo"},
            {"type": "text", "value": "rld", "marks": {"bold": true}},
        ])
    );
    // Everything unmarked: one run without marks.
    doc.unmark(&text, "bold", 0, 11, ExpandMark::None).unwrap();
    doc.unmark(&text, "em", 0, 11, ExpandMark::None).unwrap();
    let out = assert_matches_reference(&mut doc, &["text"], &text);
    assert_eq!(out, json!([{"type": "text", "value": "hello world"}]));
}

#[test]
fn blocks_split_the_text_and_carry_their_maps() {
    let (mut doc, text) = with_text("TitleFirst itemSecond item");
    block(&mut doc, &text, 26, "list-item", &["ordered-list"]);
    block(&mut doc, &text, 15, "list-item", &["ordered-list"]);
    let heading = block(&mut doc, &text, 5, "paragraph", &[]);
    let h = block(&mut doc, &text, 0, "heading", &[]);
    let attrs = doc.put_object(&h, "attrs", ObjType::Map).unwrap();
    doc.put(&attrs, "level", 1i64).unwrap();
    // An embed: a block that stands for content, not a paragraph break.
    let img = doc.split_block(&text, 29).unwrap();
    doc.put(&img, "type", "image").unwrap();
    doc.put_object(&img, "parents", ObjType::List).unwrap();
    let a = doc.put_object(&img, "attrs", ObjType::Map).unwrap();
    doc.put(&a, "src", "cat.png").unwrap();
    doc.put(&img, "isEmbed", true).unwrap();
    // A block edited after creation.
    doc.put(&heading, "type", "blockquote").unwrap();
    mark(&mut doc, &text, "bold", true, (1, 3), ExpandMark::After);
    let out = assert_matches_reference(&mut doc, &["text"], &text);
    let p = |typ: &str, parents: Value, attrs: Value| json!({"type": "block", "value": {"type": typ, "parents": parents, "attrs": attrs, "isEmbed": false}});
    assert_eq!(
        out,
        json!([
            p("heading", json!([]), json!({"level": 1})),
            {"type": "text", "value": "Ti", "marks": {"bold": true}},
            {"type": "text", "value": "tle"},
            p("blockquote", json!([]), json!({})),
            {"type": "text", "value": "First item"},
            p("list-item", json!(["ordered-list"]), json!({})),
            {"type": "text", "value": "Second item"},
            {"type": "block", "value": {"type": "image", "parents": [], "attrs": {"src": "cat.png"}, "isEmbed": true}},
            p("list-item", json!(["ordered-list"]), json!({})),
        ])
    );
    // Adjacent blocks, a block at the very end, an empty block map.
    let (mut doc, text) = with_text("x");
    doc.split_block(&text, 1).unwrap();
    doc.split_block(&text, 1).unwrap();
    let out = assert_matches_reference(&mut doc, &["text"], &text);
    assert_eq!(
        out,
        json!([
            {"type": "text", "value": "x"},
            {"type": "block", "value": {}},
            {"type": "block", "value": {}},
        ])
    );
}

#[test]
fn block_values_use_the_jsonb_mapping() {
    let (mut doc, text) = with_text("ab");
    let b = doc.split_block(&text, 1).unwrap();
    doc.put(&b, "uint", u64::MAX).unwrap();
    doc.put(&b, "nan", f64::NAN).unwrap();
    doc.put(&b, "ts", ScalarValue::Timestamp(0)).unwrap();
    doc.put(&b, "k\0", "v\0").unwrap();
    let t = doc.put_object(&b, "caption", ObjType::Text).unwrap();
    doc.splice_text(&t, 0, 0, "a caption").unwrap();
    let l = doc.put_object(&b, "list", ObjType::List).unwrap();
    let m = doc.insert_object(&l, 0, ObjType::Map).unwrap();
    doc.put(&m, "deep", true).unwrap();
    let out = assert_matches_reference(&mut doc, &["text"], &text);
    assert_eq!(
        out[1],
        json!({"type": "block", "value": {
            "uint": u64::MAX, "nan": null, "ts": "1970-01-01T00:00:00.000Z", "k\u{FFFD}": "v\u{FFFD}",
            "caption": "a caption", "list": [{"deep": true}],
        }})
    );
    // A counter shows its current value (as in automerge_to_jsonb).
    doc.put(&b, "count", ScalarValue::counter(1)).unwrap();
    doc.increment(&b, "count", 2).unwrap();
    let out = spans(&mut doc, &["text"]).unwrap().unwrap();
    assert_eq!(out[1]["value"]["count"], json!(3));
    assert_eq!(out, reference(doc.document(), &text, None));
}

#[test]
fn unicode_and_non_string_elements() {
    // Emoji with modifiers (several code points, UTF-16 surrogates) before
    // blocks and marks, a literal U+FFFC, and a non-map object inside the
    // text: Automerge shows the last two as U+FFFC characters in the text,
    // and only map objects as blocks.
    let (mut doc, text) = with_text("héllo 👋🏽 wörld \u{FFFC} ✓");
    let n = doc.text(&text).unwrap().chars().count();
    mark(&mut doc, &text, "em", true, (6, 8), ExpandMark::None);
    doc.insert_object(&text, n, ObjType::List).unwrap();
    block(&mut doc, &text, 9, "paragraph", &[]);
    block(&mut doc, &text, 16, "paragraph", &[]);
    doc.splice_text(&text, 0, 0, "🎉").unwrap();
    let out = assert_matches_reference(&mut doc, &["text"], &text);
    let types: Vec<&str> = out
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        ["text", "text", "text", "block", "text", "block", "text"]
    );
    assert_eq!(
        out[1],
        json!({"type": "text", "value": "👋🏽", "marks": {"em": true}})
    );
    assert_eq!(out[6]["value"], json!("\u{FFFC} ✓\u{FFFC}"));
    // The same whatever text encoding the document was loaded with (the
    // blocks are found by index in that encoding).
    let bytes = doc.save_nocompress();
    for encoding in [
        TextEncoding::UnicodeCodePoint,
        TextEncoding::Utf8CodeUnit,
        TextEncoding::Utf16CodeUnit,
    ] {
        let loaded =
            Automerge::load_with_options(&bytes, LoadOptions::new().text_encoding(encoding))
                .unwrap();
        let loaded = LoadedDoc::from_doc(loaded, false).unwrap();
        let got = spans_to_json(Input::Loaded(&loaded), &["text"], None).unwrap();
        assert_eq!(got, Some(out.clone()), "{encoding:?}");
    }
}

#[test]
fn nul_characters_are_replaced() {
    let (mut doc, _) = with_text("a\0b");
    let out = spans(&mut doc, &["text"]).unwrap().unwrap();
    assert_eq!(out, json!([{"type": "text", "value": "a\u{FFFD}b"}]));
}

#[test]
fn conflicting_values_under_the_texts_key() {
    let base = AutoCommit::new().with_actor(actor(1));
    let mut a = base.clone().with_actor(actor(2));
    let mut b = base.clone().with_actor(actor(3));
    let ta = a.put_object(ROOT, "body", ObjType::Text).unwrap();
    a.splice_text(&ta, 0, 0, "from a").unwrap();
    let tb = b.put_object(ROOT, "body", ObjType::Text).unwrap();
    b.splice_text(&tb, 0, 0, "from b").unwrap();
    mark(&mut b, &tb, "bold", true, (0, 4), ExpandMark::After);
    a.merge(&mut b).unwrap();
    assert_eq!(a.get_all(ROOT, "body").unwrap().len(), 2);
    // Automerge's winner (what `get` returns, and what jsonb shows).
    let (_, winner) = a.get(ROOT, "body").unwrap().unwrap();
    assert_eq!(winner, tb);
    let out = assert_matches_reference(&mut a, &["body"], &winner);
    assert_eq!(
        out,
        json!([
            {"type": "text", "value": "from", "marks": {"bold": true}},
            {"type": "text", "value": " b"},
        ])
    );
    // A scalar that wins over a text: not a text object.
    let mut c = base.clone().with_actor(actor(9));
    c.put(ROOT, "body", "plain").unwrap();
    a.merge(&mut c).unwrap();
    let err = spans(&mut a, &["body"]).unwrap_err();
    assert_eq!(
        err,
        Error::InvalidParameter(
            "automerge value at path {body} is a string scalar, not a text object".into()
        )
    );
}

#[test]
fn path_resolution_follows_jsonb() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    let notes = doc.put_object(ROOT, "notes", ObjType::List).unwrap();
    let mut texts = Vec::new();
    for i in 0..3 {
        let note = doc.insert_object(&notes, i, ObjType::Map).unwrap();
        let body = doc.put_object(&note, "body", ObjType::Text).unwrap();
        doc.splice_text(&body, 0, 0, &format!("note {i}")).unwrap();
        doc.put(&note, "n", i as i64).unwrap();
        texts.push(body);
    }
    let odd = doc.put_object(ROOT, "odd keys", ObjType::Map).unwrap();
    let t = doc.put_object(&odd, "", ObjType::Text).unwrap();
    doc.splice_text(&t, 0, 0, "empty key").unwrap();
    doc.put(ROOT, "counter", ScalarValue::counter(1)).unwrap();
    doc.put_object(ROOT, "scalar list", ObjType::List).unwrap();
    let body = |i: usize| json!([{"type": "text", "value": format!("note {i}")}]);
    for (path, expected) in [
        (vec!["notes", "0", "body"], Some(body(0))),
        (vec!["notes", "2", "body"], Some(body(2))),
        (vec!["notes", "-1", "body"], Some(body(2))),
        (vec!["notes", "-3", "body"], Some(body(0))),
        (vec!["notes", "+1", "body"], Some(body(1))),
        (vec!["notes", " 1", "body"], Some(body(1))),
        (vec!["notes", "001", "body"], Some(body(1))),
        (
            vec!["odd keys", ""],
            Some(json!([{"type": "text", "value": "empty key"}])),
        ),
        // Nothing there: NULL, as jsonb's #> says.
        (vec!["notes", "3", "body"], None),
        (vec!["notes", "-4", "body"], None),
        (vec!["notes", "1 ", "body"], None),
        (vec!["notes", "x", "body"], None),
        (vec!["notes", "", "body"], None),
        (vec!["notes", "-", "body"], None),
        (vec!["notes", "2147483648", "body"], None),
        (vec!["notes", "-2147483649", "body"], None),
        (vec!["notes", "0", "missing"], None),
        (vec!["missing"], None),
        (vec!["missing", "deeper"], None),
        // Stepping into a scalar or a text (a string in jsonb).
        (vec!["notes", "0", "n", "x"], None),
        (vec!["notes", "0", "body", "0"], None),
    ] {
        assert_eq!(spans(&mut doc, &path).unwrap(), expected, "{path:?}");
    }
    for (path, what) in [
        (vec![], "{} is a map"),
        (vec!["notes"], "{notes} is a list"),
        (vec!["notes", "0"], "{notes,0} is a map"),
        (vec!["notes", "0", "n"], "{notes,0,n} is an integer"),
        (vec!["counter"], "{counter} is a counter"),
        (vec!["odd keys"], "{\"odd keys\"} is a map"),
        (vec!["scalar list"], "{\"scalar list\"} is a list"),
    ] {
        assert_eq!(
            spans(&mut doc, &path).unwrap_err(),
            Error::InvalidParameter(format!("automerge value at path {what}, not a text object")),
            "{path:?}"
        );
    }
}

#[test]
fn historical_heads() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.commit();
    let empty_text = doc.get_heads();
    doc.splice_text(&text, 0, 0, "Hello world").unwrap();
    doc.commit();
    let plain = doc.get_heads();
    mark(&mut doc, &text, "bold", true, (0, 5), ExpandMark::After);
    let b = block(&mut doc, &text, 5, "paragraph", &[]);
    doc.commit();
    let marked = doc.get_heads();
    doc.put(&b, "type", "heading").unwrap();
    doc.unmark(&text, "bold", 0, 2, ExpandMark::None).unwrap();
    doc.splice_text(&text, 0, 5, "Bye").unwrap();
    doc.commit();
    let current = doc.get_heads();
    let d = doc.document().clone();
    for heads in [&empty_text, &plain, &marked, &current] {
        let got = spans_at(&mut doc, &["text"], heads).unwrap().unwrap();
        assert_eq!(got, reference(&d, &text, Some(heads)), "{heads:?}");
    }
    assert_eq!(
        spans_at(&mut doc, &["text"], &empty_text).unwrap().unwrap(),
        json!([])
    );
    assert_eq!(
        spans_at(&mut doc, &["text"], &marked).unwrap().unwrap(),
        json!([
            {"type": "text", "value": "Hello", "marks": {"bold": true}},
            {"type": "block", "value": {"type": "paragraph", "parents": [], "attrs": {}, "isEmbed": false}},
            {"type": "text", "value": " world"},
        ])
    );
    assert_eq!(
        spans_at(&mut doc, &["text"], &current).unwrap(),
        spans(&mut doc, &["text"]).unwrap()
    );
    // Before any change the root is empty: nothing at the path.
    assert_eq!(spans_at(&mut doc, &["text"], &[]).unwrap(), None);
    assert_eq!(
        spans_at(&mut doc, &[], &[]).unwrap_err(),
        Error::InvalidParameter("automerge value at path {} is a map, not a text object".into())
    );
    let unknown = ChangeHash([0xab; 32]);
    assert_eq!(
        spans_at(&mut doc, &["text"], &[unknown]).unwrap_err(),
        Error::InvalidParameter(format!(
            "automerge document does not contain change {unknown}"
        ))
    );
}

#[test]
fn deep_blocks_are_refused_before_automerge_renders_them() {
    // A block nested `depth` levels deep (the block's map is level 1).
    let nested = |depth: usize| {
        let (mut doc, text) = with_text("ab");
        let mut obj = doc.split_block(&text, 1).unwrap();
        for i in 1..depth {
            obj = if i % 2 == 0 {
                doc.put_object(&obj, "m", ObjType::Map).unwrap()
            } else {
                let l = doc.put_object(&obj, "l", ObjType::List).unwrap();
                doc.insert_object(&l, 0, ObjType::Map).unwrap()
            };
        }
        doc
    };
    // Lists count as levels too: build exactly `depth` containers.
    let exact = |depth: usize| {
        let (mut doc, text) = with_text("ab");
        let mut obj = doc.split_block(&text, 1).unwrap();
        for _ in 1..depth {
            obj = doc.put_object(&obj, "m", ObjType::Map).unwrap();
        }
        doc
    };
    let mut ok = exact(MAX_BLOCK_DEPTH);
    let out = spans(&mut ok, &["text"]).unwrap().unwrap();
    assert_eq!(out.as_array().unwrap().len(), 3);
    let limit = Error::LimitExceeded(format!(
        "automerge text block is nested more than {MAX_BLOCK_DEPTH} levels deep"
    ));
    assert_eq!(
        spans(&mut exact(MAX_BLOCK_DEPTH + 1), &["text"]).unwrap_err(),
        limit
    );
    // Deep enough to overflow any stack in Automerge's recursive rendering
    // (a few hundred levels do): refused, not a crash.
    assert_eq!(spans(&mut nested(20_000), &["text"]).unwrap_err(), limit);
    // Also at earlier heads, where the block was shallow: fine then.
    let (mut doc, text) = with_text("ab");
    let mut obj = doc.split_block(&text, 1).unwrap();
    doc.commit();
    let shallow = doc.get_heads();
    for _ in 0..2000 {
        obj = doc.put_object(&obj, "m", ObjType::Map).unwrap();
    }
    assert!(spans_at(&mut doc, &["text"], &shallow).unwrap().is_some());
    assert_eq!(spans(&mut doc, &["text"]).unwrap_err(), limit);
}

#[test]
fn blocks_are_found_and_checked_as_of_the_heads() {
    // A block deep at earlier heads whose deep part was deleted since:
    // refused at those heads (Automerge would render it), fine now.
    let limit = Error::LimitExceeded(format!(
        "automerge text block is nested more than {MAX_BLOCK_DEPTH} levels deep"
    ));
    let (mut doc, text) = with_text("abc");
    let b = doc.split_block(&text, 1).unwrap();
    let mut obj = doc.put_object(&b, "m", ObjType::Map).unwrap();
    for _ in 0..2000 {
        obj = doc.put_object(&obj, "m", ObjType::Map).unwrap();
    }
    doc.commit();
    let deep = doc.get_heads();
    doc.delete(&b, "m").unwrap();
    // A second block, joined (deleted) again: present only in between.
    let second = block(&mut doc, &text, 3, "heading", &["ul"]);
    doc.put(&second, "n", 1).unwrap();
    doc.commit();
    let two_blocks = doc.get_heads();
    doc.join_block(&text, 3).unwrap();
    doc.commit();
    let d = doc.document().clone();
    assert_eq!(spans_at(&mut doc, &["text"], &deep).unwrap_err(), limit);
    let got = spans_at(&mut doc, &["text"], &two_blocks).unwrap().unwrap();
    assert_eq!(got, reference(&d, &text, Some(&two_blocks)));
    assert_eq!(
        got,
        json!([
            {"type": "text", "value": "a"},
            {"type": "block", "value": {}},
            {"type": "text", "value": "b"},
            {"type": "block", "value": {
                "type": "heading", "parents": ["ul"], "attrs": {}, "isEmbed": false, "n": 1,
            }},
            {"type": "text", "value": "c"},
        ])
    );
    let now = assert_matches_reference(&mut doc, &["text"], &text);
    assert_eq!(
        now,
        json!([
            {"type": "text", "value": "a"},
            {"type": "block", "value": {}},
            {"type": "text", "value": "bc"},
        ])
    );
}

#[test]
fn block_keys_that_collide_after_nul_replacement_are_deterministic() {
    // "k\0" and "k\u{FFFD}" are one jsonb key after U+0000 is replaced:
    // the block value keeps the same one as automerge_to_jsonb does for
    // an ordinary map, whatever the order of the hydrated map (a hash map,
    // whose order differs between instances).
    for round in 0..32 {
        let (mut doc, text) = with_text("ab");
        let b = doc.split_block(&text, 1).unwrap();
        let m = doc.put_object(ROOT, "m", ObjType::Map).unwrap();
        for obj in [&b, &m] {
            for i in 0..8 {
                doc.put(obj, format!("x{i}"), i).unwrap();
            }
            doc.put(obj, "k\0", "nul").unwrap();
            doc.put(obj, "k\u{FFFD}", "replacement").unwrap();
        }
        let out = spans(&mut doc, &["text"]).unwrap().unwrap();
        let bytes = doc.save_nocompress();
        let whole =
            pg_automerge_core::json::doc_to_json(&Automerge::load(&bytes).unwrap()).unwrap();
        assert_eq!(out[1]["value"], whole["m"], "round {round}");
        assert_eq!(out[1]["value"]["k\u{FFFD}"], json!("replacement"));
    }
}

#[test]
fn expanded_and_stored_inputs_agree() {
    let (mut doc, text) = with_text("hello");
    block(&mut doc, &text, 2, "paragraph", &[]);
    let stored = spans(&mut doc, &["text"]).unwrap();
    let loaded = LoadedDoc::from_doc(doc.document().clone(), false).unwrap();
    assert_eq!(
        spans_to_json(Input::Loaded(&loaded), &["text"], None).unwrap(),
        stored
    );
}

#[test]
fn path_text_and_index_parsing() {
    assert_eq!(format_path(&[]), "{}");
    assert_eq!(format_path(&["notes", "0", "body"]), "{notes,0,body}");
    assert_eq!(
        format_path(&["a b", "", "NULL", "q\"x", "b\\s", "{,}"]),
        r#"{"a b","","NULL","q\"x","b\\s","{,}"}"#
    );
    let long = "x".repeat(500);
    assert_eq!(format_path(&[&long]).chars().count(), 203);
    for (s, i) in [
        ("0", Some(0)),
        ("12", Some(12)),
        ("-1", Some(-1)),
        ("+7", Some(7)),
        ("  \t3", Some(3)),
        ("0000000000000000005", Some(5)),
        ("2147483647", Some(2_147_483_647)),
        ("-2147483648", Some(-2_147_483_648)),
        ("2147483648", None),
        ("99999999999999999999", None),
        ("", None),
        ("+", None),
        ("1 ", None),
        ("1.0", None),
        ("0x1", None),
        ("--1", None),
        ("١", None),
    ] {
        assert_eq!(parse_index(s), i, "{s:?}");
    }
}

/// Random rich text edits on concurrent replicas, merged: every state
/// (current and at each replica's heads) matches Automerge's spans.
#[test]
fn random_rich_text_matches_automerge() {
    for seed in 1..=40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut base = AutoCommit::new().with_actor(actor(0xf0));
        let text = base.put_object(ROOT, "t", ObjType::Text).unwrap();
        base.splice_text(&text, 0, 0, "seed text").unwrap();
        base.commit();
        let mut replicas: Vec<AutoCommit> = (0..3)
            .map(|i| base.fork().with_actor(actor(i + 1)))
            .collect();
        let mut all_heads = Vec::new();
        for _ in 0..30 {
            let r = &mut replicas[rng.below(3) as usize];
            let len = r.text(&text).unwrap().chars().count();
            let at = rng.below(len as u64 + 1) as usize;
            match rng.below(6) {
                0 | 1 => {
                    let s = ["x", "yz", "🙂", "\u{FFFC}", "long run "][rng.below(5) as usize];
                    r.splice_text(&text, at, 0, s).unwrap();
                }
                2 if len > 0 => {
                    let start = rng.below(len as u64) as usize;
                    let n = 1 + rng.below((len - start).min(3) as u64) as usize;
                    r.splice_text(&text, start, n as isize, "").unwrap();
                }
                3 if at < len => {
                    let end = at + 1 + rng.below((len - at) as u64) as usize;
                    let name = ["bold", "em", "link"][rng.below(3) as usize];
                    let value: ScalarValue = match rng.below(3) {
                        0 => true.into(),
                        1 => ScalarValue::Null,
                        _ => format!("v{}", rng.below(3)).into(),
                    };
                    let expand = [
                        ExpandMark::After,
                        ExpandMark::None,
                        ExpandMark::Both,
                        ExpandMark::Before,
                    ][rng.below(4) as usize];
                    r.mark(&text, Mark::new(name.into(), value, at, end), expand)
                        .unwrap();
                }
                4 => {
                    let b = r.split_block(&text, at).unwrap();
                    r.put(&b, "type", ["p", "h1", "li"][rng.below(3) as usize])
                        .unwrap();
                    let attrs = r.put_object(&b, "attrs", ObjType::Map).unwrap();
                    r.put(&attrs, "n", rng.below(10) as i64).unwrap();
                }
                _ => {
                    let other = rng.below(3) as usize;
                    let mut src = replicas[other].fork();
                    replicas[(other + 1) % 3].merge(&mut src).unwrap();
                }
            }
            let r = &mut replicas[rng.below(3) as usize];
            r.commit();
            all_heads.push(r.get_heads());
        }
        let mut merged = replicas[0].fork().with_actor(actor(0xee));
        for r in &mut replicas[1..] {
            merged.merge(r).unwrap();
        }
        let d = merged.document().clone();
        let bytes = merged.save_nocompress();
        let ours = spans_to_json(Input::Stored(&bytes), &["t"], None)
            .unwrap()
            .unwrap();
        assert_eq!(ours, reference(&d, &text, None), "seed {seed}");
        for heads in &all_heads {
            let ours = spans_to_json(Input::Stored(&bytes), &["t"], Some(heads))
                .unwrap()
                .unwrap();
            assert_eq!(
                ours,
                reference(&d, &text, Some(heads)),
                "seed {seed} at {heads:?}"
            );
        }
    }
}
