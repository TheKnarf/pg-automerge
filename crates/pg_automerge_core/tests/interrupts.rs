//! The interrupt check registered with `set_interrupt_check` runs inside
//! the long loops, and what it raises is not swallowed by the guard that
//! turns Automerge's panics into errors. (Its own test binary: the check
//! is registered once per process.)

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind, panic_any};

use automerge::transaction::Transactable;
use automerge::{AutoCommit, ObjType, ROOT};
use pg_automerge_core::loaded::{self, Input};
use pg_automerge_core::{Error, TICK_EVERY, history, json, set_interrupt_check};

/// What the registered check does on this thread.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Count,
    /// Raise like a Postgres ERROR does under pgrx (a non-string payload).
    RaiseError,
    /// Panic like a Rust bug would (a string payload).
    PanicMessage,
}

thread_local! {
    static MODE: Cell<Mode> = const { Cell::new(Mode::Count) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

/// Stands for pgrx's `CaughtError` payload.
struct PgError;

fn check() {
    CALLS.with(|c| c.set(c.get() + 1));
    match MODE.with(Cell::get) {
        Mode::Count => {}
        Mode::RaiseError => panic_any(PgError),
        Mode::PanicMessage => panic!("bug in the check"),
    }
}

fn big_doc(items: usize) -> Vec<u8> {
    let mut doc = AutoCommit::new();
    let list = doc.put_object(ROOT, "items", ObjType::List).unwrap();
    for i in 0..items {
        doc.insert(&list, i, i as i64).unwrap();
        if i % 100 == 99 {
            doc.commit();
        }
    }
    doc.commit();
    doc.document().save_nocompress()
}

fn to_json(stored: &[u8]) -> Result<serde_json::Value, Error> {
    loaded::with_doc(Input::Stored(stored), json::doc_to_json)
}

#[test]
fn long_loops_check_for_interrupts_and_errors_pass_the_guard() {
    set_interrupt_check(check);
    let n = 5 * TICK_EVERY as usize;
    let stored = big_doc(n);

    MODE.with(|m| m.set(Mode::Count));
    CALLS.with(|c| c.set(0));
    to_json(&stored).unwrap();
    let calls = CALLS.with(Cell::get);
    assert!(calls >= 5, "{calls} interrupt checks for {n} list items");

    // History rows too (the document has ~n/100 changes: fewer than a
    // tick, so this uses a history with many changes).
    let mut doc = AutoCommit::new();
    for i in 0..2 * TICK_EVERY {
        doc.put(ROOT, "k", i64::from(i)).unwrap();
        doc.commit();
    }
    let many = doc.document().save_nocompress();
    CALLS.with(|c| c.set(0));
    history::changes_meta(Input::Stored(&many), &[]).unwrap();
    assert!(CALLS.with(Cell::get) >= 2);

    // A Postgres ERROR raised by the check unwinds through the guard as is.
    MODE.with(|m| m.set(Mode::RaiseError));
    let err = catch_unwind(AssertUnwindSafe(|| to_json(&stored))).unwrap_err();
    assert!(err.downcast_ref::<PgError>().is_some(), "payload replaced");
    let err = catch_unwind(AssertUnwindSafe(|| {
        history::changes_meta(Input::Stored(&many), &[])
    }))
    .unwrap_err();
    assert!(err.downcast_ref::<PgError>().is_some());

    // A panic with a message is still turned into an error (stored value:
    // internal).
    MODE.with(|m| m.set(Mode::PanicMessage));
    let result = catch_unwind(AssertUnwindSafe(|| to_json(&stored))).expect("caught by guard");
    assert!(matches!(result, Err(Error::Internal(m)) if m.contains("bug in the check")));

    MODE.with(|m| m.set(Mode::Count));
    to_json(&stored).unwrap();
}
