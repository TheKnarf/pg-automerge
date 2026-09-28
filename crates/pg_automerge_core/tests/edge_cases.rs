//! Adversarial / edge-case tests for the core logic: odd documents, odd
//! scalar values, odd input encodings and corrupted bytes. The SQL-level
//! counterparts live in the `#[pg_test]`s in src/lib.rs.

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ChangeHash, ObjType, ROOT, ReadDoc, ScalarValue};
use pg_automerge_core::json::MAX_DEPTH;
use pg_automerge_core::{
    Error, MergeAccumulator, Merged, contains, heads, merge, normalize, to_json,
};
use serde_json::{Value, json};

fn actor(n: u8) -> ActorId {
    ActorId::from([n; 16])
}

/// Stored bytes of `doc`.
fn stored(doc: &mut AutoCommit) -> Vec<u8> {
    normalize(&doc.save()).unwrap()
}

/// Merge two stored values and return the resulting stored bytes.
fn merged(a: &[u8], b: &[u8]) -> Vec<u8> {
    merge(a, b).unwrap().into_bytes(a, b).into_owned()
}

fn sorted_heads(doc: &mut AutoCommit) -> Vec<String> {
    let mut h: Vec<String> = doc.get_heads().iter().map(ChangeHash::to_string).collect();
    h.sort();
    h
}

// ---------------------------------------------------------------------------
// Empty documents and documents with only deletes
// ---------------------------------------------------------------------------

#[test]
fn empty_document_forms_are_equivalent() {
    let from_nothing = normalize(&[]).unwrap();
    let from_save = normalize(&AutoCommit::new().save()).unwrap();
    let from_nocompress = normalize(&AutoCommit::new().save_nocompress()).unwrap();
    assert_eq!(from_nothing, from_save);
    assert_eq!(from_nothing, from_nocompress);
    assert_eq!(normalize(&from_nothing).unwrap(), from_nothing);

    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "x", 1i64).unwrap();
    let doc = stored(&mut doc);
    let empty = from_nothing;
    // The empty document is contained in everything and merges as a no-op.
    assert_eq!(merge(&doc, &empty).unwrap(), Merged::Left);
    assert_eq!(merge(&empty, &doc).unwrap(), Merged::Right);
    assert_eq!(merge(&empty, &empty).unwrap(), Merged::Left);
    assert!(contains(&doc, &empty).unwrap());
    assert!(contains(&empty, &empty).unwrap());
    assert!(!contains(&empty, &doc).unwrap());

    let mut acc = MergeAccumulator::new();
    acc.add(&empty).unwrap();
    acc.add(&empty).unwrap();
    assert_eq!(to_json(&acc.finish().unwrap().unwrap()).unwrap(), json!({}));
}

#[test]
fn document_with_only_deletes() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "a", 1i64).unwrap();
    let list = doc.put_object(ROOT, "l", ObjType::List).unwrap();
    doc.insert(&list, 0, "x").unwrap();
    doc.commit();
    let before_delete = doc.fork().with_actor(actor(2));
    doc.delete(ROOT, "a").unwrap();
    doc.delete(ROOT, "l").unwrap();
    let bytes = stored(&mut doc);
    assert_eq!(to_json(&bytes).unwrap(), json!({}));
    // Not the empty document: the history is kept.
    assert_ne!(bytes, normalize(&[]).unwrap());
    assert_eq!(heads(&bytes).unwrap(), sorted_heads(&mut doc));

    // A concurrent edit inside the deleted list is lost with the list, but a
    // concurrent new key survives the merge.
    let mut other = before_delete;
    other.insert(&list, 1, "y").unwrap();
    other.put(ROOT, "b", 2i64).unwrap();
    let other = stored(&mut other);
    let m = merged(&bytes, &other);
    assert_eq!(to_json(&m).unwrap(), json!({ "b": 2 }));
    assert_eq!(heads(&m).unwrap(), heads(&merged(&other, &bytes)).unwrap());
}

// ---------------------------------------------------------------------------
// Large and deep documents
// ---------------------------------------------------------------------------

