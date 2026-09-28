//! Prints the fixture documents used by tests/concurrency.sh as shell
//! assignments (`NAME='\x<hex>'`), for `eval` in the script.
//!
//! Every fixture is a full save of a fork of one common base document, as a
//! backend would persist after syncing with a client. Each fork has its own
//! actor id and makes its own edit, so the correct final state contains all of
//! them; a lost update shows up as a missing key.

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ObjType, ROOT};
use pg_automerge_core::encoding::to_hex_literal;

fn main() {
    let mut base = AutoCommit::new().with_actor(ActorId::from([0x01u8; 16]));
    base.put(ROOT, "base", true).unwrap();
    let items = base.put_object(ROOT, "items", ObjType::List).unwrap();
    base.insert(&items, 0, "from base").unwrap();

    let print = |name: &str, doc: &mut AutoCommit| {
        println!("{name}='{}'", to_hex_literal(&doc.save()));
    };
    print("BASE", &mut base);

    // One fork per concurrent writer. Each sets its own key and appends to
    // the shared list, so both edits of a pair touch the same object.
    for (i, name) in [
        "A", "B", "UPSERT_A", "UPSERT_B", "NEW_A", "NEW_B", "RR_A", "RR_B",
    ]
    .into_iter()
    .enumerate()
    {
        let mut fork = base.fork().with_actor(ActorId::from([0x10 + i as u8; 16]));
        let key = name.to_lowercase();
        fork.put(ROOT, key.as_str(), true).unwrap();
        fork.insert(&items, 1, format!("from {key}")).unwrap();
        print(name, &mut fork);
    }
}
