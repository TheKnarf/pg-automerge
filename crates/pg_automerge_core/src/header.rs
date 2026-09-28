//! Reading a stored value's heads straight from its bytes, without loading
//! the document.
//!
//! A stored value is one uncompressed document chunk (`save_nocompress`).
//! Its layout (automerge 0.12, `src/storage/document.rs`, `chunk.rs`) starts
//! with everything needed:
//!
//! ```text
//! magic         85 6f 4a 83
//! checksum      4 bytes
//! chunk type    1 byte, 0 = document
//! data length   uleb128
//! data:
//!   actors      uleb128 count, then per actor: uleb128 length + bytes
//!   heads       uleb128 count, then 32 bytes per change hash
//!   ...         change/op column metadata and data, head indices
//! ```
//!
//! Loading a document chunk verifies these heads against the ones derived
//! from its changes (`VerificationMode::Check`, a "mismatching heads" error
//! otherwise), and every stored value was loaded that way when it was
//! written. So for a stored value the header heads are exactly
//! `Automerge::load(bytes).get_heads()`; a property test in
//! `tests/heads_fast_path.rs` checks this over many generated documents.
//!
//! The checksum is not recomputed (that would hash the whole value): stored
//! values were checksum-verified on the way in, and on-disk corruption is
//! Postgres' business (data checksums), as for every other type.
//!
//! Anything that is not exactly one document chunk (not the magic bytes, a
//! different chunk type, trailing data, lengths that do not fit) is reported
//! as [`HeadsPrefix::NotSingleDoc`] and callers fall back to a full load.

use automerge::ChangeHash;

const MAGIC: [u8; 4] = [0x85, 0x6f, 0x4a, 0x83];
const DOCUMENT_CHUNK: u8 = 0;

/// Result of reading something from (a prefix of) stored bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prefix<T> {
    /// The value read.
    Found(T),
    /// The prefix ends before the value does: at least this many bytes of
    /// the value (counted from its start) are needed.
    NeedMore(usize),
    /// Not a single document chunk (or not in the expected shape); do a
    /// full load instead.
    NotSingleDoc,
}

/// Result of reading heads from (a prefix of) stored bytes. `Found` holds
/// the heads in the order the header lists them (sorted by hash in
/// practice, but callers must not rely on it).
pub type HeadsPrefix = Prefix<Vec<ChangeHash>>;

/// Why parsing stopped.
enum Stop {
    NeedMore(usize),
    NotSingleDoc,
}

struct Reader<'a> {
    prefix: &'a [u8],
    total_len: usize,
    pos: usize,
}

