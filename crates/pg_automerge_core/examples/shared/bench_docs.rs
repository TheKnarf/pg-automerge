//! The benchmark documents, shared by `gen_bench` (SQL fixtures of
//! tests/bench_expanded.sh) and `bench_core` (Rust-level timings).
#![allow(dead_code)]

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ObjType, ROOT};

/// One large text (3,000,000 characters) in one change: expensive to load.
pub fn big_text() -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let body: String = (0..3_000_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'a' + (state % 26) as u8)
        })
        .collect();
    doc.splice_text(&text, 0, 0, &body).unwrap();
    doc.put(ROOT, "status", "new").unwrap();
    doc.commit();
    doc
}

/// A list of `n` small maps, committed every 50 items.
pub fn structured(n: usize) -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    let items = doc.put_object(ROOT, "items", ObjType::List).unwrap();
    for i in 0..n {
        let m = doc.insert_object(&items, i, ObjType::Map).unwrap();
        doc.put(&m, "id", i as i64).unwrap();
        doc.put(&m, "title", format!("item number {i}")).unwrap();
        doc.put(&m, "done", i % 3 == 0).unwrap();
        if i % 50 == 49 {
            doc.commit();
        }
    }
    doc.put(ROOT, "status", "new").unwrap();
    doc.commit();
    doc
}

/// A text typed character by character: `n` changes of one character each
/// (a document with many small changes).
pub fn typed_text(n: usize) -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.commit();
    for i in 0..n {
        let c = char::from(b'a' + (i % 26) as u8);
        doc.splice_text(&text, i, 0, &c.to_string()).unwrap();
        doc.commit();
    }
    doc.put(ROOT, "status", "new").unwrap();
    doc.commit();
    doc
}

/// A rich text of `n` paragraphs of 9 characters, each after a block with
/// the fields editors store (type, parents, attrs, isEmbed), every tenth
/// paragraph bold; one change.
pub fn rich_text(n: usize) -> AutoCommit {
    use automerge::marks::{ExpandMark, Mark};
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.splice_text(&text, 0, 0, &"paragraph".repeat(n))
        .unwrap();
    // Blocks from the end, so the indices before them stay put.
    for i in (0..n).rev() {
        let b = doc.split_block(&text, i * 9).unwrap();
        doc.put(&b, "type", "paragraph").unwrap();
        doc.put_object(&b, "parents", ObjType::List).unwrap();
        doc.put_object(&b, "attrs", ObjType::Map).unwrap();
        doc.put(&b, "isEmbed", false).unwrap();
    }
    for i in (0..n).step_by(10) {
        let start = i * 10 + 1;
        doc.mark(
            &text,
            Mark::new("bold".into(), true, start, start + 9),
            ExpandMark::After,
        )
        .unwrap();
    }
    doc.put(ROOT, "status", "new").unwrap();
    doc.commit();
    doc
}
