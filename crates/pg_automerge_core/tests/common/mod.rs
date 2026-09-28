//! Shared generators for the core integration tests.
#![allow(dead_code)]

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ObjType, ROOT, ReadDoc};
use pg_automerge_core::normalize;

/// xorshift64: deterministic, dependency-free.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A random edit on `doc`.
pub fn edit(doc: &mut AutoCommit, rng: &mut Rng) {
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
pub fn random_replicas(seed: u64) -> Vec<Vec<u8>> {
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