/// A multi-megabyte document: a long text object plus a list of many maps.
fn large_doc(actor_byte: u8) -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(actor(actor_byte));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    // Pseudo-random so neither Automerge nor TOAST compresses it to nothing.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let body: String = (0..1_000_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'a' + (state % 26) as u8)
        })
        .collect();
    doc.splice_text(&text, 0, 0, &body).unwrap();
    let items = doc.put_object(ROOT, "items", ObjType::List).unwrap();
    for i in 0..20_000usize {
        let item = doc.insert_object(&items, i, ObjType::Map).unwrap();
        doc.put(&item, "i", i as i64).unwrap();
        doc.put(&item, "name", format!("item number {i}")).unwrap();
    }
    doc
}

#[test]
fn large_document_round_trip_merge_and_json() {
    let mut doc = large_doc(1);
    let bytes = stored(&mut doc);
    assert!(bytes.len() > 1_000_000, "{} bytes", bytes.len());
    assert_eq!(normalize(&bytes).unwrap(), bytes);

    let mut fork = doc.fork().with_actor(actor(2));
    doc.put(ROOT, "from_a", true).unwrap();
    fork.put(ROOT, "from_b", true).unwrap();
    let text = fork.get(ROOT, "text").unwrap().unwrap().1;
    fork.splice_text(&text, 0, 5, "HELLO").unwrap();
    let (a, b) = (stored(&mut doc), stored(&mut fork));
    let m = merged(&a, &b);
    let json = to_json(&m).unwrap();
    assert_eq!(json["from_a"], true);
    assert_eq!(json["from_b"], true);
    assert_eq!(json["items"].as_array().unwrap().len(), 20_000);
    assert_eq!(
        json["items"][19_999],
        json!({ "i": 19_999, "name": "item number 19999" })
    );
    let text = json["text"].as_str().unwrap();
    assert_eq!(text.len(), 1_000_000);
    assert!(text.starts_with("HELLO"));
}

/// Nest `depth` containers below the root, alternating maps and lists, with
/// a scalar at the bottom.
fn nested(depth: usize) -> AutoCommit {
    let mut doc = AutoCommit::new();
    let mut obj = ROOT;
    for level in 0..depth {
        let in_list = doc.object_type(&obj).unwrap() == ObjType::List;
        let typ = if level % 2 == 0 {
            ObjType::List
        } else {
            ObjType::Map
        };
        obj = if in_list {
            doc.insert_object(&obj, 0, typ).unwrap()
        } else {
            doc.put_object(&obj, "k", typ).unwrap()
        };
    }
    if doc.object_type(&obj).unwrap() == ObjType::List {
        doc.insert(&obj, 0, "bottom").unwrap();
    } else {
        doc.put(&obj, "k", "bottom").unwrap();
    }
    doc
}

#[test]
fn deep_nesting_up_to_the_limit_converts() {
    // The root counts as one level, so MAX_DEPTH - 1 nested containers fit.
    let mut doc = nested(MAX_DEPTH - 1);
    let json = to_json(&stored(&mut doc)).unwrap();
    let mut v = &json;
    let mut depth = 1;
    loop {
        v = match v {
            Value::Object(m) => &m["k"],
            Value::Array(a) => &a[0],
            _ => break,
        };
        depth += 1;
    }
    assert_eq!(v, "bottom");
    assert_eq!(depth, MAX_DEPTH + 1);
    // Serializing it (what the pgrx layer does) works too.
    assert!(serde_json::to_string(&json).unwrap().contains("bottom"));

    let mut too_deep = nested(MAX_DEPTH);
    assert!(matches!(
        to_json(&stored(&mut too_deep)),
        Err(Error::Internal(_))
    ));
}

// ---------------------------------------------------------------------------
// Unicode and scalar edge cases
// ---------------------------------------------------------------------------

#[test]
fn unicode_text_and_keys() {
    let mut doc = AutoCommit::new();
    let keys = [
        "😀",
        "👩‍👩‍👧‍👦 family",
        "e\u{301}", // combining acute
        "عربى",
        "日本語",
        "",
        "\u{FEFF}bom",
        "quote\"back\\slash\nnewline\ttab\u{1}ctl",
    ];
    for (i, key) in keys.iter().enumerate() {
        doc.put(ROOT, *key, format!("{key}:{i}")).unwrap();
    }
    let text = doc.put_object(ROOT, "text 📝", ObjType::Text).unwrap();
    doc.splice_text(&text, 0, 0, "a😀b👩‍👩‍👧‍👦c").unwrap();
    // Edit after multi-unit characters; indexes are in Automerge's text
    // encoding, so compute them from its own length.
    let len = doc.length(&text);
    doc.splice_text(&text, len, 0, "!").unwrap();
    let expected_text = doc.text(&text).unwrap();
    assert!(expected_text.ends_with("c!"));

    let json = to_json(&stored(&mut doc)).unwrap();
    for (i, key) in keys.iter().enumerate() {
        assert_eq!(json[key], Value::String(format!("{key}:{i}")), "{key:?}");
    }
    assert_eq!(json["text 📝"], Value::String(expected_text));
    // The serialized form is valid JSON that round-trips.
    let text = serde_json::to_string(&json).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), json);
}

