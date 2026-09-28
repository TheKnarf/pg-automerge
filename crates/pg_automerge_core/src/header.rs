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

/// Result of reading heads from (a prefix of) stored bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadsPrefix {
    /// The heads, in the order the header lists them (sorted by hash in
    /// practice, but callers must not rely on it).
    Found(Vec<ChangeHash>),
    /// The prefix ends before the heads do: at least this many bytes of the
    /// value (counted from its start) are needed.
    NeedMore(usize),
    /// Not a single document chunk; do a full load instead.
    NotSingleDoc,
}

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
    let mut r = Reader {
        prefix,
        total_len,
        pos: 0,
    };
    if r.take(4)? != MAGIC {
        return Err(Stop::NotSingleDoc);
    }
    r.take(4)?; // checksum
    if r.take(1)?[0] != DOCUMENT_CHUNK {
        return Err(Stop::NotSingleDoc);
    }
    let data_len = r.uleb_usize()?;
    // Exactly one chunk: the data runs to the end of the value.
    if r.pos.checked_add(data_len) != Some(total_len) {
        return Err(Stop::NotSingleDoc);
    }
    let actors = r.uleb_usize()?;
    for _ in 0..actors {
        let len = r.uleb_usize()?;
        r.take(len)?;
    }
    let count = r.uleb_usize()?;
    // Bound the allocation by what the value can hold.
    if count > total_len / 32 {
        return Err(Stop::NotSingleDoc);
    }
    let mut heads = Vec::with_capacity(count);
    for _ in 0..count {
        let bytes: [u8; 32] = r.take(32)?.try_into().map_err(|_| Stop::NotSingleDoc)?;
        heads.push(ChangeHash(bytes));
    }
    Ok(heads)
}

#[cfg(test)]
mod tests {
    use super::*;
    use automerge::transaction::Transactable;
    use automerge::{ActorId, AutoCommit, ROOT};

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
