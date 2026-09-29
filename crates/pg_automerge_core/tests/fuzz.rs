//! A deterministic mutation fuzzer for the entry points that take external
//! bytes: `normalize`, `loaded::merge_changes` and
//! `loaded::contains_changes`.
//!
//! Inputs are real Automerge output (uncompressed and compressed document
//! saves, bare change chunks, concatenations) mutated at the chunk level:
//! bits flipped, bytes overwritten, inserted and deleted inside a chunk's
//! data, chunks dropped, duplicated and reordered. The chunk length and
//! checksum are then recomputed, so the mutated data gets past Automerge's
//! checksum and reaches its decoders (the corrupt-but-checksummed inputs
//! that made `Automerge::load` panic or load into unsaveable documents).
//!
//! Properties, for every input:
//!
//! - nothing panics out of the core (a decoder panic is an `Err`);
//! - `normalize` agrees exactly with the long way ([`reference_normalize`]:
//!   load, save, load again, compare heads), so its shortcuts never accept
//!   what the check would reject;
//! - whatever `normalize` returns is a stored value: it loads, with the
//!   heads the header says, and normalizes to itself;
//! - whatever `merge_changes` returns, once its stored bytes are computed,
//!   loads back with the document's heads;
//! - the load memory scan (`budget::scan_input`) never marks input that
//!   Automerge loads as malformed, and the peak memory of `normalize`
//!   (measured by a counting allocator) stays below its estimate;
//!   `normalize` rejects input over the limit (`Error::LoadLimit`) only
//!   when the estimate says so.
//!
//! `tests/corpus/*.bin` are inputs worth keeping, found by this harness:
//! `panic-*` one per distinct decoder panic, `reload-*` inputs that load
//! but whose re-save does not ("mismatching heads"), which only the
//! save-and-load check rejects. They run first, and must still end that
//! way.
//! Env: FUZZ_ITERS (default 1500), FUZZ_SEED, FUZZ_SAVE_DIR (write the
//! inputs that made Automerge panic or failed the save-and-load check
//! there, to grow the corpus). `mise run fuzz` runs a long session.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, Automerge, ROOT};
use pg_automerge_core::budget::{self, doc_estimate, scan_doc, scan_input};
use pg_automerge_core::header::heads_from_bytes;
use pg_automerge_core::loaded::{self, Input};
use pg_automerge_core::{Error, normalize};

mod common;
#[path = "common/counting.rs"]
mod counting;

use common::{Rng, chunk, edit, random_replicas, reference_normalize};

/// A chunk of an input: its type and data (compressed change chunks,
/// whose checksum covers the uncompressed form, are kept raw).
#[derive(Clone)]
enum Part {
    Chunk(u8, Vec<u8>),
    Raw(Vec<u8>),
}

fn uleb(bytes: &[u8], pos: &mut usize) -> Option<usize> {
    let mut value = 0usize;
    for i in 0..5 {
        let b = *bytes.get(*pos)?;
        *pos += 1;
        value |= usize::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn split(bytes: &[u8]) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let start = pos;
        let parsed = (|| {
            if bytes.get(pos..pos + 4)? != [0x85, 0x6f, 0x4a, 0x83] {
                return None;
            }
            pos += 8;
            let typ = *bytes.get(pos)?;
            pos += 1;
            let len = uleb(bytes, &mut pos)?;
            let data = bytes.get(pos..pos + len)?.to_vec();
            pos += len;
            Some((typ, data))
        })();
        match parsed {
            Some((typ @ (0 | 1), data)) => parts.push(Part::Chunk(typ, data)),
            Some(_) => parts.push(Part::Raw(bytes[start..pos].to_vec())),
            None => {
                parts.push(Part::Raw(bytes[start..].to_vec()));
                break;
            }
        }
    }
    parts
}

fn join(parts: &[Part]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        match part {
            Part::Chunk(typ, data) => out.extend(chunk(*typ, data)),
            Part::Raw(raw) => out.extend_from_slice(raw),
        }
    }
    out
}

