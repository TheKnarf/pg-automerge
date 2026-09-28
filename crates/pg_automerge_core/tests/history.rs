//! Access to individual changes and to historical states
//! (`pg_automerge_core::history`), checked against Automerge itself and
//! against a plain walk of the change graph.

use std::collections::{HashMap, HashSet};

use automerge::transaction::{CommitOptions, Transactable};
use automerge::{ActorId, AutoCommit, Automerge, Change, ChangeHash, ROOT, ReadDoc};
use pg_automerge_core::header::{Prefix, change_count_from_bytes, change_count_from_prefix};
use pg_automerge_core::history::{ChangeInfo, parse_hash};
use pg_automerge_core::{Error, normalize};
use serde_json::json;

mod common;

use common::history::{
    change, change_count, change_count_loaded, changes, changes_bytes, changes_meta, to_json_at,
};
use common::{Rng, heads, merge_changes, random_replicas, to_json};

fn hash(s: &str) -> ChangeHash {
    parse_hash(s).unwrap()
}

fn strip_bytes(rows: &[ChangeInfo]) -> Vec<ChangeInfo> {
    rows.iter()
        .map(|r| ChangeInfo {
            bytes: None,
            ..r.clone()
        })
        .collect()
}

/// Every change's deps that are in the list come before it.
fn assert_causal(rows: &[ChangeInfo]) {
    let mut seen = HashSet::new();
    let all: HashSet<&str> = rows.iter().map(|r| r.hash.as_str()).collect();
    for r in rows {
        for d in &r.deps {
            assert!(
                !all.contains(d.as_str()) || seen.contains(d.as_str()),
                "{} listed before its dep {d}",
                r.hash
            );
        }
        assert!(seen.insert(r.hash.as_str()), "duplicate {}", r.hash);
    }
}

/// Hashes of every change that is an ancestor of (or equal to) one of
/// `heads` that the graph has.
fn ancestors(graph: &HashMap<String, Vec<String>>, heads: &[String]) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut stack: Vec<String> = heads
        .iter()
        .filter(|h| graph.contains_key(*h))
        .cloned()
        .collect();
    while let Some(h) = stack.pop() {
        if out.insert(h.clone()) {
            stack.extend(graph[&h].iter().cloned());
        }
    }
    out
}

/// A normalized document holding exactly the history up to `heads`.
fn at(doc: &Automerge, heads: &[ChangeHash]) -> Vec<u8> {
    normalize(&doc.fork_at(heads).unwrap().save()).unwrap()
}

