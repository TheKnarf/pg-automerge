//! The load memory limit (`budget`, `pg_automerge.max_load_memory`):
//! measured peaks against the estimate, and rejections before Automerge
//! allocates.
//!
//! A counting allocator (`common/counting.rs`) measures the peak bytes of
//! each call on its thread. For generated documents (text, lists, maps,
//! many changes, many actors, conflicts, marks, deletions) and crafted
//! chunks whose run-length encoded columns describe far more than they
//! hold, on every path that takes client bytes (normalize of a save,
//! compressed or not, and of change chunks; `merge_changes`;
//! `contains_changes`; `merge` of two documents; the `merge_agg`
//! accumulator):
//!
//! - with the limit at the estimate, the input is accepted (or rejected by
//!   Automerge, for crafted input that does not load) and the measured
//!   peak stays below the estimate;
//! - with the limit one byte lower, it is rejected (`Error::LoadLimit`)
//!   with a peak that is a small fraction of it (for merges of stored
//!   documents, below what applying the changes would have taken).
//!
//! Also: deflate bombs are rejected quickly, the scan's counts equal
//! Automerge's own, the run-level `gmax` equals the row-level definition,
//! merge results that outgrow the limit are rejected, a save rejected
//! after the stored document was loaded does not hold its inflated
//! columns through that load, and the bundle chunk is refused.
//!
//! This is the test that has to be re-run when Automerge is upgraded:
//! the estimate's constants are measured costs of Automerge 0.12, and
//! `the_cost_model_was_measured_for_this_automerge` fails on another
//! Automerge (or hexane) until they have been re-measured. Run it in a
//! release build with `--nocapture` (under `ulimit -v`) to see every
//! input's peak and estimate.

use automerge::marks::{ExpandMark, Mark};
use automerge::transaction::{CommitOptions, Transactable};
use automerge::{ActorId, AutoCommit, Automerge, ObjType, ROOT, ReadDoc, ScalarValue};
use pg_automerge_core::budget::{
    self, Base, InputCounts, LimitKind, doc_estimate, scan_doc, scan_doc_exact, scan_input,
    scan_input_exact,
};
use pg_automerge_core::loaded::{self, Input, LoadedDoc, MergeOutcome};
use pg_automerge_core::test_hooks::set_limit;
use pg_automerge_core::{Error, MergeAccumulator, normalize};

#[path = "common/counting.rs"]
mod counting;
#[path = "common/craft.rs"]
mod craft;

use counting::peak_of;

fn actor(i: u64) -> ActorId {
    ActorId::from(craft::actor(i))
}

fn letters(n: usize, seed: u64) -> String {
    let mut s = seed | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            char::from(b'a' + (s % 26) as u8)
        })
        .collect()
}

/// Generated documents of every shape the estimate prices differently.
fn generated(kind: &str, n: usize) -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(actor(0));
    match kind {
        "text" => {
            let t = doc.put_object(ROOT, "t", ObjType::Text).unwrap();
            doc.splice_text(&t, 0, 0, &letters(n, 7)).unwrap();
        }
        "typed" => {
            let t = doc.put_object(ROOT, "t", ObjType::Text).unwrap();
            doc.commit();
            for i in 0..n {
                doc.splice_text(&t, i, 0, "x").unwrap();
                doc.commit();
            }
        }
        "deleted" => {
            let t = doc.put_object(ROOT, "t", ObjType::Text).unwrap();
            doc.splice_text(&t, 0, 0, &letters(n, 3)).unwrap();
            doc.commit();
            doc.splice_text(&t, 0, n as isize, "").unwrap();
        }
        "prepend" => {
            let l = doc.put_object(ROOT, "l", ObjType::List).unwrap();
            for i in 0..n {
                doc.insert(&l, 0, i as i64).unwrap();
            }
        }
        "prepend_maps" => {
            let l = doc.put_object(ROOT, "l", ObjType::List).unwrap();
            for _ in 0..n {
                doc.insert_object(&l, 0, ObjType::Map).unwrap();
            }
        }
        "keys" => {
            for i in 0..n {
                doc.put(ROOT, format!("k{i}"), i as i64).unwrap();
            }
        }
        "overwrite" => {
            for i in 0..n {
                doc.put(ROOT, "k", i as i64).unwrap();
            }
        }
        "toggle" => {
            for i in 0..n {
                doc.put(ROOT, "k", i as i64).unwrap();
                doc.delete(ROOT, "k").unwrap();
                if i % 10 == 9 {
                    doc.commit();
                }
            }
        }
        "changes" => {
            for i in 0..n {
                doc.put(ROOT, "k", i as i64).unwrap();
                doc.commit();
            }
        }
        "actors" => {
            doc.put(ROOT, "k", 0i64).unwrap();
            doc.commit();
            for i in 1..n as u64 {
                doc.set_actor(actor(i));
                doc.put(ROOT, "k", i as i64).unwrap();
                doc.commit();
            }
        }
        "conflicts" => {
            doc.put(ROOT, "base", 0i64).unwrap();
            doc.commit();
            let base = doc.clone();
            for i in 1..n as u64 {
                let mut fork = base.clone().with_actor(actor(i));
                fork.put(ROOT, "k", i as i64).unwrap();
                fork.commit();
                doc.merge(&mut fork).unwrap();
            }
        }
        "actor_changes" => {
            // Many actors, then many changes of 16 ops each: the change ×
            // actor terms.
            let mut last = ROOT;
            for i in 1..=n as u64 {
                doc.set_actor(actor(i));
                last = doc.put_object(ROOT, format!("m{i}"), ObjType::Map).unwrap();
                doc.commit();
            }
            doc.set_actor(actor(0));
            for i in 0..n {
                for j in 0..16 {
                    doc.put(&last, format!("k{j}"), i as i64).unwrap();
                }
                doc.commit();
            }
        }
        "marks" => {
            let t = doc.put_object(ROOT, "t", ObjType::Text).unwrap();
            doc.splice_text(&t, 0, 0, &letters(n, 5)).unwrap();
            for i in (0..n - 1).rev().step_by(3) {
                doc.mark(
                    &t,
                    Mark::new(format!("m{}", i % 40), i as i64, i, i + 1),
                    ExpandMark::Both,
                )
                .unwrap();
            }
        }
        "bytes" => {
            doc.put(ROOT, "b", ScalarValue::Bytes(letters(n, 11).into_bytes()))
                .unwrap();
        }
        "long_keys" => {
            // 8 actors take turns overwriting 20 keys of 64 bytes, one
            // change per round: every key is a repeat run of `n` rows of
            // the document's key column, which its rebuilt changes copy.
            let keys: Vec<String> = (0..20).map(|j| format!("{j:0>64}")).collect();
            for i in 0..n {
                doc.set_actor(actor(i as u64 % 8));
                for k in &keys {
                    doc.put(ROOT, k.as_str(), i as i64).unwrap();
                }
                doc.commit();
            }
        }
        "long_actors" => {
            // 4 actors with 32-byte ids type `n` characters each, ten per
            // change, then delete half of them: every rebuilt change
            // holds its actor and the others its ops refer to.
            let t = doc.put_object(ROOT, "t", ObjType::Text).unwrap();
            doc.commit();
            let long = |a: u64| ActorId::from([craft::actor(a), craft::actor(a)].concat());
            for a in 0..4 {
                doc.set_actor(long(a));
                for _ in 0..n / 10 {
                    let len = doc.length(&t);
                    doc.splice_text(&t, len, 0, "abcdefghij").unwrap();
                    doc.commit();
                }
            }
            for a in 0..4 {
                doc.set_actor(long(a));
                for _ in 0..n / 20 {
                    let at = doc.length(&t) / 3;
                    doc.splice_text(&t, at, 10, "").unwrap();
                    doc.commit();
                }
            }
        }
        other => panic!("unknown kind {other}"),
    }
    doc.commit();
    doc
}

const KINDS: [(&str, usize); 17] = [
    ("text", 200_000),
    ("typed", 3_000),
    ("deleted", 50_000),
    ("prepend", 40_000),
    ("prepend_maps", 20_000),
    ("keys", 40_000),
    ("overwrite", 40_000),
    ("toggle", 20_000),
    ("changes", 5_000),
    ("actors", 1_000),
    ("conflicts", 1_000),
    ("actor_changes", 400),
    ("marks", 6_000),
    ("bytes", 1_000_000),
    ("long_keys", 2_000),
    ("long_actors", 5_000),
    ("text", 20),
];

