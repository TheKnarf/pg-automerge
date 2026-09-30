//! Rust-level timings of the core primitives on the benchmark documents
//! (`mise run bench-core`, release build): loading, normalizing canonical
//! and compressed input, the load memory scan, saving, the JSON walk, and
//! (on documents with a text at `text`) the rich-text spans.
//!
//! Env: BENCH_DOCS (space-separated subset of "text3mb items20k items2k
//! typed5k rich20k", default all), BENCH_REPS (default 5). Prints the median in
//! milliseconds per operation and document.

use std::hint::black_box;
use std::time::Instant;

use automerge::{AutoCommit, Automerge, ObjType, ReadDoc};
use pg_automerge_core::json::{self, JsonSink};
use pg_automerge_core::loaded::{Input, LoadedDoc};
use pg_automerge_core::{budget, normalize, spans};

#[path = "shared/bench_docs.rs"]
mod bench_docs;

/// A sink that only looks at the events: the cost of the walk itself.
#[derive(Default)]
struct Discard(usize);

impl JsonSink for Discard {
    fn begin_object(&mut self) {}
    fn end_object(&mut self) {}
    fn begin_array(&mut self, _: usize) {}
    fn end_array(&mut self) {}
    fn key(&mut self, key: &str) {
        self.0 += key.len();
    }
    fn string(&mut self, value: &str) {
        self.0 += value.len();
    }
    fn int(&mut self, _: i64) {}
    fn uint(&mut self, _: u64) {}
    fn float(&mut self, _: f64) {}
    fn bool(&mut self, _: bool) {}
    fn null(&mut self) {}
}

/// One of the two JSON walks.
type Walk = fn(
    &Automerge,
    Option<&[automerge::ChangeHash]>,
    &mut Discard,
) -> Result<(), pg_automerge_core::Error>;

fn walk(doc: &Automerge, f: Walk) {
    let mut sink = Discard::default();
    f(doc, None, &mut sink).unwrap();
    black_box(sink.0);
}

/// A timed operation.
type Case<'a> = Box<dyn FnMut() + 'a>;

fn median_ms(reps: usize, mut f: impl FnMut()) -> f64 {
    let mut times: Vec<f64> = (0..reps)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn build(name: &str) -> AutoCommit {
    match name {
        "text3mb" => bench_docs::big_text(),
        "items20k" => bench_docs::structured(20_000),
        "items2k" => bench_docs::structured(2_000),
        "typed5k" => bench_docs::typed_text(5_000),
        "rich20k" => bench_docs::rich_text(20_000),
        other => panic!("unknown document {other}"),
    }
}

fn main() {
    let docs = std::env::var("BENCH_DOCS")
        .unwrap_or_else(|_| "text3mb items20k items2k typed5k rich20k".into());
    let reps: usize = std::env::var("BENCH_REPS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(5)
        .max(1);

    println!("{:<9} | {:>9} | operation", "doc", "ms");
    for name in docs.split_whitespace() {
        let mut doc = build(name);
        let stored = doc.document().save_nocompress();
        let compressed = doc.save();
        eprintln!(
            "==> {name}: stored {} bytes, compressed save {} bytes",
            stored.len(),
            compressed.len()
        );
        let loaded = Automerge::load(&stored).unwrap();
        let text = loaded
            .get(automerge::ROOT, "text")
            .unwrap()
            .map(|(_, id)| id);

        let cases: Vec<(&str, Case<'_>)> = vec![
            (
                "Automerge::load(stored)",
                Box::new(|| drop(black_box(Automerge::load(&stored).unwrap()))),
            ),
            (
                "normalize(stored: canonical)",
                Box::new(|| drop(black_box(normalize(&stored).unwrap()))),
            ),
            (
                "normalize(save(): compressed)",
                Box::new(|| drop(black_box(normalize(&compressed).unwrap()))),
            ),
            (
                "load memory scan (stored)",
                Box::new(|| {
                    black_box(budget::scan_input(&stored, budget::limit()));
                }),
            ),
            (
                "load memory scan (compressed)",
                Box::new(|| {
                    black_box(budget::scan_input(&compressed, budget::limit()));
                }),
            ),
            (
                "save_nocompress",
                Box::new(|| drop(black_box(loaded.save_nocompress()))),
            ),
            (
                "json walk, one sweep (no-op sink)",
                Box::new(|| walk(&loaded, json::write_json_at)),
            ),
            (
                "json walk, per object (no-op sink)",
                Box::new(|| walk(&loaded, json::write_json_per_object)),
            ),
            (
                "doc_to_json (walk + serde Value)",
                Box::new(|| drop(black_box(json::doc_to_json(&loaded).unwrap()))),
            ),
        ];
        let mut cases = cases;
        let is_text = |id: &automerge::ObjId| loaded.object_type(id) == Ok(ObjType::Text);
        if let Some(text) = text.filter(is_text) {
            let expanded = LoadedDoc::from_doc(loaded.clone(), false).unwrap();
            let expanded = Box::leak(Box::new(expanded));
            let doc = &loaded;
            cases.push((
                "automerge ReadDoc::spans (iterate, loaded)",
                Box::new(move || {
                    black_box(doc.spans(&text).unwrap().count());
                }),
            ));
            cases.push((
                "write_spans (no-op sink, loaded)",
                Box::new(|| {
                    let mut sink = Discard::default();
                    spans::write_spans(Input::Loaded(expanded), &["text"], None, &mut sink)
                        .unwrap();
                    black_box(sink.0);
                }),
            ));
            cases.push((
                "spans_to_json (stored: load + serde Value)",
                Box::new(|| {
                    drop(black_box(
                        spans::spans_to_json(Input::Stored(&stored), &["text"], None).unwrap(),
                    ))
                }),
            ));
        }
        for (label, mut f) in cases {
            let ms = median_ms(reps, &mut f);
            println!("{name:<9} | {ms:>9.1} | {label}");
        }
    }
}
