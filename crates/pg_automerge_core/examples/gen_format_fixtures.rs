//! Writes the storage-format fixtures of `tests/format_fixtures.rs` into
//! the directory given as the first argument (normally
//! `crates/pg_automerge_core/tests/fixtures/automerge-<version>`, for the
//! automerge version this was run with):
//!
//! - `<name>.save`: `save()` (compressed, what backends send),
//! - `<name>.stored`: `save_nocompress()` (what the extension stores),
//! - `<name>.heads`: the heads, sorted, one hex hash per line,
//! - `<name>.json`: the document as JSON (the jsonb mapping).
//!
//! Run once per automerge version the extension has shipped with and
//! commit the output: the fixtures of every earlier version must keep
//! loading (see docs/src/pages/design/versioning.mdx).

use std::path::PathBuf;

use automerge::transaction::{CommitOptions, Transactable};
use automerge::{ActorId, AutoCommit, ObjType, ROOT, ScalarValue};
use pg_automerge_core::json::doc_to_json;

fn commit(doc: &mut AutoCommit, t: i64) {
    doc.commit_with(
        CommitOptions::default()
            .with_time(t)
            .with_message(format!("t{t}")),
    );
}

/// Every scalar type, text, nested containers, counters, a conflict,
/// deletions and several actors.
fn kitchen_sink() -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    doc.put(ROOT, "str", "hello").unwrap();
    doc.put(ROOT, "int", -42i64).unwrap();
    doc.put(ROOT, "uint", u64::MAX).unwrap();
    doc.put(ROOT, "f64", 0.1f64).unwrap();
    doc.put(ROOT, "bool", true).unwrap();
    doc.put(ROOT, "null", ScalarValue::Null).unwrap();
    doc.put(ROOT, "ts", ScalarValue::Timestamp(1_704_164_645_678))
        .unwrap();
    doc.put(ROOT, "bytes", ScalarValue::Bytes(vec![0, 1, 0xfe, 0xff]))
        .unwrap();
    doc.put(ROOT, "counter", ScalarValue::counter(10)).unwrap();
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.splice_text(&text, 0, 0, "Hello world 😀").unwrap();
    let items = doc.put_object(ROOT, "items", ObjType::List).unwrap();
    for i in 0..30 {
        let m = doc.insert_object(&items, i, ObjType::Map).unwrap();
        doc.put(&m, "id", i as i64).unwrap();
        doc.put(&m, "title", format!("item {i}")).unwrap();
    }
    commit(&mut doc, 1_700_000_000);
    let mut other = doc.fork().with_actor(ActorId::from([2u8; 16]));
    doc.put(ROOT, "str", "ours").unwrap();
    doc.increment(ROOT, "counter", 5).unwrap();
    doc.splice_text(&text, 5, 6, ",").unwrap();
    commit(&mut doc, 1_700_000_100);
    other.put(ROOT, "str", "theirs").unwrap();
    other.increment(ROOT, "counter", -2).unwrap();
    other.delete(&items, 0).unwrap();
    commit(&mut other, 1_700_000_200);
    doc.merge(&mut other).unwrap();
    doc.delete(ROOT, "null").unwrap();
    commit(&mut doc, 1_700_000_300);
    doc
}

/// Many small changes (typing), for the change columns.
fn typed() -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([3u8; 16]));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    commit(&mut doc, 1_700_000_000);
    for i in 0..300 {
        let c = char::from(b'a' + (i % 26) as u8).to_string();
        doc.splice_text(&text, i, 0, &c).unwrap();
        commit(&mut doc, 1_700_000_001 + i as i64);
    }
    doc
}

fn main() {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: gen_format_fixtures <dir>"),
    );
    std::fs::create_dir_all(&dir).unwrap();
    for (name, mut doc) in [
        ("empty", AutoCommit::new()),
        ("kitchen_sink", kitchen_sink()),
        ("typed", typed()),
    ] {
        let save = doc.save();
        let stored = doc.document().save_nocompress();
        let mut heads: Vec<String> = doc.get_heads().iter().map(ToString::to_string).collect();
        heads.sort();
        let json = doc_to_json(doc.document()).unwrap();
        std::fs::write(dir.join(format!("{name}.save")), save).unwrap();
        std::fs::write(dir.join(format!("{name}.stored")), stored).unwrap();
        std::fs::write(
            dir.join(format!("{name}.heads")),
            heads.iter().map(|h| format!("{h}\n")).collect::<String>(),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{name}.json")),
            serde_json::to_string_pretty(&json).unwrap() + "\n",
        )
        .unwrap();
    }
}