#[test]
fn numeric_extremes() {
    let mut doc = AutoCommit::new();
    doc.put(ROOT, "imin", i64::MIN).unwrap();
    doc.put(ROOT, "imax", i64::MAX).unwrap();
    doc.put(ROOT, "umax", u64::MAX).unwrap();
    doc.put(ROOT, "umin", 0u64).unwrap();
    doc.put(ROOT, "cmin", ScalarValue::counter(i64::MIN))
        .unwrap();
    doc.put(ROOT, "cmax", ScalarValue::counter(i64::MAX))
        .unwrap();
    doc.put(ROOT, "neg_zero", -0.0f64).unwrap();
    doc.put(ROOT, "nan", f64::NAN).unwrap();
    doc.put(ROOT, "inf", f64::INFINITY).unwrap();
    doc.put(ROOT, "ninf", f64::NEG_INFINITY).unwrap();
    doc.put(ROOT, "fmax", f64::MAX).unwrap();
    doc.put(ROOT, "fmin_pos", f64::MIN_POSITIVE).unwrap();
    doc.put(ROOT, "subnormal", 5e-324f64).unwrap();
    doc.put(ROOT, "tenth", 0.1f64).unwrap();
    doc.put(ROOT, "whole", 3.0f64).unwrap();
    let json = to_json(&stored(&mut doc)).unwrap();
    let text = serde_json::to_string(&json).unwrap();
    for needle in [
        "\"imin\":-9223372036854775808",
        "\"imax\":9223372036854775807",
        "\"umax\":18446744073709551615",
        "\"umin\":0",
        "\"cmin\":-9223372036854775808",
        "\"cmax\":9223372036854775807",
        "\"nan\":null",
        "\"inf\":null",
        "\"ninf\":null",
        "\"fmax\":1.7976931348623157e+308",
        "\"fmin_pos\":2.2250738585072014e-308",
        "\"subnormal\":5e-324",
        "\"tenth\":0.1",
        "\"whole\":3.0",
    ] {
        assert!(text.contains(needle), "{needle} not in {text}");
    }
    assert_eq!(
        json["neg_zero"].as_f64().map(f64::to_bits),
        Some((-0.0f64).to_bits())
    );
}

#[test]
fn counter_overflow_does_not_panic() {
    // Automerge counters wrap or saturate; whichever it is, conversion must
    // yield a number and not panic.
    let mut doc = AutoCommit::new();
    doc.put(ROOT, "c", ScalarValue::counter(i64::MAX)).unwrap();
    doc.increment(ROOT, "c", 1).unwrap();
    let json = to_json(&stored(&mut doc)).unwrap();
    assert!(json["c"].is_i64(), "{json}");
}

#[test]
fn timestamps_around_and_before_the_epoch() {
    let mut doc = AutoCommit::new();
    for (key, ms) in [
        ("minus_one", -1i64),
        ("y1969", -86_400_000),
        ("y1900", -2_208_988_800_000),
        ("y0000", -62_167_219_200_000),
        ("before_y0", -62_167_219_200_001),
        ("min", i64::MIN),
        ("max", i64::MAX),
    ] {
        doc.put(ROOT, key, ScalarValue::Timestamp(ms)).unwrap();
    }
    let json = to_json(&stored(&mut doc)).unwrap();
    assert_eq!(json["minus_one"], "1969-12-31T23:59:59.999Z");
    assert_eq!(json["y1969"], "1969-12-31T00:00:00.000Z");
    assert_eq!(json["y1900"], "1900-01-01T00:00:00.000Z");
    assert_eq!(json["y0000"], "0000-01-01T00:00:00.000Z");
    assert_eq!(json["before_y0"], "-000001-12-31T23:59:59.999Z");
    // Far outside JavaScript's range: still a string, never a panic.
    assert!(json["min"].as_str().unwrap().starts_with('-'));
    assert!(json["max"].as_str().unwrap().starts_with('+'));
}