fn limit_error(result: Result<impl Sized, Error>) -> Option<budget::LimitError> {
    match result {
        Err(Error::LoadLimit(err)) => Some(*err),
        _ => None,
    }
}

/// `normalize` with the limit at the input's estimate (and at the
/// normalized document's, if that is larger) accepts `input` (or
/// Automerge rejects it) with a peak below the estimate; one byte less
/// rejects it before loading, with a peak that is a small fraction of
/// the estimate. Returns the estimate.
fn check_normalize(name: &str, input: &[u8], loads: bool) -> u64 {
    check_normalize_as(name, input, Some(loads)).0
}

/// [`check_normalize`], with whether Automerge accepts the input asserted
/// only when `loads` says. Returns the estimate and the peak's ratio to
/// the limit the input was accepted at.
fn check_normalize_as(name: &str, input: &[u8], loads: Option<bool>) -> (u64, f64) {
    let scanned = scan_input_exact(input, None);
    assert!(!scanned.malformed, "{name}: scan says malformed");
    let estimate = scanned.load_estimate();
    // The cheap scan's bound on Gmax is never below the exact value.
    assert!(
        scan_input(input, None).load_estimate() >= estimate,
        "{name}"
    );
    // The document it normalizes to may be priced higher (preds become
    // successors): allow for that, as normalize checks it.
    // (For input whose normalized save does not load again, the save of
    // what it loads to: normalize prices that before it checks the save.)
    set_limit(Some(None));
    let stored_estimate = normalize(input)
        .ok()
        .or_else(|| {
            // Unguarded: Automerge panics on some crafted input.
            std::panic::catch_unwind(|| Automerge::load(input).map(|d| d.save_nocompress()))
                .ok()?
                .ok()
        })
        .map_or(0, |s| doc_estimate(&scan_doc_exact(&s)));
    let limit = estimate.max(stored_estimate);

    set_limit(Some(Some(limit)));
    let start = std::time::Instant::now();
    let (result, peak) = peak_of(|| normalize(input));
    let elapsed = start.elapsed().as_secs_f64();
    // Time follows the estimate (docs/src/pages/design/resource-limits.mdx:
    // about 8 s per GB at most); only meaningful in a release build
    // (`cargo test --release`),
    // the dev build does not optimize this crate.
    let per_gb = elapsed / (limit as f64 / f64::from(1u32 << 30));
    if !cfg!(debug_assertions) && !NO_TIME_BOUND.with(std::cell::Cell::get) {
        assert!(
            elapsed <= 8.0 * limit as f64 / f64::from(1u32 << 30) + 0.25,
            "{name}: {elapsed:.2} s for an estimate of {limit} ({per_gb:.1} s per GB)"
        );
    }
    assert!(
        limit_error(result.clone()).is_none(),
        "{name}: rejected at its own estimate: {result:?}"
    );
    if let Some(loads) = loads {
        assert_eq!(result.is_ok(), loads, "{name}: {result:?}");
    }
    let ratio = peak as f64 / limit as f64;
    assert!(
        peak <= limit,
        "{name}: normalize peak {peak} above the estimate {limit} ({:.2})",
        peak as f64 / limit as f64
    );
    eprintln!(
        "{name}: {} bytes, estimate {limit}, peak {peak} ({ratio:.2}), {}, {elapsed:.3} s ({per_gb:.2} s per GB)",
        input.len(),
        if result.is_ok() {
            "loads"
        } else {
            "refused by Automerge"
        }
    );

    // At the input's own estimate, when the document it normalizes to is
    // priced higher: loading and saving it stay below the input's
    // estimate, and the result is refused as the normalized document.
    if stored_estimate > estimate {
        set_limit(Some(Some(estimate)));
        let (result, peak) = peak_of(|| normalize(input));
        assert!(
            limit_error(result.clone()).is_none_or(|e| e.kind == LimitKind::Normalized),
            "{name}: rejected at its own estimate: {result:?}"
        );
        assert!(
            peak <= estimate,
            "{name}: normalize peak {peak} above the input's estimate {estimate} ({:.2})",
            peak as f64 / estimate as f64
        );
    }

    set_limit(Some(Some(estimate - 1)));
    let (result, peak) = peak_of(|| normalize(input));
    let err =
        limit_error(result).unwrap_or_else(|| panic!("{name}: not rejected below its estimate"));
    assert_eq!(err.kind, LimitKind::Input);
    assert!(
        peak <= small(&scanned, estimate, input.len()),
        "{name}: rejected, but with a peak of {peak} (estimate {estimate})"
    );
    set_limit(None);
    (estimate, ratio)
}

/// What a rejection may cost: the scan's own inflation (with slack for
/// vector growth), the distinct actor ids of change chunks it keeps (at
/// most the input's bytes, each charged 10 in the estimate) and a little
/// more, never close to the estimate.
fn small(scanned: &InputCounts, estimate: u64, input_len: usize) -> u64 {
    let inflated = scanned.parse_estimate();
    (3 * inflated + input_len as u64 + (256 << 10))
        .min(estimate / 3)
        .max(64 << 10)
}