fn mutate(input: &[u8], other: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut parts = split(input);
    if parts.is_empty() {
        return input.to_vec();
    }
    for _ in 0..1 + rng.below(3) {
        let n = parts.len() as u64;
        match rng.below(10) {
            // Chunk-level edits.
            0 if n > 1 => {
                parts.remove(rng.below(n) as usize);
            }
            1 => {
                let p = parts[rng.below(n) as usize].clone();
                parts.insert(rng.below(n + 1) as usize, p);
            }
            2 if n > 1 => {
                let (i, j) = (rng.below(n) as usize, rng.below(n) as usize);
                parts.swap(i, j);
            }
            3 => {
                let others = split(other);
                if !others.is_empty() {
                    let p = others[rng.below(others.len() as u64) as usize].clone();
                    parts.insert(rng.below(n + 1) as usize, p);
                }
            }
            // Byte-level edits inside a chunk's data.
            _ => {
                let i = rng.below(n) as usize;
                let Part::Chunk(_, data) = &mut parts[i] else {
                    continue;
                };
                if data.is_empty() {
                    continue;
                }
                let at = rng.below(data.len() as u64) as usize;
                match rng.below(6) {
                    0 => data[at] ^= 1 << rng.below(8),
                    1 => data[at] = [0, 1, 0x7f, 0x80, 0xff][rng.below(5) as usize],
                    2 => data[at] = rng.next() as u8,
                    3 => data.insert(at, rng.next() as u8),
                    4 => {
                        let end = (at + 1 + rng.below(8) as usize).min(data.len());
                        data.drain(at..end);
                    }
                    _ => data.truncate(at),
                }
            }
        }
    }
    join(&parts)
}

/// Seed inputs: saves (plain and compressed), incremental change chunks,
/// and concatenations of those, from random histories.
fn seeds() -> Vec<Vec<u8>> {
    let mut seeds = Vec::new();
    for seed in 0..8 {
        for stored in random_replicas(seed).into_iter().take(2) {
            let doc = Automerge::load(&stored).unwrap();
            seeds.push(doc.save());
            seeds.push(stored);
        }
    }
    let mut rng = Rng(7);
    let mut doc = AutoCommit::new().with_actor(ActorId::from([7u8; 16]));
    for _ in 0..60 {
        edit(&mut doc, &mut rng);
    }
    doc.commit();
    let heads = doc.get_heads();
    let base = doc.save();
    for _ in 0..20 {
        edit(&mut doc, &mut rng);
    }
    doc.put(ROOT, "last", true).unwrap();
    doc.commit();
    let changes = doc.save_after(&heads);
    seeds.push([base.as_slice(), &changes].concat());
    seeds.push(changes);
    seeds.push(doc.save());
    seeds
}

fn sorted(mut heads: Vec<automerge::ChangeHash>) -> Vec<automerge::ChangeHash> {
    heads.sort_unstable();
    heads
}

/// What `normalize` made of an input, beyond accepting or rejecting it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Notable {
    Nothing,
    /// Automerge panicked (turned into an error).
    DecoderPanic,
    /// It loaded, but its re-save did not load back the same: rejected by
    /// the save-and-load check.
    ReloadRejected,
}

