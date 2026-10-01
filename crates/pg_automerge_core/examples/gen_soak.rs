//! Prints the fixtures of tests/soak.sh (the soak test's load harness) as
//! SQL with COPY data: 20 template documents ("slots") of mixed shapes
//! and sizes, and for each template 3 writer "lanes", each a chain of
//! single-change edits by its own actor, as a backend persisting edit by
//! edit sends them.
//!
//! - `soak_template (slot, class, base)`: the base document, a compressed
//!   save (`Automerge.save()`). Row `id` of the soak table is a copy of
//!   template `id % 20`.
//! - `soak_change (slot, lane, pos, changes)`: change `pos` (1-based) of
//!   the lane as `save_after(heads before it)`; change `pos` depends on
//!   change `pos - 1` of the same lane (and the first on the base), so a
//!   lane's changes apply in order, and the lanes of one row are
//!   concurrent with each other (a row has at most 3 heads).
//! - `soak_save (slot, lane, pos, save)`: every CHECKPOINT-th position, the
//!   compressed full save of the base plus the lane's changes up to `pos`
//!   (the upserts of full saves).
//!
//! Classes and sizes (DOC_KB, the first argument, is the size of the
//! largest documents in kB, default 1000; the medium ones are a tenth of
//! it, the small ones a few kB whatever DOC_KB):
//!
//! | slots | class      | shape                                                      |
//! |-------|------------|------------------------------------------------------------|
//! | 0-9   | `board`    | a map with a list of 20-92 small maps, a counter, a status |
//! | 10-13 | `note`     | a rich text of 2-8 k characters: paragraphs, headings, marks |
//! | 14-16 | `list`     | a list of small maps, DOC_KB/20, /10 and /5                 |
//! | 17    | `longnote` | a rich text of about DOC_KB/10                              |
//! | 18    | `biglist`  | a list of small maps of about DOC_KB                       |
//! | 19    | `bigtext`  | a rich text of about DOC_KB                                |
//!
//! Lane edits: list documents set the status (`open`/`review`/`closed`,
//! which the soak's GIN queries look for), toggle, insert, retitle and
//! delete items and increment a counter; texts insert and delete words,
//! add bold and link marks, split paragraphs (blocks as editors store
//! them) and set the status. Every change has a message and a time.
//!
//! Arguments: DOC_KB (default 1000), LANE_CHANGES (default 250),
//! CHECKPOINT (default 25). The byte sizes of the templates go to stderr.
//! Deterministic: the same arguments print the same bytes.

use automerge::marks::{ExpandMark, Mark};
use automerge::transaction::{CommitOptions, Transactable};
use automerge::{ActorId, AutoCommit, ObjId, ObjType, ROOT, ReadDoc, ScalarValue};
use pg_automerge_core::encoding::to_hex_literal;
use std::io::{BufWriter, Write};

const SLOTS: usize = 20;
const LANES: usize = 3;
/// Bytes per item of a list document's uncompressed save (measured).
const ITEM_BYTES: usize = 44;

const WORDS: &[&str] = &[
    "the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "shopping", "list", "meeting",
    "notes", "project", "status", "review", "draft", "budget", "plan", "garden", "recipe",
    "travel", "tickets", "morning", "evening", "weekend", "library", "coffee", "bread", "cheese",
    "milk", "apples", "report", "summary", "question", "answer", "idea", "later", "today",
];
const STATUSES: &[&str] = &["open", "review", "closed"];

/// xorshift64: small, deterministic, good enough for picking edits.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn word(&mut self) -> &'static str {
        WORDS[self.below(WORDS.len())]
    }
    fn words(&mut self, n: usize) -> String {
        (0..n).map(|_| self.word()).collect::<Vec<_>>().join(" ")
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    List,
    Text,
}

fn class_of(slot: usize) -> (&'static str, Shape) {
    match slot {
        0..=9 => ("board", Shape::List),
        10..=13 => ("note", Shape::Text),
        14..=16 => ("list", Shape::List),
        17 => ("longnote", Shape::Text),
        18 => ("biglist", Shape::List),
        _ => ("bigtext", Shape::Text),
    }
}

/// Items of a list template, characters of a text template.
fn size_of(slot: usize, doc_kb: usize) -> usize {
    let big = doc_kb * 1024;
    match slot {
        0..=9 => 20 + slot * 8,
        10..=13 => 2000 * (slot - 9),
        14 => big / 20 / ITEM_BYTES,
        15 => big / 10 / ITEM_BYTES,
        16 => big / 5 / ITEM_BYTES,
        17 => big / 10,
        18 => big / ITEM_BYTES,
        _ => big,
    }
}

fn commit(doc: &mut AutoCommit, message: String, time: i64) {
    doc.commit_with(
        CommitOptions::default()
            .with_message(message)
            .with_time(time),
    );
}

fn put_item(doc: &mut AutoCommit, items: &ObjId, index: usize, id: i64, rng: &mut Rng) {
    let m = doc.insert_object(items, index, ObjType::Map).unwrap();
    doc.put(&m, "id", id).unwrap();
    doc.put(&m, "title", rng.words(3)).unwrap();
    doc.put(&m, "done", rng.below(3) == 0).unwrap();
}