thread_local! {
    /// [`check_normalize`] does not hold this thread's loads to the time
    /// bound ([`without_time_bound`]).
    static NO_TIME_BOUND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with its loads exempt from the time bound of
/// [`check_normalize`] (still measured and printed): for the inputs whose
/// time is known not to follow the estimate
/// (docs/src/pages/design/resource-limits.mdx, "What the limit cannot do":
/// overlapping marks applied as changes take time
/// quadratic in their number).
fn without_time_bound<T>(f: impl FnOnce() -> T) -> T {
    NO_TIME_BOUND.with(|c| c.set(true));
    let result = f();
    NO_TIME_BOUND.with(|c| c.set(false));
    result
}

#[test]
fn generated_saves_and_changes_stay_within_the_estimate() {
    for (kind, n) in KINDS {
        let mut doc = generated(kind, n);
        let plain = doc.document().save_nocompress();
        let compressed = doc.save();
        let chunks = doc.document().save_after(&[]);
        check_normalize(&format!("{kind} {n} save"), &plain, true);
        check_normalize(&format!("{kind} {n} compressed"), &compressed, true);
        check_normalize(&format!("{kind} {n} chunks"), &chunks, true);
    }
}

#[test]
fn crafted_inputs_are_priced_before_they_load() {
    // Loads fine.
    // Loads, but lists no heads: rejected by Automerge's check of them.
    check_normalize("crafted list 200k", &craft::list(200_000), false);
    check_normalize("crafted actors 1000", &craft::actors(1_000), false);
    check_normalize("crafted change 100k ops", &craft::change_ops(100_000), true);
    check_normalize(
        "crafted change 1M preds",
        &craft::change_preds(1_000_000),
        true,
    );
    // Rejected by Automerge after reconstruction ("mismatching heads"):
    // the memory is spent before that, so it must be priced too.
    check_normalize(
        "crafted empty changes 20k",
        &craft::empty_changes(20_000),
        false,
    );
    check_normalize("crafted deps 1000x1000", &craft::deps(1_000), false);
    check_normalize("crafted succ 200k", &craft::succ(200_000), false);
    // Length-prefixed header lists, which Automerge parses into vectors
    // entry by entry, duplicates included: other actors that are empty
    // (one byte each) or all the same, and dependencies (unknown: the
    // change is left missing its dependencies). Counts just past a power
    // of two, where a doubling vector overshoots most.
    for n in [1_100_000, 2_200_000] {
        check_normalize(
            &format!("crafted {n} empty other actors"),
            &craft::listing(0, n, 0),
            false,
        );
        check_normalize(
            &format!("crafted {n} empty other actors, compressed"),
            &craft::compressed_listing(0, n, 0),
            false,
        );
    }
    check_normalize(
        "crafted 1.1M duplicate other actors",
        &craft::listing(0, 1_100_000, 16),
        false,
    );
    check_normalize(
        "crafted 300k duplicate deps",
        &craft::listing(300_000, 0, 0),
        false,
    );
    check_normalize(
        "crafted 1.1M duplicate deps, compressed",
        &craft::compressed_listing(1_100_000, 0, 0),
        false,
    );
    check_normalize(
        "crafted 300k deps and 1.1M other actors",
        &craft::listing(300_000, 1_100_000, 0),
        false,
    );
    // Trailing change chunks after a save: many changes, each a chunk.
    let mut doc = generated("changes", 100);
    let base = doc.save();
    let heads = doc.get_heads();
    for i in 0..2_000 {
        doc.put(ROOT, "t", i as i64).unwrap();
        doc.commit();
    }
    let trailing = [base.as_slice(), &doc.document().save_after(&heads)].concat();
    check_normalize("save + 2000 trailing change chunks", &trailing, true);
}

/// `merge_changes` of `input` into a small stored document, with the
/// limit at the stored document's estimate plus the input's (loaded on
/// its own or applied, whichever is larger): not refused before loading,
/// and the peak stays below that limit. Returns the peak's ratio.
fn check_apply(name: &str, input: &[u8]) -> f64 {
    // Another actor's document (the crafted changes are actor 0's).
    let (base, _) = concurrent("text", 100, 9);
    check_apply_onto(name, &base, input)
}

/// [`check_apply`] onto the stored document `base`.
fn check_apply_onto(name: &str, base: &[u8], input: &[u8]) -> f64 {
    let doc_a = doc_estimate(&scan_doc_exact(base));
    let scanned = scan_input_exact(input, None);
    let input_estimate = scanned
        .load_estimate()
        .max(scanned.apply_estimate(Base::from(&scan_doc(base))));
    let limit = doc_a + input_estimate;
    set_limit(Some(Some(limit)));
    let (result, peak) = peak_of(|| loaded::merge_changes(Input::Stored(base), input));
    set_limit(None);
    assert!(
        limit_error(result.as_ref().map(|_| ()).map_err(Clone::clone))
            .is_none_or(|e| e.kind == LimitKind::Merged),
        "{name}: merge_changes refused before loading: {:?}",
        result.err()
    );
    let ratio = peak as f64 / limit as f64;
    assert!(
        peak <= limit,
        "{name}: merge_changes peak {peak} above {limit} ({ratio:.2})"
    );
    eprintln!("{name} merge_changes: estimate {limit}, peak {peak} ({ratio:.2})");
    ratio
}

/// `input` after another actor's save: a document chunk that is not the
/// first of an input is rebuilt into changes and applied (unless the
/// document has its heads: the crafted ones list none, so they are
/// rebuilt this way only with a made-up head, [`craft::with_head`]).
fn after_save(input: &[u8]) -> Vec<u8> {
    let (save, _) = concurrent("text", 100, 9);
    [save.as_slice(), input].concat()
}

/// A crafted document chunk (which lists no heads, so that a load after
/// another document, or `merge_changes`, would skip it as already
/// contained) with a made-up head: after another save and through
/// `merge_changes`, where Automerge rebuilds its changes and applies them
/// (the made-up head is not checked: they load).
fn check_rebuilt(name: &str, crafted: &[u8]) {
    check_rebuilt_as(name, crafted, Some(true));
}

/// [`check_rebuilt`], with whether the load after another save succeeds
/// asserted only when `loads` says.
fn check_rebuilt_as(name: &str, crafted: &[u8], loads: Option<bool>) {
    let input = craft::with_head(crafted);
    check_apply(&format!("{name}, one head"), &input);
    check_normalize_as(
        &format!("save + {name}, one head"),
        &after_save(&input),
        loads,
    );
}

/// A save written by Automerge ([`written`]) on its own and after another
/// save (where it is rebuilt and applied), through `normalize` and
/// `merge_changes`.
fn check_written(kind: &str, name: &str) {
    let input = written(kind);
    check_normalize(&format!("written: {name}"), &input, true);
    check_apply(&format!("written: {name}"), &input);
    let input = after_save(&input);
    check_normalize(&format!("save + written: {name}"), &input, true);
    check_apply(&format!("save + written: {name}"), &input);
}

/// A compressed save written by Automerge of 2,000 one-op changes that
/// share a 100 kB string: every change's message (`"message"`, one repeat
/// run of the message column), their actor id (`"actor"`), or the key
/// every change puts (`"key"`, one repeat run of the key column), or the
/// name of the mark every change makes on a 2,000-character text
/// (`"mark"`, one repeat run of the mark name column). Loads fine, and
/// its heads are real.
fn written(kind: &str) -> Vec<u8> {
    let big = "x".repeat(100_000);
    let mut doc = match kind {
        "actor" => AutoCommit::new().with_actor(ActorId::from(vec![7u8; 100_000])),
        _ => AutoCommit::new().with_actor(actor(3)),
    };
    let text = (kind == "mark").then(|| {
        let t = doc.put_object(ROOT, "t", ObjType::Text).unwrap();
        doc.splice_text(&t, 0, 0, &letters(2_000, 13)).unwrap();
        doc.commit();
        t
    });
    for i in 0..2_000i64 {
        if let Some(t) = &text {
            let at = i as usize;
            doc.mark(t, Mark::new(big.clone(), i, at, at + 1), ExpandMark::None)
                .unwrap();
        } else {
            let key = if kind == "key" { big.as_str() } else { "k" };
            doc.put(ROOT, key, i).unwrap();
        }
        let mut options = CommitOptions::default();
        if kind == "message" {
            options.set_message(big.clone());
        }
        doc.commit_with(options);
    }
    doc.save()
}

/// A document's change messages, one repeat run of a string: Automerge
/// rebuilds every change with its own copy of the message (in the
/// change's bytes and as a `String`), and a load whose heads do not match
/// clones every rebuilt change into its error.
#[test]
fn repeated_messages_are_priced() {
    for (name, input) in [
        (
            "doc 2000 changes, one 100 kB message",
            craft::repeated_messages(2_000, 100_000, false),
        ),
        (
            "doc 2000 changes, one 100 kB message, deflated",
            craft::repeated_messages(2_000, 100_000, true),
        ),
        (
            "doc 3 changes, one 30 MB message",
            craft::repeated_messages(3, 30_000_000, false),
        ),
    ] {
        check_normalize(name, &input, false);
        check_apply(name, &input);
        check_rebuilt(name, &input);
    }
    check_written("message", "2000 changes, one 100 kB message");
}

/// Keys and mark names repeated by a run: applying a change makes an
/// owned string of every op's key and mark name, the document holds a key
/// literally where other keys come between its rows, and every rebuilt
/// change of a document holds its ops' keys.
#[test]
fn repeated_keys_and_mark_names_are_priced() {
    for (name, input, loads) in [
        (
            "change 2000 ops, one 100 kB key",
            craft::repeated_keys(2_000, 100_000),
            true,
        ),
        // Loads, but its save does not load again (Automerge's own
        // mark-order check): refused by the verification of the result.
        (
            "change 2000 marks, one 100 kB name",
            craft::repeated_mark_names(2_000, 100_000),
            false,
        ),
        (
            "change 500 maps, one 50 kB key interleaved",
            craft::interleaved_keys(500, 50_000),
            false,
        ),
        (
            "doc 2000 changes, one 100 kB key",
            craft::repeated_doc_keys(2_000, 100_000),
            false,
        ),
    ] {
        check_normalize(name, &input, loads);
        check_apply(name, &input);
    }
    check_rebuilt(
        "doc 2000 changes, one 100 kB key",
        &craft::repeated_doc_keys(2_000, 100_000),
    );
    check_written("key", "2000 changes, one 100 kB key");
    check_written("mark", "2000 changes, one 100 kB mark name");
}

/// Actor ids longer than the 16 bytes an `ActorId` holds inline: every
/// rebuilt change of a document holds its own actor and the other actors
/// its ops refer to, in its bytes and as `ActorId`s (and a load whose
/// heads do not match clones them all into its error).
#[test]
fn long_actor_ids_are_priced() {
    for (name, input) in [
        (
            "doc 2000 changes by a 100 kB actor",
            craft::long_actor_changes(2_000, 100_000),
        ),
        (
            "doc 2000 changes referring to a 100 kB actor",
            craft::long_actor_refs(2_000, 100_000),
        ),
        (
            "doc 20000 changes by a 17-byte actor",
            craft::long_actor_changes(20_000, 17),
        ),
    ] {
        check_normalize(name, &input, false);
        check_apply(name, &input);
        check_rebuilt(name, &input);
    }
    check_written("actor", "2000 changes by a 100 kB actor");
}

/// Column metadata entries: Automerge keeps every entry of a metadata
/// block (and copies the list as it validates the layout), while an
/// empty column takes two input bytes. Counts just past a power of two,
/// where a doubling vector overshoots most. (None loads: the document
/// lists no heads, and Automerge reads a change's object actors from one
/// of the empty columns, so its save does not load again.)
#[test]
fn column_metadata_entries_are_priced() {
    for n in [2_100_000u64, 4_200_000] {
        for (name, input, loads) in [
            (
                format!("doc {n} empty columns"),
                craft::doc_columns(n),
                false,
            ),
            (
                format!("change {n} empty columns"),
                craft::change_columns(n),
                false,
            ),
            (
                format!("change {n} empty columns, compressed"),
                craft::compressed_change_columns(n),
                false,
            ),
        ] {
            check_normalize(&name, &input, loads);
            check_apply(&name, &input);
        }
    }
}

/// 12,000,000 empty column metadata entries in a 24 kB compressed change
/// chunk: 2.4 GB at 200 bytes per entry, over the default limit. The scan
/// stops at the declared count, after the inflation (24 MB), without
/// stepping through the entries; the uncompressed chunk (24 MB) and a
/// document chunk too.
#[test]
fn column_metadata_bombs_are_rejected_before_they_are_walked() {
    set_limit(Some(Some(budget::DEFAULT_LIMIT)));
    for (name, input) in [
        ("compressed", craft::compressed_change_columns(12_000_000)),
        ("plain", craft::change_columns(12_000_000)),
        ("document", craft::doc_columns(12_000_000)),
    ] {
        let start = std::time::Instant::now();
        let (result, peak) = peak_of(|| normalize(&input));
        let err = limit_error(result).unwrap_or_else(|| panic!("{name}: not rejected"));
        assert!(err.at_least, "{name}: the scan stopped early");
        assert!(peak < 3 * (24 << 20), "{name}: peak {peak}");
        assert!(
            start.elapsed().as_secs() < 2,
            "{name}: took {:?}",
            start.elapsed()
        );
        let base = Automerge::new().save_nocompress();
        let (result, peak) = peak_of(|| loaded::merge_changes(Input::Stored(&base), &input));
        assert!(limit_error(result).is_some(), "{name}: merge_changes");
        assert!(peak < 3 * (24 << 20), "{name}: merge_changes peak {peak}");
    }
    assert!(craft::compressed_change_columns(12_000_000).len() < 30_000);
    set_limit(None);
}

#[test]
fn rle_bombs_are_rejected_with_a_tiny_peak() {
    // Tiny inputs describing hundreds of millions of rows: with the
    // default limit they are rejected before anything large is allocated.
    set_limit(Some(Some(budget::DEFAULT_LIMIT)));
    for (name, input) in [
        ("list 10^8", craft::list(100_000_000)),
        ("empty changes 10^8", craft::empty_changes(100_000_000)),
        ("deps 1000x10^6", craft::deps(1_000_000)),
        ("succ 10^8", craft::succ(100_000_000)),
        ("change 10^8 ops", craft::change_ops(100_000_000)),
        ("change 10^9 preds", craft::change_preds(1_000_000_000)),
        ("2^62 nulls", craft::list(1 << 62)),
    ] {
        assert!(input.len() < 256, "{name}: {} bytes", input.len());
        let (result, peak) = peak_of(|| normalize(&input));
        let err = limit_error(result).unwrap_or_else(|| panic!("{name}: not rejected"));
        assert!(!err.at_least, "{name}");
        assert!(peak < 64 << 10, "{name}: peak {peak}");
        // Applied to a document: the crafted document chunks list no
        // heads, so a load of `a ++ input` would not reconstruct them
        // (nothing new, nothing loaded); the change chunks are priced.
        let base = Automerge::new().save_nocompress();
        let (result, peak) = peak_of(|| loaded::merge_changes(Input::Stored(&base), &input));
        assert!(
            matches!(result, Ok(None)) || limit_error(result).is_some(),
            "{name}: merge_changes"
        );
        assert!(peak < 64 << 10, "{name}: merge_changes peak {peak}");
        let (result, peak) = peak_of(|| loaded::contains_changes(Input::Stored(&base), &input));
        assert!(
            matches!(result, Ok(true)) || limit_error(result).is_some(),
            "{name}: contains_changes"
        );
        assert!(peak < 64 << 10, "{name}: contains_changes peak {peak}");
    }
    set_limit(None);
}

#[test]
fn header_list_bombs_are_rejected_before_they_are_walked() {
    // 20,000,000 empty other actors in a 19 kB compressed change chunk:
    // 2.2 GB at 110 bytes per entry, over the default limit. The scan
    // stops at the declared count, after the inflation (20 MB), without
    // stepping through the entries; the uncompressed chunk too.
    set_limit(Some(Some(budget::DEFAULT_LIMIT)));
    for (name, input) in [
        ("compressed", craft::compressed_listing(0, 20_000_000, 0)),
        ("plain", craft::listing(0, 20_000_000, 0)),
    ] {
        let start = std::time::Instant::now();
        let (result, peak) = peak_of(|| normalize(&input));
        let err = limit_error(result).unwrap_or_else(|| panic!("{name}: not rejected"));
        assert!(err.at_least, "{name}: the scan stopped early");
        assert!(peak < 3 * (20 << 20), "{name}: peak {peak}");
        assert!(
            start.elapsed().as_secs() < 2,
            "{name}: took {:?}",
            start.elapsed()
        );
        let base = Automerge::new().save_nocompress();
        let (result, peak) = peak_of(|| loaded::merge_changes(Input::Stored(&base), &input));
        assert!(limit_error(result).is_some(), "{name}: merge_changes");
        assert!(peak < 3 * (20 << 20), "{name}: merge_changes peak {peak}");
        let result = loaded::contains_changes(Input::Stored(&base), &input);
        assert!(limit_error(result).is_some(), "{name}: contains_changes");
    }
    assert!(craft::compressed_listing(0, 20_000_000, 0).len() < 20_000);
    set_limit(None);
}

#[test]
fn deflate_bombs_are_rejected_quickly() {
    // 256 MB of zeros deflated to a few hundred kB; with a 100 MB limit
    // the scan stops after inflating a tenth of it.
    let limit = 100 << 20;
    for (name, input) in [
        ("deflated column", craft::deflated_column(256 << 20)),
        ("compressed change chunk", craft::compressed_bomb(256 << 20)),
    ] {
        assert!(input.len() < 1 << 20, "{name}: {} bytes", input.len());
        set_limit(Some(Some(limit)));
        let start = std::time::Instant::now();
        let (result, peak) = peak_of(|| normalize(&input));
        let err = limit_error(result).unwrap_or_else(|| panic!("{name}: not rejected"));
        assert!(err.at_least, "{name}: the scan stopped early");
        assert!(peak < limit / 4, "{name}: peak {peak}");
        assert!(
            start.elapsed().as_secs() < 5,
            "{name}: took {:?}",
            start.elapsed()
        );
        let base = Automerge::new().save_nocompress();
        let (result, peak) = peak_of(|| loaded::merge_changes(Input::Stored(&base), &input));
        assert!(limit_error(result).is_some(), "{name}: merge_changes");
        assert!(peak < limit / 4, "{name}: merge_changes peak {peak}");
    }
    set_limit(None);
}

#[test]
fn text_input_length_is_checked_before_decoding() {
    set_limit(Some(Some(1 << 20)));
    assert!(budget::check_input_len(100_000).is_ok());
    let err = limit_error(budget::check_input_len(200_000)).unwrap();
    assert!(err.at_least);
    assert!(err.message().contains("(1 MB)"), "{}", err.message());
    set_limit(Some(None));
    assert!(budget::check_input_len(usize::MAX).is_ok());
    set_limit(None);
}

/// A stored document of `kind` with `n` items by `a`, plus its bytes.
fn stored(kind: &str, n: usize, a: u64) -> Vec<u8> {
    let mut doc = generated(kind, n).with_actor(actor(a));
    // Make it a different history from other actors' documents.
    doc.put(ROOT, format!("by{a}"), a as i64).unwrap();
    doc.commit();
    doc.document().save_nocompress()
}

/// A document concurrent with `stored(kind, n, 0)`: a fresh one by `a`.
fn concurrent(kind: &str, n: usize, a: u64) -> (Vec<u8>, Automerge) {
    let mut doc = AutoCommit::new().with_actor(actor(a));
    let t = doc
        .put_object(ROOT, format!("t{a}"), ObjType::Text)
        .unwrap();
    doc.splice_text(&t, 0, 0, &letters(n, a)).unwrap();
    doc.commit();
    let _ = kind;
    (doc.document().save_nocompress(), doc.document().clone())
}

#[test]
fn merges_price_the_changes_they_apply() {
    for (kind, n) in [("text", 100_000), ("prepend", 20_000), ("changes", 2_000)] {
        let a = stored(kind, n, 0);
        let (b, b_doc) = concurrent(kind, n, 9);
        let name = format!("merge {kind} {n}");
        let doc_a = doc_estimate(&scan_doc_exact(&a));
        let doc_b = doc_estimate(&scan_doc_exact(&b));
        let added = scan_input(&b_doc.save_after(&[]), None).changes;
        let apply = budget::changes_estimate(&added, Base::from(&scan_doc(&a)));

        // Within the limit: the peak of loading both, applying and
        // counting stays below the sum of the estimates.
        set_limit(Some(Some(doc_a + doc_b + apply)));
        let (result, peak) = peak_of(|| loaded::merge(Input::Stored(&a), Input::Stored(&b)));
        let merged = match result {
            Ok(MergeOutcome::New(doc)) => doc,
            other => panic!("{name}: {:?}", other.err()),
        };
        let bound = doc_a + doc_b + apply;
        assert!(peak <= bound, "{name}: peak {peak} above {bound}");
        eprintln!(
            "{name}: peak {peak} of {bound} ({:.2})",
            peak as f64 / bound as f64
        );
        let merged_bytes = merged.stored().unwrap().to_vec();

        // The accumulator, both inputs stored.
        let (result, peak) = peak_of(|| {
            let mut acc = MergeAccumulator::new();
            acc.add_input(Input::Stored(&a))?;
            acc.add_input(Input::Stored(&b))?;
            Ok::<_, Error>(acc)
        });
        result.unwrap();
        assert!(
            peak <= bound,
            "{name} accumulator: peak {peak} above {bound}"
        );

        // merge_changes with the other document's changes as chunks.
        let chunks = b_doc.save_after(&[]);
        let scanned = scan_input_exact(&chunks, None);
        let apply_input = scanned.apply_estimate(Base::from(&scan_doc(&a)));
        set_limit(Some(Some(doc_a + apply_input)));
        let (result, peak) = peak_of(|| loaded::merge_changes(Input::Stored(&a), &chunks));
        result.unwrap().unwrap();
        assert!(
            peak <= doc_a + apply_input,
            "{name} merge_changes: peak {peak} above {}",
            doc_a + apply_input
        );

        // Below the apply estimate: rejected before applying anything,
        // with less than the loads alone would allow.
        set_limit(Some(Some(apply - 1)));
        let (result, peak) = peak_of(|| loaded::merge(Input::Stored(&a), Input::Stored(&b)));
        let err = limit_error(result).unwrap_or_else(|| panic!("{name}: not rejected"));
        assert_eq!(err.kind, LimitKind::Apply, "{name}");
        assert!(peak <= doc_a + doc_b, "{name}: rejected with peak {peak}");
        let result = {
            let mut acc = MergeAccumulator::new();
            acc.add_input(Input::Stored(&a))
                .and_then(|()| acc.add_input(Input::Stored(&b)))
        };
        assert_eq!(
            limit_error(result).map(|e| e.kind),
            Some(LimitKind::Apply),
            "{name}"
        );
        set_limit(Some(Some(apply_input - 1)));
        let result = loaded::merge_changes(Input::Stored(&a), &chunks);
        assert_eq!(
            limit_error(result).map(|e| e.kind),
            Some(LimitKind::Input),
            "{name}"
        );
        let result = loaded::contains_changes(Input::Stored(&a), &chunks);
        // Decided by the chunks' dependencies, or priced before the load.
        assert!(
            matches!(result, Ok(false) | Err(Error::LoadLimit(_))),
            "{name}: {result:?}"
        );

        // Loading the merged result is never checked.
        set_limit(Some(Some(1)));
        assert!(LoadedDoc::from_stored(&merged_bytes).is_ok());
        set_limit(None);
    }
}

#[test]
fn merge_results_that_outgrow_the_limit_are_rejected() {
    // A document just under the limit, and small changes on top: each
    // write alone is cheap, the result is not.
    let mut doc = generated("keys", 20_000).with_actor(actor(1));
    let a = doc.document().save_nocompress();
    let heads = doc.get_heads();
    for i in 0..500 {
        doc.put(ROOT, format!("new{i}"), i as i64).unwrap();
    }
    doc.commit();
    let change = doc.document().save_after(&heads);
    let b = doc.document().save_nocompress();
    let limit = doc_estimate(&scan_doc_exact(&a)) + 1_000;
    assert!(doc_estimate(&scan_doc_exact(&b)) > limit);
    set_limit(Some(Some(limit)));

    let err = limit_error(loaded::merge_changes(Input::Stored(&a), &change)).unwrap();
    assert_eq!(err.kind, LimitKind::Merged);
    assert!(
        err.message()
            .starts_with("estimated memory to load merged automerge document exceeds")
    );
    // A newer save is loaded on its own: its own estimate is over.
    let err = limit_error(loaded::merge_changes(Input::Stored(&a), &b)).unwrap();
    assert_eq!(err.kind, LimitKind::Input);
    // In memory (a PL/pgSQL chain): the same, and the document is left
    // as it was.
    let loaded_a = LoadedDoc::from_stored(&a).unwrap();
    let err = limit_error(loaded::merge_changes(Input::Loaded(&loaded_a), &change)).unwrap();
    assert_eq!(err.kind, LimitKind::Merged);
    // merge of two documents, and the accumulator.
    let c = {
        let mut other = Automerge::load(&a).unwrap();
        other.load_incremental(&change).unwrap();
        other.save_nocompress()
    };
    let d = {
        let mut other = AutoCommit::load(&a).unwrap().with_actor(actor(2));
        other.put(ROOT, "other", 1i64).unwrap();
        other.commit();
        other.document().save_nocompress()
    };
    let err = limit_error(loaded::merge(Input::Stored(&c), Input::Stored(&d)).map(|_| ())).unwrap();
    assert_eq!(err.kind, LimitKind::Merged);
    let mut acc = MergeAccumulator::new();
    let result = acc
        .add_input(Input::Stored(&d))
        .and_then(|()| acc.add_input(Input::Stored(&c)));
    assert_eq!(limit_error(result).map(|e| e.kind), Some(LimitKind::Merged));

    // With room for the result, all of them pass, and a chain of small
    // merges in memory saves only when the upper bound says it must.
    set_limit(Some(Some(doc_estimate(&scan_doc_exact(&b)) * 2)));
    let merged = loaded::merge_changes(Input::Loaded(&loaded_a), &change)
        .unwrap()
        .unwrap();
    assert_eq!(merged.heads(), sorted_heads(&b));
    assert!(loaded::merge(Input::Stored(&c), Input::Stored(&d)).is_ok());
    set_limit(Some(None));
    assert!(
        loaded::merge_changes(Input::Stored(&a), &change)
            .unwrap()
            .is_some()
    );
    set_limit(None);
}

/// `merge_changes` of a compressed save with no more changes than the
/// stored `a` loads `a` first, before the save is priced (to answer "`a`
/// has it" as a load of `a ++ save` would). The columns the scan inflated
/// are not held through that load: when the save is then rejected, the
/// peak is the load of `a`, not that plus the inflated save.
#[test]
fn a_save_rejected_after_loading_a_does_not_hold_its_columns() {
    const SIZE: usize = 4 << 20;
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "small", 1i64).unwrap();
    doc.commit();
    let bytes: Vec<u8> = (0..SIZE).map(|i| (i * 7919 % 251) as u8).collect();
    doc.put(ROOT, "big", ScalarValue::Bytes(bytes)).unwrap();
    doc.commit();
    let a = doc.document().save_nocompress();
    let mut other = AutoCommit::new().with_actor(actor(2));
    other
        .put(ROOT, "big", ScalarValue::Bytes(vec![7; SIZE]))
        .unwrap();
    other.commit();
    let input = other.save();
    assert!(input.len() < SIZE / 100, "compresses well");

    set_limit(Some(None));
    let (loaded_a, load_a) = peak_of(|| LoadedDoc::from_stored(&a).unwrap());
    drop(loaded_a);
    let estimate = scan_input(&input, None).load_estimate();
    set_limit(Some(Some(estimate - 1)));
    let loads = pg_automerge_core::test_hooks::loads();
    let (result, peak) = peak_of(|| loaded::merge_changes(Input::Stored(&a), &input));
    set_limit(None);
    assert_eq!(limit_error(result).map(|e| e.kind), Some(LimitKind::Input));
    // Rejected after loading `a` (step 2), before loading the save.
    assert_eq!(pg_automerge_core::test_hooks::loads() - loads, 1);
    assert!(
        peak < load_a + (SIZE as u64) / 4,
        "peak {peak}, load of a {load_a}, inflated save {SIZE}"
    );
}

