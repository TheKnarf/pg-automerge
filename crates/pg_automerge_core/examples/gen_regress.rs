//! Prints the psql `\set` lines holding the fixture documents used by
//! tests/pg_regress/sql/automerge.sql. Regenerate with
//! `cargo run -p pg_automerge_core --example gen_regress`.

use automerge::marks::{ExpandMark, Mark};
use automerge::transaction::{CommitOptions, Transactable};
use automerge::{ActorId, AutoCommit, ObjType, ROOT, ScalarValue};
use pg_automerge_core::encoding::to_hex_literal;

#[path = "../tests/common/craft.rs"]
mod craft;

fn hex(doc: &mut AutoCommit) -> String {
    to_hex_literal(&doc.document().save_nocompress())
}

fn main() {
    let mut base = AutoCommit::new().with_actor(ActorId::from([0x01u8; 16]));
    let title = base.put_object(ROOT, "title", ObjType::Text).unwrap();
    base.splice_text(&title, 0, 0, "Groceries").unwrap();
    base.put(ROOT, "status", "open").unwrap();
    let items = base.put_object(ROOT, "items", ObjType::List).unwrap();
    let milk = base.insert_object(&items, 0, ObjType::Map).unwrap();
    base.put(&milk, "name", "milk").unwrap();
    base.put(&milk, "done", false).unwrap();

    let mut alice = base.fork().with_actor(ActorId::from([0xaau8; 16]));
    alice.put(&milk, "done", true).unwrap();
    alice.splice_text(&title, 9, 0, " for Sunday").unwrap();

    let mut bob = base.fork().with_actor(ActorId::from([0xbbu8; 16]));
    let eggs = bob.insert_object(&items, 1, ObjType::Map).unwrap();
    bob.put(&eggs, "name", "eggs").unwrap();
    bob.put(&eggs, "done", false).unwrap();
    // Only bob's own changes: what a backend persisting incrementally sends.
    let bob_changes = bob.save_after(&base.get_heads());

    let mut types = AutoCommit::new().with_actor(ActorId::from([0x02u8; 16]));
    types.put(ROOT, "str", "hello").unwrap();
    types.put(ROOT, "int", -42i64).unwrap();
    types.put(ROOT, "uint", u64::MAX).unwrap();
    types.put(ROOT, "float", 3.25f64).unwrap();
    types.put(ROOT, "nan", f64::NAN).unwrap();
    types.put(ROOT, "bool", true).unwrap();
    types.put(ROOT, "null", ScalarValue::Null).unwrap();
    types.put(ROOT, "visits", ScalarValue::counter(1)).unwrap();
    types.increment(ROOT, "visits", 41).unwrap();
    types
        .put(ROOT, "created", ScalarValue::Timestamp(1_704_164_645_678))
        .unwrap();
    types
        .put(
            ROOT,
            "avatar",
            ScalarValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
        )
        .unwrap();

    println!("\\set base '\\{}'", hex(&mut base));
    println!("\\set alice '\\{}'", hex(&mut alice));
    println!("\\set bob '\\{}'", hex(&mut bob));
    println!("\\set bob_changes '\\{}'", to_hex_literal(&bob_changes));
    println!("\\set types '\\{}'", hex(&mut types));
    // A short history with commit messages and times (Unix seconds).
    let mut log = AutoCommit::new().with_actor(ActorId::from([0x04u8; 16]));
    for (status, message, time) in [
        ("draft", "create", 1_704_164_645),
        ("review", "submit", 1_704_251_045),
        ("published", "publish", 1_704_337_445),
    ] {
        log.put(ROOT, "status", status).unwrap();
        log.commit_with(
            CommitOptions::default()
                .with_message(message)
                .with_time(time),
        );
    }
    println!("\\set log '\\{}'", hex(&mut log));
    // A compressed, incremental save (document chunk + a trailing change).
    let mut inc = base.fork().with_actor(ActorId::from([0x03u8; 16]));
    let heads = inc.get_heads();
    inc.put(ROOT, "status", "closed").unwrap();
    let mut bytes = base.save();
    bytes.extend(inc.save_after(&heads));
    println!("\\set incremental '\\{}'", to_hex_literal(&bytes));
    // A writer that reused bob's actor id: its first change differs from
    // bob's, so the two cannot be merged.
    let mut reused = base.fork().with_actor(ActorId::from([0xbbu8; 16]));
    reused.put(ROOT, "status", "cancelled").unwrap();
    println!("\\set bob_reused '\\{}'", hex(&mut reused));
    // Rich text: a heading and a paragraph block, then marks (a second
    // commit), and a plain string scalar next to it.
    let mut note = AutoCommit::new().with_actor(ActorId::from([0x05u8; 16]));
    note.put(ROOT, "title", "Tips").unwrap();
    let body = note.put_object(ROOT, "body", ObjType::Text).unwrap();
    note.splice_text(&body, 0, 0, "Shopping tipsBuy fresh milk on Sunday.")
        .unwrap();
    for (at, typ, level) in [(13, "paragraph", None), (0, "heading", Some(1i64))] {
        let block = note.split_block(&body, at).unwrap();
        note.put(&block, "type", typ).unwrap();
        note.put_object(&block, "parents", ObjType::List).unwrap();
        let attrs = note.put_object(&block, "attrs", ObjType::Map).unwrap();
        if let Some(level) = level {
            note.put(&attrs, "level", level).unwrap();
        }
    }
    note.commit_with(CommitOptions::default().with_message("write"));
    note.mark(
        &body,
        Mark::new("bold".into(), true, 19, 29),
        ExpandMark::After,
    )
    .unwrap();
    note.mark(
        &body,
        Mark::new("link".into(), "https://example.com/sunday", 33, 39),
        ExpandMark::None,
    )
    .unwrap();
    note.commit_with(CommitOptions::default().with_message("format"));
    println!("\\set note '\\{}'", hex(&mut note));
    // A text block nested 2,000 levels deep, which automerge_spans refuses
    // (Automerge's own rendering of it would overflow the stack).
    let mut deep = AutoCommit::new().with_actor(ActorId::from([0x06u8; 16]));
    let body = deep.put_object(ROOT, "body", ObjType::Text).unwrap();
    deep.splice_text(&body, 0, 0, "x").unwrap();
    let mut obj = deep.split_block(&body, 0).unwrap();
    for _ in 1..2000 {
        obj = deep.put_object(&obj, "m", ObjType::Map).unwrap();
    }
    println!("\\set deep_block '\\{}'", hex(&mut deep));
    // A change chunk whose run-length encoded columns describe 10,000,000
    // list inserts: over 10 GB to load.
    println!(
        "\\set bomb '\\{}'",
        to_hex_literal(&craft::change_ops(10_000_000))
    );
}