#[test]
fn bytes_values() {
    let mut doc = AutoCommit::new();
    doc.put(ROOT, "one", ScalarValue::Bytes(vec![0xff]))
        .unwrap();
    doc.put(ROOT, "two", ScalarValue::Bytes(vec![0xff, 0x00]))
        .unwrap();
    doc.put(ROOT, "three", ScalarValue::Bytes(vec![0xff, 0x00, 0x80]))
        .unwrap();
    doc.put(ROOT, "nul", ScalarValue::Bytes(vec![0; 4]))
        .unwrap();
    let all: Vec<u8> = (0..=255).collect();
    doc.put(ROOT, "all", ScalarValue::Bytes(all)).unwrap();
    let json = to_json(&stored(&mut doc)).unwrap();
    assert_eq!(json["one"], "/w==");
    assert_eq!(json["two"], "/wA=");
    assert_eq!(json["three"], "/wCA");
    assert_eq!(json["nul"], "AAAAAA==");
    let all = json["all"].as_str().unwrap();
    assert_eq!(all.len(), 344);
    assert!(all.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g"));
    assert!(all.ends_with("+/w=="));
}

// ---------------------------------------------------------------------------
// Input encodings: incremental chunks, compression, concatenated saves
// ---------------------------------------------------------------------------

#[test]
fn save_followed_by_incremental_chunks_normalizes() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "v", 0i64).unwrap();
    let mut bytes = doc.save();
    for i in 1..=5i64 {
        doc.put(ROOT, "v", i).unwrap();
        doc.put(ROOT, format!("k{i}"), "x".repeat(100)).unwrap();
        bytes.extend(doc.save_incremental());
    }
    let expected = doc.document().save_nocompress();
    assert_eq!(normalize(&bytes).unwrap(), expected);
    assert_eq!(to_json(&expected).unwrap()["v"], 5);

    // Repeated chunks (a backend re-sending changes) are harmless.
    let mut dup = bytes.clone();
    dup.extend(doc.save_after(&[]));
    assert_eq!(normalize(&dup).unwrap(), expected);

    // Two full saves concatenated load as their merge.
    let mut other = AutoCommit::new().with_actor(actor(2));
    other.put(ROOT, "other", true).unwrap();
    let mut both = doc.save();
    both.extend(other.save());
    let m = merged(&expected, &stored(&mut other));
    assert_eq!(normalize(&both).unwrap(), m);
}

#[test]
fn incremental_chunk_alone_is_rejected_without_its_base() {
    // What a backend gets from `save_incremental()` after the first save
    // depends on changes that are not in the chunk: it is not a document.
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "a", 1i64).unwrap();
    doc.save();
    doc.put(ROOT, "b", 2i64).unwrap();
    let incremental = doc.save_incremental();
    assert!(matches!(
        normalize(&incremental),
        Err(Error::InvalidInput(_))
    ));
    // The very first incremental save has no deps and is a valid document.
    let mut fresh = AutoCommit::new();
    fresh.put(ROOT, "a", 1i64).unwrap();
    let first = fresh.save_incremental();
    assert_eq!(
        to_json(&normalize(&first).unwrap()).unwrap(),
        json!({ "a": 1 })
    );
}

#[test]
fn compressed_save_normalizes_to_uncompressed() {
    let mut doc = large_doc(3);
    let compressed = doc.save();
    let plain = doc.save_nocompress();
    // Make sure compression actually kicked in, or the test proves nothing.
    assert!(
        compressed.len() < plain.len(),
        "{} vs {}",
        compressed.len(),
        plain.len()
    );
    assert_eq!(normalize(&compressed).unwrap(), plain);
    assert_eq!(normalize(&plain).unwrap(), plain);
}

