//! `blocks::has_blocks`, which decides whether the JSON walk may use
//! Automerge's document iterator (which renders every block recursively):
//! true exactly when a map was ever made inside a text object, from the
//! stored bytes, a compressed save, or a save of its own; the safe answer
//! (true) for bytes it cannot read as one document chunk. And the walks
//! that use it (`loaded::write_json`, `history::write_json_at`, a loaded
//! document's cached answer) give the per-object walk's result.

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, Automerge, ObjType, ROOT, ReadDoc};
use pg_automerge_core::json::{self, ValueSink};
use pg_automerge_core::loaded::{self, Input, LoadedDoc, MergeOutcome};
use pg_automerge_core::{blocks, history};

mod common;

use common::{Rng, edit, random_replicas};

fn doc() -> AutoCommit {
    AutoCommit::new().with_actor(ActorId::from([1u8; 16]))
}

/// `has_blocks` from the stored bytes, from a compressed save, and from a
/// save of its own must agree, and match `expected`.
fn check(doc: &mut AutoCommit, expected: bool, what: &str) {
    let stored = doc.document().save_nocompress();
    let compressed = doc.save();
    let loaded = Automerge::load(&stored).unwrap();
    assert_eq!(
        blocks::has_blocks(&loaded, Some(&stored)),
        expected,
        "{what}: stored"
    );
    assert_eq!(
        blocks::has_blocks(&loaded, Some(&compressed)),
        expected,
        "{what}: compressed"
    );
    assert_eq!(
        blocks::has_blocks(&loaded, None),
        expected,
        "{what}: own save"
    );
    assert_eq!(
        blocks::has_blocks(doc.document(), None),
        expected,
        "{what}: in memory"
    );
}

fn per_object(doc: &Automerge, heads: Option<&[automerge::ChangeHash]>) -> serde_json::Value {
    let mut sink = ValueSink::default();
    json::write_json_per_object(doc, heads, &mut sink).unwrap();
    sink.into_value().unwrap()
}

#[test]
fn documents_without_blocks() {
    let mut d = doc();
    check(&mut d, false, "empty");
    d.put(ROOT, "a", 1).unwrap();
    let m = d.put_object(ROOT, "m", ObjType::Map).unwrap();
    let inner = d.put_object(&m, "inner", ObjType::Map).unwrap();
    d.put(&inner, "x", "y").unwrap();
    let l = d.put_object(ROOT, "l", ObjType::List).unwrap();
    for i in 0..50 {
        let item = d.insert_object(&l, i, ObjType::Map).unwrap();
        d.put(&item, "i", i as i64).unwrap();
        let t = d.put_object(&item, "title", ObjType::Text).unwrap();
        d.splice_text(&t, 0, 0, "a title").unwrap();
    }
    // A text holding other objects (not maps) and marks.
    let t = d.put_object(ROOT, "t", ObjType::Text).unwrap();
    d.splice_text(&t, 0, 0, "hello").unwrap();
    d.insert_object(&t, 2, ObjType::List).unwrap();
    d.insert_object(&t, 3, ObjType::Text).unwrap();
    d.mark(
        &t,
        automerge::marks::Mark::new("bold".into(), true, 0, 3),
        automerge::marks::ExpandMark::After,
    )
    .unwrap();
    d.commit();
    check(&mut d, false, "maps, lists of maps, texts without blocks");
    // Deleted maps inside deleted maps are still maps in maps.
    d.delete(ROOT, "m").unwrap();
    d.delete(&l, 3).unwrap();
    d.commit();
    check(&mut d, false, "deleted nested maps");
    // Many objects holding maps: every one is looked up.
    for i in 0..200 {
        let m = d.put_object(ROOT, format!("k{i}"), ObjType::Map).unwrap();
        d.put_object(&m, "m", ObjType::Map).unwrap();
    }
    d.commit();
    check(&mut d, false, "many parents");
}