/// Check every property on one input.
fn check(input: &[u8], base: &[u8]) -> Notable {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let scanned = scan_input(input, budget::limit());
        let loads =
            catch_unwind(AssertUnwindSafe(|| Automerge::load(input).is_ok())).unwrap_or(false);
        if loads {
            assert!(
                !scanned.malformed,
                "Automerge loads what the scan calls malformed"
            );
        }
        let (normalized, peak) = counting::peak_of(|| normalize(input));
        if let Err(Error::LoadLimit(err)) = &normalized {
            // Over the limit by the estimate; the reference load of such
            // input could take gigabytes, so it is not run.
            assert!(err.estimate > err.limit);
            return Notable::Nothing;
        }
        if let Ok(stored) = &normalized {
            let estimate = scanned.load_estimate().max(doc_estimate(&scan_doc(stored)));
            assert!(
                peak <= estimate,
                "normalize peak {peak} above the estimate {estimate}"
            );
        }
        let notable = match &normalized {
            Err(Error::InvalidInput(m)) if m.contains("malformed data") => Notable::DecoderPanic,
            Err(Error::InvalidInput(m))
                if m.contains("does not survive") || m.contains("heads change") =>
            {
                Notable::ReloadRejected
            }
            _ => Notable::Nothing,
        };
        assert_eq!(
            normalized.as_ref().ok(),
            reference_normalize(input).as_ref(),
            "normalize disagrees with load-save-load"
        );
        if let Ok(stored) = &normalized {
            let doc = Automerge::load(stored).expect("a stored value loads");
            assert_eq!(
                sorted(heads_from_bytes(stored).expect("one document chunk")),
                sorted(doc.get_heads())
            );
            assert_eq!(normalize(stored).as_ref(), Ok(stored));
        }
        if let Ok(Some(doc)) = loaded::merge_changes(Input::Stored(base), input)
            && let Ok(stored) = doc.stored()
        {
            let reloaded = Automerge::load(stored).expect("merge result loads");
            assert_eq!(sorted(reloaded.get_heads()), doc.heads());
        }
        let _ = loaded::contains_changes(Input::Stored(base), input);
        notable
    }));
    match outcome {
        Ok(notable) => notable,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            panic!(
                "property failed or a panic escaped ({msg}) on input {}",
                pg_automerge_core::encoding::to_hex_literal(input)
            );
        }
    }
}

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

/// Keep automerge's own panic messages (caught and turned into errors)
/// out of the test output.
fn quiet_automerge_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let from_automerge = info.location().is_some_and(|l| {
            let file = l.file();
            // Automerge, its column library, or std code it called.
            file.contains("automerge-") || file.contains("hexane-") || file.contains("/library/")
        });
        if !from_automerge {
            default(info);
        }
    }));
}

#[test]
fn corpus_inputs_hold_the_properties() {
    quiet_automerge_panics();
    let base = random_replicas(3).remove(0);
    let mut n = 0;
    for entry in std::fs::read_dir(corpus_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "bin") {
            let notable = check(&std::fs::read(&path).unwrap(), &base);
            // Each kept input still does what it was kept for.
            let name = path.file_name().unwrap().to_string_lossy();
            let expected = if name.starts_with("reload-") {
                Notable::ReloadRejected
            } else {
                Notable::DecoderPanic
            };
            assert_eq!(notable, expected, "{name}");
            n += 1;
        }
    }
    assert!(n > 0, "empty corpus");
}

#[test]
fn mutated_inputs_hold_the_properties() {
    quiet_automerge_panics();
    let iters: u64 = std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
    let seed: u64 = std::env::var("FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x5eed);
    let save_dir = std::env::var_os("FUZZ_SAVE_DIR").map(PathBuf::from);
    let seeds = seeds();
    let base = random_replicas(3).remove(0);
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let (mut panics, mut rejected) = (0, 0);
    for i in 0..iters {
        let input = &seeds[rng.below(seeds.len() as u64) as usize];
        let other = &seeds[rng.below(seeds.len() as u64) as usize];
        let mutated = mutate(input, other, &mut rng);
        let kind = match check(&mutated, &base) {
            Notable::Nothing => continue,
            Notable::DecoderPanic => {
                panics += 1;
                "panic"
            }
            Notable::ReloadRejected => {
                rejected += 1;
                "reload"
            }
        };
        if let Some(dir) = &save_dir {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(format!("{kind}-{seed:x}-{i}.bin")), &mutated).unwrap();
        }
    }
    eprintln!(
        "{iters} inputs: {panics} decoder panics caught, {rejected} rejected by the save-and-load check"
    );
}