#[test]
fn generated_histories_agree_with_automerge_and_the_graph() {
    let mut checked_since = 0;
    for seed in 1..=60u64 {
        let mut rng = Rng(seed | 1);
        for stored in random_replicas(seed) {
            let doc = Automerge::load(&stored).unwrap();
            let all = changes(&stored, &[]).unwrap();
            let meta = changes_meta(&stored, &[]).unwrap();
            // Metadata from the graph equals metadata of the rebuilt changes.
            assert_eq!(strip_bytes(&all), meta, "seed {seed}");
            assert_causal(&all);
            assert_eq!(all.len() as u64, doc.stats().num_changes);
            assert_eq!(change_count(&stored).unwrap(), all.len() as u64);
            assert_eq!(change_count_loaded(&stored).unwrap(), all.len() as u64);
            assert_eq!(
                change_count_from_bytes(&stored),
                Some(all.len() as u64),
                "seed {seed}: header count"
            );
            // Any prefix: NeedMore (asking for more than it has) or the count.
            let cut = (rng.next() as usize) % (stored.len() + 1);
            match change_count_from_prefix(&stored[..cut], stored.len()) {
                Prefix::Found(n) => assert_eq!(n, all.len() as u64),
                Prefix::NeedMore(n) => assert!(n > cut && n <= stored.len()),
                Prefix::NotSingleDoc => panic!("seed {seed}: prefix {cut}"),
            }

            // Bytes: each chunk is the change itself (its hash covers its
            // bytes) and matches what Automerge hands out.
            let original: HashMap<ChangeHash, Vec<u8>> = doc
                .get_changes(&[])
                .into_iter()
                .map(|c| (c.hash(), c.raw_bytes().to_vec()))
                .collect();
            for row in &all {
                let bytes = row.bytes.as_ref().unwrap();
                let parsed = Change::from_bytes(bytes.clone()).unwrap();
                assert_eq!(parsed.hash().to_string(), row.hash);
                assert_eq!(&original[&hash(&row.hash)], bytes);
                assert_eq!(
                    change(&stored, &hash(&row.hash)).unwrap().as_ref(),
                    Some(row)
                );
            }
            // Bare changes rebuild the whole document.
            let everything = changes_bytes(&stored, &[]).unwrap();
            let rebuilt = normalize(&everything).unwrap();
            assert_eq!(heads(&rebuilt).unwrap(), heads(&stored).unwrap());
            assert_eq!(to_json(&rebuilt).unwrap(), to_json(&stored).unwrap());

            // "Since": random sets of known heads, plus an unknown hash.
            let graph: HashMap<String, Vec<String>> = all
                .iter()
                .map(|r| (r.hash.clone(), r.deps.clone()))
                .collect();
            for round in 0..4 {
                if all.is_empty() {
                    break;
                }
                let mut since: Vec<String> = (0..1 + rng.below(3))
                    .map(|_| all[rng.below(all.len() as u64) as usize].hash.clone())
                    .collect();
                if round == 3 {
                    since.push("ee".repeat(32));
                }
                let since_hashes: Vec<ChangeHash> = since.iter().map(|s| hash(s)).collect();
                let reachable = ancestors(&graph, &since);
                let expected: Vec<&str> = all
                    .iter()
                    .filter(|r| !reachable.contains(&r.hash))
                    .map(|r| r.hash.as_str())
                    .collect();
                let got = changes(&stored, &since_hashes).unwrap();
                assert_causal(&got);
                let got_hashes: Vec<&str> = got.iter().map(|r| r.hash.as_str()).collect();
                // Same set, and the same relative order as the full list.
                assert_eq!(got_hashes, expected, "seed {seed} since {since:?}");
                assert_eq!(
                    strip_bytes(&got),
                    changes_meta(&stored, &since_hashes).unwrap()
                );

                // Round trip: the state at `since` plus the changes since it
                // is the whole document again.
                let known: Vec<ChangeHash> = since_hashes
                    .iter()
                    .copied()
                    .filter(|h| graph.contains_key(&h.to_string()))
                    .collect();
                let base = at(&doc, &known);
                let delta = changes_bytes(&stored, &heads_of(&base)).unwrap();
                let merged = merge_changes(&base, &delta).unwrap();
                let merged = merged.as_deref().unwrap_or(&base);
                assert_eq!(heads(merged).unwrap(), heads(&stored).unwrap());
                assert_eq!(to_json(merged).unwrap(), to_json(&stored).unwrap());

                // Historical state equals the state of that fork.
                assert_eq!(
                    to_json_at(&stored, &known).unwrap(),
                    to_json(&base).unwrap(),
                    "seed {seed} at {known:?}"
                );
                checked_since += 1;
            }
        }
    }
    assert!(checked_since > 300, "{checked_since}");
}

fn heads_of(stored: &[u8]) -> Vec<ChangeHash> {
    heads(stored).unwrap().iter().map(|h| hash(h)).collect()
}

