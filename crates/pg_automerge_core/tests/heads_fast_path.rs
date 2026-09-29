//! The heads fast path (reading heads from the document chunk header) must
//! agree exactly with `Automerge::load(bytes).get_heads()`, and the merge /
//! contains shortcuts built on it must agree with the slow paths.

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, Automerge, ChangeHash, ROOT, ReadDoc};
use pg_automerge_core::header::{
    HeadsPrefix, change_count_from_bytes, document_parses, heads_from_bytes, heads_from_prefix,
};
use pg_automerge_core::{
    MergeAccumulator, contains_by_heads, contains_by_heads_and_counts, contains_input_by_header,
    normalize, stored_heads,
};

mod common;

use common::{
    Merged, StoredAccumulator, contains, contains_changes, contains_loaded, heads, merge,
    random_replicas,
};

fn load_heads(bytes: &[u8]) -> Vec<ChangeHash> {
    let mut h = Automerge::load(bytes).unwrap().get_heads();
    h.sort();
    h
}

fn sorted(mut h: Vec<ChangeHash>) -> Vec<ChangeHash> {
    h.sort();
    h
}

#[test]
fn fast_heads_match_full_load_for_generated_documents() {
    let mut checked = 0;
    let mut max_heads = 0;
    // The empty document, directly and via every input form.
    let empty = normalize(&[]).unwrap();
    assert_eq!(heads_from_bytes(&empty), Some(vec![]));
    assert_eq!(load_heads(&empty), Vec::<ChangeHash>::new());

    for seed in 1..=150u64 {
        let replicas = random_replicas(seed);
        let mut acc = MergeAccumulator::new();
        let mut all = replicas.clone();
        for pair in replicas.windows(2) {
            let m = merge(&pair[0], &pair[1])
                .unwrap()
                .into_bytes(&pair[0], &pair[1])
                .into_owned();
            all.push(m);
        }
        for r in &replicas {
            acc.add(r).unwrap();
        }
        all.push(acc.finish().unwrap().unwrap().into_owned());

        for bytes in &all {
            let fast = heads_from_bytes(bytes).expect("stored value is one document chunk");
            let slow = load_heads(bytes);
            assert_eq!(sorted(fast.clone()), slow, "seed {seed}");
            // Header order is the sorted order in practice (not relied upon).
            assert_eq!(fast, slow, "seed {seed}: header heads not sorted");
            assert_eq!(sorted(stored_heads(bytes).unwrap()), slow);
            assert_eq!(
                heads(bytes).unwrap(),
                slow.iter().map(ToString::to_string).collect::<Vec<_>>()
            );
            // Any prefix gives either NeedMore or the same heads.
            let cut = (seed as usize * 7) % (bytes.len() + 1);
            match heads_from_prefix(&bytes[..cut], bytes.len()) {
                HeadsPrefix::Found(h) => assert_eq!(sorted(h), slow),
                HeadsPrefix::NeedMore(n) => assert!(n > cut && n <= bytes.len()),
                HeadsPrefix::NotSingleDoc => panic!("seed {seed}: prefix {cut} not a doc"),
            }
            max_heads = max_heads.max(slow.len());
            checked += 1;
        }
    }
    assert!(checked > 500, "{checked}");
    assert!(
        max_heads >= 5,
        "generator never made many heads: {max_heads}"
    );
}

#[test]
fn fast_heads_fall_back_for_other_encodings() {
    let replicas = random_replicas(7);
    let mut doc = AutoCommit::load(&replicas[0]).unwrap();
    let heads_before = doc.get_heads();
    doc.put(ROOT, "later", true).unwrap();
    // Compressed save: still one document chunk, but not a stored value.
    let compressed = doc.save();
    // Document chunk plus a trailing change chunk: not a single chunk.
    let mut trailing = replicas[0].clone();
    trailing.extend(doc.save_after(&heads_before));
    // Bare changes.
    let changes = doc.save_after(&[]);
    for bytes in [&compressed, &trailing, &changes] {
        assert_eq!(
            sorted(stored_heads(bytes).unwrap()),
            load_heads(bytes),
            "fallback must match a full load"
        );
    }
    assert_eq!(heads_from_bytes(&trailing), None);
    assert_eq!(heads_from_bytes(&changes), None);
}

