//! The heads fast path (reading heads from the document chunk header) must
//! agree exactly with `Automerge::load(bytes).get_heads()`, and the merge /
//! contains shortcuts built on it must agree with the slow paths.

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, Automerge, ChangeHash, ObjType, ROOT, ReadDoc};
use pg_automerge_core::header::{HeadsPrefix, heads_from_bytes, heads_from_prefix};
use pg_automerge_core::{
    MergeAccumulator, Merged, contains, contains_by_heads, contains_loaded, heads, merge,
    normalize, stored_heads,
};

/// xorshift64: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn load_heads(bytes: &[u8]) -> Vec<ChangeHash> {
    let mut h = Automerge::load(bytes).unwrap().get_heads();
    h.sort();
    h
}

fn sorted(mut h: Vec<ChangeHash>) -> Vec<ChangeHash> {
    h.sort();
    h
}

/// A random edit on `doc`.
fn edit(doc: &mut AutoCommit, rng: &mut Rng) {
    match rng.below(5) {
        0 => {
            doc.put(ROOT, format!("k{}", rng.below(20)), rng.next() as i64)
                .unwrap();
        }
        1 => {
            let list = match doc.get(ROOT, "list").unwrap() {
                Some((_, id)) => id,
                None => doc.put_object(ROOT, "list", ObjType::List).unwrap(),
            };
            let len = doc.length(&list);
            doc.insert(&list, rng.below(len as u64 + 1) as usize, "x")
                .unwrap();
        }
        2 => {
            let text = match doc.get(ROOT, "text").unwrap() {
                Some((_, id)) => id,
                None => doc.put_object(ROOT, "text", ObjType::Text).unwrap(),
            };
            let len = doc.length(&text);
            doc.splice_text(&text, rng.below(len as u64 + 1) as usize, 0, "abc")
                .unwrap();
        }
        3 => {
            let _ = doc.delete(ROOT, format!("k{}", rng.below(20)));
        }
        _ => {
            let m = doc.put_object(ROOT, "map", ObjType::Map).unwrap();
            doc.put(&m, "n", rng.below(100) as i64).unwrap();
        }
    }
    if rng.below(3) == 0 {
        doc.commit();
    }
}

/// A random document history: several actors editing forks, with random
/// merges between them. Returns every replica's final stored bytes.
fn random_replicas(seed: u64) -> Vec<Vec<u8>> {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let n_actors = 1 + rng.below(12) as usize;
    let mut base = AutoCommit::new().with_actor(ActorId::from([0xf0u8; 16]));
    for _ in 0..rng.below(3) {
        edit(&mut base, &mut rng);
    }
    let mut replicas: Vec<AutoCommit> = (0..n_actors)
        .map(|i| {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&seed.to_le_bytes());
            id[15] = i as u8;
            base.fork().with_actor(ActorId::from(id))
        })
        .collect();
    for _ in 0..rng.below(40) {
        let i = rng.below(n_actors as u64) as usize;
        if rng.below(4) == 0 && n_actors > 1 {
            let j = rng.below(n_actors as u64) as usize;
            if i != j {
                let mut other = replicas[j].fork();
                replicas[i].merge(&mut other).unwrap();
                continue;
            }
        }
        edit(&mut replicas[i], &mut rng);
    }
    replicas
        .iter_mut()
        .map(|r| normalize(&r.save()).unwrap())
        .collect()
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
    // ... and a value that must be loaded still reports the corruption.
    assert!(merge(&old, &bad_new).is_err());
    assert!(contains(&bad_new, &old).is_err());
}
