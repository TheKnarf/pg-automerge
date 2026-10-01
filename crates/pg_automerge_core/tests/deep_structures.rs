//! No entry point recurses in proportion to a document's structure: every
//! one of them runs on documents nested 20,000 levels deep (maps, lists,
//! blocks holding maps or lists, texts in blocks in texts) and on a
//! history of 20,000 changes, on a thread with a 1 MB stack
//! ([`common::on_small_stack`]), which any recursion of more than about 50
//! bytes per level overflows (aborting the test process). Automerge's own
//! recursive rendering of a block (`hydrate`, about 13 kB per level in a
//! release build) is only reached by `automerge_spans`, after its depth
//! check, and never by the JSON walks (see
//! docs/src/pages/design/deep-blocks.mdx).
//!
//! The results are checked too: the jsonb views of deep maps and lists
//! stop at the nesting cap (54000), those of deep blocks are the text with
//! U+FFFC, and spans of a deep block are refused (54000).

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, Automerge, ChangeHash, ObjType, ROOT};
use pg_automerge_core::json::{self, ValueSink};
use pg_automerge_core::loaded::{self, Input, LoadedDoc, MergeOutcome};
use pg_automerge_core::{Error, MergeAccumulator, history, normalize, spans};
use serde_json::json;

mod common;

const DEPTH: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    Maps,
    Lists,
    BlockMaps,
    BlockLists,
    TextChain,
    History,
}

fn build(shape: Shape) -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    match shape {
        Shape::Maps => {
            let mut o = doc.put_object(ROOT, "m", ObjType::Map).unwrap();
            for _ in 1..DEPTH {
                o = doc.put_object(&o, "m", ObjType::Map).unwrap();
            }
        }
        Shape::Lists => {
            let mut o = doc.put_object(ROOT, "l", ObjType::List).unwrap();
            for _ in 1..DEPTH {
                o = doc.insert_object(&o, 0, ObjType::List).unwrap();
            }
        }
        Shape::BlockMaps | Shape::BlockLists => {
            let t = doc.put_object(ROOT, "body", ObjType::Text).unwrap();
            doc.splice_text(&t, 0, 0, "x").unwrap();
            let mut o = doc.split_block(&t, 0).unwrap();
            for _ in 1..DEPTH {
                o = if shape == Shape::BlockMaps {
                    doc.put_object(&o, "m", ObjType::Map).unwrap()
                } else {
                    let l = doc.put_object(&o, "l", ObjType::List).unwrap();
                    doc.insert_object(&l, 0, ObjType::Map).unwrap()
                };
            }
        }
        Shape::TextChain => {
            // text -> block -> text -> block ...
            let mut t = doc.put_object(ROOT, "body", ObjType::Text).unwrap();
            for _ in 0..DEPTH {
                doc.splice_text(&t, 0, 0, "x").unwrap();
                let b = doc.split_block(&t, 0).unwrap();
                t = doc.put_object(&b, "t", ObjType::Text).unwrap();
            }
        }
        Shape::History => {
            for i in 0..DEPTH {
                doc.put(ROOT, "n", i as i64).unwrap();
                doc.commit();
            }
        }
    }
    // The deep structure is the first change.
    doc.commit();
    doc.put(ROOT, "status", "new").unwrap();
    doc.commit();
    doc
}

/// What the entry points returned, for the checks after the thread ends.
#[derive(Debug)]
struct Seen {
    json: Result<serde_json::Value, Error>,
    json_loaded: Result<serde_json::Value, Error>,
    json_merged: Result<serde_json::Value, Error>,
    json_at_first: Result<serde_json::Value, Error>,
    spans: Result<Option<serde_json::Value>, Error>,
}

fn write_json(input: Input<'_>) -> Result<serde_json::Value, Error> {
    let mut sink = ValueSink::default();
    loaded::write_json(input, &mut sink)?;
    Ok(sink.into_value().expect("one object"))
}