fn sorted_heads(bytes: &[u8]) -> Vec<automerge::ChangeHash> {
    let mut heads = Automerge::load(bytes).unwrap().get_heads();
    heads.sort();
    heads
}

#[test]
fn scanned_counts_are_automerges() {
    for (kind, n) in KINDS {
        let mut doc = generated(kind, n.min(20_000));
        for bytes in [doc.document().save_nocompress(), doc.save()] {
            let counts = scan_input(&bytes, None);
            assert!(counts.single_doc_parses, "{kind}");
            let stats = Automerge::load(&bytes).unwrap().stats();
            let d = counts.first_doc;
            assert_eq!(d.ops, stats.num_ops, "{kind} ops");
            assert_eq!(d.changes, stats.num_changes, "{kind} changes");
            assert_eq!(d.actors, stats.num_actors, "{kind} actors");
            // The same counts from the save and the compressed save.
            let mut plain = scan_doc(&doc.document().save_nocompress());
            plain.inflated = d.inflated;
            assert_eq!(plain, d, "{kind}");
        }
        let chunks = doc.document().save_after(&[]);
        let counts = scan_input(&chunks, None);
        let changes = doc.document().get_changes(&[]);
        assert_eq!(counts.changes.changes, changes.len() as u64, "{kind}");
        assert_eq!(
            counts.changes.ops,
            changes.iter().map(|c| c.len() as u64).sum::<u64>(),
            "{kind}"
        );
        assert_eq!(
            counts.changes.deps,
            changes.iter().map(|c| c.deps().len() as u64).sum::<u64>()
        );
    }
}