#[test]
fn normalization_is_a_fixed_point_for_many_shapes() {
    // Stored values are compared byte-wise in merge's fast path, so
    // normalizing stored bytes again must not change them.
    let mut a = AutoCommit::new().with_actor(actor(1));
    a.put(ROOT, "k", "a").unwrap();
    let t = a.put_object(ROOT, "t", ObjType::Text).unwrap();
    a.splice_text(&t, 0, 0, "hello").unwrap();
    let mut b = a.fork().with_actor(actor(2));
    a.put(ROOT, "k", "a2").unwrap();
    b.put(ROOT, "k", "b").unwrap();
    b.splice_text(&t, 5, 0, " world").unwrap();
    let mut unrelated = AutoCommit::new().with_actor(actor(3));
    unrelated.put(ROOT, "k", "u").unwrap();
    let docs = [stored(&mut a), stored(&mut b), stored(&mut unrelated)];
    for x in &docs {
        for y in &docs {
            let m = merged(x, y);
            assert_eq!(normalize(&m).unwrap(), m);
        }
    }
}

// ---------------------------------------------------------------------------
// Merge topologies
// ---------------------------------------------------------------------------

#[test]
fn merge_unrelated_documents() {
    // No common history: both create the same keys independently.
    let mut a = AutoCommit::new().with_actor(actor(1));
    let mut b = AutoCommit::new().with_actor(actor(2));
    a.put(ROOT, "same", "from a").unwrap();
    b.put(ROOT, "same", "from b").unwrap();
    a.put(ROOT, "only_a", 1i64).unwrap();
    b.put(ROOT, "only_b", 2i64).unwrap();
    let la = a.put_object(ROOT, "list", ObjType::List).unwrap();
    a.insert(&la, 0, "a").unwrap();
    let lb = b.put_object(ROOT, "list", ObjType::List).unwrap();
    b.insert(&lb, 0, "b").unwrap();
    let (sa, sb) = (stored(&mut a), stored(&mut b));

    let ab = merged(&sa, &sb);
    let ba = merged(&sb, &sa);
    assert_eq!(heads(&ab).unwrap(), heads(&ba).unwrap());
    assert_eq!(heads(&ab).unwrap().len(), 2);
    let json = to_json(&ab).unwrap();
    assert_eq!(json, to_json(&ba).unwrap());
    // Same result Automerge itself computes.
    a.merge(&mut b).unwrap();
    assert_eq!(json, to_json(&stored(&mut a)).unwrap());
    // Conflicting keys resolve to one winner (the higher actor), the
    // conflicting lists are not concatenated.
    assert_eq!(json["same"], "from b");
    assert_eq!(json["list"], json!(["b"]));
    assert_eq!(json["only_a"], 1);
    assert_eq!(json["only_b"], 2);
    assert!(!contains(&sa, &sb).unwrap());
    assert!(contains(&ab, &sa).unwrap() && contains(&ab, &sb).unwrap());
}

#[test]
fn merge_with_ancestors_in_both_directions() {
    let mut base = AutoCommit::new().with_actor(actor(1));
    base.put(ROOT, "n", 0i64).unwrap();
    let mut x = base.fork().with_actor(actor(2));
    let mut y = base.fork().with_actor(actor(3));
    x.put(ROOT, "x", 1i64).unwrap();
    y.put(ROOT, "y", 1i64).unwrap();
    let (sx, sy) = (stored(&mut x), stored(&mut y));
    // A descendant with two heads, then one more change on top.
    let xy = merged(&sx, &sy);
    let mut top = AutoCommit::load(&xy).unwrap().with_actor(actor(4));
    top.put(ROOT, "top", true).unwrap();
    let top = stored(&mut top);
    let base = stored(&mut base);

    for ancestor in [&base, &sx, &sy, &xy] {
        assert_eq!(merge(&top, ancestor).unwrap(), Merged::Left);
        assert_eq!(merge(ancestor, &top).unwrap(), Merged::Right);
        assert!(contains(&top, ancestor).unwrap());
        assert!(!contains(ancestor, &top).unwrap());
    }
    assert_eq!(
        to_json(&top).unwrap(),
        json!({ "n": 0, "x": 1, "y": 1, "top": true })
    );
}

