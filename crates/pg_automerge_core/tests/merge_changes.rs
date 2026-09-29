//! `merge_changes`: applying external bytes (full saves or bare change
//! chunks) on top of a stored value, as `merge(automerge, bytea)` does.

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, Automerge, ChangeHash, ROOT};
use pg_automerge_core::{Error, contains_changes_by_heads, normalize, stored_heads};
use serde_json::json;

mod common;

use common::{contains_changes, heads, merge, merge_changes, to_json};

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
        Err(e @ Error::MissingDependencies(_)) => e.to_string(),
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

/// Two writers that share an actor id: each history is valid, together
/// they hold two different changes with the same (actor, seq). Every path
/// that combines them reports the same [`Error::ConflictingChanges`]
/// (22000 in SQL), with the actor and seq in the message and a hint.
#[test]
fn reused_actor_id_is_a_conflict_on_every_path() {
    let mut base = AutoCommit::new().with_actor(actor(2));
    base.put(ROOT, "base", true).unwrap();
    base.commit();
    let base_heads = base.get_heads();
    let mut a = base.fork().with_actor(actor(1));
    let mut b = base.fork().with_actor(actor(1));
    a.put(ROOT, "x", 1i64).unwrap();
    a.commit();
    b.put(ROOT, "x", 2i64).unwrap();
    b.commit();
    let stored_a = stored(&mut a);
    let stored_b = stored(&mut b);
    let b_change = b.save_after(&base_heads);

    let conflict = |what: &str, err: Error| {
        let Error::ConflictingChanges(msg) = &err else {
            panic!("{what}: expected ConflictingChanges, got {err:?}");
        };
        assert_eq!(
            msg,
            &format!(
                "conflicting automerge changes: actor {} has two different changes with seq 1",
                actor(1)
            ),
            "{what}"
        );
        assert!(err.detail().is_some(), "{what}");
        assert!(
            err.hint().is_some_and(|h| h.contains("its own actor id")),
            "{what}"
        );
    };
    // merge(automerge, automerge) of two stored documents.
    conflict("merge", merge(&stored_a, &stored_b).err().unwrap());
    // merge(automerge, bytea): a full save, compressed or not; bare change
    // chunks; a save followed by change chunks (the concatenating load).
    conflict("save", merge_changes(&stored_a, &b.save()).unwrap_err());
    conflict(
        "uncompressed save",
        merge_changes(&stored_a, &stored_b).unwrap_err(),
    );
    conflict(
        "change chunk",
        merge_changes(&stored_a, &b_change).unwrap_err(),
    );
    let mut save_then_change = base.save();
    save_then_change.extend_from_slice(&b_change);
    conflict(
        "save and chunk",
        merge_changes(&stored_a, &save_then_change).unwrap_err(),
    );
    // One input that holds both histories (text input, the bytea cast).
    conflict(
        "normalize",
        normalize(&[a.save(), b_change.clone()].concat()).unwrap_err(),
    );
    // merge_agg.
    let mut acc = pg_automerge_core::MergeAccumulator::new();
    acc.add_input(pg_automerge_core::loaded::Input::Stored(&stored_a))
        .unwrap();
    let err = acc
        .add_input(pg_automerge_core::loaded::Input::Stored(&stored_b))
        .and_then(|()| acc.finish_loaded().map(|_| ()))
        .unwrap_err();
    conflict("merge_agg", err);
    // automerge_contains(doc, bytea) when it has to load `a ++ changes`:
    // the same error (the changes could never be merged either).
    conflict(
        "contains",
        contains_changes(&stored_a, &b_change).unwrap_err(),
    );
}

/// Other errors of Automerge keep their class: only the actor conflicts
/// are [`Error::ConflictingChanges`].
#[test]
fn only_actor_conflicts_are_conflicts() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "x", 1i64).unwrap();
    let base = stored(&mut doc);
    let msg = invalid(merge_changes(&base, b"\x85\x6f\x4a\x83garbage"));
    assert!(msg.starts_with("invalid automerge"), "{msg}");
}