#[test]
fn documents_with_blocks() {
    let mut d = doc();
    let t = d.put_object(ROOT, "t", ObjType::Text).unwrap();
    d.splice_text(&t, 0, 0, "ab").unwrap();
    d.commit();
    check(&mut d, false, "a text");
    d.split_block(&t, 1).unwrap();
    d.commit();
    check(&mut d, true, "a block");
    // Joined (deleted) again: still in the history.
    d.join_block(&t, 1).unwrap();
    d.commit();
    check(&mut d, true, "a deleted block");
    // A block in a text inside a list inside a map.
    let mut d = doc();
    let m = d.put_object(ROOT, "m", ObjType::Map).unwrap();
    let l = d.put_object(&m, "l", ObjType::List).unwrap();
    let t = d.insert_object(&l, 0, ObjType::Text).unwrap();
    d.splice_text(&t, 0, 0, "x").unwrap();
    d.split_block(&t, 0).unwrap();
    d.commit();
    check(&mut d, true, "a nested text's block");
    // A map put over a text element rather than inserted.
    let mut d = doc();
    let t = d.put_object(ROOT, "t", ObjType::Text).unwrap();
    d.splice_text(&t, 0, 0, "xyz").unwrap();
    if d.put_object(&t, 1, ObjType::Map).is_ok() {
        d.commit();
        check(&mut d, true, "a map put over a character");
    }
}

#[test]
fn bytes_that_are_not_one_document_chunk_say_true() {
    let mut d = doc();
    d.put(ROOT, "a", 1).unwrap();
    d.commit();
    let heads = d.get_heads();
    d.put(ROOT, "b", 2).unwrap();
    d.commit();
    let loaded = d.document().clone();
    let stored = d.document().save_nocompress();
    let trailing = [stored.as_slice(), &d.save_after(&heads)].concat();
    let changes = d.save_after(&[]);
    for (what, bytes) in [
        ("trailing change chunk", trailing.as_slice()),
        ("change chunks", changes.as_slice()),
        ("truncated", &stored[..stored.len() - 1]),
        ("empty", &[][..]),
        ("garbage", &[1u8, 2, 3][..]),
    ] {
        assert!(blocks::has_blocks(&loaded, Some(bytes)), "{what}");
    }
    assert!(!blocks::has_blocks(&loaded, Some(&stored)));
    // The save of another document whose maps are in objects this one
    // lacks: not a save of it, so true.
    let mut other = AutoCommit::new().with_actor(ActorId::from([9u8; 16]));
    let m = other.put_object(ROOT, "m", ObjType::Map).unwrap();
    other.put_object(&m, "n", ObjType::Map).unwrap();
    other.commit();
    assert!(blocks::has_blocks(
        &loaded,
        Some(&other.document().save_nocompress())
    ));
}

#[test]
fn generated_documents_agree_with_a_scan_of_every_object() {
    // The reference: some text object of the document (at any time) has
    // a map element, found by walking every object of every change.
    fn reference(doc: &Automerge) -> bool {
        use automerge::legacy::{ObjectId, OpType};
        doc.get_changes(&[]).iter().any(|change| {
            change.decode().operations.iter().any(|op| match &op.obj {
                ObjectId::Id(id) if op.action == OpType::Make(ObjType::Map) => {
                    let obj = format!("{}@{}", id.0, id.1);
                    doc.import(&obj).map(|(_, t)| t).unwrap() == ObjType::Text
                }
                _ => false,
            })
        })
    }
    let mut seen = [false, false];
    for seed in 0..40 {
        for (i, stored) in random_replicas(seed).into_iter().enumerate() {
            // Every other replica gets a block in its text (made if it has
            // none), in a change of its own, on top of the random edits.
            let stored = if (seed + i as u64).is_multiple_of(2) {
                let mut d = AutoCommit::load(&stored).unwrap();
                let t = match d.get(ROOT, "text").unwrap() {
                    Some((automerge::Value::Object(ObjType::Text), id)) => id,
                    _ => d.put_object(ROOT, "text", ObjType::Text).unwrap(),
                };
                d.split_block(&t, 0).unwrap();
                d.commit();
                d.document().save_nocompress()
            } else {
                stored
            };
            let loaded = Automerge::load(&stored).unwrap();
            let expected = reference(&loaded);
            seen[usize::from(expected)] = true;
            assert_eq!(
                blocks::has_blocks(&loaded, Some(&stored)),
                expected,
                "seed {seed} replica {i}"
            );
        }
    }
    assert_eq!(seen, [true, true], "both kinds were checked");
}

