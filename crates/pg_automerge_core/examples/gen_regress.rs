//! Prints the psql `\set` lines holding the fixture documents used by
//! tests/pg_regress/sql/automerge.sql. Regenerate with
//! `cargo run -p pg_automerge_core --example gen_regress`.

use automerge::transaction::{CommitOptions, Transactable};
use automerge::{ActorId, AutoCommit, ObjType, ROOT, ScalarValue};
use pg_automerge_core::encoding::to_hex_literal;

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
}
