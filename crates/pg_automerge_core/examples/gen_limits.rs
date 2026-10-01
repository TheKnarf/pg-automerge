//! Writes the inputs of tests/limits.sh into the directory given as the
//! first argument:
//!
//! - `text.bin`: `Automerge.save()` of a document holding one text of
//!   `LIMITS_TEXT_CHARS` (default 12,000,000) repeated characters. About
//!   12 kB compressed; loading it takes over a gigabyte (the input that
//!   aborted a memory-capped backend, docs/src/pages/design/installation.mdx,
//!   "Why the extension is not trusted").
//! - `ops.bin`: a crafted change chunk of about 100 bytes whose
//!   run-length encoded columns describe `LIMITS_OPS` (default 20,000,000)
//!   list inserts, which Automerge loads (several GB).
//! - `others.bin`: a compressed change chunk of about 19 kB whose header
//!   lists the empty actor id `LIMITS_OTHERS` (default 20,000,000) times;
//!   Automerge's parse keeps one 32-byte entry each (over a gigabyte)
//!   before it rejects the change.
//! - `messages.bin`: a document chunk of 6,000 empty changes whose
//!   messages are one repeat run of a 100 kB string, its columns deflated
//!   (under 1 kB): Automerge rebuilds every change with its own copy of
//!   the message, held twice, and clones them all into the error when the
//!   heads do not match (about 2.4 GB).
//! - `keys.bin`: a change chunk of 16,000 puts of one 100 kB key (a
//!   repeat run, about 100 kB): applying it makes an owned copy of the key
//!   per op (about 1.6 GB).
//! - `actors.bin`: a document chunk of 6,000 empty changes by one actor
//!   whose id is 100 kB long (about 100 kB): every rebuilt change holds
//!   the id twice, cloned again into the error (about 2.4 GB).
//! - `columns.bin`: a compressed change chunk of about 24 kB whose column
//!   metadata lists 12,000,000 empty columns: Automerge's parse keeps
//!   every entry in doubling vectors (about 2 GB).
//! - `deep_block.bin`: `Automerge.save()` of {status: "deep", body: a
//!   text "x" after a block whose map holds maps and lists nested 5,000
//!   levels deep} (under 30 kB): Automerge's recursive rendering of that
//!   block needs about 65 MB of stack (docs/src/pages/design/deep-blocks.mdx).
//!   `deep_changes.bin`: the same document as change chunks
//!   (`save_after([])`), for merges into an existing value.
//! - `small.bin`: an ordinary small document.

use std::path::PathBuf;

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ObjType, ROOT};

#[path = "../tests/common/craft.rs"]
mod craft;

fn env_or(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: gen_limits <dir>"));
    let chars = env_or("LIMITS_TEXT_CHARS", 12_000_000);
    let ops = env_or("LIMITS_OPS", 20_000_000);
    let others = env_or("LIMITS_OTHERS", 20_000_000);

    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.splice_text(&text, 0, 0, &"a".repeat(chars)).unwrap();
    std::fs::write(dir.join("text.bin"), doc.save()).unwrap();

    std::fs::write(dir.join("ops.bin"), craft::change_ops(ops as u64)).unwrap();
    std::fs::write(
        dir.join("others.bin"),
        craft::compressed_listing(0, others as u64, 0),
    )
    .unwrap();

    std::fs::write(
        dir.join("messages.bin"),
        craft::repeated_messages(6_000, 100_000, true),
    )
    .unwrap();

    std::fs::write(dir.join("keys.bin"), craft::repeated_keys(16_000, 100_000)).unwrap();

    std::fs::write(
        dir.join("actors.bin"),
        craft::long_actor_changes(6_000, 100_000),
    )
    .unwrap();

    std::fs::write(
        dir.join("columns.bin"),
        craft::compressed_change_columns(12_000_000),
    )
    .unwrap();

    let mut deep = AutoCommit::new().with_actor(ActorId::from([3u8; 16]));
    deep.put(ROOT, "status", "deep").unwrap();
    let body = deep.put_object(ROOT, "body", ObjType::Text).unwrap();
    deep.splice_text(&body, 0, 0, "x").unwrap();
    let mut obj = deep.split_block(&body, 0).unwrap();
    for i in 1..5_000 {
        obj = if i % 2 == 0 {
            deep.put_object(&obj, "m", ObjType::Map).unwrap()
        } else {
            let list = deep.put_object(&obj, "l", ObjType::List).unwrap();
            deep.insert_object(&list, 0, ObjType::Map).unwrap()
        };
    }
    deep.commit();
    std::fs::write(dir.join("deep_block.bin"), deep.save()).unwrap();
    std::fs::write(dir.join("deep_changes.bin"), deep.save_after(&[])).unwrap();

    let mut small = AutoCommit::new().with_actor(ActorId::from([2u8; 16]));
    small.put(ROOT, "status", "small").unwrap();
    std::fs::write(dir.join("small.bin"), small.save()).unwrap();
}
