//! `loaded`: documents kept in memory between calls (the Rust side of
//! expanded `automerge` values). The key property: whatever sequence of
//! merges runs on loaded documents, the stored bytes at the end are exactly
//! the bytes the flat path (load, apply, save at every step) produces.

mod common;

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, Automerge, ROOT, ReadDoc};
use pg_automerge_core::loaded::{self, Input, LoadedDoc, MergeOutcome};
use pg_automerge_core::{Error, MergeAccumulator, normalize};

use common::{Merged, Rng, StoredAccumulator, merge, merge_changes, random_replicas};

/// Reference for `merge(automerge, bytea)` (None: heads unchanged): a save
/// that contains `a` is the result itself, normalized (as `merge` of two
/// documents returns the one that contains the other); anything else is a
/// strict load of `a ++ changes`, saved.
fn reference_apply(a: &[u8], changes: &[u8]) -> Option<Vec<u8>> {
    if pg_automerge_core::header::starts_with_document(changes) {
        let save = Automerge::load(changes).unwrap();
        let a_heads = Automerge::load(a).unwrap().get_heads();
        if save.get_missing_deps(&a_heads).is_empty() {
            let (mut h1, mut h2) = (save.get_heads(), a_heads);
            h1.sort();
            h2.sort();
            return (h1 != h2).then(|| normalize(changes).unwrap());
        }
    }
    let mut combined = a.to_vec();
    combined.extend_from_slice(changes);
    let doc = Automerge::load(&combined).unwrap();
    let before = Automerge::load(a).unwrap();
    let (mut h1, mut h2) = (doc.get_heads(), before.get_heads());
    h1.sort();
    h2.sort();
    (h1 != h2).then(|| doc.save_nocompress())
}

/// The full history of a generated replica set, in causal order, and the
/// merged document.
fn history(seed: u64) -> (Automerge, Vec<Vec<u8>>) {
    let replicas = random_replicas(seed);
    let mut full = Automerge::load(&replicas[0]).unwrap();
    for r in &replicas[1..] {
        full.merge(&mut Automerge::load(r).unwrap()).unwrap();
    }
    let changes = full
        .get_changes(&[])
        .iter()
        .map(|c| c.raw_bytes().to_vec())
        .collect();
    (full, changes)
}

/// Stored bytes of the document made of the first `n` changes.
fn prefix_doc(changes: &[Vec<u8>], n: usize) -> Vec<u8> {
    normalize(&changes[..n].concat()).unwrap()
}

#[test]
fn chains_of_change_sets_store_the_same_bytes_as_the_flat_path() {
    for seed in 1..=60u64 {
        let (full, changes) = history(seed);
        let mut rng = Rng(seed | 1);
        let start = rng.below(changes.len() as u64 + 1) as usize;
        let base = prefix_doc(&changes, start);

        let mut flat = base.clone();
        let mut doc = LoadedDoc::from_stored(&base).unwrap();
        assert_eq!(doc.cached_stored(), Some(base.as_slice()));
        let mut i = start;
        let mut step = 0;
        while i < changes.len() {
            let n = 1 + rng.below(4) as usize;
            let end = (i + n).min(changes.len());
            // Mostly bare change chunks (the parse-and-apply path); every
            // fourth step a compressed full save of everything so far (the
            // save path: the result is the save, loaded once and verified
            // by its own encoding).
            let is_save = step % 4 == 3;
            let input = if is_save {
                Automerge::load(&changes[..end].concat()).unwrap().save()
            } else {
                changes[i..end].concat()
            };
            let expected = reference_apply(&flat, &input);
            // The flat wrapper agrees with the reference...
            assert_eq!(
                merge_changes(&flat, &input).unwrap(),
                expected,
                "seed {seed}: flat step {step}"
            );
            // ...and so does the loaded document, step by step.
            match loaded::merge_changes(Input::Loaded(&doc), &input).unwrap() {
                None => assert!(expected.is_none(), "seed {seed}: step {step}"),
                Some(next) if is_save => {
                    assert!(!next.is_unverified());
                    assert_eq!(next.cached_stored(), expected.as_deref());
                    doc = next;
                }
                Some(next) => {
                    assert!(next.is_unverified());
                    assert_eq!(next.cached_stored(), None);
                    // Check only some steps' bytes, so that most chains
                    // run several merges without a save in between.
                    if rng.below(3) == 0 {
                        assert_eq!(
                            Some(next.stored().unwrap().to_vec()),
                            expected,
                            "seed {seed}: step {step}"
                        );
                        assert!(!next.is_unverified());
                    }
                    doc = next;
                }
            }
            if let Some(bytes) = expected {
                flat = bytes;
            }
            i = end;
            step += 1;
        }
        assert_eq!(doc.stored().unwrap(), flat.as_slice(), "seed {seed}");
        // (Not byte-equal to `full`'s save: a document chunk lists changes
        // in the order they were applied. Same state though.)
        assert_eq!(
            common::to_json(&flat).unwrap(),
            pg_automerge_core::json::doc_to_json(&full).unwrap()
        );
        let mut heads = full.get_heads();
        heads.sort();
        assert_eq!(doc.heads(), heads.as_slice());
    }
}

