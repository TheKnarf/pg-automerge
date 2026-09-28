//! `loaded`: documents kept in memory between calls (the Rust side of
//! expanded `automerge` values). The key property: whatever sequence of
//! merges runs on loaded documents, the stored bytes at the end are exactly
//! the bytes the flat path (load, apply, save at every step) produces.

mod common;

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, Automerge, ROOT};
use pg_automerge_core::loaded::{self, Input, LoadedDoc, MergeOutcome};
use pg_automerge_core::{Error, MergeAccumulator, Merged, merge, merge_changes, normalize};

use common::{Rng, random_replicas};

/// What `merge(automerge, bytea)` stored before loaded documents existed:
/// a strict load of `a ++ changes`, saved (None: heads unchanged).
fn reference_apply(a: &[u8], changes: &[u8]) -> Option<Vec<u8>> {
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
            // `a ++ changes` load path, starting from a fresh save of the
            // loaded document).
            let input = if step % 4 == 3 {
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
            pg_automerge_core::to_json(&flat).unwrap(),
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
        assert!(matches!(err, Error::InvalidInput(_)), "{what}: {err:?}");
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
            let flat = pg_automerge_core::contains_changes(&base, &slice).unwrap();
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
        let flat = pg_automerge_core::contains(&base, &other).unwrap();
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