#[test]
fn metadata_fields() {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([0xa1u8; 16]));
    doc.put(ROOT, "a", 1i64).unwrap();
    doc.put(ROOT, "b", 2i64).unwrap();
    doc.commit_with(
        CommitOptions::default()
            .with_message("first")
            .with_time(1_700_000_000),
    );
    let first = doc.get_heads()[0];
    doc.put(ROOT, "a", 3i64).unwrap();
    doc.commit();
    let second = doc.get_heads()[0];
    // Empty commits exist only on `Automerge`.
    let mut raw = Automerge::load(&doc.save())
        .unwrap()
        .with_actor(ActorId::from([0xa1u8; 16]));
    let empty = raw.empty_commit(CommitOptions::default().with_message("empty"));
    let stored = normalize(&raw.save()).unwrap();

    let rows = changes(&stored, &[]).unwrap();
    assert_eq!(rows.len(), 3);
    let actor = "a1".repeat(16);
    assert_eq!(
        strip_bytes(&rows),
        vec![
            ChangeInfo {
                hash: first.to_string(),
                actor: actor.clone(),
                seq: 1,
                start_op: 1,
                op_count: 2,
                time: 1_700_000_000,
                message: Some("first".into()),
                deps: vec![],
                bytes: None,
            },
            ChangeInfo {
                hash: second.to_string(),
                actor: actor.clone(),
                seq: 2,
                start_op: 3,
                op_count: 1,
                time: 0,
                message: None,
                deps: vec![first.to_string()],
                bytes: None,
            },
            ChangeInfo {
                hash: empty.to_string(),
                actor,
                seq: 3,
                start_op: 4,
                op_count: 0,
                time: 0,
                message: Some("empty".into()),
                deps: vec![second.to_string()],
                bytes: None,
            },
        ]
    );
    assert_eq!(changes_meta(&stored, &[]).unwrap(), strip_bytes(&rows));
    assert_eq!(change_count(&stored).unwrap(), 3);

    // Since the latest head: nothing (decided from the header).
    assert!(changes(&stored, &[empty]).unwrap().is_empty());
    assert!(changes_bytes(&stored, &[empty]).unwrap().is_empty());
    // Since the first change: the other two.
    let later: Vec<String> = changes_meta(&stored, &[first])
        .unwrap()
        .into_iter()
        .map(|r| r.hash)
        .collect();
    assert_eq!(later, vec![second.to_string(), empty.to_string()]);

    // State as of each change.
    assert_eq!(
        to_json_at(&stored, &[first]).unwrap(),
        json!({"a": 1, "b": 2})
    );
    assert_eq!(
        to_json_at(&stored, &[second]).unwrap(),
        json!({"a": 3, "b": 2})
    );
    assert_eq!(to_json_at(&stored, &[]).unwrap(), json!({}));
    // Redundant heads (an ancestor next to its descendant) are fine.
    assert_eq!(
        to_json_at(&stored, &[first, second]).unwrap(),
        json!({"a": 3, "b": 2})
    );
    assert_eq!(
        to_json_at(&stored, &[empty]).unwrap(),
        to_json(&stored).unwrap()
    );
    let unknown = hash(&"ab".repeat(32));
    let err = to_json_at(&stored, &[first, unknown]).unwrap_err();
    assert_eq!(
        err,
        Error::InvalidParameter(format!(
            "automerge document does not contain change {unknown}"
        ))
    );
    assert_eq!(change(&stored, &unknown).unwrap(), None);
}

#[test]
fn concurrent_history_and_text() {
    let mut base = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    let text = base
        .put_object(ROOT, "text", automerge::ObjType::Text)
        .unwrap();
    base.splice_text(&text, 0, 0, "hello").unwrap();
    let list = base
        .put_object(ROOT, "list", automerge::ObjType::List)
        .unwrap();
    base.insert(&list, 0, "x").unwrap();
    base.commit();
    let base_heads = base.get_heads();
    let mut a = base.fork().with_actor(ActorId::from([2u8; 16]));
    let mut b = base.fork().with_actor(ActorId::from([3u8; 16]));
    a.splice_text(&text, 5, 0, " world").unwrap();
    a.put(ROOT, "k", "a").unwrap();
    a.commit();
    b.insert(&list, 1, "y").unwrap();
    b.put(ROOT, "k", "b").unwrap();
    b.commit();
    let a_heads = a.get_heads();
    let b_heads = b.get_heads();
    a.merge(&mut b).unwrap();
    let stored = normalize(&a.save()).unwrap();

    assert_eq!(
        to_json_at(&stored, &base_heads).unwrap(),
        json!({"text": "hello", "list": ["x"]})
    );
    assert_eq!(
        to_json_at(&stored, &a_heads).unwrap(),
        json!({"text": "hello world", "list": ["x"], "k": "a"})
    );
    assert_eq!(
        to_json_at(&stored, &b_heads).unwrap(),
        json!({"text": "hello", "list": ["x", "y"], "k": "b"})
    );
    let both = [a_heads[0], b_heads[0]];
    assert_eq!(
        to_json_at(&stored, &both).unwrap(),
        to_json(&stored).unwrap()
    );
    // The changes since base are the two concurrent ones.
    let since_base = changes_meta(&stored, &base_heads).unwrap();
    assert_eq!(since_base.len(), 2);
    // Since one branch: the other.
    let since_a = changes_meta(&stored, &a_heads).unwrap();
    assert_eq!(since_a.len(), 1);
    assert_eq!(since_a[0].hash, b_heads[0].to_string());
}