#[test]
fn merges_of_loaded_documents_match_the_flat_merge() {
    for seed in 1..=60u64 {
        let replicas = random_replicas(seed);
        let loaded: Vec<LoadedDoc> = replicas
            .iter()
            .map(|r| LoadedDoc::from_stored(r).unwrap())
            .collect();
        for (i, a) in replicas.iter().enumerate() {
            for (j, b) in replicas.iter().enumerate() {
                let flat = merge(a, b).unwrap();
                for (x, y) in [
                    (Input::Loaded(&loaded[i]), Input::Stored(b)),
                    (Input::Stored(a), Input::Loaded(&loaded[j])),
                    (Input::Loaded(&loaded[i]), Input::Loaded(&loaded[j])),
                ] {
                    match (&flat, loaded::merge(x, y).unwrap()) {
                        (Merged::Left, MergeOutcome::Left)
                        | (Merged::Right, MergeOutcome::Right) => {}
                        (Merged::New(bytes), MergeOutcome::New(doc)) => {
                            assert!(!doc.is_unverified());
                            assert_eq!(doc.stored().unwrap(), bytes.as_slice(), "seed {seed}");
                        }
                        (f, _) => {
                            panic!("seed {seed} ({i}, {j}): flat merge gave {f:?}, loaded differs")
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn a_failed_merge_leaves_the_loaded_document_as_it_was() {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    doc.put(ROOT, "x", 1i64).unwrap();
    doc.commit();
    let stored = normalize(&doc.save()).unwrap();
    let heads = doc.get_heads();
    let loaded_doc = LoadedDoc::from_stored(&stored).unwrap();

    // Orphaned changes: their parent is missing.
    doc.put(ROOT, "y", 2i64).unwrap();
    doc.commit();
    let middle = doc.get_heads();
    doc.put(ROOT, "z", 3i64).unwrap();
    doc.commit();
    let orphan = doc.save_after(&middle);
    // The same (actor, seq) as the stored document's change, other content.
    let mut other = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    other.put(ROOT, "x", 99i64).unwrap();
    other.commit();
    let duplicate = other.save_after(&[]);
    // Garbage, and a chunk with a flipped byte (bad checksum).
    let mut flipped = doc.save_after(&heads);
    let last = flipped.len() - 1;
    flipped[last] ^= 1;

    for (what, input) in [
        ("orphan", orphan),
        ("duplicate seq", duplicate),
        ("garbage", b"not automerge".to_vec()),
        ("flipped", flipped),
    ] {
        let err = loaded::merge_changes(Input::Loaded(&loaded_doc), &input)
            .err()
            .unwrap_or_else(|| panic!("{what}: expected an error"));
        assert!(
            matches!(err, Error::InvalidInput(_) | Error::MissingDependencies(_)),
            "{what}: {err:?}"
        );
        // Untouched: same heads, same cached bytes, still loads the same.
        let mut h = heads.clone();
        h.sort();
        assert_eq!(loaded_doc.heads(), h.as_slice(), "{what}");
        assert_eq!(
            loaded_doc.cached_stored(),
            Some(stored.as_slice()),
            "{what}"
        );
        assert_eq!(loaded_doc.doc().save_nocompress(), stored, "{what}");
    }
    let err = loaded::merge_changes(Input::Loaded(&loaded_doc), &doc.save_after(&middle))
        .err()
        .unwrap();
    assert!(err.to_string().contains("missing 1 dependency"), "{err}");
}

#[test]
fn stored_bytes_belong_to_their_document() {
    // The stale-flatten bug to avoid: bytes computed before a merge must
    // not be handed out for the merged document.
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    doc.put(ROOT, "v", 1i64).unwrap();
    doc.commit();
    let first = normalize(&doc.save()).unwrap();
    let heads = doc.get_heads();
    let one = LoadedDoc::from_stored(&first).unwrap();
    assert_eq!(one.stored().unwrap(), first.as_slice());
    doc.put(ROOT, "v", 2i64).unwrap();
    doc.commit();
    let two = loaded::merge_changes(Input::Loaded(&one), &doc.save_after(&heads))
        .unwrap()
        .unwrap();
    let second = doc.document().save_nocompress();
    assert_eq!(two.stored().unwrap(), second.as_slice());
    assert_ne!(first, second);
    // The old document keeps its own bytes.
    assert_eq!(one.stored().unwrap(), first.as_slice());
    assert_eq!(two.cached_stored(), Some(second.as_slice()));
    // Nothing new: None, and no work.
    assert!(
        loaded::merge_changes(Input::Loaded(&two), &doc.save_after(&heads))
            .unwrap()
            .is_none()
    );
    assert!(
        loaded::merge_changes(Input::Loaded(&two), &[])
            .unwrap()
            .is_none()
    );
}

#[test]
fn containment_agrees_between_stored_and_loaded() {
    for seed in 1..=40u64 {
        let (_, changes) = history(seed);
        let mut rng = Rng(seed | 1);
        let at = rng.below(changes.len() as u64 + 1) as usize;
        let base = prefix_doc(&changes, at);
        let doc = LoadedDoc::from_stored(&base).unwrap();
        for _ in 0..10 {
            let i = rng.below(changes.len() as u64 + 1) as usize;
            let j = i + rng.below((changes.len() - i) as u64 + 1) as usize;
            let slice = changes[i..j].concat();
            let flat = common::contains_changes(&base, &slice).unwrap();
            let mem = loaded::contains_changes(Input::Loaded(&doc), &slice).unwrap();
            assert_eq!(flat, mem, "seed {seed}: {i}..{j} on {at}");
            // And merge agrees: a no-op exactly when contained (unless the
            // slice has changes whose deps are missing: then it errors).
            match loaded::merge_changes(Input::Loaded(&doc), &slice) {
                Ok(None) => assert!(mem),
                Ok(Some(_)) => assert!(!mem),
                Err(_) => assert!(!mem),
            }
        }
        let other = prefix_doc(&changes, rng.below(changes.len() as u64 + 1) as usize);
        let other_doc = LoadedDoc::from_stored(&other).unwrap();
        let flat = common::contains(&base, &other).unwrap();
        assert_eq!(
            loaded::contains(Input::Loaded(&doc), Input::Stored(&other)).unwrap(),
            flat
        );
        assert_eq!(
            loaded::contains(Input::Stored(&base), Input::Loaded(&other_doc)).unwrap(),
            flat
        );
        assert_eq!(
            loaded::contains(Input::Loaded(&doc), Input::Loaded(&other_doc)).unwrap(),
            flat
        );
        assert!(loaded::contains(Input::Loaded(&doc), Input::Loaded(&doc)).unwrap());
    }
}

#[test]
fn accumulator_accepts_loaded_documents() {
    for seed in 1..=30u64 {
        let replicas = random_replicas(seed);
        let mut flat = MergeAccumulator::new();
        let mut mixed = MergeAccumulator::new();
        for (i, r) in replicas.iter().enumerate() {
            flat.add(r).unwrap();
            if i % 2 == 0 {
                let doc = LoadedDoc::from_stored(r).unwrap();
                mixed.add_input(Input::Loaded(&doc)).unwrap();
            } else {
                mixed.add(r).unwrap();
            }
        }
        let a = flat.finish().unwrap().unwrap().into_owned();
        let b = mixed.finish().unwrap().unwrap().into_owned();
        assert_eq!(a, b, "seed {seed}");
        // The final function may run more than once: finish_loaded copies.
        let again = mixed.finish().unwrap().unwrap().into_owned();
        assert_eq!(a, again);
    }
}

#[test]
fn unverified_documents_pass_the_check_on_accumulation() {
    // A loaded document with bytea changes, fed to merge_agg, keeps its
    // "unverified" mark until its bytes are computed (with the check).
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    doc.put(ROOT, "v", 1i64).unwrap();
    doc.commit();
    let base = normalize(&doc.save()).unwrap();
    let heads = doc.get_heads();
    doc.put(ROOT, "w", 2i64).unwrap();
    doc.commit();
    let loaded_base = LoadedDoc::from_stored(&base).unwrap();
    let merged = loaded::merge_changes(Input::Loaded(&loaded_base), &doc.save_after(&heads))
        .unwrap()
        .unwrap();
    assert!(merged.is_unverified());
    let mut acc = MergeAccumulator::new();
    acc.add_input(Input::Loaded(&merged)).unwrap();
    acc.add(&base).unwrap();
    match acc.finish_loaded().unwrap().unwrap() {
        pg_automerge_core::Accumulated::Loaded(result) => {
            assert!(result.is_unverified());
            assert_eq!(result.stored().unwrap(), doc.document().save_nocompress());
        }
        pg_automerge_core::Accumulated::Stored(_) => panic!("expected a new document"),
    }
}

/// Versions of one document: `v[i]` has `i + 1` changes (a linear
/// history), plus `fork`, a concurrent change on top of `v[1]`.
fn versions() -> (Vec<Vec<u8>>, Vec<u8>) {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    let mut v = Vec::new();
    for i in 0..4i64 {
        doc.put(ROOT, "n", i).unwrap();
        doc.put(ROOT, format!("k{i}"), i).unwrap();
        doc.commit();
        v.push(normalize(&doc.save()).unwrap());
    }
    let mut fork = AutoCommit::load(&v[1])
        .unwrap()
        .with_actor(ActorId::from([2u8; 16]));
    fork.put(ROOT, "fork", true).unwrap();
    fork.commit();
    (v, normalize(&fork.save()).unwrap())
}

/// Loads taken by accumulating `inputs`, and the result.
fn accumulate(inputs: &[&[u8]]) -> (usize, Vec<u8>) {
    let before = pg_automerge_core::test_hooks::loads();
    let mut acc = MergeAccumulator::new();
    for input in inputs {
        acc.add(input).unwrap();
    }
    let loads = pg_automerge_core::test_hooks::loads() - before;
    (loads, acc.finish().unwrap().unwrap().into_owned())
}

#[test]
fn accumulator_loads_only_what_a_merge_needs() {
    let (v, fork) = versions();
    // One input, or identical inputs: nothing loaded, the input is the
    // result.
    assert_eq!(accumulate(&[&v[3]]), (0, v[3].clone()));
    assert_eq!(accumulate(&[&v[3], &v[3], &v[3]]), (0, v[3].clone()));
    // A linear history in either order: one load (of the newest, which
    // contains the others; older inputs are decided by their heads), and
    // the newest input's bytes are the result (no save).
    assert_eq!(accumulate(&[&v[0], &v[1], &v[2], &v[3]]).1, v[3]);
    assert_eq!(accumulate(&[&v[3], &v[2], &v[1], &v[0]]), (1, v[3].clone()));
    assert_eq!(accumulate(&[&v[1], &v[3]]), (1, v[3].clone()));
    assert_eq!(accumulate(&[&v[3], &v[1]]), (1, v[3].clone()));
    // Oldest first: each newer input is loaded (it may add something) and
    // replaces the state; still no merge and no save.
    assert_eq!(accumulate(&[&v[0], &v[1], &v[2], &v[3]]).0, 3);
    // Concurrent versions: both loaded and merged, once.
    let (loads, merged) = accumulate(&[&v[3], &fork]);
    assert_eq!(loads, 2);
    let (loads_rev, merged_rev) = accumulate(&[&fork, &v[3], &v[2], &v[0]]);
    assert_eq!(loads_rev, 2);
    for m in [&merged, &merged_rev] {
        let doc = Automerge::load(m).unwrap();
        assert!(doc.get(ROOT, "fork").unwrap().is_some());
        assert!(doc.get(ROOT, "k3").unwrap().is_some());
    }
    assert_eq!(
        common::heads(&merged).unwrap(),
        common::heads(&merged_rev).unwrap()
    );
    // A merged state that a later input contains is replaced by it.
    let all = merge(&v[3], &fork)
        .unwrap()
        .into_bytes(&v[3], &fork)
        .into_owned();
    assert_eq!(accumulate(&[&v[3], &fork, &all]).1, all);
}

#[test]
fn accumulator_has_heads_matches_adding() {
    let (v, fork) = versions();
    let heads = |b: &[u8]| {
        let mut h = Automerge::load(b).unwrap().get_heads();
        h.sort();
        h
    };
    let mut acc = MergeAccumulator::new();
    assert!(!acc.has_heads(&[]).unwrap(), "empty state has nothing");
    acc.add(&v[2]).unwrap();
    // Pending (not loaded): decided by the heads alone, conservatively.
    assert!(acc.has_heads(&heads(&v[2])).unwrap());
    assert!(!acc.has_heads(&heads(&v[3])).unwrap());
    assert!(!acc.has_heads(&heads(&fork)).unwrap());
    acc.add(&v[3]).unwrap();
    // Loaded: by the history.
    for old in &v {
        assert!(acc.has_heads(&heads(old)).unwrap());
    }
    assert!(!acc.has_heads(&heads(&fork)).unwrap());
    acc.add(&fork).unwrap();
    assert!(acc.has_heads(&heads(&fork)).unwrap());
    assert!(acc.has_heads(&heads(&v[3])).unwrap());
}

#[test]
fn accumulator_result_is_order_independent_in_state() {
    // Every order of a few replica sets gives the same heads and JSON, and
    // the same as merging pairwise.
    for seed in 1..=12u64 {
        let replicas = random_replicas(seed);
        let n = replicas.len().min(5);
        let inputs: Vec<&[u8]> = replicas[..n].iter().map(Vec::as_slice).collect();
        let mut reference = inputs[0].to_vec();
        for r in &inputs[1..] {
            reference = merge(&reference, r)
                .unwrap()
                .into_bytes(&reference, r)
                .into_owned();
        }
        let want = (
            common::heads(&reference).unwrap(),
            common::to_json(&reference).unwrap(),
        );
        let mut rng = Rng(seed | 1);
        for _ in 0..8 {
            let mut order = inputs.clone();
            for i in (1..order.len()).rev() {
                order.swap(i, rng.below(i as u64 + 1) as usize);
            }
            let (_, out) = accumulate(&order);
            assert_eq!(
                (common::heads(&out).unwrap(), common::to_json(&out).unwrap()),
                want,
                "seed {seed}"
            );
            // Stored results load back to the same heads (a real save).
            let mut h = Automerge::load(&out).unwrap().get_heads();
            h.sort();
            assert_eq!(
                h.iter().map(ToString::to_string).collect::<Vec<_>>(),
                want.0
            );
        }
    }
}
