//! Shared helpers for the core integration tests: stored-bytes wrappers
//! around the `Input`-based API, and random document generators.
#![allow(dead_code)]

use std::borrow::Cow;

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ChangeHash, ObjType, ROOT, ReadDoc};
use pg_automerge_core::loaded::{self, Input, MergeOutcome};
use pg_automerge_core::{Accumulated, Error, MergeAccumulator, normalize};

// ---------------------------------------------------------------------------
// Stored-bytes conveniences (every value is a stored value: `Input::Stored`)
// ---------------------------------------------------------------------------

/// Which bytes `merge` produced. `Left`/`Right`: one input already
/// contained the other and is the result as is.
#[derive(Debug, PartialEq, Eq)]
pub enum Merged {
    Left,
    Right,
    New(Vec<u8>),
}

impl Merged {
    pub fn into_bytes<'a>(self, a: &'a [u8], b: &'a [u8]) -> Cow<'a, [u8]> {
        match self {
            Merged::Left => Cow::Borrowed(a),
            Merged::Right => Cow::Borrowed(b),
            Merged::New(bytes) => Cow::Owned(bytes),
        }
    }
}

/// `loaded::merge` of two stored values, with the new document saved.
pub fn merge(a: &[u8], b: &[u8]) -> Result<Merged, Error> {
    Ok(match loaded::merge(Input::Stored(a), Input::Stored(b))? {
        MergeOutcome::Left => Merged::Left,
        MergeOutcome::Right => Merged::Right,
        MergeOutcome::New(doc) => Merged::New(doc.stored()?.to_vec()),
    })
}

/// `loaded::merge_changes` on a stored value, with the result saved (which
/// runs the save-and-load check).
pub fn merge_changes(a: &[u8], changes: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    match loaded::merge_changes(Input::Stored(a), changes)? {
        None => Ok(None),
        Some(doc) => Ok(Some(doc.stored()?.to_vec())),
    }
}

pub fn contains(a: &[u8], b: &[u8]) -> Result<bool, Error> {
    loaded::contains(Input::Stored(a), Input::Stored(b))
}

pub fn contains_changes(a: &[u8], changes: &[u8]) -> Result<bool, Error> {
    loaded::contains_changes(Input::Stored(a), changes)
}

/// Whether the stored value `a` has every change of the history ending at
/// `heads_b`, from its loaded history (never from the heads alone).
pub fn contains_loaded(a: &[u8], heads_b: &[ChangeHash]) -> Result<bool, Error> {
    loaded::contains_heads(Input::Stored(a), heads_b)
}

/// Current heads as sorted lowercase hex.
pub fn heads(bytes: &[u8]) -> Result<Vec<String>, Error> {
    Ok(pg_automerge_core::heads_to_strings(
        pg_automerge_core::stored_heads(bytes)?,
    ))
}

pub fn to_json(bytes: &[u8]) -> Result<serde_json::Value, Error> {
    loaded::with_doc(Input::Stored(bytes), pg_automerge_core::json::doc_to_json)
}

/// Stored-bytes use of a [`MergeAccumulator`].
pub trait StoredAccumulator {
    fn add(&mut self, bytes: &[u8]) -> Result<(), Error>;
    /// The merged stored bytes, or `None` if nothing was added.
    fn finish(&self) -> Result<Option<Cow<'_, [u8]>>, Error>;
}

impl StoredAccumulator for MergeAccumulator {
    fn add(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.add_input(Input::Stored(bytes))
    }

    fn finish(&self) -> Result<Option<Cow<'_, [u8]>>, Error> {
        Ok(match self.finish_loaded()? {
            None => None,
            Some(Accumulated::Stored(bytes)) => Some(Cow::Borrowed(bytes)),
            Some(Accumulated::Loaded(doc)) => Some(Cow::Owned(doc.stored()?.to_vec())),
        })
    }
}

/// The history functions on stored bytes.
pub mod history {
    use automerge::ChangeHash;
    use pg_automerge_core::Error;
    use pg_automerge_core::history::{self, ChangeInfo};
    use pg_automerge_core::loaded::{Input, LoadedDoc};

    pub fn changes_meta(stored: &[u8], since: &[ChangeHash]) -> Result<Vec<ChangeInfo>, Error> {
        history::changes_meta(Input::Stored(stored), since)
    }

    pub fn changes(stored: &[u8], since: &[ChangeHash]) -> Result<Vec<ChangeInfo>, Error> {
        history::changes(Input::Stored(stored), since)
    }

    pub fn changes_bytes(stored: &[u8], since: &[ChangeHash]) -> Result<Vec<u8>, Error> {
        history::changes_bytes(Input::Stored(stored), since)
    }

    pub fn change(stored: &[u8], hash: &ChangeHash) -> Result<Option<ChangeInfo>, Error> {
        history::change(Input::Stored(stored), hash)
    }

    pub fn change_count(stored: &[u8]) -> Result<u64, Error> {
        history::change_count(Input::Stored(stored))
    }

    /// [`change_count`] from the loaded document (never the header).
    pub fn change_count_loaded(stored: &[u8]) -> Result<u64, Error> {
        history::change_count(Input::Loaded(&LoadedDoc::from_stored(stored)?))
    }

    pub fn to_json_at(stored: &[u8], heads: &[ChangeHash]) -> Result<serde_json::Value, Error> {
        history::to_json_at(Input::Stored(stored), heads)
    }
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Reference implementations
// ---------------------------------------------------------------------------

/// What `normalize` computes, the long way: load strictly (no missing
/// dependencies), save uncompressed, load the save back and compare heads.
/// `None` where `normalize` must fail (a panic counts as a failure).
pub fn reference_normalize(bytes: &[u8]) -> Option<Vec<u8>> {
    std::panic::catch_unwind(|| {
        let doc = automerge::Automerge::load(bytes).ok()?;
        if !doc.get_missing_deps(&[]).is_empty() {
            return None;
        }
        let saved = doc.save_nocompress();
        let reloaded = automerge::Automerge::load(&saved).ok()?;
        let mut before = doc.get_heads();
        before.sort();
        let mut after = reloaded.get_heads();
        after.sort();
        (before == after).then_some(saved)
    })
    .ok()
    .flatten()
}

/// A chunk of type `chunk_type` around `data`, with a correct checksum.
pub fn chunk(chunk_type: u8, data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut len = Vec::new();
    let mut v = data.len() as u64;
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            len.push(b);
            break;
        }
        len.push(b | 0x80);
    }
    let mut hasher = Sha256::new();
    hasher.update([chunk_type]);
    hasher.update(&len);
    hasher.update(data);
    let hash: [u8; 32] = hasher.finalize().into();
    let mut out = vec![0x85, 0x6f, 0x4a, 0x83];
    out.extend_from_slice(&hash[..4]);
    out.push(chunk_type);
    out.extend_from_slice(&len);
    out.extend_from_slice(data);
    out
}

/// The stack of [`on_small_stack`]'s thread: far below what any recursion
/// once per level of the deep documents of the tests needs, above what
/// every entry point needs on shallow ones (in a debug build).
pub const SMALL_STACK: usize = 1 << 20;

/// Run `f` on a thread with a [`SMALL_STACK`] stack. A recursion as deep
/// as a test document overflows it: the test process aborts ("has
/// overflowed its stack"), a failure.
pub fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(f)
        .expect("spawn a thread")
        .join()
        .expect("the thread finished")
}