#[test]
fn empty_document() {
    let empty = normalize(&[]).unwrap();
    assert!(changes(&empty, &[]).unwrap().is_empty());
    assert!(changes_meta(&empty, &[]).unwrap().is_empty());
    assert!(changes_bytes(&empty, &[]).unwrap().is_empty());
    assert_eq!(change_count(&empty).unwrap(), 0);
    assert_eq!(change_count_from_bytes(&empty), Some(0));
    assert_eq!(to_json_at(&empty, &[]).unwrap(), json!({}));
    let h = hash(&"00".repeat(32));
    assert!(changes(&empty, &[h]).unwrap().is_empty());
    assert!(matches!(
        to_json_at(&empty, &[h]),
        Err(Error::InvalidParameter(_))
    ));
}

#[test]
fn change_count_falls_back_for_other_encodings() {
    let replicas = random_replicas(11);
    let mut doc = AutoCommit::load(&replicas[0]).unwrap();
    let n = change_count(&replicas[0]).unwrap();
    let before = doc.get_heads();
    doc.put(ROOT, "later", true).unwrap();
    let mut trailing = replicas[0].clone();
    trailing.extend(doc.save_after(&before));
    assert_eq!(change_count_from_bytes(&trailing), None);
    assert_eq!(change_count(&trailing).unwrap(), n + 1);
    // A compressed save is still counted right (fast path or not).
    assert_eq!(change_count(&doc.save()).unwrap(), n + 1);
    // Bit flips never panic; whatever the fast path returns for a flipped
    // actor column is irrelevant (stored values are never corrupt), but it
    // must not crash.
    let stored = &replicas[0];
    for i in 0..stored.len() {
        let mut b = stored.clone();
        b[i] ^= 0xff;
        let _ = change_count_from_bytes(&b);
        let _ = change_count_from_prefix(&b[..i], b.len());
    }
}

#[test]
fn many_actors_alternating() {
    // Alternating actors produce literal runs in the actor column; long
    // single-actor stretches produce repeat runs.
    let mut shared = AutoCommit::new();
    for round in 0..200u64 {
        let i = if round < 100 { (round % 5) as u8 } else { 0 };
        shared.set_actor(ActorId::from([i + 1; 16]));
        shared.put(ROOT, format!("r{round}"), round as i64).unwrap();
        shared.commit();
    }
    let stored = normalize(&shared.save()).unwrap();
    let n = Automerge::load(&stored).unwrap().stats().num_changes;
    assert_eq!(n, 200);
    assert_eq!(change_count_from_bytes(&stored), Some(n));
}

/// The shortcuts that need no load, observed through a stored value whose
/// header and change columns are intact but whose tail is corrupt.
#[test]
fn header_shortcuts_do_not_load() {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    doc.put(ROOT, "x", 1i64).unwrap();
    doc.commit();
    doc.put(ROOT, "y", 2i64).unwrap();
    let stored = normalize(&doc.save()).unwrap();
    let mut bad = stored.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xff; // a head index, after all columns
    assert!(Automerge::load(&bad).is_err());
    let heads = heads_of(&stored);
    // Nothing since the current heads: decided from the header.
    assert!(changes_meta(&bad, &heads).unwrap().is_empty());
    assert!(changes(&bad, &heads).unwrap().is_empty());
    assert!(changes_bytes(&bad, &heads).unwrap().is_empty());
    // The count comes from the change actor column.
    assert_eq!(change_count(&bad).unwrap(), 2);
    // Everything else loads, and reports the corruption as internal.
    assert!(matches!(changes_meta(&bad, &[]), Err(Error::Internal(_))));
    assert!(matches!(change_count_loaded(&bad), Err(Error::Internal(_))));
    assert!(matches!(to_json_at(&bad, &heads), Err(Error::Internal(_))));
}