#[test]
fn contains_changes_common_cases_need_no_load() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "v", 0i64).unwrap();
    let base = stored(&mut doc);
    let base_heads = doc.get_heads();
    doc.put(ROOT, "v", 1i64).unwrap();
    let next = doc.save_after(&base_heads);
    let one = apply(&base, &next);
    let heads_base = stored_heads(&base).unwrap();
    let heads_one = stored_heads(&one).unwrap();

    // New changes on top of the current heads: known missing.
    assert_eq!(contains_changes_by_heads(&heads_base, &next), Some(false));
    assert!(!contains_changes(&base, &next).unwrap());
    // Re-sending the change that made the current head: known present, and
    // merge returns the value as is without loading it.
    assert_eq!(contains_changes_by_heads(&heads_one, &next), Some(true));
    assert!(contains_changes(&one, &next).unwrap());
    assert_eq!(merge_changes(&one, &next).unwrap(), None);
    // Empty input is contained.
    assert_eq!(contains_changes_by_heads(&heads_base, &[]), Some(true));
    assert!(contains_changes(&base, &[]).unwrap());

    // An older change (not a head) needs a load: still contained.
    doc.put(ROOT, "v", 2i64).unwrap();
    let two = apply(&one, &doc.save_after(&heads_one));
    let heads_two = stored_heads(&two).unwrap();
    assert_eq!(contains_changes_by_heads(&heads_two, &next), None);
    assert!(contains_changes(&two, &next).unwrap());

    // A full save goes through the load, either way.
    assert_eq!(contains_changes_by_heads(&heads_two, &doc.save()), None);
    assert!(contains_changes(&two, &doc.save()).unwrap());
    assert!(!contains_changes(&base, &doc.save()).unwrap());

    // Orphaned changes are not contained (merge would reject them).
    let orphan = doc.save_after(&heads_one);
    assert_eq!(contains_changes_by_heads(&heads_base, &orphan), None);
    assert!(!contains_changes(&base, &orphan).unwrap());

    // Garbage is invalid input, whichever path.
    assert!(matches!(
        contains_changes(&base, b"garbage"),
        Err(Error::InvalidInput(_))
    ));
    let mut bad = next.clone();
    let last = bad.len() - 1;
    bad[last] ^= 1; // checksum no longer matches
    assert_eq!(contains_changes_by_heads(&heads_one, &bad), None);
    assert!(matches!(
        contains_changes(&one, &bad),
        Err(Error::InvalidInput(_))
    ));
}

