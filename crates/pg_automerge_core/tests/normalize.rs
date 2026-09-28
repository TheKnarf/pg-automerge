//! `normalize` on compressed input: the shortcut that skips the
//! save-and-load check when the inflated input is the canonical save
//! (`header::inflate_document`) must give exactly what the general path
//! gives, and must only be taken when it is sound.

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, Automerge, ObjType, ROOT};
use pg_automerge_core::header::inflate_document;
use pg_automerge_core::test_hooks::reload_checks;
use pg_automerge_core::{Error, normalize};

mod common;

use common::{Rng, edit, random_replicas, reference_normalize};

/// `normalize(bytes)`, and whether it ran the save-and-load check.
fn normalize_counted(bytes: &[u8]) -> (Result<Vec<u8>, Error>, bool) {
    let before = reload_checks();
    let result = normalize(bytes);
    (result, reload_checks() > before)
}

/// A document large enough for `save()` to deflate some columns.
fn compressible(seed: u8) -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(ActorId::from([seed; 16]));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.splice_text(&text, 0, 0, &"lorem ipsum ".repeat(300))
        .unwrap();
    doc.commit();
    let items = doc.put_object(ROOT, "items", ObjType::List).unwrap();
    for i in 0..200 {
        let m = doc.insert_object(&items, i, ObjType::Map).unwrap();
        doc.put(&m, "id", i as i64).unwrap();
        doc.put(&m, "title", format!("item {i}")).unwrap();
        if i % 20 == 19 {
            doc.commit();
        }
    }
    doc.commit();
    doc
}

#[test]
fn compressed_saves_skip_the_reload_and_match_the_general_path() {
    let mut docs: Vec<Automerge> = vec![compressible(1).document().clone()];
    // Larger random histories: several actors, hundreds of edits.
    for seed in 0..12u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut a = AutoCommit::new().with_actor(ActorId::from([0x10 + seed as u8; 16]));
        let mut b = a.fork().with_actor(ActorId::from([0x40 + seed as u8; 16]));
        for i in 0..(200 + 100 * seed) {
            edit(if i % 3 == 0 { &mut b } else { &mut a }, &mut rng);
            if rng.below(50) == 0 {
                a.merge(&mut b).unwrap();
            }
        }
        a.merge(&mut b).unwrap();
        docs.push(a.document().clone());
    }
    for seed in 0..60 {
        for stored in random_replicas(seed) {
            docs.push(Automerge::load(&stored).unwrap());
        }
    }
    let mut deflated = 0;
    for doc in &docs {
        let canonical = doc.save_nocompress();
        let compressed = doc.save();
        let (got, reloaded) = normalize_counted(&compressed);
        let got = got.unwrap();
        assert_eq!(got, canonical);
        assert_eq!(Some(got), reference_normalize(&compressed));
        if compressed != canonical {
            deflated += 1;
            assert_eq!(inflate_document(&compressed), Some(canonical.clone()));
            assert!(!reloaded, "a compressed save must not be reloaded");
        }
        // Canonical input never reloads either.
        let (again, reloaded) = normalize_counted(&canonical);
        assert_eq!(again.unwrap(), canonical);
        assert!(!reloaded);
    }
    assert!(deflated >= 5, "only {deflated} saves had deflated columns");
}

#[test]
fn a_save_with_trailing_changes_takes_the_general_path() {
    let mut doc = compressible(2);
    let compressed = doc.save();
    let heads = doc.get_heads();
    doc.put(ROOT, "later", true).unwrap();
    doc.commit();
    let input = [compressed.as_slice(), &doc.save_after(&heads)].concat();
    assert_eq!(inflate_document(&input), None);
    let (got, reloaded) = normalize_counted(&input);
    assert!(reloaded);
    let got = got.unwrap();
    assert_eq!(got, doc.document().save_nocompress());
    assert_eq!(Some(got), reference_normalize(&input));
}

// ---------------------------------------------------------------------------
// Hand-edited compressed chunks
// ---------------------------------------------------------------------------