impl Reader<'_> {
    /// `n` bytes at the current position.
    fn take(&mut self, n: usize) -> Result<&[u8], Stop> {
        let end = self.pos.checked_add(n).ok_or(Stop::NotSingleDoc)?;
        if end > self.total_len {
            return Err(Stop::NotSingleDoc);
        }
        if end > self.prefix.len() {
            return Err(Stop::NeedMore(end));
        }
        let bytes = &self.prefix[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    /// An unsigned LEB128 value of at most 64 bits.
    fn uleb(&mut self) -> Result<u64, Stop> {
        let mut value = 0u64;
        for i in 0..10 {
            let byte = self.take(1)?[0];
            let bits = u64::from(byte & 0x7f);
            if i == 9 && bits > 1 {
                return Err(Stop::NotSingleDoc);
            }
            value |= bits << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Stop::NotSingleDoc)
    }

    fn uleb_usize(&mut self) -> Result<usize, Stop> {
        usize::try_from(self.uleb()?).map_err(|_| Stop::NotSingleDoc)
    }

    /// A signed LEB128 value of at most 64 bits.
    fn sleb(&mut self) -> Result<i64, Stop> {
        let mut value = 0i64;
        for i in 0..10 {
            let byte = self.take(1)?[0];
            let bits = i64::from(byte & 0x7f);
            if i == 9 && !(bits == 0 || bits == 0x7f) {
                return Err(Stop::NotSingleDoc);
            }
            value |= bits << (7 * i);
            if byte & 0x80 == 0 {
                let shift = 7 * (i + 1);
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Ok(value);
            }
        }
        Err(Stop::NotSingleDoc)
    }
}

/// Read the heads of a stored value given its first `prefix.len()` bytes and
/// its total length `total_len`.
///
/// Never panics, whatever the input.
pub fn heads_from_prefix(prefix: &[u8], total_len: usize) -> HeadsPrefix {
    let prefix = &prefix[..prefix.len().min(total_len)];
    match parse(prefix, total_len) {
        Ok(heads) => HeadsPrefix::Found(heads),
        Err(Stop::NeedMore(n)) => HeadsPrefix::NeedMore(n),
        Err(Stop::NotSingleDoc) => HeadsPrefix::NotSingleDoc,
    }
}

/// [`heads_from_prefix`] on complete bytes: `None` if they are not a single
/// document chunk.
pub fn heads_from_bytes(bytes: &[u8]) -> Option<Vec<ChangeHash>> {
    match heads_from_prefix(bytes, bytes.len()) {
        HeadsPrefix::Found(heads) => Some(heads),
        // NeedMore cannot happen with the whole value, but be defensive.
        HeadsPrefix::NeedMore(_) | HeadsPrefix::NotSingleDoc => None,
    }
}

fn parse(prefix: &[u8], total_len: usize) -> Result<Vec<ChangeHash>, Stop> {
    parse_heads(&mut Reader {
        prefix,
        total_len,
        pos: 0,
    })
}

/// Parse up to the end of the heads, leaving `r` just after them.
fn parse_heads(r: &mut Reader<'_>) -> Result<Vec<ChangeHash>, Stop> {
    if r.take(4)? != MAGIC {
        return Err(Stop::NotSingleDoc);
    }
    r.take(4)?; // checksum
    if r.take(1)?[0] != DOCUMENT_CHUNK {
        return Err(Stop::NotSingleDoc);
    }
    let data_len = r.uleb_usize()?;
    // Exactly one chunk: the data runs to the end of the value.
    if r.pos.checked_add(data_len) != Some(r.total_len) {
        return Err(Stop::NotSingleDoc);
    }
    let actors = r.uleb_usize()?;
    for _ in 0..actors {
        let len = r.uleb_usize()?;
        r.take(len)?;
    }
    let count = r.uleb_usize()?;
    // Bound the allocation by what the value can hold.
    if count > r.total_len / 32 {
        return Err(Stop::NotSingleDoc);
    }
    let mut heads = Vec::with_capacity(count);
    for _ in 0..count {
        let bytes: [u8; 32] = r.take(32)?.try_into().map_err(|_| Stop::NotSingleDoc)?;
        heads.push(ChangeHash(bytes));
    }
    Ok(heads)
}

/// Column spec of the change actor column: column id 0, type actor (1), not
/// deflated (automerge `change_graph.rs`, `ids::ACTOR_COL_SPEC`).
const CHANGE_ACTOR_SPEC: u64 = 0x01;
/// The deflate bit of a column spec.
const DEFLATE_BIT: u64 = 0x08;

/// Read the number of changes of a stored value from (a prefix of) its
/// bytes, without loading it.
///
/// After the heads, a document chunk has the change column metadata
/// (uleb128 count, then uleb128 spec and length per column), the op column
/// metadata, then the column data, change columns first. Every change has
/// exactly one entry in the change actor column, which is RLE encoded
/// (signed LEB128 count: `n > 0` a run of `n` copies of the next value,
/// `n < 0` that many literal values, `0` a run of nulls); Automerge's
/// loader takes the number of changes from this column's length and
/// rejects documents whose other change columns disagree. An absent column
/// means no changes.
///
/// So only the header, the metadata and the (small) actor column are read.
/// Anything unexpected (a deflated or repeated actor column, a null run, a
/// value that does not end where the column does, lengths beyond the value)
/// is `NotSingleDoc`, and callers load the document instead. Never panics.
pub fn change_count_from_prefix(prefix: &[u8], total_len: usize) -> Prefix<u64> {
    let prefix = &prefix[..prefix.len().min(total_len)];
    let mut r = Reader {
        prefix,
        total_len,
        pos: 0,
    };
    match parse_change_count(&mut r) {
        Ok(n) => Prefix::Found(n),
        Err(Stop::NeedMore(n)) => Prefix::NeedMore(n),
        Err(Stop::NotSingleDoc) => Prefix::NotSingleDoc,
    }
}

/// [`change_count_from_prefix`] on complete bytes.
pub fn change_count_from_bytes(bytes: &[u8]) -> Option<u64> {
    match change_count_from_prefix(bytes, bytes.len()) {
        Prefix::Found(n) => Some(n),
        Prefix::NeedMore(_) | Prefix::NotSingleDoc => None,
    }
}

/// Column metadata: the offset (within this group's data) and length of the
/// column with spec `wanted`, if present, and the group's total data length.
fn column_metadata(
    r: &mut Reader<'_>,
    wanted: Option<u64>,
) -> Result<(Option<(usize, usize)>, usize), Stop> {
    let count = r.uleb_usize()?;
    // Each entry takes at least two bytes.
    if count > r.total_len / 2 {
        return Err(Stop::NotSingleDoc);
    }
    let mut offset = 0usize;
    let mut found = None;
    for _ in 0..count {
        let spec = r.uleb()?;
        if spec > u64::from(u32::MAX) {
            return Err(Stop::NotSingleDoc);
        }
        let len = r.uleb_usize()?;
        if Some(spec & !DEFLATE_BIT) == wanted {
            if found.is_some() || spec & DEFLATE_BIT != 0 {
                return Err(Stop::NotSingleDoc);
            }
            found = Some((offset, len));
        }
        offset = offset.checked_add(len).ok_or(Stop::NotSingleDoc)?;
    }
    Ok((found, offset))
}

fn parse_change_count(r: &mut Reader<'_>) -> Result<u64, Stop> {
    parse_heads(r)?;
    let (actor_col, change_len) = column_metadata(r, Some(CHANGE_ACTOR_SPEC))?;
    let (_, ops_len) = column_metadata(r, None)?;
    let data_start = r.pos;
    let data_end = data_start
        .checked_add(change_len)
        .and_then(|n| n.checked_add(ops_len))
        .ok_or(Stop::NotSingleDoc)?;
    if data_end > r.total_len {
        return Err(Stop::NotSingleDoc);
    }
    let Some((offset, len)) = actor_col else {
        return Ok(0);
    };
    r.pos = data_start + offset;
    let column = r.take(len)?;
    count_rle(column)
}

/// Number of values in a complete RLE column of uleb128 values without
/// nulls.
fn count_rle(column: &[u8]) -> Result<u64, Stop> {
    let mut r = Reader {
        prefix: column,
        total_len: column.len(),
        pos: 0,
    };
    let mut count = 0u64;
    while r.pos < column.len() {
        let n = r.sleb()?;
        let values = match n {
            n if n > 0 => {
                r.uleb()?;
                n.unsigned_abs()
            }
            n if n < 0 => {
                let k = n.unsigned_abs();
                // Each literal value takes at least one byte.
                if k > (column.len() - r.pos) as u64 {
                    return Err(Stop::NotSingleDoc);
                }
                for _ in 0..k {
                    r.uleb()?;
                }
                k
            }
            _ => return Err(Stop::NotSingleDoc),
        };
        count = count.checked_add(values).ok_or(Stop::NotSingleDoc)?;
    }
    Ok(count)
}

/// Chunk type of an uncompressed change chunk.
const CHANGE_CHUNK: u8 = 1;

/// One uncompressed change chunk of external input: its change hash and
/// its dependencies, read without decoding the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeChunk {
    pub hash: ChangeHash,
    pub deps: Vec<ChangeHash>,
}