/// `contains_changes` (and its no-load shortcut, whenever it answers)
/// agrees with a direct check against the loaded document, over generated
/// histories and slices of their change lists.
#[test]
fn contains_changes_matches_loaded_document() {
    let (mut fast_true, mut fast_false, mut slow) = (0, 0, 0);
    for seed in 1..=60u64 {
        let replicas = common::random_replicas(seed);
        let docs: Vec<Automerge> = replicas.iter().map(|r| load(r)).collect();
        let mut rng = common::Rng(seed | 1);
        for (i, a) in replicas.iter().enumerate() {
            let heads_a = stored_heads(a).unwrap();
            for (j, other) in docs.iter().enumerate() {
                let since = match rng.below(3) {
                    0 => vec![],
                    1 => docs[i].get_heads(),
                    _ => docs[rng.below(docs.len() as u64) as usize].get_heads(),
                };
                let changes = other.get_changes(&since);
                if changes.is_empty() {
                    continue;
                }
                // A contiguous slice (causal order kept, but possibly
                // without its dependencies).
                let start = rng.below(changes.len() as u64) as usize;
                let end = start + 1 + rng.below((changes.len() - start) as u64) as usize;
                let slice = &changes[start..end];
                let bytes: Vec<u8> = slice.iter().flat_map(|c| c.raw_bytes().to_vec()).collect();
                let expected = slice
                    .iter()
                    .all(|c| docs[i].get_change_meta_by_hash(&c.hash()).is_some());
                let got = contains_changes(a, &bytes).unwrap();
                assert_eq!(got, expected, "seed {seed} a={i} other={j} {start}..{end}");
                match contains_changes_by_heads(&heads_a, &bytes) {
                    Some(true) => fast_true += 1,
                    Some(false) => fast_false += 1,
                    None => slow += 1,
                }
                // merge agrees: a no-op exactly when contained (or an
                // error for orphaned changes, which are not contained).
                match merge_changes(a, &bytes) {
                    Ok(None) => assert!(expected),
                    Ok(Some(_)) | Err(Error::InvalidInput(_) | Error::MissingDependencies(_)) => {
                        assert!(!expected)
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        }
    }
    assert!(
        fast_true > 50 && fast_false > 50 && slow > 50,
        "{fast_true} {fast_false} {slow}"
    );
}

/// Loads taken by `f` on this thread.
fn loads_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = pg_automerge_core::test_hooks::loads();
    let result = f();
    (result, pg_automerge_core::test_hooks::loads() - before)
}

#[test]
fn full_saves_load_once_when_they_contain_the_document() {
    use pg_automerge_core::loaded::{self, Input, LoadedDoc};
    use pg_automerge_core::test_hooks::reload_checks;

    let mut base = AutoCommit::new().with_actor(actor(1));
    base.put(ROOT, "pad", "long enough to compress ".repeat(40))
        .unwrap();
    base.commit();
    let older = base.save();
    base.put(ROOT, "status", "stored").unwrap();
    base.commit();
    let a = stored(&mut base);
    let mut newer = AutoCommit::load(&base.save()).unwrap().with_actor(actor(2));
    newer.put(ROOT, "status", "edited").unwrap();
    newer.commit();
    let newer_stored = stored(&mut newer);

    // A newer save, compressed or canonical: one load (the save), no
    // check, and the result is the save's own stored bytes.
    for input in [newer.save(), newer_stored.clone()] {
        let checks = reload_checks();
        let (got, n) = loads_of(|| loaded::merge_changes(Input::Stored(&a), &input).unwrap());
        let got = got.expect("new changes");
        assert_eq!(n, 1);
        assert!(!got.is_unverified());
        assert_eq!(got.cached_stored(), Some(newer_stored.as_slice()));
        assert_eq!(reload_checks(), checks);
        // The same through a loaded `a`: nothing loaded at all.
        let loaded_a = LoadedDoc::from_stored(&a).unwrap();
        let (got, n) =
            loads_of(|| loaded::merge_changes(Input::Loaded(&loaded_a), &input).unwrap());
        assert_eq!(n, 1);
        assert_eq!(got.unwrap().cached_stored(), Some(newer_stored.as_slice()));
    }
    // The document's own save, or one it contains: the header decides
    // (no load), or `a` is loaded first because the save has fewer
    // changes (one load, never the save).
    for input in [a.clone(), base.save()] {
        assert_eq!(loads_of(|| merge_changes(&a, &input).unwrap()), (None, 0));
    }
    assert_eq!(loads_of(|| merge_changes(&a, &older).unwrap()), (None, 1));
    assert_eq!(
        loads_of(|| contains_changes(&a, &older).unwrap()),
        (true, 1)
    );
    // A newer save lists more changes than `a` has: not contained, and
    // nothing is loaded.
    assert_eq!(
        loads_of(|| contains_changes(&a, &newer.save()).unwrap()),
        (false, 0)
    );
    assert_eq!(loads_of(|| contains_changes(&a, &a).unwrap()), (true, 0));

    // A concurrent save: both loaded, merged into `a`'s document, checked
    // when saved.
    let mut other = AutoCommit::load(&older).unwrap().with_actor(actor(3));
    other.put(ROOT, "other", true).unwrap();
    other.commit();
    let (got, n) = loads_of(|| loaded::merge_changes(Input::Stored(&a), &other.save()).unwrap());
    let got = got.unwrap();
    assert_eq!(n, 2);
    assert!(got.is_unverified());
    let checks = reload_checks();
    let bytes = got.stored().unwrap().to_vec();
    assert_eq!(reload_checks(), checks + 1);
    assert_eq!(to_json(&bytes).unwrap()["other"], json!(true));
    assert_eq!(to_json(&bytes).unwrap()["status"], json!("stored"));
    assert_eq!(heads(&bytes).unwrap().len(), 2);

    // A save whose trailing changes depend on changes only `a` has: loaded
    // as `a ++ input`, as before.
    let mut writer = AutoCommit::load(&a).unwrap().with_actor(actor(4));
    let heads_a = writer.get_heads();
    writer.put(ROOT, "later", 1i64).unwrap();
    let trailing = [older.as_slice(), &writer.save_after(&heads_a)].concat();
    let got = apply(&a, &trailing);
    assert_eq!(heads(&got).unwrap(), sorted_heads(&mut writer));
    assert_eq!(to_json(&got).unwrap()["later"], json!(1));

    // Strict: a save with a bad checksum is rejected even when its heads
    // are known; any corruption is a clean error or a valid result.
    let mut bad = a.clone();
    bad[5] ^= 1;
    assert!(invalid(merge_changes(&a, &bad)).contains("bad checksum"));
    let save = newer.save();
    for i in (0..save.len()).step_by(7) {
        let mut b = save.clone();
        b[i] ^= 0x5a;
        match merge_changes(&a, &b) {
            Ok(_) | Err(Error::InvalidInput(_) | Error::MissingDependencies(_)) => {}
            Err(e) => panic!("byte {i}: {e:?}"),
        }
    }
}

#[test]
fn verification_can_be_switched_off() {
    use pg_automerge_core::loaded::{self, Input, LoadedDoc};
    use pg_automerge_core::test_hooks::{reload_checks, set_verification};

    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            set_verification(None);
        }
    }
    let _reset = Reset;

    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "x", 1i64).unwrap();
    let heads = doc.get_heads();
    let save = doc.save();
    doc.put(ROOT, "y", 2i64).unwrap();
    let change = doc.save_after(&heads);
    let trailing = [save.as_slice(), &change].concat();
    let a = normalize(&save).unwrap();
    assert!(pg_automerge_core::verification_enabled());

    // On: input that is not its own encoding is loaded back.
    let checks = reload_checks();
    let (on, n) = loads_of(|| normalize(&trailing).unwrap());
    assert_eq!((n, reload_checks()), (2, checks + 1));
    // Off: the same bytes, one load, no check; also for merge results.
    set_verification(Some(false));
    assert!(!pg_automerge_core::verification_enabled());
    let (off, n) = loads_of(|| normalize(&trailing).unwrap());
    assert_eq!((n, reload_checks()), (1, checks + 1));
    assert_eq!(on, off);
    let loaded = LoadedDoc::from_external(&trailing).unwrap();
    assert_eq!(loaded.cached_stored(), Some(on.as_slice()));
    let merged = loaded::merge_changes(Input::Stored(&a), &change)
        .unwrap()
        .unwrap();
    assert!(merged.is_unverified());
    let (bytes, n) = loads_of(|| merged.stored().unwrap().to_vec());
    assert_eq!((n, reload_checks()), (0, checks + 1));
    assert_eq!(bytes, on);
    set_verification(None);
    assert!(pg_automerge_core::verification_enabled());
}

