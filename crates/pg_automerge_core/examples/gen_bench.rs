//! Prints SQL that loads the fixtures of tests/bench_expanded.sh and
//! tests/bench_sql.sh: three documents of different shapes and sizes
//! (`bench_doc`), and for each a chain of small incremental change sets
//! (`bench_changes`, change `i` builds on change `i - 1`, as a backend
//! persisting edit by edit sends them) plus concurrent forks
//! (`bench_forks`, full saves of the base with one extra change each, for
//! `merge_agg`), each document's compressed save as `bytea`
//! (`bench_saves`, for timing writes), and a newer version as a backend
//! sends it (`bench_newer`: the base loaded from its save plus one change
//! by another actor, ROOT "status" = "edited", as a compressed full save
//! `save` and as the changes since the base, `save_after(base heads)`).

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ROOT};
use pg_automerge_core::encoding::to_hex_literal;

#[path = "shared/bench_docs.rs"]
mod bench_docs;

use bench_docs::{big_text, structured};

const CHANGES: usize = 20;
const FORKS: usize = 8;

fn main() {
    println!("CREATE TABLE bench_doc (name text PRIMARY KEY, doc automerge NOT NULL);");
    println!("CREATE TABLE bench_changes (name text, i int, c bytea, PRIMARY KEY (name, i));");
    println!("CREATE TABLE bench_forks (name text, i int, doc automerge, PRIMARY KEY (name, i));");
    println!("CREATE TABLE bench_saves (name text PRIMARY KEY, save bytea NOT NULL);");
    println!(
        "CREATE TABLE bench_newer (name text PRIMARY KEY, save bytea NOT NULL, changes bytea NOT NULL);"
    );
    for (name, mut doc) in [
        ("text3mb", big_text()),
        ("items20k", structured(20_000)),
        ("items2k", structured(2_000)),
    ] {
        // The compressed save a backend sends (`Automerge.save()`), stored
        // as bytea so the benchmark can time its validation.
        let saved = doc.save();
        let save = to_hex_literal(&saved);
        println!("INSERT INTO bench_doc VALUES ('{name}', '{save}');");
        println!("INSERT INTO bench_saves VALUES ('{name}', '{save}');");
        let mut newer = AutoCommit::load(&saved)
            .unwrap()
            .with_actor(ActorId::from([2u8; 16]));
        let base_heads = newer.get_heads();
        newer.put(ROOT, "status", "edited").unwrap();
        newer.commit();
        println!(
            "INSERT INTO bench_newer VALUES ('{name}', '{}', '{}');",
            to_hex_literal(&newer.save()),
            to_hex_literal(&newer.save_after(&base_heads))
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
