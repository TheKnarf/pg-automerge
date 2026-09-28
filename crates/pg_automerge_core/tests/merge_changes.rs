//! `merge_changes`: applying external bytes (full saves or bare change
//! chunks) on top of a stored value, as `merge(automerge, bytea)` does.

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, Automerge, ChangeHash, ROOT};
use pg_automerge_core::{Error, heads, merge_changes, normalize, to_json};
use serde_json::json;

fn actor(n: u8) -> ActorId {
    ActorId::from([n; 16])
}

fn stored(doc: &mut AutoCommit) -> Vec<u8> {
    normalize(&doc.save()).unwrap()
}

fn sorted_heads(doc: &mut AutoCommit) -> Vec<String> {
    let mut h: Vec<String> = doc.get_heads().iter().map(ChangeHash::to_string).collect();
    h.sort();
    h
}

/// Apply and return the resulting stored bytes.
fn apply(a: &[u8], changes: &[u8]) -> Vec<u8> {
    merge_changes(a, changes)
        .unwrap()
        .unwrap_or_else(|| a.to_vec())
}

fn invalid(result: Result<Option<Vec<u8>>, Error>) -> String {
    match result {
        Err(Error::InvalidInput(msg)) => msg,
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[test]
fn bare_incremental_changes_apply_on_top() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "v", 0i64).unwrap();
    let base = stored(&mut doc);
    doc.save_incremental(); // everything so far is "persisted"

    doc.put(ROOT, "v", 1i64).unwrap();
    let one = doc.save_incremental();
    doc.put(ROOT, "w", "x").unwrap();
    doc.commit();
    doc.put(ROOT, "v", 2i64).unwrap();
    let two = doc.save_incremental(); // two change chunks

    // Chunks one by one.
    let step1 = apply(&base, &one);
    let step2 = apply(&step1, &two);
    assert_eq!(heads(&step2).unwrap(), sorted_heads(&mut doc));
    assert_eq!(to_json(&step2).unwrap(), json!({ "v": 2, "w": "x" }));
    // Result is normalized: a fixed point of normalize, identical to a
    // fresh save of the same document.
    assert_eq!(normalize(&step2).unwrap(), step2);
    assert_eq!(step2, doc.document().save_nocompress());

    // All at once, concatenated.
    let both = [one.as_slice(), &two].concat();
    assert_eq!(apply(&base, &both), step2);
    // save_after() output behaves the same.
    let after = doc.save_after(&load(&base).get_heads());
    assert_eq!(apply(&base, &after), step2);
}

fn load(bytes: &[u8]) -> Automerge {
    Automerge::load(bytes).unwrap()
}

#[test]
fn full_saves_in_every_form() {
    let mut base = AutoCommit::new().with_actor(actor(1));
    base.put(ROOT, "base", true).unwrap();
    let stored_base = stored(&mut base);
    let mut fork = base.fork().with_actor(actor(2));
    fork.put(ROOT, "pad", "long enough to compress ".repeat(40))
        .unwrap();
    let mid = fork.get_heads();
    fork.put(ROOT, "fork", true).unwrap();
    let expected = sorted_heads(&mut fork);

    let compressed = fork.save();
    let nocompress = fork.save_nocompress();
    // A save at `mid` followed by the later change chunk.
    let mut doc_then_changes = fork.fork_at(&mid).unwrap().save();
    doc_then_changes.extend(fork.save_after(&mid));

    for input in [&compressed, &nocompress, &doc_then_changes] {
        let got = apply(&stored_base, input);
        assert_eq!(heads(&got).unwrap(), expected);
        assert_eq!(to_json(&got).unwrap()["fork"], json!(true));
    }
    // An unrelated document merges like merge(a, b).
    let mut other = AutoCommit::new().with_actor(actor(3));
    other.put(ROOT, "other", 1i64).unwrap();
    let got = apply(&stored_base, &other.save());
    assert_eq!(to_json(&got).unwrap(), json!({ "base": true, "other": 1 }));
    assert_eq!(heads(&got).unwrap().len(), 2);
}