#[test]
fn gmax_run_by_run_is_the_row_by_row_definition() {
    let mut inputs: Vec<Vec<u8>> = Vec::new();
    for (kind, n) in [
        ("overwrite", 3_000),
        ("toggle", 2_000),
        ("deleted", 3_000),
        ("keys", 2_000),
        ("prepend", 2_000),
        ("prepend_maps", 1_000),
        ("conflicts", 300),
        ("marks", 2_000),
        ("typed", 500),
        ("actor_changes", 60),
    ] {
        inputs.push(generated(kind, n).document().save_nocompress());
    }
    let mut rng = Rng(11);
    for seed in 0..30 {
        let mut doc = AutoCommit::new().with_actor(actor(seed));
        let list = doc.put_object(ROOT, "l", ObjType::List).unwrap();
        let text = doc.put_object(ROOT, "t", ObjType::Text).unwrap();
        let mut forks = Vec::new();
        for step in 0..300i64 {
            match rng.below(9) {
                0 => doc.put(ROOT, format!("k{}", rng.below(5)), step).unwrap(),
                1 => {
                    let _ = doc.delete(ROOT, format!("k{}", rng.below(5)));
                }
                2 => {
                    let len = doc.length(&list);
                    doc.insert(&list, rng.below(len as u64 + 1) as usize, step)
                        .unwrap();
                }
                3 => {
                    let len = doc.length(&list);
                    if len > 0 {
                        let at = rng.below(len as u64) as usize;
                        if rng.below(2) == 0 {
                            doc.delete(&list, at).unwrap();
                        } else {
                            doc.put(&list, at, -step).unwrap();
                        }
                    }
                }
                4 => {
                    let len = doc.length(&text);
                    let at = rng.below(len as u64 + 1) as usize;
                    let del = rng.below(3).min((len - at) as u64) as isize;
                    doc.splice_text(&text, at, del, "ab").unwrap();
                }
                5 => doc.commit_with(Default::default()).map_or((), |_| ()),
                6 => forks.push(doc.fork().with_actor(actor(100 + step as u64))),
                7 => {
                    if let Some(mut fork) = forks.pop() {
                        fork.put(ROOT, format!("k{}", rng.below(5)), -1).unwrap();
                        doc.merge(&mut fork).unwrap();
                    }
                }
                _ => {
                    doc.increment(ROOT, "c", 1)
                        .or_else(|_| doc.put(ROOT, "c", ScalarValue::counter(0)))
                        .unwrap();
                }
            }
        }
        doc.commit();
        inputs.push(doc.document().save_nocompress());
    }
    inputs.push(craft::succ(1_000));
    inputs.push(craft::list(1_000));
    for (i, bytes) in inputs.iter().enumerate() {
        let by_runs = scan_doc_exact(bytes).gmax;
        // The cheap scan takes the bound: every successor entry.
        assert!(scan_doc(bytes).gmax >= by_runs, "input {i}");
        let by_rows = budget::gmax_by_rows(bytes).unwrap();
        assert_eq!(by_runs, by_rows, "input {i}");
    }
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n.max(1)
    }
}