/// Every core entry point on `doc`'s bytes; panics on unexpected errors.
fn exercise(stored: Vec<u8>, compressed: Vec<u8>, changes: Vec<u8>, first: ChangeHash) -> Seen {
    let heads = pg_automerge_core::stored_heads(&stored).unwrap();
    let mut other = AutoCommit::new().with_actor(ActorId::from([2u8; 16]));
    other.put(ROOT, "other", 1).unwrap();
    other.commit();
    let other = other.document().save_nocompress();
    let empty = Automerge::new().save_nocompress();

    // Input: every form normalizes to the same stored bytes.
    assert_eq!(normalize(&stored).unwrap(), stored);
    assert_eq!(normalize(&compressed).unwrap(), stored);
    assert_eq!(
        pg_automerge_core::stored_heads(&normalize(&changes).unwrap()).unwrap(),
        heads
    );
    let external = LoadedDoc::from_external(&compressed).unwrap();
    assert_eq!(external.stored().unwrap(), stored);
    let from_stored = LoadedDoc::from_stored(&stored).unwrap();

    // Reads.
    let json = write_json(Input::Stored(&stored));
    let json_loaded = write_json(Input::Loaded(&external));
    assert_eq!(
        write_json(Input::Loaded(&from_stored)).map_err(|e| e.to_string()),
        json.clone().map_err(|e| e.to_string())
    );
    let doc = Automerge::load(&stored).unwrap();
    let plain = json::doc_to_json(&doc);
    assert_eq!(
        plain.map_err(|e| e.to_string()),
        json.clone().map_err(|e| e.to_string())
    );
    let at_now = history::to_json_at(Input::Stored(&stored), &heads);
    assert_eq!(
        at_now.map_err(|e| e.to_string()),
        json.clone().map_err(|e| e.to_string())
    );
    let json_at_first = history::to_json_at(Input::Stored(&stored), &[first]);
    let spans = spans::spans_to_json(Input::Stored(&stored), &["body"], None);
    let _ = spans::spans_to_json(Input::Stored(&stored), &["body"], Some(&[first]));

    // History.
    let count = history::change_count(Input::Stored(&stored)).unwrap();
    assert_eq!(
        history::changes(Input::Stored(&stored), &[]).unwrap().len() as u64,
        count
    );
    assert_eq!(
        history::changes_meta(Input::Stored(&stored), &[])
            .unwrap()
            .len() as u64,
        count
    );
    assert!(
        !history::changes_bytes(Input::Stored(&stored), &[])
            .unwrap()
            .is_empty()
    );
    assert!(
        history::change(Input::Stored(&stored), &heads[0])
            .unwrap()
            .is_some()
    );

    // Merges and containment, both ways, stored and loaded.
    let MergeOutcome::New(merged) =
        loaded::merge(Input::Stored(&other), Input::Stored(&stored)).unwrap()
    else {
        panic!("a new document")
    };
    let json_merged = write_json(Input::Loaded(&merged));
    merged.stored().unwrap();
    let MergeOutcome::New(merged) =
        loaded::merge(Input::Loaded(&from_stored), Input::Stored(&other)).unwrap()
    else {
        panic!("a new document")
    };
    merged.stored().unwrap();
    let applied = loaded::merge_changes(Input::Stored(&empty), &changes)
        .unwrap()
        .expect("new changes");
    assert_eq!(applied.heads(), &heads[..]);
    applied.stored().unwrap();
    assert!(!loaded::contains(Input::Stored(&other), Input::Stored(&stored)).unwrap());
    assert!(loaded::contains(Input::Stored(&stored), Input::Loaded(&external)).unwrap());
    assert!(!loaded::contains_changes(Input::Stored(&other), &changes).unwrap());
    assert!(loaded::contains_changes(Input::Stored(&stored), &changes).unwrap());
    let mut acc = MergeAccumulator::new();
    acc.add_input(Input::Stored(&other)).unwrap();
    acc.add_input(Input::Stored(&stored)).unwrap();
    acc.add_input(Input::Loaded(&external)).unwrap();
    assert!(acc.finish_loaded().unwrap().is_some());

    Seen {
        json,
        json_loaded,
        json_merged,
        json_at_first,
        spans,
    }
}

fn run(shape: Shape) -> Seen {
    let mut doc = build(shape);
    let stored = doc.document().save_nocompress();
    let compressed = doc.save();
    let changes = doc.save_after(&[]);
    let first = doc.get_changes(&[])[0].hash();
    common::on_small_stack(move || exercise(stored, compressed, changes, first))
}

fn nesting_error() -> Error {
    Error::LimitExceeded(format!(
        "automerge document is nested more than {} levels deep",
        json::MAX_DEPTH
    ))
}

#[test]
fn deep_maps_and_lists() {
    for shape in [Shape::Maps, Shape::Lists] {
        let seen = run(shape);
        for got in [
            seen.json,
            seen.json_loaded,
            seen.json_merged,
            seen.json_at_first,
        ] {
            assert_eq!(got.unwrap_err(), nesting_error(), "{shape:?}");
        }
        assert_eq!(seen.spans.unwrap(), None, "{shape:?}");
    }
}

#[test]
fn deep_blocks() {
    for shape in [Shape::BlockMaps, Shape::BlockLists] {
        let seen = run(shape);
        let expected = json!({"body": "\u{fffc}x", "status": "new"});
        assert_eq!(seen.json.unwrap(), expected, "{shape:?}");
        assert_eq!(seen.json_loaded.unwrap(), expected, "{shape:?}");
        let mut merged = seen.json_merged.unwrap();
        assert_eq!(
            merged.as_object_mut().unwrap().remove("other"),
            Some(json!(1))
        );
        assert_eq!(merged, expected, "{shape:?}");
        // The first change holds the whole block.
        assert_eq!(seen.json_at_first.unwrap(), json!({"body": "\u{fffc}x"}));
        assert_eq!(
            seen.spans.unwrap_err(),
            Error::LimitExceeded(format!(
                "automerge text block is nested more than {} levels deep",
                spans::MAX_BLOCK_DEPTH
            )),
            "{shape:?}"
        );
    }
}

#[test]
fn texts_in_blocks_in_texts() {
    // Texts are leaves both to the JSON walks (a string) and to the depth
    // check of spans (Automerge renders a text inside a block with
    // `text()`, which does not recurse).
    let seen = run(Shape::TextChain);
    let expected = json!({"body": "\u{fffc}x", "status": "new"});
    assert_eq!(seen.json.unwrap(), expected);
    let spans = seen.spans.unwrap().unwrap();
    assert_eq!(
        spans[0],
        json!({"type": "block", "value": {"t": "\u{fffc}x"}})
    );
}

#[test]
fn long_histories() {
    let seen = run(Shape::History);
    assert_eq!(
        seen.json.unwrap(),
        json!({"n": DEPTH as i64 - 1, "status": "new"})
    );
    assert_eq!(seen.json_at_first.unwrap(), json!({"n": 0}));
}
