//! `stats`: the per-process counters behind `automerge_memory_usage()`.
//!
//! One test function: the counters are per process, and this binary's
//! tests would otherwise run in parallel threads and see each other's
//! documents.

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ROOT};
use pg_automerge_core::loaded::{self, Input, LoadedDoc};
use pg_automerge_core::stats::{self, Snapshot};
use pg_automerge_core::{MergeAccumulator, normalize};

fn save(actor: u8, key: &str) -> Vec<u8> {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([actor; 16]));
    doc.put(ROOT, key, i64::from(actor)).unwrap();
    doc.save_nocompress()
}

#[test]
fn live_documents_and_loads() {
    let start = stats::snapshot();
    let a = save(1, "a");
    let b = save(2, "b");

    // A loaded document is live until dropped; its load is counted.
    let doc = LoadedDoc::from_stored(&a).unwrap();
    let s = stats::snapshot();
    assert_eq!(s.live_documents, start.live_documents + 1);
    assert_eq!(s.loads, start.loads + 1);
    drop(doc);
    assert_eq!(stats::snapshot().live_documents, start.live_documents);

    // A read loads without keeping a document.
    let before = stats::snapshot();
    loaded::with_doc(Input::Stored(&a), |_| Ok(())).unwrap();
    let s = stats::snapshot();
    assert_eq!(s.live_documents, before.live_documents);
    assert_eq!(s.loads, before.loads + 1);

    // Input (normalize) is counted, including a failed load.
    let before = stats::snapshot();
    normalize(&a).unwrap();
    assert!(normalize(b"\x85\x6f\x4a\x83garbage").is_err());
    assert!(stats::snapshot().loads >= before.loads + 2);

    // The merge_agg state holds one document while it has loaded one; a
    // merge result is another.
    let before = stats::snapshot();
    let mut acc = MergeAccumulator::new();
    acc.add_input(Input::Stored(&a)).unwrap();
    acc.add_input(Input::Stored(&b)).unwrap();
    assert_eq!(
        stats::snapshot().live_documents,
        before.live_documents + 1,
        "the accumulator's document"
    );
    let result = acc.finish_loaded().unwrap();
    assert!(matches!(
        result,
        Some(pg_automerge_core::Accumulated::Loaded(_))
    ));
    assert_eq!(stats::snapshot().live_documents, before.live_documents + 2);
    drop(result);
    drop(acc);
    assert_eq!(stats::snapshot().live_documents, before.live_documents);

    // reset() zeroes the cumulative counters only.
    let kept = LoadedDoc::from_stored(&b).unwrap();
    stats::reset();
    let s = stats::snapshot();
    assert_eq!(
        s,
        Snapshot {
            live_documents: start.live_documents + 1,
            loads: 0,
            load_time: std::time::Duration::ZERO,
        }
    );
    drop(kept);
    assert_eq!(stats::snapshot().live_documents, start.live_documents);
}
