//! The one-sweep JSON walk (`json::write_json_at`) emits exactly the events
//! of the per-object walk (`json::write_json_per_object`, the reference),
//! for current and historical states, over generated documents and the
//! shapes where the two read Automerge differently (text with blocks,
//! marks and non-string elements; objects under deleted or overwritten
//! keys; conflicts; deep nesting). Tables (which take the per-object walk)
//! cannot be created with automerge 0.12, only loaded from old saves.

use automerge::marks::{ExpandMark, Mark};
use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, Automerge, ChangeHash, ObjType, ROOT, ScalarValue};
use pg_automerge_core::json::{self, JsonSink};

mod common;

use common::{Rng, edit, random_replicas};

/// Every event, as text.
#[derive(Default)]
struct Recorder(Vec<String>);

impl JsonSink for Recorder {
    fn begin_object(&mut self) {
        self.0.push("{".into());
    }
    fn end_object(&mut self) {
        self.0.push("}".into());
    }
    fn begin_array(&mut self, len_hint: usize) {
        self.0.push(format!("[{len_hint}"));
    }
    fn end_array(&mut self) {
        self.0.push("]".into());
    }
    fn key(&mut self, key: &str) {
        self.0.push(format!("key {key:?}"));
    }
    fn string(&mut self, value: &str) {
        self.0.push(format!("str {value:?}"));
    }
    fn int(&mut self, value: i64) {
        self.0.push(format!("int {value}"));
    }
    fn uint(&mut self, value: u64) {
        self.0.push(format!("uint {value}"));
    }
    fn float(&mut self, value: f64) {
        self.0.push(format!("float {:?}", value.to_bits()));
    }
    fn bool(&mut self, value: bool) {
        self.0.push(format!("bool {value}"));
    }
    fn null(&mut self) {
        self.0.push("null".into());
    }
}

/// One of the two walks.
type Walk =
    fn(&Automerge, Option<&[ChangeHash]>, &mut Recorder) -> Result<(), pg_automerge_core::Error>;

fn events(
    doc: &Automerge,
    heads: Option<&[ChangeHash]>,
    walk: Walk,
) -> Result<Vec<String>, pg_automerge_core::Error> {
    let mut rec = Recorder::default();
    walk(doc, heads, &mut rec).map(|()| rec.0)
}

/// Both walks agree on the current state and on the state as of every
/// change's heads. The length hint of arrays may differ for historical
/// states (the per-object walk does not compute it), so it is compared
/// only for the current state.
fn assert_same(doc: &Automerge, what: &str) {
    let strip = |events: Vec<String>| -> Vec<String> {
        events
            .into_iter()
            .map(|e| if e.starts_with('[') { "[".into() } else { e })
            .collect()
    };
    let sweep = events(doc, None, json::write_json_at);
    let reference = events(doc, None, json::write_json_per_object);
    assert_eq!(sweep, reference, "{what}: current state");
    for change in doc.get_changes(&[]) {
        let heads = [change.hash()];
        let sweep = events(doc, Some(&heads), json::write_json_at).map(strip);
        let reference = events(doc, Some(&heads), json::write_json_per_object).map(strip);
        assert_eq!(sweep, reference, "{what}: as of {}", change.hash());
    }
}

#[test]
fn generated_documents() {
    for seed in 0..80 {
        for (i, stored) in random_replicas(seed).into_iter().enumerate() {
            assert_same(
                &Automerge::load(&stored).unwrap(),
                &format!("seed {seed} replica {i}"),
            );
        }
    }
    for seed in 0..10u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut doc = AutoCommit::new().with_actor(ActorId::from([seed as u8 + 1; 16]));
        for _ in 0..400 {
            edit(&mut doc, &mut rng);
        }
        doc.commit();
        assert_same(doc.document(), &format!("long history {seed}"));
    }
}

#[test]
fn text_with_blocks_marks_and_other_elements() {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.splice_text(&text, 0, 0, "hello brave new world")
        .unwrap();
    doc.commit();
    doc.split_block(&text, 5).unwrap();
    doc.mark(
        &text,
        Mark::new("bold".into(), true, 0, 4),
        ExpandMark::Both,
    )
    .unwrap();
    doc.commit();
    doc.delete(&text, 7).unwrap();
    // Non-string elements in a text: a scalar, a nested object.
    doc.insert(&text, 2, 42i64).unwrap();
    doc.insert_object(&text, 3, ObjType::Map).unwrap();
    doc.put_object(ROOT, "empty_text", ObjType::Text).unwrap();
    let nested = doc.put_object(ROOT, "list", ObjType::List).unwrap();
    let t2 = doc.insert_object(&nested, 0, ObjType::Text).unwrap();
    doc.splice_text(&t2, 0, 0, "a\0b").unwrap();
    doc.commit();
    // Concurrent text edits.
    let mut other = doc.fork().with_actor(ActorId::from([2u8; 16]));
    other.splice_text(&text, 0, 0, "X").unwrap();
    doc.splice_text(&text, 0, 0, "Y").unwrap();
    doc.merge(&mut other).unwrap();
    assert_same(doc.document(), "text");
}

#[test]
fn unreachable_objects_conflicts_and_nesting() {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
    let gone = doc.put_object(ROOT, "gone", ObjType::Map).unwrap();
    doc.put(&gone, "x", 1i64).unwrap();
    let replaced = doc.put_object(ROOT, "replaced", ObjType::List).unwrap();
    doc.insert(&replaced, 0, "old").unwrap();
    let list = doc.put_object(ROOT, "list", ObjType::List).unwrap();
    for i in 0..5 {
        let m = doc.insert_object(&list, i, ObjType::Map).unwrap();
        doc.put(&m, "i", i as i64).unwrap();
    }
    doc.commit();
    doc.delete(ROOT, "gone").unwrap();
    doc.put(ROOT, "replaced", "scalar now").unwrap();
    doc.delete(&list, 2).unwrap();
    doc.put(ROOT, "counter", ScalarValue::counter(1)).unwrap();
    doc.commit();
    let mut other = doc.fork().with_actor(ActorId::from([2u8; 16]));
    other.put_object(ROOT, "k", ObjType::Map).unwrap();
    other.increment(ROOT, "counter", 5).unwrap();
    doc.put(ROOT, "k", 1i64).unwrap();
    doc.increment(ROOT, "counter", 2).unwrap();
    doc.merge(&mut other).unwrap();
    let mut obj = doc.put_object(ROOT, "deep", ObjType::Map).unwrap();
    for i in 0..50 {
        obj = if i % 2 == 0 {
            doc.put_object(&obj, "m", ObjType::List).unwrap()
        } else {
            doc.insert_object(&obj, 0, ObjType::Map).unwrap()
        };
    }
    doc.commit();
    assert_same(doc.document(), "unreachable/conflicts/nesting");
}

#[test]
fn nesting_limit_is_the_same() {
    for depth in [json::MAX_DEPTH - 1, json::MAX_DEPTH] {
        let mut doc = AutoCommit::new();
        let mut obj = ROOT;
        for _ in 0..depth {
            obj = doc.put_object(&obj, "k", ObjType::Map).unwrap();
        }
        let sweep = events(doc.document(), None, json::write_json_at);
        let reference = events(doc.document(), None, json::write_json_per_object);
        assert_eq!(sweep.is_ok(), reference.is_ok(), "depth {depth}");
        assert_eq!(sweep.is_ok(), depth < json::MAX_DEPTH);
        if let (Err(a), Err(b)) = (sweep, reference) {
            assert_eq!(a, b);
        }
    }
}