#[test]
fn bundles_are_refused_whatever_the_limit() {
    let bundle = craft::chunk(3, &[0, 0, 0]);
    let base = Automerge::new().save_nocompress();
    for limit in [Some(None), Some(Some(budget::DEFAULT_LIMIT))] {
        set_limit(limit);
        let expected = Err(Error::Unsupported(
            "automerge bundle chunks are not supported".into(),
        ));
        assert_eq!(normalize(&bundle), expected);
        let saved = [base.as_slice(), &bundle].concat();
        assert_eq!(normalize(&saved), expected);
        assert!(matches!(
            loaded::merge_changes(Input::Stored(&base), &bundle),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            loaded::contains_changes(Input::Stored(&base), &bundle),
            Err(Error::Unsupported(_))
        ));
    }
    set_limit(None);
}

#[test]
fn limit_errors_read_well() {
    set_limit(Some(Some(1 << 20)));
    let err = limit_error(normalize(&craft::list(100_000))).unwrap();
    assert_eq!(
        err.message(),
        "estimated memory to load automerge input exceeds \"pg_automerge.max_load_memory\" (1 MB)"
    );
    let detail = err.detail();
    assert!(
        detail.starts_with("Loading it could take up to ")
            && detail.contains(" MB (100001 operations, 1 change, 1 actor, ")
            && detail.ends_with(" bytes uncompressed)."),
        "{detail}"
    );
    assert_eq!(
        budget::LimitError::hint(),
        "A superuser can raise \"pg_automerge.max_load_memory\"."
    );
    let e = Error::LoadLimit(Box::new(err));
    assert_eq!(e.hint(), Some(budget::LimitError::hint()));
    assert!(e.detail().is_some());
    // A limit that is not a whole number of MB is shown in kB.
    set_limit(Some(Some(1536 << 10)));
    let err = limit_error(normalize(&craft::list(100_000))).unwrap();
    assert!(err.message().ends_with("(1536 kB)"), "{}", err.message());
    set_limit(None);
}

