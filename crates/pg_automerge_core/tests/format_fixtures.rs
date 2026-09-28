//! Values stored by earlier releases keep working: the documents in
//! `tests/fixtures/automerge-<version>/` were saved by the automerge crate
//! version the extension shipped with (written by the
//! `gen_format_fixtures` example; see docs/DESIGN.md, "Versioning and
//! upgrades").
//!
//! For every fixture set:
//!
//! - the stored bytes and the compressed save still load, with the same
//!   heads and the same JSON (the jsonb mapping); this must hold forever;
//! - normalizing them still gives exactly those stored bytes. When an
//!   automerge upgrade changes its canonical encoding this fails: stored
//!   values would then be re-encoded by the next write of each row, and
//!   the release notes must say so (and `doc::bytea` comparisons against
//!   old values stop matching). Regenerate a new fixture set for the new
//!   version rather than editing the old ones.

use std::path::PathBuf;

use automerge::Automerge;
use pg_automerge_core::normalize;

fn fixture_sets() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut sets: Vec<PathBuf> = std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    sets.sort();
    sets
}

#[test]
fn stored_values_of_every_shipped_automerge_version_still_work() {
    let sets = fixture_sets();
    assert!(!sets.is_empty());
    for set in sets {
        let mut names: Vec<String> = std::fs::read_dir(&set)
            .unwrap()
            .filter_map(|e| {
                let path = e.unwrap().path();
                (path.extension()? == "stored")
                    .then(|| path.file_stem()?.to_str().map(str::to_owned))?
            })
            .collect();
        names.sort();
        assert!(!names.is_empty(), "{set:?} has no fixtures");
        for name in names {
            let read = |ext: &str| std::fs::read(set.join(format!("{name}.{ext}"))).unwrap();
            let what = format!("{}/{name}", set.display());
            let stored = read("stored");
            let save = read("save");
            let heads: Vec<String> = String::from_utf8(read("heads"))
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect();
            let json: serde_json::Value = serde_json::from_slice(&read("json")).unwrap();

            for bytes in [&stored, &save] {
                let doc = Automerge::load(bytes)
                    .unwrap_or_else(|e| panic!("{what}: no longer loads: {e}"));
                assert_eq!(
                    pg_automerge_core::heads_to_strings(doc.get_heads()),
                    heads,
                    "{what}: heads changed"
                );
                assert_eq!(
                    pg_automerge_core::json::doc_to_json(&doc).unwrap(),
                    json,
                    "{what}: the jsonb mapping changed"
                );
            }
            assert_eq!(
                normalize(&stored).unwrap(),
                stored,
                "{what}: the canonical encoding changed (see the module docs)"
            );
            assert_eq!(
                normalize(&save).unwrap(),
                stored,
                "{what}: normalizing the save changed"
            );
        }
    }
}