#[test]
fn nothing_new_is_a_no_op() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "x", 1i64).unwrap();
    let first = doc.save_incremental();
    doc.put(ROOT, "y", 2i64).unwrap();
    let second = doc.save_incremental();
    let a = stored(&mut doc);

    assert_eq!(merge_changes(&a, &[]).unwrap(), None);
    for input in [
        first.clone(),
        second.clone(),
        [first.as_slice(), &second].concat(),
        doc.save(),
        a.clone(),
        normalize(&[]).unwrap(),
        AutoCommit::new().save(),
    ] {
        assert_eq!(merge_changes(&a, &input).unwrap(), None);
    }
    // Partially known input applies only what is new.
    doc.put(ROOT, "z", 3i64).unwrap();
    let third = doc.save_incremental();
    let all = [first.as_slice(), &second, &third].concat();
    assert_eq!(apply(&a, &all), doc.document().save_nocompress());
}

#[test]
fn missing_dependencies_are_named() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "x", 1i64).unwrap();
    let a = stored(&mut doc);
    doc.put(ROOT, "y", 2i64).unwrap();
    let skipped = doc.get_heads(); // this change never reaches the database
    doc.save_incremental();
    doc.put(ROOT, "z", 3i64).unwrap();
    let orphan = doc.save_incremental();

    let msg = invalid(merge_changes(&a, &orphan));
    assert!(msg.contains("missing 1 dependency"), "{msg}");
    assert!(msg.contains(&skipped[0].to_string()), "{msg}");

    // Applied onto the empty document, the first change's own dependency
    // is missing too.
    let empty = normalize(&[]).unwrap();
    let msg = invalid(merge_changes(&empty, &orphan));
    assert!(msg.contains(&skipped[0].to_string()), "{msg}");

    // With the missing change supplied first, it applies.
    let fixed = [doc.save_after(&load(&a).get_heads())].concat();
    assert_eq!(
        to_json(&apply(&a, &fixed)).unwrap(),
        json!({ "x": 1, "y": 2, "z": 3 })
    );
}

#[test]
fn many_missing_dependencies_are_summarized() {
    // Ten actors each made a change on top of a base that is not stored.
    let mut base = AutoCommit::new().with_actor(actor(1));
    base.put(ROOT, "base", true).unwrap();
    let mut bytes = Vec::new();
    let mut missing = Vec::new();
    for i in 0..10u8 {
        let mut f = base.fork().with_actor(actor(10 + i));
        f.put(ROOT, "k", i64::from(i)).unwrap();
        f.commit();
        let mid = f.get_heads();
        missing.extend(mid.clone());
        f.put(ROOT, "k2", i64::from(i)).unwrap();
        bytes.extend(f.save_after(&mid));
    }
    let msg = invalid(merge_changes(&normalize(&[]).unwrap(), &bytes));
    assert!(msg.contains("missing 10 dependencies"), "{msg}");
    assert!(msg.contains("and 5 more"), "{msg}");
}

#[test]
fn malformed_input_is_rejected_strictly() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "x", 1i64).unwrap();
    let a = stored(&mut doc);
    doc.put(ROOT, "y", "hello").unwrap();
    let change = doc.save_incremental();

    for bad in [
        b"garbage".to_vec(),
        change[..change.len() - 1].to_vec(),
        [change.as_slice(), b"trailing"].concat(),
        vec![0; 64],
    ] {
        invalid(merge_changes(&a, &bad));
    }
    // Automerge's own load_incremental silently ignores a bad trailing
    // chunk; merge_changes must not.
    let mut lenient = load(&a);
    lenient
        .load_incremental(&[change.as_slice(), b"trailing"].concat())
        .unwrap();
    // Every single-byte corruption: a clean error or a valid result, never
    // a panic or an Internal error.
    for i in 0..change.len() {
        let mut b = change.clone();
        b[i] ^= 0x5a;
        match merge_changes(&a, &b) {
            Ok(_) | Err(Error::InvalidInput(_)) => {}
            Err(e) => panic!("byte {i}: {e:?}"),
        }
    }
}

#[test]
fn reused_actor_id_is_an_error() {
    let mut a = AutoCommit::new().with_actor(actor(1));
    let mut b = AutoCommit::new().with_actor(actor(1));
    a.put(ROOT, "x", 1i64).unwrap();
    b.put(ROOT, "x", 2i64).unwrap();
    let msg = invalid(merge_changes(&stored(&mut a), &b.save()));
    assert!(msg.contains("duplicate seq"), "{msg}");
}