#[test]
fn accumulator_order_independent_with_ancestors_and_unrelated() {
    let mut base = AutoCommit::new().with_actor(actor(1));
    base.put(ROOT, "base", true).unwrap();
    let mut f = base.fork().with_actor(actor(2));
    f.put(ROOT, "f", true).unwrap();
    let mut u = AutoCommit::new().with_actor(actor(3));
    u.put(ROOT, "u", true).unwrap();
    let inputs = [
        stored(&mut base),
        stored(&mut f),
        stored(&mut u),
        normalize(&[]).unwrap(),
    ];
    let mut results = vec![];
    // All 24 orders.
    let mut order = [0usize, 1, 2, 3];
    permute(&mut order, 0, &mut |order| {
        let mut acc = MergeAccumulator::new();
        for &i in order {
            acc.add(&inputs[i]).unwrap();
        }
        let out = acc.finish().unwrap().unwrap().into_owned();
        results.push((heads(&out).unwrap(), to_json(&out).unwrap()));
    });
    assert_eq!(results.len(), 24);
    assert!(results.iter().all(|r| r == &results[0]));
    assert_eq!(results[0].1, json!({ "base": true, "f": true, "u": true }));
}

fn permute(xs: &mut [usize; 4], k: usize, f: &mut impl FnMut(&[usize; 4])) {
    if k == xs.len() {
        return f(xs);
    }
    for i in k..xs.len() {
        xs.swap(k, i);
        permute(xs, k + 1, f);
        xs.swap(k, i);
    }
}

// ---------------------------------------------------------------------------
// Garbage input
// ---------------------------------------------------------------------------

/// `normalize` must return (Ok or InvalidInput) and never panic. A panic
/// would still be turned into an ERROR by pgrx, but with an internal error
/// code and a confusing message.
fn assert_clean(bytes: &[u8]) {
    match std::panic::catch_unwind(|| normalize(bytes)) {
        Ok(Ok(_)) | Ok(Err(Error::InvalidInput(_))) => {}
        Ok(Err(e)) => panic!("unexpected error kind for {bytes:02x?}: {e:?}"),
        Err(_) => panic!("normalize panicked on {} bytes: {bytes:02x?}", bytes.len()),
    }
}

fn sample_saves() -> Vec<Vec<u8>> {
    let mut doc = AutoCommit::new().with_actor(actor(9));
    doc.put(ROOT, "s", "hello").unwrap();
    doc.put(ROOT, "n", -5i64).unwrap();
    let l = doc.put_object(ROOT, "l", ObjType::List).unwrap();
    doc.insert(&l, 0, 1.5f64).unwrap();
    let t = doc.put_object(ROOT, "t", ObjType::Text).unwrap();
    doc.splice_text(&t, 0, 0, "text").unwrap();
    let first = doc.save();
    doc.put(ROOT, "later", true).unwrap();
    let mut with_change = first.clone();
    with_change.extend(doc.save_incremental());
    vec![first, doc.save_nocompress(), with_change]
}

#[test]
fn truncated_input_is_rejected_cleanly() {
    for save in sample_saves() {
        for len in 0..save.len() {
            assert_clean(&save[..len]);
        }
    }
}

#[test]
fn corrupted_input_is_rejected_cleanly() {
    for save in sample_saves() {
        for i in 0..save.len() {
            for mask in [0x01u8, 0x80, 0xff] {
                let mut bytes = save.clone();
                bytes[i] ^= mask;
                assert_clean(&bytes);
            }
        }
        // Trailing garbage after a valid save.
        let mut bytes = save.clone();
        bytes.extend_from_slice(b"garbage");
        assert_clean(&bytes);
        assert!(normalize(&bytes).is_err());
    }
}

#[test]
fn corrupted_stored_bytes_are_internal_errors() {
    // Stored bytes are trusted to be valid; if they are not, every reader
    // reports an internal error rather than panicking.
    let mut doc = AutoCommit::new();
    doc.put(ROOT, "x", 1i64).unwrap();
    let mut bad = stored(&mut doc);
    let good = bad.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xff;
    bad.truncate(bad.len() / 2);
    assert!(matches!(to_json(&bad), Err(Error::Internal(_))));
    assert!(matches!(heads(&bad), Err(Error::Internal(_))));
    assert!(matches!(merge(&good, &bad), Err(Error::Internal(_))));
    assert!(matches!(contains(&bad, &good), Err(Error::Internal(_))));
}

// ---------------------------------------------------------------------------
// Structure-aware corruption: valid checksums, malformed chunk bodies
// ---------------------------------------------------------------------------

/// Every chunk starts with magic (4), checksum (4), type (1), uleb128 data
/// length, then the data. The checksum is the first 4 bytes of
/// sha256(type || uleb len || data).
const MAGIC: [u8; 4] = [0x85, 0x6f, 0x4a, 0x83];