fn base_list(slot: usize, n: usize, rng: &mut Rng) -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([slot as u8 + 1; 16]));
    doc.put(ROOT, "title", format!("{} {slot}", rng.words(2)))
        .unwrap();
    doc.put(ROOT, "status", STATUSES[slot % 3]).unwrap();
    doc.put(ROOT, "slot", slot as i64).unwrap();
    doc.put(ROOT, "votes", ScalarValue::counter(0)).unwrap();
    let tags = doc.put_object(ROOT, "tags", ObjType::List).unwrap();
    for i in 0..3 {
        doc.insert(&tags, i, rng.word()).unwrap();
    }
    let items = doc.put_object(ROOT, "items", ObjType::List).unwrap();
    for i in 0..n {
        put_item(&mut doc, &items, i, i as i64, rng);
        if i % 500 == 499 {
            commit(&mut doc, format!("import {i}"), 1_700_000_000 + i as i64);
        }
    }
    commit(&mut doc, "created".into(), 1_700_000_000);
    doc
}

fn new_block(doc: &mut AutoCommit, text: &ObjId, at: usize, heading: bool) {
    let b = doc.split_block(text, at).unwrap();
    doc.put(&b, "type", if heading { "heading" } else { "paragraph" })
        .unwrap();
    doc.put_object(&b, "parents", ObjType::List).unwrap();
    let attrs = doc.put_object(&b, "attrs", ObjType::Map).unwrap();
    if heading {
        doc.put(&attrs, "level", 2).unwrap();
    }
    doc.put(&b, "isEmbed", false).unwrap();
}

fn base_text(slot: usize, chars: usize, rng: &mut Rng) -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([slot as u8 + 1; 16]));
    doc.put(ROOT, "title", format!("{} {slot}", rng.words(2)))
        .unwrap();
    doc.put(ROOT, "status", STATUSES[slot % 3]).unwrap();
    doc.put(ROOT, "slot", slot as i64).unwrap();
    let text = doc.put_object(ROOT, "body", ObjType::Text).unwrap();
    // Paragraphs of about 400 characters, built front to back, a heading
    // every tenth, some words bold.
    let mut len = 0;
    let mut para = 0;
    while len < chars {
        new_block(&mut doc, &text, len, para % 10 == 0);
        len += 1;
        let body = rng.words(60);
        let body = &body[..body.len().min(chars.saturating_sub(len).max(1))];
        doc.splice_text(&text, len, 0, body).unwrap();
        if para % 3 == 1 && body.len() > 20 {
            doc.mark(
                &text,
                Mark::new("bold".into(), true, len + 5, len + 15),
                ExpandMark::After,
            )
            .unwrap();
        }
        len += body.len();
        para += 1;
        if para % 200 == 199 {
            commit(
                &mut doc,
                format!("typing {para}"),
                1_700_000_000 + para as i64,
            );
        }
    }
    commit(&mut doc, "created".into(), 1_700_000_000);
    doc
}

/// Sets ROOT "status" to the next of [`STATUSES`]: a put of the value a
/// key already has is not an operation, and the lane edits must each make
/// exactly one change.
fn next_status(doc: &mut AutoCommit) {
    let current = match doc.get(ROOT, "status").unwrap() {
        Some((v, _)) => v.as_str().unwrap_or_default().to_owned(),
        None => String::new(),
    };
    let i = STATUSES
        .iter()
        .position(|s| *s == current)
        .map_or(0, |i| i + 1);
    doc.put(ROOT, "status", STATUSES[i % STATUSES.len()])
        .unwrap();
}

/// One lane edit of a list document: change `k` (1-based).
fn edit_list(doc: &mut AutoCommit, k: usize, base_len: usize, rng: &mut Rng) {
    let items = match doc.get(ROOT, "items").unwrap() {
        Some((_, id)) => id,
        None => unreachable!("items list"),
    };
    let len = doc.length(&items);
    match k % 5 {
        0 => next_status(doc),
        1 => {
            let m = match doc.get(&items, rng.below(len)).unwrap() {
                Some((_, id)) => id,
                None => unreachable!("item"),
            };
            let done = matches!(
                doc.get(&m, "done").unwrap(),
                Some((automerge::Value::Scalar(v), _)) if v.as_ref() == &ScalarValue::Boolean(true)
            );
            doc.put(&m, "done", !done).unwrap();
        }
        2 => put_item(doc, &items, rng.below(len + 1), 1_000_000 + k as i64, rng),
        3 => doc.increment(ROOT, "votes", 1).unwrap(),
        _ => {
            if len > base_len {
                doc.delete(&items, rng.below(len)).unwrap();
            } else {
                let m = match doc.get(&items, rng.below(len)).unwrap() {
                    Some((_, id)) => id,
                    None => unreachable!("item"),
                };
                doc.put(&m, "title", format!("{} {k}", rng.words(3)))
                    .unwrap();
            }
        }
    }
}