#[test]
fn walks_that_read_stored_bytes_give_the_reference() {
    let mut d = doc();
    let t = d.put_object(ROOT, "t", ObjType::Text).unwrap();
    d.splice_text(&t, 0, 0, "abc").unwrap();
    let b = d.split_block(&t, 1).unwrap();
    d.put(&b, "type", "paragraph").unwrap();
    let mut obj = d.put_object(&b, "m", ObjType::Map).unwrap();
    for _ in 0..3000 {
        obj = d.put_object(&obj, "m", ObjType::Map).unwrap();
    }
    d.commit();
    let first = d.get_changes(&[])[0].hash();
    d.put(ROOT, "status", "new").unwrap();
    d.commit();
    let stored = d.document().save_nocompress();
    let loaded = Automerge::load(&stored).unwrap();
    let expected = per_object(&loaded, None);
    assert_eq!(expected["t"], "a\u{fffc}bc");

    // Every entry point, each in a thread with a small stack: rendering
    // the block with Automerge's `hydrate` would need about 40 MB.
    let expected_at = per_object(&loaded, Some(&[first]));
    let got = common::on_small_stack(move || {
        let mut out = Vec::new();
        let mut sink = ValueSink::default();
        loaded::write_json(Input::Stored(&stored), &mut sink).unwrap();
        out.push(sink.into_value().unwrap());
        let expanded = LoadedDoc::from_stored(&stored).unwrap();
        let mut sink = ValueSink::default();
        loaded::write_json(Input::Loaded(&expanded), &mut sink).unwrap();
        out.push(sink.into_value().unwrap());
        // A merge result: no stored bytes yet (a save is made and kept).
        let mut other = AutoCommit::new().with_actor(ActorId::from([2u8; 16]));
        other.put(ROOT, "other", 1).unwrap();
        other.commit();
        let other = other.document().save_nocompress();
        let MergeOutcome::New(merged) =
            loaded::merge(Input::Stored(&stored), Input::Stored(&other)).unwrap()
        else {
            panic!("a new document")
        };
        assert!(merged.cached_stored().is_none());
        let mut sink = ValueSink::default();
        loaded::write_json(Input::Loaded(&merged), &mut sink).unwrap();
        let mut value = sink.into_value().unwrap();
        value.as_object_mut().unwrap().remove("other");
        out.push(value);
        // The save made for it is the one stored.
        assert_eq!(merged.stored().unwrap(), merged.doc().save_nocompress());
        let doc = Automerge::load(&stored).unwrap();
        out.push(json::doc_to_json(&doc).unwrap());
        let heads = doc.get_heads();
        out.push(history::to_json_at(Input::Stored(&stored), &heads).unwrap());
        out.push(history::to_json_at(Input::Stored(&stored), &[first]).unwrap());
        out
    });
    for (i, value) in got.iter().enumerate() {
        let want = if i == got.len() - 1 {
            &expected_at
        } else {
            &expected
        };
        assert_eq!(value, want, "entry point {i}");
    }
}

#[test]
fn random_edits_with_blocks() {
    // Generated documents plus blocks at random places: the checked walk
    // is the per-object walk.
    for seed in 0..20u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut d = doc();
        let t = d.put_object(ROOT, "text", ObjType::Text).unwrap();
        d.splice_text(&t, 0, 0, "hello world").unwrap();
        for _ in 0..30 {
            edit(&mut d, &mut rng);
            if rng.below(3) == 0 {
                let len = d.length(&t);
                let b = d
                    .split_block(&t, rng.below(len as u64 + 1) as usize)
                    .unwrap();
                d.put(&b, "n", rng.below(100) as i64).unwrap();
            }
            d.commit();
        }
        let stored = d.document().save_nocompress();
        let loaded = Automerge::load(&stored).unwrap();
        assert!(blocks::has_blocks(&loaded, Some(&stored)));
        let mut sink = ValueSink::default();
        loaded::write_json(Input::Stored(&stored), &mut sink).unwrap();
        assert_eq!(
            sink.into_value().unwrap(),
            per_object(&loaded, None),
            "seed {seed}"
        );
    }
}
