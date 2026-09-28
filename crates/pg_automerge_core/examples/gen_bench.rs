//! Prints SQL that loads the fixtures of tests/bench_expanded.sh: three
//! documents of different shapes and sizes (`bench_doc`), and for each a
//! chain of small incremental change sets (`bench_changes`, change `i`
//! builds on change `i - 1`, as a backend persisting edit by edit sends
//! them) plus concurrent forks (`bench_forks`, full saves of the base with
//! one extra change each, for `merge_agg`).

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ObjType, ROOT};
use pg_automerge_core::encoding::to_hex_literal;

const CHANGES: usize = 20;
const FORKS: usize = 8;

/// One large text (3,000,000 characters) in one change: expensive to load.
fn big_text() -> AutoCommit {
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
fn structured(n: usize) -> AutoCommit {
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

fn main() {
    println!("CREATE TABLE bench_doc (name text PRIMARY KEY, doc automerge NOT NULL);");
    println!("CREATE TABLE bench_changes (name text, i int, c bytea, PRIMARY KEY (name, i));");
    println!("CREATE TABLE bench_forks (name text, i int, doc automerge, PRIMARY KEY (name, i));");
    for (name, mut doc) in [
        ("text3mb", big_text()),
        ("items20k", structured(20_000)),
        ("items2k", structured(2_000)),
    ] {
        println!(
            "INSERT INTO bench_doc VALUES ('{name}', '{}');",
            to_hex_literal(&doc.save())
        );
        let mut writer = doc.fork().with_actor(ActorId::from([2u8; 16]));
        for i in 1..=CHANGES {
            let heads = writer.get_heads();
            writer.put(ROOT, "status", format!("edit {i}")).unwrap();
            writer.put(ROOT, format!("k{i}"), i as i64).unwrap();
            writer.commit();
            println!(
                "INSERT INTO bench_changes VALUES ('{name}', {i}, '{}');",
                to_hex_literal(&writer.save_after(&heads))
            );
        }
        for i in 1..=FORKS {
            let mut fork = doc.fork().with_actor(ActorId::from([0x10 + i as u8; 16]));
            fork.put(ROOT, format!("fork{i}"), true).unwrap();
            println!(
                "INSERT INTO bench_forks VALUES ('{name}', {i}, '{}');",
                to_hex_literal(&fork.save())
            );
        }
    }
}