#[test]
fn a_lowered_limit_leaves_stored_values_and_no_op_writes_alone() {
    let mut doc = generated("keys", 20_000);
    let heads = doc.get_heads();
    doc.put(ROOT, "last", 1i64).unwrap();
    doc.commit();
    let stored = doc.document().save_nocompress();
    let older = doc.document().fork_at(&heads).unwrap().save_nocompress();
    let latest = doc.document().save_after(&heads);
    // Far below what anything costs.
    set_limit(Some(Some(1)));
    // Reads of stored values are never checked.
    let loaded = LoadedDoc::from_stored(&stored).unwrap();
    assert!(loaded::with_doc(Input::Stored(&stored), |d| Ok(d.length(ROOT))).unwrap() > 0);
    // Merges that add nothing load nothing new: re-sent uncompressed
    // saves (parsed in place), older saves, re-sent change chunks, the
    // same or an older document.
    for input in [Input::Stored(&stored), Input::Loaded(&loaded)] {
        for bytes in [&stored, &older, &latest] {
            assert!(matches!(loaded::merge_changes(input, bytes), Ok(None)));
            assert_eq!(loaded::contains_changes(input, bytes), Ok(true));
        }
        assert!(matches!(
            loaded::merge(input, Input::Stored(&older)),
            Ok(MergeOutcome::Left)
        ));
    }
    let mut acc = MergeAccumulator::new();
    acc.add_input(Input::Stored(&stored)).unwrap();
    acc.add_input(Input::Stored(&older)).unwrap();
    acc.add_input(Input::Loaded(&loaded)).unwrap();
    // New input is refused.
    assert!(limit_error(normalize(&stored)).is_some());
    set_limit(None);
}

// ---------------------------------------------------------------------------
// Neighbouring shapes of the string and actor terms
// (docs/src/pages/design/benchmarks/2026-10-01.mdx, "The estimate's
// neighbourhood"): every string-valued column as one repeat run,
// many distinct strings and runs interleaved with literals, plain and
// compressed, as a document chunk, a change chunk and after another save;
// long actor ids from every actor column; each also through
// `merge_changes` onto a stored document.
// ---------------------------------------------------------------------------

use craft::Strings;

/// How many strings, and how long, for a shape of a string column that
/// is run-length encoded (messages, keys, mark names): a repeat run is
/// priced by what it expands to, so it can be long; distinct strings are
/// input bytes; interleaved ones are both.
fn rle_size(shape: Strings) -> (u64, usize) {
    match shape {
        Strings::Run => (2_000, 100_000),
        Strings::Distinct => (2_000, 2_000),
        Strings::Interleaved => (2_000, 20_000),
    }
}

/// The same for a raw column (values, extra bytes), which holds every
/// value's bytes whatever its metadata's runs say.
const RAW_SIZE: (u64, usize) = (2_000, 2_000);

/// A crafted document chunk (no heads: Automerge refuses it after
/// rebuilding its changes) on its own, through `merge_changes`, and with a
/// made-up head after another save and through `merge_changes`.
fn check_doc_chunk(name: &str, input: &[u8]) {
    check_normalize_as(name, input, Some(false));
    check_apply(name, input);
    check_rebuilt_as(name, input, None);
}

/// A change chunk (or chunks) on its own, through `merge_changes`, and
/// after another save (a save and its trailing changes), also through
/// `merge_changes`.
fn check_change_chunks(name: &str, input: &[u8]) {
    check_normalize_as(name, input, None);
    check_apply(name, input);
    let trailing = after_save(input);
    check_normalize_as(&format!("save + {name}"), &trailing, None);
    check_apply(&format!("save + {name}"), &trailing);
}

/// A document chunk of every string-valued column, in every shape, plain
/// and with its columns deflated.
fn doc_strings(column: &str) {
    for shape in Strings::ALL {
        for deflate in [false, true] {
            let (n, len) = match column {
                "values" | "extra bytes" => RAW_SIZE,
                _ => rle_size(shape),
            };
            let input = match column {
                "messages" => craft::doc_messages(shape, n, len, deflate),
                "keys" => craft::doc_keys(shape, n, len, deflate),
                "mark names" => craft::doc_mark_names(shape, n, len, deflate),
                "values" => craft::doc_values(shape, n, len, deflate),
                "extra bytes" => craft::doc_extra(shape, n, len, deflate),
                other => panic!("{other}"),
            };
            let name = format!(
                "doc {column} {shape:?} {n} x {len}{}",
                if deflate { ", deflated" } else { "" }
            );
            if column == "mark names" {
                without_time_bound(|| check_doc_chunk(&name, &input));
            } else {
                check_doc_chunk(&name, &input);
            }
        }
    }
}

#[test]
fn doc_chunk_messages_in_every_shape_are_priced() {
    doc_strings("messages");
}

#[test]
fn doc_chunk_keys_in_every_shape_are_priced() {
    doc_strings("keys");
}

#[test]
fn doc_chunk_mark_names_in_every_shape_are_priced() {
    doc_strings("mark names");
}

/// A document chunk's repeated mark names are rebuilt bytes, priced like
/// messages (3.0 bytes per byte measured: at the 3 of keys, 2,000 changes
/// sharing one 100 kB name peaked at 0.996 of the estimate); repeated keys
/// stay rebuilt key bytes.
#[test]
fn repeated_mark_names_are_rebuilt_bytes_and_keys_rebuilt_key_bytes() {
    for deflate in [false, true] {
        let marks = scan_doc(&craft::doc_mark_names(Strings::Run, 10, 100, deflate));
        assert_eq!((marks.rebuilt, marks.rebuilt_keys), (9 * 100, 0));
        let keys = scan_doc(&craft::doc_keys(Strings::Run, 10, 100, deflate));
        assert_eq!((keys.rebuilt, keys.rebuilt_keys), (0, 9 * 100));
        let messages = scan_doc(&craft::doc_messages(Strings::Run, 10, 100, deflate));
        assert_eq!((messages.rebuilt, messages.rebuilt_keys), (9 * 100, 0));
        // Interleaved: runs of nine (eight copies beyond the one held).
        let marks = scan_doc(&craft::doc_mark_names(
            Strings::Interleaved,
            20,
            100,
            deflate,
        ));
        assert_eq!((marks.rebuilt, marks.rebuilt_keys), (2 * 8 * 100, 0));
    }
}

#[test]
fn doc_chunk_values_and_extra_bytes_in_every_shape_are_priced() {
    doc_strings("values");
    doc_strings("extra bytes");
}

/// A change chunk of every string-valued op column, in every shape,
/// plain and compressed.
#[test]
fn change_chunk_strings_in_every_shape_are_priced() {
    for shape in Strings::ALL {
        for compressed in [false, true] {
            for column in ["keys", "mark names", "values"] {
                let (n, len) = if column == "values" {
                    RAW_SIZE
                } else {
                    rle_size(shape)
                };
                let cols = match column {
                    "keys" => craft::change_key_ops(shape, n, len),
                    "mark names" => craft::change_mark_ops(shape, n, len),
                    _ => craft::change_value_ops(shape, n, len),
                };
                let name = format!(
                    "change {column} {shape:?} {n} x {len}{}",
                    if compressed { ", compressed" } else { "" }
                );
                let input = craft::change_of(&cols, compressed);
                if column == "mark names" {
                    without_time_bound(|| check_change_chunks(&name, &input));
                } else {
                    check_change_chunks(&name, &input);
                }
            }
        }
    }
}

/// Change messages and extra bytes of change chunks, which each chunk
/// holds literally: one large, and a chain of 2,000 chunks with the same,
/// different or interleaved strings.
#[test]
fn change_chunk_messages_and_extra_bytes_are_priced() {
    for compressed in [false, true] {
        let suffix = if compressed { ", compressed" } else { "" };
        let big = "m".repeat(30_000_000);
        check_change_chunks(
            &format!("change one 30 MB message{suffix}"),
            &craft::change_chain(1, |_| big.clone(), |_| Vec::new(), compressed),
        );
        check_change_chunks(
            &format!("change 30 MB of extra bytes{suffix}"),
            &craft::change_chain(1, |_| String::new(), |_| vec![7; 30_000_000], compressed),
        );
        for shape in Strings::ALL {
            let (n, len) = RAW_SIZE;
            let strings = craft::string_values(shape, n, len);
            check_change_chunks(
                &format!("{n} changes, messages {shape:?} x {len}{suffix}"),
                &craft::change_chain(
                    n,
                    |i| strings[i as usize].clone(),
                    |_| Vec::new(),
                    compressed,
                ),
            );
            check_change_chunks(
                &format!("{n} changes, extra bytes {shape:?} x {len}{suffix}"),
                &craft::change_chain(
                    n,
                    |_| String::new(),
                    |i| strings[i as usize].clone().into_bytes(),
                    compressed,
                ),
            );
        }
    }
}

/// The lengths of the long actor ids: just over what an `ActorId` holds
/// inline, 1 kB and 100 kB; and how many changes or ops refer to them.
const ACTOR_LENS: [(usize, u64); 3] = [(17, 20_000), (1_000, 20_000), (100_000, 2_000)];