/// Split `bytes` into uncompressed change chunks (`save_incremental()` /
/// `save_after()` output), computing each change's hash and reading its
/// dependencies, without loading anything.
///
/// A change hash is `sha256(chunk type || uleb128 data length || data)`
/// (automerge `storage/chunk.rs`), and the chunk's checksum is its first
/// four bytes; the data starts with the dependencies (uleb128 count, 32
/// bytes each; `storage/change.rs`). Only the framing, the checksum and the
/// dependency list are checked, not the rest of the change.
///
/// `None` unless `bytes` is one or more such chunks and nothing else: a
/// document chunk, a compressed change, a bad checksum, a non-canonical
/// length, or trailing bytes all give `None` (callers then load). Empty
/// input is `Some(vec![])`. Never panics.
pub fn change_chunks(bytes: &[u8]) -> Option<Vec<ChangeChunk>> {
    use sha2::{Digest, Sha256};

    let mut r = Reader {
        prefix: bytes,
        total_len: bytes.len(),
        pos: 0,
    };
    let mut chunks = Vec::new();
    while r.pos < bytes.len() {
        let chunk = (|| -> Result<ChangeChunk, Stop> {
            if r.take(4)? != MAGIC {
                return Err(Stop::NotSingleDoc);
            }
            let checksum: [u8; 4] = r.take(4)?.try_into().map_err(|_| Stop::NotSingleDoc)?;
            if r.take(1)?[0] != CHANGE_CHUNK {
                return Err(Stop::NotSingleDoc);
            }
            let len_start = r.pos;
            let len = r.uleb_usize()?;
            let len_bytes = &bytes[len_start..r.pos];
            // Automerge rejects overlong LEB128; the hash covers the
            // canonical encoding, so only accept that.
            if len_bytes.len() > 1 && len_bytes[len_bytes.len() - 1] == 0 {
                return Err(Stop::NotSingleDoc);
            }
            let data = r.take(len)?;
            let mut hasher = Sha256::new();
            hasher.update([CHANGE_CHUNK]);
            hasher.update(len_bytes);
            hasher.update(data);
            let hash: [u8; 32] = hasher.finalize().into();
            if hash[..4] != checksum {
                return Err(Stop::NotSingleDoc);
            }
            let mut d = Reader {
                prefix: data,
                total_len: data.len(),
                pos: 0,
            };
            let count = d.uleb_usize()?;
            if count > data.len() / 32 {
                return Err(Stop::NotSingleDoc);
            }
            let mut deps = Vec::with_capacity(count);
            for _ in 0..count {
                let dep: [u8; 32] = d.take(32)?.try_into().map_err(|_| Stop::NotSingleDoc)?;
                deps.push(ChangeHash(dep));
            }
            Ok(ChangeChunk {
                hash: ChangeHash(hash),
                deps,
            })
        })()
        .ok()?;
        chunks.push(chunk);
    }
    Some(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use automerge::transaction::Transactable;
    use automerge::{ActorId, AutoCommit, ROOT};

    #[test]
    fn change_chunks_match_automerge() {
        let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
        doc.put(ROOT, "x", 1i64).unwrap();
        let first = doc.get_heads();
        let mut fork = doc.fork().with_actor(ActorId::from([2u8; 16]));
        fork.put(ROOT, "y", 1i64).unwrap();
        doc.put(ROOT, "z", "a longer value ".repeat(20)).unwrap();
        doc.merge(&mut fork).unwrap();
        doc.put(ROOT, "w", 1i64).unwrap();

        let bytes = doc.save_after(&first);
        let chunks = change_chunks(&bytes).unwrap();
        let expected = doc.get_changes(&first);
        assert_eq!(chunks.len(), expected.len());
        for (chunk, change) in chunks.iter().zip(&expected) {
            assert_eq!(chunk.hash, change.hash());
            assert_eq!(chunk.deps, change.deps());
        }
        // The very first change has no deps.
        let all = change_chunks(&doc.save_after(&[])).unwrap();
        assert!(all[0].deps.is_empty());

        assert_eq!(change_chunks(&[]), Some(vec![]));
        // A document chunk, a compressed save, garbage, truncation and a
        // flipped byte are all None.
        assert_eq!(change_chunks(&doc.document().save_nocompress()), None);
        assert_eq!(change_chunks(&doc.save()), None);
        assert_eq!(change_chunks(b"garbage"), None);
        assert_eq!(change_chunks(&bytes[..bytes.len() - 1]), None);
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0x55;
            // Every byte is covered by the magic, the checksum or the hash.
            assert_eq!(change_chunks(&b), None, "byte {i} flipped");
        }
    }

    #[test]
    fn reads_heads_and_asks_for_more() {
        let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
        doc.put(ROOT, "x", 1i64).unwrap();
        let mut fork = doc.fork().with_actor(ActorId::from([2u8; 16]));
        fork.put(ROOT, "y", 1i64).unwrap();
        doc.put(ROOT, "z", 1i64).unwrap();
        doc.merge(&mut fork).unwrap();
        let bytes = doc.document().save_nocompress();
        let mut expected = doc.get_heads();
        expected.sort();

        let mut got = heads_from_bytes(&bytes).unwrap();
        got.sort();
        assert_eq!(got, expected);

        // Growing prefixes: NeedMore until enough, then the same answer; the
        // requested length is always larger than what was given.
        let mut needed = 0;
        for n in 0..bytes.len() {
            match heads_from_prefix(&bytes[..n], bytes.len()) {
                HeadsPrefix::NeedMore(m) => {
                    assert!(m > n, "{m} <= {n}");
                    assert!(m <= bytes.len());
                }
                HeadsPrefix::Found(h) => {
                    needed = n;
                    let mut h = h;
                    h.sort();
                    assert_eq!(h, expected);
                    break;
                }
                HeadsPrefix::NotSingleDoc => panic!("prefix of {n} bytes: not a doc"),
            }
        }
        assert!(needed > 0 && needed < bytes.len());
        // 10 header bytes, actors (2 x 17), heads (2 x 32) and two counts.
        assert!(needed <= 12 + 2 * 17 + 2 * 32 + 2);
    }

    #[test]
    fn rejects_anything_but_one_document_chunk() {
        let mut doc = AutoCommit::new().with_actor(ActorId::from([1u8; 16]));
        doc.put(ROOT, "x", 1i64).unwrap();
        let heads = doc.get_heads();
        let bytes = doc.document().save_nocompress();
        doc.put(ROOT, "y", 1i64).unwrap();
        let change = doc.save_after(&heads);

        assert_eq!(heads_from_bytes(&[]), None);
        assert_eq!(heads_from_bytes(b"not automerge at all"), None);
        // Trailing change chunk.
        assert_eq!(
            heads_from_bytes(&[bytes.as_slice(), &change].concat()),
            None
        );
        // A change chunk alone.
        assert_eq!(heads_from_bytes(&change), None);
        // Truncated.
        assert_eq!(heads_from_bytes(&bytes[..bytes.len() - 1]), None);
        // Oversized lengths never panic or allocate wildly.
        let mut huge = MAGIC.to_vec();
        huge.extend([0, 0, 0, 0, 0]);
        huge.extend([0xff; 9]);
        huge.push(0x01);
        assert_eq!(heads_from_bytes(&huge), None);
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0xff;
            let _ = heads_from_bytes(&b);
            let _ = heads_from_prefix(&b[..i], b.len());
        }
    }
}