fn uleb(mut n: usize, out: &mut Vec<u8>) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_uleb(bytes: &[u8]) -> (usize, usize) {
    let (mut n, mut shift) = (0usize, 0);
    for (i, b) in bytes.iter().enumerate() {
        n |= ((b & 0x7f) as usize) << shift;
        if b & 0x80 == 0 {
            return (n, i + 1);
        }
        shift += 7;
    }
    panic!("unterminated uleb128");
}

/// Split concatenated chunks into (type, data).
fn split_chunks(mut bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut chunks = Vec::new();
    while !bytes.is_empty() {
        assert_eq!(bytes[..4], MAGIC);
        let typ = bytes[8];
        let (len, n) = read_uleb(&bytes[9..]);
        let start = 9 + n;
        chunks.push((typ, bytes[start..start + len].to_vec()));
        bytes = &bytes[start + len..];
    }
    chunks
}

/// Encode a chunk with a correct checksum for whatever `data` is.
fn write_chunk(typ: u8, data: &[u8], out: &mut Vec<u8>) {
    use sha2::{Digest, Sha256};
    let mut header = vec![typ];
    uleb(data.len(), &mut header);
    let hash = Sha256::new()
        .chain_update(&header)
        .chain_update(data)
        .finalize();
    out.extend(MAGIC);
    out.extend(&hash[..4]);
    out.extend(header);
    out.extend(data);
}

/// Deterministic xorshift64 so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

thread_local!(static QUIET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });

/// Run `f` with the default panic message silenced on this thread (the
/// decoder panics we provoke on purpose are caught inside `f`), keeping it
/// for assertion failures and every other test.
fn quietly<T>(f: impl FnOnce() -> T) -> T {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !QUIET.with(|q| q.get()) {
                default(info);
            }
        }));
    });
    QUIET.with(|q| q.set(true));
    let result = f();
    QUIET.with(|q| q.set(false));
    result
}

#[test]
fn checksummed_corruption_is_rejected_cleanly() {
    // Mutations that keep the checksum valid get past Automerge's integrity
    // check into its column decoders, which panic on some malformed data.
    // `normalize` must turn those panics into InvalidInput, and whatever it
    // accepts must be a fully usable stored value.
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut decoder_panics, mut accepted) = (0, 0);
    let saves: Vec<Vec<u8>> = sample_saves()
        .into_iter()
        .map(|s| normalize(&s).unwrap()) // uncompressed document chunk
        .chain(sample_saves().into_iter().skip(2)) // doc chunk + change chunk
        .collect();
    for save in &saves {
        let chunks = split_chunks(save);
        for _ in 0..4000 {
            let mut chunks = chunks.clone();
            let which = rng.below(chunks.len());
            let (_, data) = &mut chunks[which];
            for _ in 0..1 + rng.below(3) {
                if data.is_empty() {
                    data.push(rng.next() as u8);
                }
                let i = rng.below(data.len());
                match rng.below(4) {
                    0 => data[i] ^= 1 << rng.below(8),
                    1 => data[i] = rng.next() as u8,
                    2 => {
                        data.remove(i);
                    }
                    _ => data.insert(i, rng.next() as u8),
                }
            }
            let mut bytes = Vec::new();
            for (typ, data) in &chunks {
                write_chunk(*typ, data, &mut bytes);
            }
            match quietly(|| std::panic::catch_unwind(|| normalize(&bytes))) {
                Err(_) => panic!("normalize panicked on {bytes:02x?}"),
                Ok(Err(Error::InvalidInput(m))) => {
                    if m.contains("malformed data") {
                        decoder_panics += 1;
                    }
                }
                Ok(Err(e)) => panic!("unexpected error kind for {bytes:02x?}: {e:?}"),
                Ok(Ok(stored)) => {
                    accepted += 1;
                    let json = quietly(|| to_json(&stored));
                    assert!(json.is_ok(), "{json:?} for {bytes:02x?}");
                    let heads = quietly(|| heads(&stored));
                    assert!(heads.is_ok(), "{heads:?} for {bytes:02x?}");
                    let again = quietly(|| normalize(&stored));
                    assert_eq!(again.as_ref(), Ok(&stored), "{bytes:02x?}");
                }
            }
        }
    }
    // The test only means something if it reached the decoder's panics.
    assert!(
        decoder_panics > 0,
        "no decoder panic provoked ({accepted} accepted)"
    );
}
