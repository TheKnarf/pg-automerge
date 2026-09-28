//! Basic properties of normalization, merge and the merge accumulator on
//! stored values.

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, Automerge, ROOT};
use pg_automerge_core::{Error, MergeAccumulator, normalize};
use serde_json::json;

mod common;

use common::{Merged, StoredAccumulator, contains, heads, merge, to_json};

fn actor(n: u8) -> ActorId {
    ActorId::from([n; 16])
}

/// A base document and two forks with concurrent edits, all as stored bytes.
fn forked() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut base = AutoCommit::new().with_actor(actor(1));
    base.put(ROOT, "title", "base").unwrap();
    base.put(ROOT, "n", 1i64).unwrap();
    let mut a = base.fork().with_actor(actor(2));
    let mut b = base.fork().with_actor(actor(3));
    a.put(ROOT, "a", true).unwrap();
    a.put(ROOT, "title", "from a").unwrap();
    b.put(ROOT, "b", true).unwrap();
    b.delete(ROOT, "n").unwrap();
    (
        normalize(&base.save()).unwrap(),
        normalize(&a.save()).unwrap(),
        normalize(&b.save()).unwrap(),
    )
}

#[test]
fn normalize_is_idempotent_and_uncompressed() {
    let (_, a, _) = forked();
    assert_eq!(normalize(&a).unwrap(), a);
    let doc = Automerge::load(&a).unwrap();
    assert_eq!(doc.save_nocompress(), a);
}

#[test]
fn normalize_accepts_compressed_and_incremental_saves() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "x", 1i64).unwrap();
    let first = doc.save();
    let heads = doc.get_heads();
    doc.put(ROOT, "y", "long enough to maybe compress ".repeat(20))
        .unwrap();
    let mut bytes = first.clone();
    bytes.extend(doc.save_after(&heads));
    let expected = doc.document().save_nocompress();
    assert_eq!(normalize(&bytes).unwrap(), expected);
    assert_eq!(normalize(&doc.save()).unwrap(), expected);
    // Changes alone (no document chunk) load too.
    assert_eq!(normalize(&doc.save_after(&[])).unwrap(), expected);
}

#[test]
fn normalize_rejects_garbage_and_orphans() {
    assert!(matches!(
        normalize(b"not automerge"),
        Err(Error::InvalidInput(_))
    ));
    let mut doc = AutoCommit::new();
    doc.put(ROOT, "x", 1i64).unwrap();
    let heads = doc.get_heads();
    doc.put(ROOT, "y", 2i64).unwrap();
    // A change whose parent is absent.
    let orphan = doc.save_after(&heads);
    let err = normalize(&orphan).unwrap_err();
    assert!(
        matches!(err, Error::InvalidInput(ref m) if m.contains("depend")),
        "{err:?}"
    );
    // Same, trailing a document chunk that lacks the parent.
    let mut other = AutoCommit::new();
    other.put(ROOT, "z", 1i64).unwrap();
    let mut bytes = other.save();
    bytes.extend(orphan);
    assert!(matches!(normalize(&bytes), Err(Error::InvalidInput(_))));
}

#[test]
fn empty_input_is_empty_document() {
    let bytes = normalize(&[]).unwrap();
    assert!(!bytes.is_empty());
    assert_eq!(to_json(&bytes).unwrap(), json!({}));
    assert!(heads(&bytes).unwrap().is_empty());
}

#[test]
fn merge_is_commutative_and_idempotent() {
    let (base, a, b) = forked();
    let ab = merge(&a, &b).unwrap().into_bytes(&a, &b).into_owned();
    let ba = merge(&b, &a).unwrap().into_bytes(&b, &a).into_owned();
    assert_eq!(heads(&ab).unwrap(), heads(&ba).unwrap());
    assert_eq!(heads(&ab).unwrap().len(), 2);
    let json = to_json(&ab).unwrap();
    assert_eq!(json, to_json(&ba).unwrap());
    assert_eq!(json, json!({ "title": "from a", "a": true, "b": true }));

    assert_eq!(merge(&ab, &ab).unwrap(), Merged::Left);
    assert_eq!(merge(&ab, &a).unwrap(), Merged::Left);
    assert_eq!(merge(&base, &ab).unwrap(), Merged::Right);
    assert!(contains(&ab, &a).unwrap());
    assert!(contains(&ab, &base).unwrap());
    assert!(!contains(&a, &b).unwrap());
    assert!(!contains(&base, &a).unwrap());
}

#[test]
fn accumulator() {
    let (base, a, b) = forked();
    let mut acc = MergeAccumulator::new();
    assert!(acc.finish().unwrap().is_none());
    acc.add(&base).unwrap();
    acc.add(&base).unwrap();
    assert_eq!(acc.finish().unwrap().unwrap().as_ref(), base.as_slice());
    acc.add(&a).unwrap();
    acc.add(&b).unwrap();
    let merged = acc.finish().unwrap().unwrap().into_owned();
    let expected = merge(&a, &b).unwrap().into_bytes(&a, &b).into_owned();
    assert_eq!(heads(&merged).unwrap(), heads(&expected).unwrap());
    assert_eq!(to_json(&merged).unwrap(), to_json(&expected).unwrap());
}
