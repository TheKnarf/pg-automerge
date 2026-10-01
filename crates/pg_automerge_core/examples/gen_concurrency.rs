//! Prints the fixture documents used by tests/concurrency.sh and
//! tests/notify.sh as shell assignments (`NAME='\x<hex>'`), for `eval` in
//! the scripts.
//!
//! Every fixture is a full save of a fork of one common base document, as a
//! backend would persist after syncing with a client, except `INC_*`: those
//! are only the fork's own changes (`save_after(base heads)`), as a backend
//! persisting incrementally with `merge(doc, $1::bytea)` sends them. Each fork has its own
//! actor id and makes its own edit, so the correct final state contains all of
//! them; a lost update shows up as a missing key. `MANY` (for the notify
//! test) is the base merged with 150 concurrent forks: 150 heads, too many
//! for a NOTIFY payload. `BIG` (for tests/memory.sh) is a text of 400,000
//! random characters, whose load takes tens of megabytes.

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
        "A", "B", "UPSERT_A", "UPSERT_B", "NEW_A", "NEW_B", "RR_A", "RR_B", "INC_A", "INC_B",
    ]
    .into_iter()
    .enumerate()
    {
        let mut fork = base.fork().with_actor(ActorId::from([0x10 + i as u8; 16]));
        let key = name.to_lowercase();
        fork.put(ROOT, key.as_str(), true).unwrap();
        fork.insert(&items, 1, format!("from {key}")).unwrap();
        if name.starts_with("INC_") {
            let changes = fork.save_after(&base.get_heads());
            println!("{name}='{}'", to_hex_literal(&changes));
        } else {
            print(name, &mut fork);
        }
    }

    let mut many = base.fork().with_actor(ActorId::from([0xeeu8; 16]));
    for i in 0..150u32 {
        let mut id = [0xa0u8; 16];
        id[12..].copy_from_slice(&i.to_be_bytes());
        let mut fork = base.fork().with_actor(ActorId::from(id));
        fork.put(ROOT, format!("many{i}"), i64::from(i)).unwrap();
        many.merge(&mut fork).unwrap();
    }
    print("MANY", &mut many);

    let mut big = AutoCommit::new().with_actor(ActorId::from([0xbbu8; 16]));
    let text = big.put_object(ROOT, "text", ObjType::Text).unwrap();
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let body: String = (0..400_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            char::from(b'a' + (x % 26) as u8)
        })
        .collect();
    big.splice_text(&text, 0, 0, &body).unwrap();
    print("BIG", &mut big);
}