/// One lane edit of a text document: change `k` (1-based).
fn edit_text(doc: &mut AutoCommit, k: usize, rng: &mut Rng) {
    let text = match doc.get(ROOT, "body").unwrap() {
        Some((_, id)) => id,
        None => unreachable!("body text"),
    };
    let len = doc.length(&text);
    // Positions after the first block marker.
    let at = 1 + rng.below(len.saturating_sub(1));
    match k % 5 {
        0 | 1 => {
            let n = 1 + rng.below(4);
            let words = format!(" {}", rng.words(n));
            doc.splice_text(&text, at, 0, &words).unwrap();
        }
        2 => {
            let end = (at + 5 + rng.below(20)).min(len);
            if end <= at {
                doc.splice_text(&text, at, 0, " marked").unwrap();
            } else {
                let mark = if rng.below(2) == 0 {
                    Mark::new("bold".into(), true, at, end)
                } else {
                    Mark::new("link".into(), format!("https://example.com/{k}"), at, end)
                };
                doc.mark(&text, mark, ExpandMark::None).unwrap();
            }
        }
        3 => new_block(doc, &text, at, rng.below(10) == 0),
        _ => {
            let del = (3 + rng.below(8)).min(len - at);
            doc.splice_text(&text, at, del as isize, "").unwrap();
            next_status(doc);
        }
    }
}

fn copy_row(out: &mut impl Write, fields: &[String], bytes: &[u8]) {
    // COPY text format: a backslash in the data is written twice.
    let hex = to_hex_literal(bytes);
    writeln!(out, "{}\t\\{hex}", fields.join("\t")).unwrap();
}

fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .map(|a| {
            a.parse()
                .expect("numeric arguments: DOC_KB LANE_CHANGES CHECKPOINT")
        })
        .collect();
    let doc_kb = args.first().copied().unwrap_or(1000).max(10);
    let lane_changes = args.get(1).copied().unwrap_or(250).max(1);
    let checkpoint = args.get(2).copied().unwrap_or(25).max(1);

    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    writeln!(
        out,
        "CREATE TABLE soak_template (slot int PRIMARY KEY, class text NOT NULL, base bytea NOT NULL);\n\
         CREATE TABLE soak_change (slot int, lane int, pos int, changes bytea NOT NULL, PRIMARY KEY (slot, lane, pos));\n\
         CREATE TABLE soak_save (slot int, lane int, pos int, save bytea NOT NULL, PRIMARY KEY (slot, lane, pos));"
    )
    .unwrap();

    let mut templates = Vec::new();
    let mut changes = Vec::new();
    let mut saves = Vec::new();
    for slot in 0..SLOTS {
        let (class, shape) = class_of(slot);
        let size = size_of(slot, doc_kb);
        let mut rng = Rng::new(slot as u64 + 1);
        let mut base = match shape {
            Shape::List => base_list(slot, size, &mut rng),
            Shape::Text => base_text(slot, size, &mut rng),
        };
        let base_save = base.save();
        eprintln!(
            "slot {slot:2} {class:8} {:>9} bytes uncompressed, {:>8} compressed, {} changes",
            base.save_nocompress().len(),
            base_save.len(),
            base.get_changes(&[]).len()
        );
        for lane in 0..LANES {
            let mut rng = Rng::new(((slot as u64) << 8) + lane as u64 + 1000);
            let mut w = base.fork().with_actor(ActorId::from([
                0x80 | lane as u8,
                slot as u8,
                0x5a,
                0xa5,
                1,
                2,
                3,
                4,
                5,
                6,
                7,
                8,
                9,
                10,
                11,
                12,
            ]));
            for k in 1..=lane_changes {
                let heads = w.get_heads();
                match shape {
                    Shape::List => edit_list(&mut w, k, size, &mut rng),
                    Shape::Text => edit_text(&mut w, k, &mut rng),
                }
                commit(
                    &mut w,
                    format!("lane {lane} edit {k}"),
                    1_750_000_000 + (k * 60) as i64,
                );
                // tests/soak.sh counts on one change per edit.
                let change = w.save_after(&heads);
                assert_eq!(
                    w.get_changes(&heads).len(),
                    1,
                    "slot {slot} lane {lane} edit {k} made no change"
                );
                copy_row(
                    &mut changes,
                    &[slot.to_string(), lane.to_string(), k.to_string()],
                    &change,
                );
                if k % checkpoint == 0 || k == lane_changes {
                    copy_row(
                        &mut saves,
                        &[slot.to_string(), lane.to_string(), k.to_string()],
                        &w.save(),
                    );
                }
            }
        }
        copy_row(
            &mut templates,
            &[slot.to_string(), class.to_string()],
            &base_save,
        );
    }
    for (table, rows) in [
        ("soak_template", templates),
        ("soak_change", changes),
        ("soak_save", saves),
    ] {
        writeln!(out, "COPY {table} FROM stdin;").unwrap();
        out.write_all(&rows).unwrap();
        writeln!(out, "\\.").unwrap();
    }
    out.flush().unwrap();
}