fn uleb(bytes: &[u8], pos: &mut usize) -> u64 {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let b = bytes[*pos];
        *pos += 1;
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

fn write_uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// A document chunk split into its parts (all data after the chunk
/// header): prefix (actors, heads), column metadata, column data, suffix.
struct DocChunk {
    prefix: Vec<u8>,
    cols: [Vec<(u64, Vec<u8>)>; 2],
    suffix: Vec<u8>,
}

impl DocChunk {
    fn parse(bytes: &[u8]) -> Self {
        let mut pos = 9;
        let len = uleb(bytes, &mut pos) as usize;
        assert_eq!(pos + len, bytes.len());
        let start = pos;
        let actors = uleb(bytes, &mut pos);
        for _ in 0..actors {
            let n = uleb(bytes, &mut pos) as usize;
            pos += n;
        }
        let heads = uleb(bytes, &mut pos) as usize;
        pos += 32 * heads;
        let prefix = bytes[start..pos].to_vec();
        let mut metas = [Vec::new(), Vec::new()];
        for meta in &mut metas {
            let n = uleb(bytes, &mut pos);
            for _ in 0..n {
                let spec = uleb(bytes, &mut pos);
                let len = uleb(bytes, &mut pos) as usize;
                meta.push((spec, len));
            }
        }
        let mut cols = [Vec::new(), Vec::new()];
        for (meta, out) in metas.iter().zip(&mut cols) {
            for &(spec, len) in meta {
                out.push((spec, bytes[pos..pos + len].to_vec()));
                pos += len;
            }
        }
        DocChunk {
            prefix,
            cols,
            suffix: bytes[pos..].to_vec(),
        }
    }

    /// Serialize with a correct checksum.
    fn write(&self) -> Vec<u8> {
        let mut data = self.prefix.clone();
        for cols in &self.cols {
            write_uleb(&mut data, cols.len() as u64);
            for (spec, bytes) in cols {
                write_uleb(&mut data, *spec);
                write_uleb(&mut data, bytes.len() as u64);
            }
        }
        for cols in &self.cols {
            for (_, bytes) in cols {
                data.extend_from_slice(bytes);
            }
        }
        data.extend_from_slice(&self.suffix);
        common::chunk(0, &data)
    }

    /// The first deflated column.
    fn deflated_column(&mut self) -> &mut Vec<u8> {
        self.cols
            .iter_mut()
            .flatten()
            .find(|(spec, _)| spec & 0x08 != 0)
            .map(|(_, bytes)| bytes)
            .expect("a deflated column")
    }
}

#[test]
fn corrupt_deflate_streams_are_rejected_like_the_general_path() {
    let compressed = compressible(3).save();
    let original = DocChunk::parse(&compressed);
    assert_eq!(original.write(), compressed, "the test parser round-trips");
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut rejected = 0;
    for _ in 0..300 {
        let mut chunk = DocChunk::parse(&compressed);
        let col = chunk.deflated_column();
        match rng.below(3) {
            0 => {
                let i = rng.below(col.len() as u64) as usize;
                col[i] ^= 1 << rng.below(8);
            }
            1 => {
                let n = 1 + rng.below(col.len() as u64 - 1) as usize;
                col.truncate(n);
            }
            _ => {
                let i = rng.below(col.len() as u64) as usize;
                col[i] = rng.next() as u8;
                col.push(rng.next() as u8);
            }
        }
        let input = chunk.write();
        let expected = reference_normalize(&input);
        let got = normalize(&input);
        if expected.is_none() {
            rejected += 1;
        }
        assert_eq!(got.ok(), expected);
    }
    assert!(rejected > 0);
}

#[test]
fn trailing_bytes_after_a_deflate_stream_are_handled_like_automerge() {
    // Automerge inflates with read_to_end, which stops at the end of the
    // deflate stream: bytes after it inside the column are ignored. The
    // input is not canonical but loads; the result must be the same as on
    // the general path.
    let mut doc = compressible(4);
    let compressed = doc.save();
    let mut chunk = DocChunk::parse(&compressed);
    chunk.deflated_column().extend_from_slice(b"trailing junk");
    let input = chunk.write();
    let expected = reference_normalize(&input);
    assert_eq!(normalize(&input).ok(), expected);
    if let Some(bytes) = expected {
        assert_eq!(bytes, doc.document().save_nocompress());
    }
}