/// Document chunks with a long actor id in every actor column: the change
/// actor, the op id actor, the object and key actors, the pred a rebuilt
/// op holds (the successor column holding the short actor) and the
/// successor actor, and the actor table alone.
#[test]
fn long_actor_ids_in_every_doc_column_are_priced() {
    for (len, n) in ACTOR_LENS {
        let m = (20_000_000 / len as u64).min(1_000);
        for (column, input) in [
            ("change actor", craft::long_actor_changes(n, len)),
            ("op id actor", craft::long_actor_ops(n, len)),
            ("obj actor", craft::long_actor_refs(n, len)),
            ("key actor", craft::long_actor_keys(n, len)),
            (
                "pred (succ by a short actor)",
                craft::long_actor_succ(n, len, true),
            ),
            ("succ actor", craft::long_actor_succ(n, len, false)),
            ("actor table only", craft::long_actor_table(m, len)),
        ] {
            let name = format!("doc {len}-byte actor, {column}, {n}");
            check_doc_chunk(&name, &input);
        }
    }
}

/// Change chunks with a long actor id as the change's actor (on their
/// own, after another save and through `merge_changes`), and as an other
/// actor their ops refer to by object, key or pred, or that nothing
/// refers to (many of them): after the long actor's document, and
/// through `merge_changes` into it.
#[test]
fn long_actor_ids_in_every_change_column_are_priced() {
    for (len, n) in ACTOR_LENS {
        let m = (20_000_000 / len as u64).min(1_000);
        for compressed in [false, true] {
            let suffix = if compressed { ", compressed" } else { "" };
            check_change_chunks(
                &format!("change {len}-byte actor, change actor, {n}{suffix}"),
                &craft::long_actor_change(n, len, compressed),
            );
            for (column, count) in [("obj", n), ("key", n), ("pred", n), ("others", m)] {
                let name = format!("change {len}-byte other actor, {column}, {count}{suffix}");
                let (base, change) = craft::long_other_actor(column, count, len, compressed);
                let input = [base.as_slice(), &change].concat();
                // Other actors that no op refers to make Automerge panic as
                // it applies the change (refused as malformed).
                let loads = column != "others";
                check_normalize_as(&format!("save + {name}"), &input, Some(loads));
                check_apply_onto(&name, &base, &change);
            }
        }
    }
}

/// A document written by Automerge where 2,000 commits each add one
/// string to a column: the root key they put (`"key"`), the string value
/// they put at root key "k" (`"value"`), their message (`"message"`), or
/// the name of a mark on a 2,000-character text (`"mark"`); the strings
/// as `shape` lays them out (Automerge writes runs where they repeat).
fn written_strings(column: &str, shape: Strings, len: usize) -> AutoCommit {
    let n = 2_000;
    let strings = craft::string_values(shape, n, len);
    let mut doc = AutoCommit::new().with_actor(actor(3));
    let text = (column == "mark").then(|| {
        let t = doc.put_object(ROOT, "t", ObjType::Text).unwrap();
        doc.splice_text(&t, 0, 0, &letters(n as usize, 13)).unwrap();
        doc.commit();
        t
    });
    for (i, s) in strings.iter().enumerate() {
        match column {
            "key" => doc.put(ROOT, s.as_str(), i as i64).unwrap(),
            "value" => doc.put(ROOT, "k", s.as_str()).unwrap(),
            "message" => doc.put(ROOT, "k", i as i64).unwrap(),
            "mark" => {
                let t = text.as_ref().unwrap();
                doc.mark(
                    t,
                    Mark::new(s.clone(), i as i64, i, i + 1),
                    ExpandMark::None,
                )
                .unwrap();
            }
            other => panic!("{other}"),
        }
        let mut options = CommitOptions::default();
        if column == "message" {
            options.set_message(s.clone());
        }
        doc.commit_with(options);
    }
    doc
}

/// A written document as a save (plain and compressed), as change chunks
/// (`save_after`), and after another save, through `normalize` and
/// `merge_changes`.
fn check_written_doc(name: &str, doc: &mut AutoCommit) {
    let plain = doc.document().save_nocompress();
    let compressed = doc.save();
    let chunks = doc.document().save_after(&[]);
    for (form, input) in [
        ("save", plain),
        ("compressed", compressed),
        ("chunks", chunks),
    ] {
        let name = format!("written: {name}, {form}");
        check_normalize_as(&name, &input, Some(true));
        check_apply(&name, &input);
        let trailing = after_save(&input);
        check_normalize_as(&format!("save + {name}"), &trailing, Some(true));
        check_apply(&format!("save + {name}"), &trailing);
    }
}

/// Documents written by Automerge with every string-valued column in
/// every shape.
#[test]
fn written_strings_in_every_shape_are_priced() {
    for shape in Strings::ALL {
        for column in ["key", "value", "message", "mark"] {
            let (_, len) = if column == "value" {
                RAW_SIZE
            } else {
                rle_size(shape)
            };
            let mut doc = written_strings(column, shape, len);
            check_written_doc(&format!("{column} {shape:?} x {len}"), &mut doc);
        }
    }
}

/// A document written by Automerge by two actors with `len`-byte ids
/// taking turns, `rounds` commits: the first makes a map and a text, then
/// each commit overwrites a key of the map the other one put last (pred
/// and successor actors), and inserts into the text after the other's
/// characters while deleting one of them (key and successor actors); the
/// objects are the first actor's (object actor).
fn written_actors(len: usize, rounds: usize) -> AutoCommit {
    let id = |b: u8| {
        let mut v = vec![b; len];
        v[0] = b;
        ActorId::from(v)
    };
    let (a, b) = (id(0xa0), id(0xb0));
    let mut doc = AutoCommit::new().with_actor(a.clone());
    let m = doc.put_object(ROOT, "m", ObjType::Map).unwrap();
    let t = doc.put_object(ROOT, "t", ObjType::Text).unwrap();
    doc.splice_text(&t, 0, 0, "abcdef").unwrap();
    doc.commit();
    for i in 0..rounds {
        doc.set_actor(if i % 2 == 0 { b.clone() } else { a.clone() });
        doc.put(&m, format!("k{}", i % 10), i as i64).unwrap();
        let len = doc.length(&t);
        let at = (i * 7) % (len - 1);
        doc.splice_text(&t, at, 1, "xy").unwrap();
        doc.commit();
    }
    doc
}

/// Documents written by Automerge whose actors have long ids, in every
/// role.
#[test]
fn written_long_actor_ids_are_priced() {
    for (len, rounds) in [(17, 5_000), (1_000, 5_000), (100_000, 1_000)] {
        let mut doc = written_actors(len, rounds);
        check_written_doc(&format!("{len}-byte actors, {rounds} commits"), &mut doc);
    }
}

/// The cost model's constants are measured costs of this Automerge (and
/// of hexane, its column store, which Automerge's requirement does not
/// pin): another version can allocate more for the same input, and an
/// allocation beyond the estimate aborts the backend. Fails on any other
/// version, until the model has been re-measured and
/// `budget::MEASURED_AUTOMERGE` / `MEASURED_HEXANE` updated.
#[test]
fn the_cost_model_was_measured_for_this_automerge() {
    let lock = include_str!("../../../Cargo.lock");
    let version = |name: &str| {
        let header = format!("name = \"{name}\"\nversion = \"");
        let at = lock
            .find(&header)
            .unwrap_or_else(|| panic!("{name} is not in Cargo.lock"))
            + header.len();
        lock[at..].split('"').next().unwrap().to_string()
    };
    for (name, measured) in [
        ("automerge", budget::MEASURED_AUTOMERGE),
        ("hexane", budget::MEASURED_HEXANE),
    ] {
        let locked = version(name);
        assert_eq!(
            locked,
            measured,
            "{name} is {locked}, but the load memory cost model \
             (pg_automerge_core::budget) was measured for {name} {measured}: \
             re-measure the cost model (cargo test --release -p pg_automerge_core \
             --test memory_bounds -- --nocapture, and mise run fuzz; revisit the \
             constants in budget.rs and docs/src/pages/design/resource-limits.mdx, \"The estimate\"), then set \
             budget::MEASURED_{} to {locked}",
            name.to_uppercase()
        );
    }
}