#[test]
fn shortcuts_agree_with_the_slow_paths() {
    // Pairs the heads cannot decide but the change counts do.
    let mut by_counts = 0;
    for seed in 200..260u64 {
        let replicas = random_replicas(seed);
        let empty = normalize(&[]).unwrap();
        let mut docs = replicas.clone();
        docs.push(empty);
        if replicas.len() > 1 {
            docs.push(
                merge(&replicas[0], &replicas[1])
                    .unwrap()
                    .into_bytes(&replicas[0], &replicas[1])
                    .into_owned(),
            );
        }
        for a in &docs {
            let doc_a = Automerge::load(a).unwrap();
            for b in &docs {
                let hb = load_heads(b);
                let slow = doc_a.get_missing_deps(&hb).is_empty();
                assert_eq!(contains(a, b).unwrap(), slow, "seed {seed}");
                assert_eq!(contains_loaded(a, &hb).unwrap(), slow);
                if let Some(fast) = contains_by_heads(&load_heads(a), &hb) {
                    assert_eq!(fast, slow, "seed {seed}: heads shortcut wrong");
                }
                let (count_a, count_b) = (change_count_from_bytes(a), change_count_from_bytes(b));
                assert!(count_a.is_some() && count_b.is_some());
                if let Some(fast) =
                    contains_by_heads_and_counts(&load_heads(a), &hb, count_a, count_b)
                {
                    assert_eq!(fast, slow, "seed {seed}: counts shortcut wrong");
                    if contains_by_heads(&load_heads(a), &hb).is_none() {
                        by_counts += 1;
                    }
                }
                // The same for `b` as a compressed save (bytea input): the
                // header shortcut, and the full check.
                let save_b = Automerge::load(b).unwrap().save();
                // Every save Automerge writes passes the chunk check that
                // the header shortcuts rely on.
                assert!(
                    document_parses(b) && document_parses(&save_b),
                    "seed {seed}"
                );
                if let Some(fast) = contains_input_by_header(&load_heads(a), || count_a, &save_b) {
                    assert_eq!(fast, slow, "seed {seed}: save header shortcut wrong");
                }
                assert_eq!(contains_changes(a, &save_b).unwrap(), slow, "seed {seed}");
                // merge's no-op results are exactly the containment cases,
                // and the merged heads are those of a real Automerge merge.
                let merged = merge(a, b).unwrap();
                let mut expected = Automerge::load(a).unwrap();
                expected.merge(&mut Automerge::load(b).unwrap()).unwrap();
                let expected = sorted(expected.get_heads());
                match &merged {
                    Merged::Left => assert!(slow),
                    Merged::Right => assert!(contains(b, a).unwrap()),
                    Merged::New(_) => assert!(!slow && !contains(b, a).unwrap()),
                }
                assert_eq!(load_heads(&merged.into_bytes(a, b)), expected);
            }
        }
    }
    assert!(by_counts > 20, "{by_counts}");
}

/// Which inputs the no-op checks load, observed through a stored value whose
/// header is intact but whose body is corrupt (loading it fails).
#[test]
fn no_op_checks_load_only_what_they_need() {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    doc.put(ROOT, "x", 1i64).unwrap();
    let old = normalize(&doc.save()).unwrap();
    doc.put(ROOT, "y", "a longer value that makes the newer save larger")
        .unwrap();
    let new = normalize(&doc.save()).unwrap();
    assert!(new.len() > old.len());
    let corrupt = |bytes: &[u8]| {
        let mut b = bytes.to_vec();
        let last = b.len() - 1;
        b[last] ^= 0xff; // head indices / op data, after the heads
        assert!(Automerge::load(&b).is_err());
        assert_eq!(heads_from_bytes(&b), heads_from_bytes(bytes));
        b
    };
    let bad_old = corrupt(&old);
    let bad_new = corrupt(&new);

    // Same heads: decided from the headers, nothing loaded.
    assert_eq!(merge(&new, &bad_new).unwrap(), Merged::Left);
    assert!(contains(&bad_new, &new).unwrap());
    assert_eq!(heads(&bad_new).unwrap(), heads(&new).unwrap());
    // Linear history: only the larger (newer) value is loaded, in either
    // argument order; contains loads only its first argument.
    assert_eq!(merge(&bad_old, &new).unwrap(), Merged::Right);
    assert_eq!(merge(&new, &bad_old).unwrap(), Merged::Left);
    assert!(contains(&new, &bad_old).unwrap());
    // The older value does not contain the newer one: it has fewer
    // changes, so nothing is loaded, as for a newer save.
    assert!(!contains(&bad_old, &new).unwrap());
    assert!(!contains_changes(&bad_old, &doc.save()).unwrap());
    // Concurrent versions with as many changes each: neither contains the
    // other, again without a load.
    let mut fork = AutoCommit::load(&old)
        .unwrap()
        .with_actor(ActorId::from([2u8; 16]));
    fork.put(ROOT, "z", 3i64).unwrap();
    let fork = normalize(&fork.save()).unwrap();
    assert_eq!(
        change_count_from_bytes(&fork),
        change_count_from_bytes(&new)
    );
    assert!(!contains(&bad_new, &fork).unwrap());
    assert!(!contains(&corrupt(&fork), &new).unwrap());
    assert!(!contains_changes(&bad_new, &fork).unwrap());
    // ... and a value that must be loaded still reports the corruption.
    assert!(merge(&old, &bad_new).is_err());
    assert!(contains(&bad_new, &old).is_err());
    assert!(contains_changes(&bad_new, &old).is_err());
}