/// A document chunk whose header lists heads the stored document has, but
/// whose body Automerge's chunk parser rejects, is malformed input (22P02),
/// not a no-op: a load of `a ++ bytes` fails on it too (Automerge skips
/// only the reconstruction of a known chunk, not its parsing). A chunk
/// that does parse is decided from its header, as that load decides it.
#[test]
fn saves_with_known_heads_and_malformed_bodies_are_rejected() {
    use pg_automerge_core::contains_input_by_header;
    use pg_automerge_core::header::document_parses;
    use pg_automerge_core::loaded::{self, Input, LoadedDoc};

    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "x", 1i64).unwrap();
    doc.commit();
    let a = stored(&mut doc);
    let head = doc.get_heads()[0];
    let loaded_a = LoadedDoc::from_stored(&a).unwrap();
    // A chunk's data: after magic, checksum, type and the LEB128 length.
    let data_of = |bytes: &[u8]| -> Vec<u8> {
        let mut pos = 9;
        while bytes[pos] & 0x80 != 0 {
            pos += 1;
        }
        bytes[pos + 1..].to_vec()
    };
    let data_a = data_of(&a);
    // Header of a chunk with no actors and `head` as its only head.
    let known = |rest: &[u8]| -> Vec<u8> {
        let mut data = vec![0x00, 0x01];
        data.extend_from_slice(&head.0);
        data.extend_from_slice(rest);
        common::chunk(0, &data)
    };

    let malformed = [
        // The review's probe: garbage after the heads.
        (
            "garbage columns",
            known(&[0xff, 0xff, 0xff, 0xff, 0x01, 0x02]),
        ),
        // Valid save data with a byte after the head indices.
        (
            "leftover data",
            common::chunk(0, &[data_a.as_slice(), &[0]].concat()),
        ),
        // Overlong LEB128 for the actor count.
        (
            "overlong leb128",
            common::chunk(0, &[&[data_a[0] | 0x80, 0x00][..], &data_a[1..]].concat()),
        ),
        // A deflated change column that does not inflate.
        (
            "bad deflate",
            known(&[0x01, 0x09, 0x02, 0x00, 0xff, 0xff, 0x00]),
        ),
        // A value column without its metadata column.
        ("lone value column", known(&[0x01, 0x57, 0x00, 0x00, 0x00])),
        // Columns out of order.
        (
            "out of order",
            known(&[0x02, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00]),
        ),
        // A truncated head index.
        ("truncated head index", known(&[0x00, 0x00, 0x80])),
    ];
    for (what, bad) in &malformed {
        assert!(
            Automerge::load(&[a.as_slice(), bad].concat()).is_err(),
            "{what}: Automerge should reject it"
        );
        assert!(!document_parses(bad), "{what}");
        assert_eq!(
            contains_input_by_header(&stored_heads(&a).unwrap(), || Some(1), bad),
            None,
            "{what}"
        );
        invalid(merge_changes(&a, bad));
        assert!(
            matches!(
                loaded::merge_changes(Input::Loaded(&loaded_a), bad),
                Err(Error::InvalidInput(_))
            ),
            "{what}"
        );
        assert!(
            matches!(contains_changes(&a, bad), Err(Error::InvalidInput(_))),
            "{what}"
        );
        assert!(
            matches!(
                loaded::contains_changes(Input::Loaded(&loaded_a), bad),
                Err(Error::InvalidInput(_))
            ),
            "{what}"
        );
    }

    // A chunk Automerge does accept on top of `a` (no columns, one head
    // index): decided from its header, as the load decides it.
    let empty_body = known(&[0x00, 0x00, 0x00]);
    assert!(Automerge::load(&[a.as_slice(), &empty_body].concat()).is_ok());
    assert!(document_parses(&empty_body));
    assert_eq!(merge_changes(&a, &empty_body).unwrap(), None);
    assert!(contains_changes(&a, &empty_body).unwrap());
    // And the document's own saves.
    for save in [a.clone(), doc.save()] {
        assert!(document_parses(&save));
        assert_eq!(merge_changes(&a, &save).unwrap(), None);
        assert!(contains_changes(&a, &save).unwrap());
    }
}
