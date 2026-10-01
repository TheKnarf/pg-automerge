//! Hand-built chunks whose run-length encoded columns describe far more
//! than they hold: the inputs of `tests/memory_bounds.rs` (from the
//! measurements in docs/DESIGN.md, "Input amplification measurements").
//!
//! Include with `#[path = "common/craft.rs"] mod craft;`.

#![allow(dead_code)]

use sha2::{Digest, Sha256};

pub fn uleb(out: &mut Vec<u8>, mut v: u64) {
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

pub fn sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0) {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// A column's bytes, built run by run.
#[derive(Default, Clone)]
pub struct Col(pub Vec<u8>);

impl Col {
    /// `n` copies of an unsigned value.
    pub fn run_u(mut self, n: u64, v: u64) -> Self {
        match n {
            0 => self,
            1 => self.lit_u(&[v]),
            _ => {
                sleb(&mut self.0, n as i64);
                uleb(&mut self.0, v);
                self
            }
        }
    }

    /// `n` copies of a signed value (in a delta column: `n` steps of `v`).
    pub fn run_s(mut self, n: u64, v: i64) -> Self {
        match n {
            0 => self,
            1 => self.lit_s(&[v]),
            _ => {
                sleb(&mut self.0, n as i64);
                sleb(&mut self.0, v);
                self
            }
        }
    }

    pub fn lit_u(mut self, vals: &[u64]) -> Self {
        sleb(&mut self.0, -(vals.len() as i64));
        for v in vals {
            uleb(&mut self.0, *v);
        }
        self
    }

    pub fn lit_s(mut self, vals: &[i64]) -> Self {
        sleb(&mut self.0, -(vals.len() as i64));
        for v in vals {
            sleb(&mut self.0, *v);
        }
        self
    }

    pub fn nulls(mut self, n: u64) -> Self {
        sleb(&mut self.0, 0);
        uleb(&mut self.0, n);
        self
    }

    /// `n` copies of a string (one literal for `n == 1`).
    pub fn run_str(mut self, n: u64, s: &str) -> Self {
        sleb(&mut self.0, if n == 1 { -1 } else { n as i64 });
        uleb(&mut self.0, s.len() as u64);
        self.0.extend(s.as_bytes());
        self
    }

    /// A boolean column: alternating run lengths, starting with false.
    pub fn bools(mut self, runs: &[u64]) -> Self {
        for r in runs {
            uleb(&mut self.0, *r);
        }
        self
    }
}

/// A chunk of `chunk_type` around `data`, with a valid checksum.
pub fn chunk(chunk_type: u8, data: &[u8]) -> Vec<u8> {
    let mut len = Vec::new();
    uleb(&mut len, data.len() as u64);
    let mut h = Sha256::new();
    h.update([chunk_type]);
    h.update(&len);
    h.update(data);
    let hash: [u8; 32] = h.finalize().into();
    let mut out = vec![0x85, 0x6f, 0x4a, 0x83];
    out.extend(&hash[..4]);
    out.push(chunk_type);
    out.extend(len);
    out.extend(data);
    out
}

/// Deflate `raw` (what Automerge's compressed columns and chunks hold).
pub fn deflate(raw: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(raw).unwrap();
    e.finish().unwrap()
}

/// A document chunk: actors, no heads, change and op columns (specs with
/// their data; an extra-bytes metadata column is added for `changes`
/// changes).
pub fn document(
    actors: u64,
    changes: u64,
    change_cols: Vec<(u64, Col)>,
    op_cols: Vec<(u64, Col)>,
) -> Vec<u8> {
    let actors: Vec<Vec<u8>> = (0..actors).map(actor).collect();
    document_with(&actors, changes, change_cols, op_cols, 0)
}

/// [`document`] with these actor ids, and `empty` column metadata entries
/// of spec 0x01 and length 0 listed before the op columns (Automerge keeps
/// every entry of a metadata block while it parses it).
pub fn document_with(
    actors: &[Vec<u8>],
    changes: u64,
    mut change_cols: Vec<(u64, Col)>,
    op_cols: Vec<(u64, Col)>,
    empty: u64,
) -> Vec<u8> {
    if !change_cols.iter().any(|(s, _)| *s & !0x08 == 0x56) {
        change_cols.push((0x56, Col::default().run_u(changes, 7)));
    }
    let mut data = Vec::new();
    uleb(&mut data, actors.len() as u64);
    for a in actors {
        uleb(&mut data, a.len() as u64);
        data.extend(a);
    }
    uleb(&mut data, 0); // heads
    metadata(&mut data, &change_cols, 0);
    metadata(&mut data, &op_cols, empty);
    for cols in [&change_cols, &op_cols] {
        for (_, c) in cols {
            data.extend(&c.0);
        }
    }
    chunk(0, &data)
}

/// A column metadata block: `empty` entries (0x01, length 0), then one
/// entry per column.
fn metadata(out: &mut Vec<u8>, cols: &[(u64, Col)], empty: u64) {
    uleb(out, cols.len() as u64 + empty);
    for _ in 0..empty {
        out.extend([0x01, 0x00]);
    }
    for (spec, c) in cols {
        uleb(out, *spec);
        uleb(out, c.0.len() as u64);
    }
}

/// The same columns, each deflated (the deflate bit set in its spec), as
/// Automerge's compressed saves hold them.
pub fn deflated(cols: Vec<(u64, Col)>) -> Vec<(u64, Col)> {
    cols.into_iter()
        .map(|(spec, c)| (spec | 0x08, Col(deflate(&c.0))))
        .collect()
}

/// A `len`-byte actor id (longer than the 16 bytes `ActorId` holds
/// inline): zeros, which sort before every [`actor`].
pub fn long_actor(len: usize) -> Vec<u8> {
    vec![0u8; len]
}

/// Actor `i`: 16 bytes.
pub fn actor(i: u64) -> Vec<u8> {
    let mut b = vec![0u8; 16];
    b[..8].copy_from_slice(&i.to_be_bytes());
    b[15] = 1;
    b
}

/// Change columns of one change by actor 0 (seq 1, no deps) ending at
/// op `max_op`.
fn one_change(max_op: u64) -> Vec<(u64, Col)> {
    vec![
        (0x01, Col::default().run_u(1, 0)),
        (0x03, Col::default().run_s(1, 1)),
        (0x13, Col::default().run_s(1, max_op as i64)),
        (0x23, Col::default().run_s(1, 0)),
        (0x40, Col::default().run_u(1, 0)),
    ]
}

/// Document op columns: op 1 makes a list at root key "l", ops 2..=n+1
/// insert null after the previous element (counters 1..=n+1).
fn list_ops(n: u64) -> Vec<(u64, Col)> {
    vec![
        (0x01, Col::default().nulls(1).run_u(n, 0)),
        (0x02, Col::default().nulls(1).run_u(n, 1)),
        (0x11, Col::default().nulls(2).run_u(n - 1, 0)),
        (0x13, Col::default().nulls(1).lit_s(&[0, 2]).run_s(n - 2, 1)),
        (0x15, Col::default().run_str(1, "l").nulls(n)),
        (0x21, Col::default().run_u(n + 1, 0)),
        (0x23, Col::default().run_s(n + 1, 1)),
        (0x34, Col::default().bools(&[1, n])),
        (0x42, Col::default().lit_u(&[2]).run_u(n, 1)),
        (0x56, Col::default().run_u(n + 1, 0)),
        (0x80, Col::default().run_u(n + 1, 0)),
    ]
}

/// A document of one change that makes a list of `n` nulls (`n >= 3`):
/// loads fine.
pub fn list(n: u64) -> Vec<u8> {
    document(1, 1, one_change(n + 1), list_ops(n))
}

/// A document of `n` empty changes of one actor, each depending on the
/// previous one (heads missing: rejected after reconstruction).
pub fn empty_changes(n: u64) -> Vec<u8> {
    document(
        1,
        n,
        vec![
            (0x01, Col::default().run_u(n, 0)),
            (0x03, Col::default().run_s(n, 1)),
            (0x13, Col::default().run_s(n, 0)),
            (0x23, Col::default().run_s(n, 0)),
            (0x40, Col::default().lit_u(&[0]).run_u(n - 1, 1)),
            (0x43, Col::default().lit_s(&[0]).run_s(n - 2, 1)),
        ],
        vec![],
    )
}

/// The change columns of `n` empty changes of actor 0, each depending on
/// the previous one.
fn chained_changes(n: u64) -> Vec<(u64, Col)> {
    vec![
        (0x01, Col::default().run_u(n, 0)),
        (0x03, Col::default().run_s(n, 1)),
        (0x13, Col::default().run_s(n, 0)),
        (0x23, Col::default().run_s(n, 0)),
        (0x40, Col::default().lit_u(&[0]).run_u(n - 1, 1)),
        (0x43, Col::default().lit_s(&[0]).run_s(n - 2, 1)),
    ]
}

/// [`empty_changes`] whose change messages are one repeat run of a
/// `len`-byte string (Automerge rebuilds every change with its own copy
/// of the message; rejected after reconstruction). `deflate`: the columns
/// deflated, as in a compressed save.
pub fn repeated_messages(n: u64, len: usize, deflate: bool) -> Vec<u8> {
    let mut cols = chained_changes(n);
    cols.insert(4, (0x35, Col::default().run_str(n, &"m".repeat(len))));
    cols.push((0x56, Col::default().run_u(n, 7)));
    if deflate {
        cols = deflated(cols);
    }
    document_with(&[actor(0)], n, cols, vec![], 0)
}

/// [`empty_changes`] of one actor whose id is `len` bytes long (every
/// rebuilt change holds the id; rejected after reconstruction).
pub fn long_actor_changes(n: u64, len: usize) -> Vec<u8> {
    document_with(&[long_actor(len)], n, chained_changes(n), vec![], 0)
}

/// A document where actor 0, whose id is `len` bytes long, makes a map
/// (op 1) and `n` chained changes of actor 1 (16 bytes) each put key "k"
/// in it: every rebuilt change lists the long actor among its other
/// actors (rejected after reconstruction).
pub fn long_actor_refs(n: u64, len: usize) -> Vec<u8> {
    let changes = vec![
        (0x01, Col::default().lit_u(&[0]).run_u(n, 1)),
        (0x03, Col::default().lit_s(&[1, 0]).run_s(n - 1, 1)),
        (0x13, Col::default().run_s(n + 1, 1)),
        (0x23, Col::default().run_s(n + 1, 0)),
        (0x40, Col::default().lit_u(&[0]).run_u(n, 1)),
        (0x43, Col::default().lit_s(&[0]).run_s(n - 1, 1)),
    ];
    let ops = vec![
        (0x01, Col::default().nulls(1).run_u(n, 0)),
        (0x02, Col::default().nulls(1).run_u(n, 1)),
        (0x15, Col::default().run_str(1, "m").run_str(n, "k")),
        (0x21, Col::default().lit_u(&[0]).run_u(n, 1)),
        (0x23, Col::default().run_s(n + 1, 1)),
        (0x34, Col::default().bools(&[n + 1])),
        (0x42, Col::default().lit_u(&[0]).run_u(n, 1)),
        (0x56, Col::default().run_u(n + 1, 0)),
        (0x80, Col::default().run_u(n + 1, 0)),
    ];
    document_with(&[long_actor(len), actor(1)], n + 1, changes, ops, 0)
}

/// A document of `n` chained changes of actor 0, each putting the same
/// `len`-byte key at the root (a repeat run of the key column): every
/// rebuilt change holds its own copy of the key (rejected after
/// reconstruction).
pub fn repeated_doc_keys(n: u64, len: usize) -> Vec<u8> {
    let mut changes = chained_changes(n);
    changes[2] = (0x13, Col::default().run_s(n, 1));
    let ops = vec![
        (0x15, Col::default().run_str(n, &"k".repeat(len))),
        (0x21, Col::default().run_u(n, 0)),
        (0x23, Col::default().run_s(n, 1)),
        (0x34, Col::default().bools(&[n])),
        (0x42, Col::default().run_u(n, 1)),
        (0x56, Col::default().run_u(n, 0)),
        (0x80, Col::default().run_u(n, 0)),
    ];
    document_with(&[actor(0)], n, changes, ops, 0)
}

/// A document of 1000 changes, each after the first listing `n`
/// dependencies (rejected after reconstruction).
pub fn deps(n: u64) -> Vec<u8> {
    let c = 1000u64;
    document(
        1,
        c,
        vec![
            (0x01, Col::default().run_u(c, 0)),
            (0x03, Col::default().run_s(c, 1)),
            (0x13, Col::default().run_s(c, 0)),
            (0x23, Col::default().run_s(c, 0)),
            (0x40, Col::default().lit_u(&[0]).run_u(c - 1, n)),
            (0x43, Col::default().run_s((c - 1) * n, 0)),
        ],
        vec![],
    )
}

/// A document of one change: a list of two nulls, the last one with `n`
/// successor entries (deletes by ops that do not exist; rejected after
/// reconstruction).
pub fn succ(n: u64) -> Vec<u8> {
    let mut ops = list_ops(2);
    ops.retain(|(s, _)| *s != 0x80);
    ops.push((0x80, Col::default().run_u(2, 0).lit_u(&[n])));
    ops.push((0x81, Col::default().run_u(n, 0)));
    ops.push((0x83, Col::default().lit_s(&[4]).run_s(n - 1, 1)));
    document(1, 1, one_change(n + 3), ops)
}

/// A document of one change with `n` actors (a list of two nulls): loads.
pub fn actors(n: u64) -> Vec<u8> {
    document(n, 1, one_change(3), list_ops(2))
}

/// A document chunk whose value column is deflated and inflates to `n`
/// zero bytes (a deflate bomb).
pub fn deflated_column(n: u64) -> Vec<u8> {
    let mut ops = list_ops(2);
    ops.retain(|(s, _)| *s != 0x80);
    ops.insert(10, (0x57 | 0x08, Col(deflate(&vec![0u8; n as usize]))));
    ops.push((0x80, Col::default().run_u(3, 0)));
    document(1, 1, one_change(3), ops)
}

/// One change chunk of actor 0 (seq 1, start op 1, no deps) with these
/// op columns.
pub fn change_chunk(cols: &[(u64, Col)]) -> Vec<u8> {
    chunk(1, &change_data(0, 1, 1, &[], cols))
}

/// The data of a change chunk of actor `a`, seq `seq`, starting at op
/// `start_op`, depending on `deps`.
pub fn change_data(
    a: u64,
    seq: u64,
    start_op: u64,
    deps: &[[u8; 32]],
    cols: &[(u64, Col)],
) -> Vec<u8> {
    let mut data = Vec::new();
    uleb(&mut data, deps.len() as u64);
    for d in deps {
        data.extend(d);
    }
    let actor = actor(a);
    uleb(&mut data, actor.len() as u64);
    data.extend(&actor);
    uleb(&mut data, seq);
    uleb(&mut data, start_op);
    sleb(&mut data, 0); // time
    uleb(&mut data, 0); // message
    uleb(&mut data, 0); // other actors
    uleb(&mut data, cols.len() as u64);
    for (spec, c) in cols {
        uleb(&mut data, *spec);
        uleb(&mut data, c.0.len() as u64);
    }
    for (_, c) in cols {
        data.extend(&c.0);
    }
    data
}

/// Change op columns: op 1 makes a list at root key "l", ops 2..=n+1
/// insert null after the previous element (no preds).
pub fn list_change_ops(n: u64) -> Vec<(u64, Col)> {
    vec![
        (0x01, Col::default().nulls(1).run_u(n, 0)),
        (0x02, Col::default().nulls(1).run_u(n, 1)),
        (0x11, Col::default().nulls(2).run_u(n - 1, 0)),
        (0x13, Col::default().nulls(1).lit_s(&[0, 2]).run_s(n - 2, 1)),
        (0x15, Col::default().run_str(1, "l").nulls(n)),
        (0x34, Col::default().bools(&[1, n])),
        (0x42, Col::default().lit_u(&[2]).run_u(n, 1)),
        (0x56, Col::default().run_u(n + 1, 0)),
        (0x70, Col::default().run_u(n + 1, 0)),
    ]
}

/// A change chunk making a list of `n` nulls: loads fine.
pub fn change_ops(n: u64) -> Vec<u8> {
    change_chunk(&list_change_ops(n))
}

/// A change chunk of two puts of root key "k", the second with `n` preds
/// (loads fine).
pub fn change_preds(n: u64) -> Vec<u8> {
    change_chunk(&[
        (0x15, Col::default().run_str(2, "k")),
        (0x34, Col::default().bools(&[2])),
        (0x42, Col::default().run_u(2, 1)),
        (0x56, Col::default().run_u(2, 0)),
        (0x70, Col::default().lit_u(&[0, n])),
        (0x71, Col::default().run_u(n, 0)),
        (0x73, Col::default().lit_s(&[1]).run_s(n - 1, 0)),
    ])
}

/// A compressed change chunk (type 2) whose data inflates to `n` zero
/// bytes (a deflate bomb; not a valid change).
pub fn compressed_bomb(n: u64) -> Vec<u8> {
    let raw = vec![0u8; n as usize];
    let mut out = chunk(1, &raw);
    // Same checksum (Automerge's compressed chunks carry the checksum of
    // the uncompressed change), type 2, deflated data.
    let deflated = deflate(&raw);
    let mut len = Vec::new();
    uleb(&mut len, deflated.len() as u64);
    out.truncate(8);
    out.push(2);
    out.extend(len);
    out.extend(deflated);
    out
}

/// The data of an empty change chunk of actor 0 (seq 1, start op 1, no
/// ops) whose header lists `deps` dependencies (all the same hash) and
/// `others` other actors, each `other_len` bytes (all the same): the
/// length-prefixed lists Automerge parses into vectors, one entry per
/// listed item, duplicates included.
pub fn listing_data(deps: u64, others: u64, other_len: usize) -> Vec<u8> {
    let mut data = Vec::new();
    uleb(&mut data, deps);
    for _ in 0..deps {
        data.extend([7u8; 32]);
    }
    let a = actor(0);
    uleb(&mut data, a.len() as u64);
    data.extend(&a);
    uleb(&mut data, 1); // seq
    uleb(&mut data, 1); // start op
    sleb(&mut data, 0); // time
    uleb(&mut data, 0); // message
    uleb(&mut data, others);
    let other = vec![3u8; other_len];
    for _ in 0..others {
        uleb(&mut data, other_len as u64);
        data.extend(&other);
    }
    uleb(&mut data, 0); // no columns
    data
}

/// A change chunk of [`listing_data`].
pub fn listing(deps: u64, others: u64, other_len: usize) -> Vec<u8> {
    chunk(1, &listing_data(deps, others, other_len))
}

/// The same change as a compressed chunk (type 2: the checksum of the
/// uncompressed chunk, deflated data).
pub fn compressed_listing(deps: u64, others: u64, other_len: usize) -> Vec<u8> {
    compress(&listing_data(deps, others, other_len))
}

/// A compressed change chunk (type 2) of the change chunk data `raw`.
pub fn compress(raw: &[u8]) -> Vec<u8> {
    let mut out = chunk(1, raw);
    let deflated = deflate(raw);
    let mut len = Vec::new();
    uleb(&mut len, deflated.len() as u64);
    out.truncate(8);
    out.push(2);
    out.extend(len);
    out.extend(deflated);
    out
}

/// The data of a change chunk (actor 0, seq 1, start op 1, no deps) with
/// `empty` column metadata entries (0x01, length 0) before its columns.
pub fn change_data_with(cols: &[(u64, Col)], empty: u64) -> Vec<u8> {
    let mut data = Vec::new();
    uleb(&mut data, 0); // deps
    let a = actor(0);
    uleb(&mut data, a.len() as u64);
    data.extend(&a);
    uleb(&mut data, 1); // seq
    uleb(&mut data, 1); // start op
    sleb(&mut data, 0); // time
    uleb(&mut data, 0); // message
    uleb(&mut data, 0); // other actors
    metadata(&mut data, cols, empty);
    for (_, c) in cols {
        data.extend(&c.0);
    }
    data
}

/// A change chunk of `n` puts of the same `len`-byte key at the root (one
/// repeat run of the key column): applying it makes one owned key per op.
pub fn repeated_keys(n: u64, len: usize) -> Vec<u8> {
    change_chunk(&[
        (0x15, Col::default().run_str(n, &"k".repeat(len))),
        (0x34, Col::default().bools(&[n])),
        (0x42, Col::default().run_u(n, 1)),
        (0x56, Col::default().run_u(n, 0)),
        (0x70, Col::default().run_u(n, 0)),
    ])
}

/// A change chunk making a text (op 1) and inserting `n` mark ops into
/// it whose names are one repeat run of a `len`-byte string: applying it
/// makes one owned name per op.
pub fn repeated_mark_names(n: u64, len: usize) -> Vec<u8> {
    change_chunk(&[
        (0x01, Col::default().nulls(1).run_u(n, 0)),
        (0x02, Col::default().nulls(1).run_u(n, 1)),
        (0x11, Col::default().nulls(n + 1)),
        (0x13, Col::default().nulls(1).run_s(n, 0)),
        (0x15, Col::default().run_str(1, "t").nulls(n)),
        (0x34, Col::default().bools(&[1, n])),
        (0x42, Col::default().lit_u(&[4]).run_u(n, 7)),
        (0x56, Col::default().run_u(n + 1, 0)),
        (0x70, Col::default().run_u(n + 1, 0)),
        (0x94, Col::default().bools(&[n + 1])),
        (0xa5, Col::default().nulls(1).run_str(n, &"n".repeat(len))),
    ])
}

/// A change chunk making `m` maps at root key "a", then putting a
/// `len`-byte key in each (one repeat run), then key "b" in each: the
/// document orders each map's keys "b" before the long one, so its key
/// column holds the long key `m` times, literally.
pub fn interleaved_keys(m: u64, len: usize) -> Vec<u8> {
    let ctrs: Vec<u64> = (1..=m).collect();
    change_chunk(&[
        (0x01, Col::default().nulls(m).run_u(2 * m, 0)),
        (0x02, Col::default().nulls(m).lit_u(&ctrs).lit_u(&ctrs)),
        (
            0x15,
            Col::default()
                .run_str(m, "a")
                .run_str(m, &"k".repeat(len))
                .run_str(m, "b"),
        ),
        (0x34, Col::default().bools(&[3 * m])),
        (0x42, Col::default().run_u(m, 0).run_u(2 * m, 1)),
        (0x56, Col::default().run_u(3 * m, 0)),
        (0x70, Col::default().run_u(3 * m, 0)),
    ])
}

/// A document chunk making a list of 4 nulls with `n` empty column
/// metadata entries before its op columns.
pub fn doc_columns(n: u64) -> Vec<u8> {
    document_with(&[actor(0)], 1, one_change(5), list_ops(4), n)
}

/// A change chunk making a list of 4 nulls with `n` empty column metadata
/// entries before its columns.
pub fn change_columns(n: u64) -> Vec<u8> {
    chunk(1, &change_data_with(&list_change_ops(4), n))
}

/// [`change_columns`] as a compressed change chunk.
pub fn compressed_change_columns(n: u64) -> Vec<u8> {
    compress(&change_data_with(&list_change_ops(4), n))
}

/// Read a ULEB128 at `*at`, advancing it.
fn read_uleb(bytes: &[u8], at: &mut usize) -> u64 {
    let (mut v, mut shift) = (0u64, 0);
    loop {
        let b = bytes[*at];
        *at += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

/// A document chunk built by [`document_with`] (no heads) listing one
/// made-up head instead: a load of it after another document chunk
/// rebuilds its changes and applies them (Automerge does not check a
/// later chunk's changes against its heads), where a chunk without heads
/// is skipped as already contained.
pub fn with_head(doc: &[u8]) -> Vec<u8> {
    assert_eq!(doc[8], 0, "a document chunk");
    let mut at = 9;
    let len = read_uleb(doc, &mut at) as usize;
    let data = &doc[at..];
    assert_eq!(data.len(), len);
    let mut at = 0;
    for _ in 0..read_uleb(data, &mut at) {
        let n = read_uleb(data, &mut at) as usize;
        at += n;
    }
    assert_eq!(data[at], 0, "no heads");
    let mut out = data[..at].to_vec();
    out.push(1);
    out.extend([0x5a; 32]);
    out.extend(&data[at + 1..]);
    chunk(0, &out)
}

// ---------------------------------------------------------------------------
// Neighbouring shapes of the string and actor terms: every string-valued
// column as one repeat run, many distinct strings, and runs interleaved
// with literals; long actor ids from every actor column.
// ---------------------------------------------------------------------------

/// How the `n` strings of a column are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strings {
    /// One repeat run: `n` copies of one string, stored once.
    Run,
    /// One literal run of `n` different strings, each stored.
    Distinct,
    /// Groups of ten: a repeat run of nine copies of one string, then a
    /// different string as a literal (`n` a multiple of ten).
    Interleaved,
}

impl Strings {
    pub const ALL: [Strings; 3] = [Strings::Run, Strings::Distinct, Strings::Interleaved];
}

/// A `len`-byte string, different for every `i` (its decimal digits at
/// the end of `fill` bytes; `len` at least 8).
pub fn nth_string(fill: char, i: u64, len: usize) -> String {
    let digits = i.to_string();
    assert!(len >= digits.len(), "{len} bytes cannot hold {i}");
    let mut s: String = std::iter::repeat_n(fill, len - digits.len()).collect();
    s.push_str(&digits);
    s
}

/// The `n` strings of `shape`, in order (what the column describes).
pub fn string_values(shape: Strings, n: u64, len: usize) -> Vec<String> {
    match shape {
        Strings::Run => vec![nth_string('s', 0, len); n as usize],
        Strings::Distinct => (0..n).map(|i| nth_string('s', i, len)).collect(),
        Strings::Interleaved => (0..n)
            .map(|i| {
                if i % 10 == 9 {
                    nth_string('t', i, len)
                } else {
                    nth_string('s', i / 10, len)
                }
            })
            .collect(),
    }
}

impl Col {
    /// One literal run of these strings.
    pub fn lit_str<S: AsRef<str>>(mut self, vals: &[S]) -> Self {
        sleb(&mut self.0, -(vals.len() as i64));
        for v in vals {
            uleb(&mut self.0, v.as_ref().len() as u64);
            self.0.extend(v.as_ref().as_bytes());
        }
        self
    }

    /// `n` strings of `len` bytes laid out as `shape` says.
    pub fn strings(self, shape: Strings, n: u64, len: usize) -> Self {
        match shape {
            Strings::Run => self.run_str(n, &nth_string('s', 0, len)),
            Strings::Distinct => self.lit_str(&string_values(shape, n, len)),
            Strings::Interleaved => {
                assert_eq!(n % 10, 0, "groups of ten");
                (0..n / 10).fold(self, |c, g| {
                    c.run_str(9, &nth_string('s', g, len)).lit_str(&[nth_string(
                        't',
                        g * 10 + 9,
                        len,
                    )])
                })
            }
        }
    }
}

/// A value metadata column and its raw column for these values (type 6,
/// string; type 7, bytes): the metadata run-length encoded, the raw
/// column every value's bytes (it is not run-length encoded).
pub fn value_cols(values: &[String], ty: u64) -> (Col, Col) {
    let mut meta = Col::default();
    let mut raw = Col::default();
    let mut i = 0;
    while i < values.len() {
        let len = values[i].len();
        let run = values[i..].iter().take_while(|v| v.len() == len).count();
        meta = meta.run_u(run as u64, ((len as u64) << 4) | ty);
        i += run;
    }
    for v in values {
        raw.0.extend(v.as_bytes());
    }
    (meta, raw)
}

/// The change columns of `n` empty changes of actor 0 (chained), every
/// change a single op ending at op `i + 1` (`ops`), or none.
fn chain(n: u64, ops: bool) -> Vec<(u64, Col)> {
    let mut cols = chained_changes(n);
    if ops {
        cols[2] = (0x13, Col::default().run_s(n, 1));
    }
    cols
}

/// Op columns of `n` ops of actor 0, op `i + 1` putting `values` (or
/// null) at root key `keys[i]` (each a column of `n` strings).
fn put_ops(keys: Col, values: Option<&[String]>, n: u64) -> Vec<(u64, Col)> {
    let mut ops = vec![
        (0x15, keys),
        (0x21, Col::default().run_u(n, 0)),
        (0x23, Col::default().run_s(n, 1)),
        (0x34, Col::default().bools(&[n])),
        (0x42, Col::default().run_u(n, 1)),
    ];
    match values {
        Some(values) => {
            let (meta, raw) = value_cols(values, 6);
            ops.push((0x56, meta));
            ops.push((0x57, raw));
        }
        None => ops.push((0x56, Col::default().run_u(n, 0))),
    }
    ops.push((0x80, Col::default().run_u(n, 0)));
    ops
}

/// The deflated form of these columns when `deflate` (as a compressed
/// save holds them).
fn maybe_deflated(cols: Vec<(u64, Col)>, deflate: bool) -> Vec<(u64, Col)> {
    if deflate { deflated(cols) } else { cols }
}

/// A document chunk of `n` chained empty changes of actor 0 whose
/// messages are `n` strings of `len` bytes as `shape` lays them out
/// (rejected after reconstruction: no heads).
pub fn doc_messages(shape: Strings, n: u64, len: usize, deflate: bool) -> Vec<u8> {
    let mut cols = chained_changes(n);
    cols.insert(4, (0x35, Col::default().strings(shape, n, len)));
    document_with(&[actor(0)], n, maybe_deflated(cols, deflate), vec![], 0)
}

/// A document chunk of `n` chained empty changes of actor 0 whose extra
/// bytes (the change columns' value pair 0x56/0x57) are `n` strings of
/// `len` bytes as `shape` lays them out (every one is in the raw column).
pub fn doc_extra(shape: Strings, n: u64, len: usize, deflate: bool) -> Vec<u8> {
    let mut cols = chained_changes(n);
    let (meta, raw) = value_cols(&string_values(shape, n, len), 7);
    cols.push((0x56, meta));
    cols.push((0x57, raw));
    document_with(&[actor(0)], n, maybe_deflated(cols, deflate), vec![], 0)
}

/// A document chunk of `n` chained changes of actor 0, each putting null
/// at a root key; the keys are `n` strings of `len` bytes as `shape` lays
/// them out (rejected after reconstruction).
pub fn doc_keys(shape: Strings, n: u64, len: usize, deflate: bool) -> Vec<u8> {
    let ops = put_ops(Col::default().strings(shape, n, len), None, n);
    document_with(
        &[actor(0)],
        n,
        maybe_deflated(chain(n, true), deflate),
        maybe_deflated(ops, deflate),
        0,
    )
}

/// A document chunk of `n` chained changes of actor 0, each putting a
/// string value at root key "k" (conflicting: no successors); the values
/// are `n` strings of `len` bytes as `shape` lays them out (rejected
/// after reconstruction).
pub fn doc_values(shape: Strings, n: u64, len: usize, deflate: bool) -> Vec<u8> {
    let values = string_values(shape, n, len);
    let ops = put_ops(Col::default().run_str(n, "k"), Some(&values), n);
    document_with(
        &[actor(0)],
        n,
        maybe_deflated(chain(n, true), deflate),
        maybe_deflated(ops, deflate),
        0,
    )
}

/// A document chunk of `n + 1` chained changes of actor 0: the first
/// makes a text at root key "t" (op 1), each other one marks it (ops 2 to
/// `n + 1`, in document order latest first); the mark names are `n`
/// strings of `len` bytes as `shape` lays them out (rejected after
/// reconstruction).
pub fn doc_mark_names(shape: Strings, n: u64, len: usize, deflate: bool) -> Vec<u8> {
    let changes = chain(n + 1, true);
    let ops = vec![
        (0x01, Col::default().nulls(1).run_u(n, 0)),
        (0x02, Col::default().nulls(1).run_u(n, 1)),
        (0x11, Col::default().nulls(n + 1)),
        (0x13, Col::default().nulls(1).run_s(n, 0)),
        (0x15, Col::default().run_str(1, "t").nulls(n)),
        (0x21, Col::default().run_u(n + 1, 0)),
        (0x23, Col::default().lit_s(&[1, n as i64]).run_s(n - 1, -1)),
        (0x34, Col::default().bools(&[1, n])),
        (0x42, Col::default().lit_u(&[4]).run_u(n, 7)),
        (0x56, Col::default().run_u(n + 1, 0)),
        (0x80, Col::default().run_u(n + 1, 0)),
        (0x94, Col::default().bools(&[n + 1])),
        (0xa5, Col::default().nulls(1).strings(shape, n, len)),
    ];
    document_with(
        &[actor(0)],
        n + 1,
        maybe_deflated(changes, deflate),
        maybe_deflated(ops, deflate),
        0,
    )
}

/// The data of a change chunk: actor `actor` (its id), seq `seq`, start
/// op `start_op`, depending on `deps`, with this message, these other
/// actors, op columns and trailing extra bytes.
#[allow(clippy::too_many_arguments)]
pub fn change_data_full(
    actor: &[u8],
    seq: u64,
    start_op: u64,
    deps: &[[u8; 32]],
    message: &str,
    others: &[Vec<u8>],
    cols: &[(u64, Col)],
    extra: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();
    uleb(&mut data, deps.len() as u64);
    for d in deps {
        data.extend(d);
    }
    uleb(&mut data, actor.len() as u64);
    data.extend(actor);
    uleb(&mut data, seq);
    uleb(&mut data, start_op);
    sleb(&mut data, 0); // time
    uleb(&mut data, message.len() as u64);
    data.extend(message.as_bytes());
    uleb(&mut data, others.len() as u64);
    for o in others {
        uleb(&mut data, o.len() as u64);
        data.extend(o);
    }
    metadata(&mut data, cols, 0);
    for (_, c) in cols {
        data.extend(&c.0);
    }
    data.extend(extra);
    data
}

/// The hash of a change chunk of these data (its id, what other changes
/// list as a dependency).
pub fn change_hash(data: &[u8]) -> [u8; 32] {
    let mut len = Vec::new();
    uleb(&mut len, data.len() as u64);
    let mut h = Sha256::new();
    h.update([1u8]);
    h.update(&len);
    h.update(data);
    h.finalize().into()
}

/// Change op columns of `n` puts of null at root keys that are `n`
/// strings of `len` bytes as `shape` lays them out (no preds).
pub fn change_key_ops(shape: Strings, n: u64, len: usize) -> Vec<(u64, Col)> {
    vec![
        (0x15, Col::default().strings(shape, n, len)),
        (0x34, Col::default().bools(&[n])),
        (0x42, Col::default().run_u(n, 1)),
        (0x56, Col::default().run_u(n, 0)),
        (0x70, Col::default().run_u(n, 0)),
    ]
}

/// Change op columns of `n` puts at root key "k" of string values that
/// are `n` strings of `len` bytes as `shape` lays them out (no preds).
pub fn change_value_ops(shape: Strings, n: u64, len: usize) -> Vec<(u64, Col)> {
    let (meta, raw) = value_cols(&string_values(shape, n, len), 6);
    vec![
        (0x15, Col::default().run_str(n, "k")),
        (0x34, Col::default().bools(&[n])),
        (0x42, Col::default().run_u(n, 1)),
        (0x56, meta),
        (0x57, raw),
        (0x70, Col::default().run_u(n, 0)),
    ]
}

/// Change op columns making a text (op 1) and inserting `n` mark ops
/// whose names are `n` strings of `len` bytes as `shape` lays them out.
pub fn change_mark_ops(shape: Strings, n: u64, len: usize) -> Vec<(u64, Col)> {
    vec![
        (0x01, Col::default().nulls(1).run_u(n, 0)),
        (0x02, Col::default().nulls(1).run_u(n, 1)),
        (0x11, Col::default().nulls(n + 1)),
        (0x13, Col::default().nulls(1).run_s(n, 0)),
        (0x15, Col::default().run_str(1, "t").nulls(n)),
        (0x34, Col::default().bools(&[1, n])),
        (0x42, Col::default().lit_u(&[4]).run_u(n, 7)),
        (0x56, Col::default().run_u(n + 1, 0)),
        (0x70, Col::default().run_u(n + 1, 0)),
        (0x94, Col::default().bools(&[n + 1])),
        (0xa5, Col::default().nulls(1).strings(shape, n, len)),
    ]
}

/// One change chunk of actor 0 (seq 1, start op 1, no deps) with these op
/// columns, plain or compressed.
pub fn change_of(cols: &[(u64, Col)], compressed: bool) -> Vec<u8> {
    let data = change_data_full(&actor(0), 1, 1, &[], "", &[], cols, &[]);
    if compressed {
        compress(&data)
    } else {
        chunk(1, &data)
    }
}

/// `m` change chunks of actor 0 (seq 1 to `m`, each depending on the
/// one before), each putting null at root key "k" (one op), with message
/// `message(i)` and extra bytes `extra(i)`; plain or compressed.
pub fn change_chain(
    m: u64,
    message: impl Fn(u64) -> String,
    extra: impl Fn(u64) -> Vec<u8>,
    compressed: bool,
) -> Vec<u8> {
    let ops = [
        (0x15, Col::default().run_str(1, "k")),
        (0x34, Col::default().bools(&[1])),
        (0x42, Col::default().run_u(1, 1)),
        (0x56, Col::default().run_u(1, 0)),
        (0x70, Col::default().run_u(1, 0)),
    ];
    let mut out = Vec::new();
    let mut deps: Vec<[u8; 32]> = Vec::new();
    for i in 0..m {
        let data = change_data_full(
            &actor(0),
            i + 1,
            i + 1,
            &deps,
            &message(i),
            &[],
            &ops,
            &extra(i),
        );
        deps = vec![change_hash(&data)];
        out.extend(if compressed {
            compress(&data)
        } else {
            chunk(1, &data)
        });
    }
    out
}

/// A document chunk of `n` chained changes by an actor whose id is `len`
/// bytes long, each putting null at root key "k" (op ids of the long
/// actor; rejected after reconstruction).
pub fn long_actor_ops(n: u64, len: usize) -> Vec<u8> {
    let ops = put_ops(Col::default().run_str(n, "k"), None, n);
    document_with(&[long_actor(len)], n, chain(n, true), ops, 0)
}

/// A document chunk where actor 1 (16 bytes) makes a list at root key
/// "l" (op 1), actor 0, whose id is `len` bytes long, inserts an element
/// (op 2), and `n` chained changes of actor 1 each insert after that
/// element (ops 3 to `n + 2`): the key actor column refers to the long
/// actor `n` times (rejected after reconstruction).
pub fn long_actor_keys(n: u64, len: usize) -> Vec<u8> {
    let changes = vec![
        (0x01, Col::default().lit_u(&[1, 0]).run_u(n, 1)),
        (0x03, Col::default().lit_s(&[1, 0, 1]).run_s(n - 1, 1)),
        (0x13, Col::default().run_s(n + 2, 1)),
        (0x23, Col::default().run_s(n + 2, 0)),
        (0x40, Col::default().lit_u(&[0]).run_u(n + 1, 1)),
        (0x43, Col::default().lit_s(&[0]).run_s(n, 1)),
    ];
    // Document order: the list, its element, then the inserts after it,
    // latest first.
    let ops = vec![
        (0x01, Col::default().nulls(1).run_u(n + 1, 1)),
        (0x02, Col::default().nulls(1).run_u(n + 1, 1)),
        (0x11, Col::default().nulls(2).run_u(n, 0)),
        (0x13, Col::default().nulls(1).lit_s(&[0, 2]).run_s(n - 1, 0)),
        (0x15, Col::default().run_str(1, "l").nulls(n + 1)),
        (0x21, Col::default().lit_u(&[1, 0]).run_u(n, 1)),
        (
            0x23,
            Col::default()
                .run_s(2, 1)
                .lit_s(&[n as i64])
                .run_s(n - 1, -1),
        ),
        (0x34, Col::default().bools(&[1, n + 1])),
        (0x42, Col::default().lit_u(&[2]).run_u(n + 1, 1)),
        (0x56, Col::default().run_u(n + 2, 0)),
        (0x80, Col::default().run_u(n + 2, 0)),
    ];
    document_with(&[long_actor(len), actor(1)], n + 2, changes, ops, 0)
}

/// A document chunk of `n` keys, each put first by one actor and then
/// overwritten by another (the second op a successor of the first): with
/// `long_first`, one change of actor 0, whose id is `len` bytes long, puts
/// them all and `n` chained changes of actor 1 (16 bytes) overwrite one
/// each (every one of those refers to the long actor by its pred, and
/// the successor actor column holds actor 1); otherwise the roles are
/// swapped (the successor actor column holds the long actor). Rejected
/// after reconstruction.
pub fn long_actor_succ(n: u64, len: usize, long_first: bool) -> Vec<u8> {
    let (first, second) = if long_first { (0, 1) } else { (1, 0) };
    let keys: Vec<String> = (0..n).map(|i| nth_string('k', i, 8)).collect();
    let changes = vec![
        (0x01, Col::default().lit_u(&[first]).run_u(n, second)),
        (0x03, Col::default().lit_s(&[1, 0]).run_s(n - 1, 1)),
        (0x13, Col::default().lit_s(&[n as i64]).run_s(n, 1)),
        (0x23, Col::default().run_s(n + 1, 0)),
        (0x40, Col::default().lit_u(&[0]).run_u(n, 1)),
        (0x43, Col::default().lit_s(&[0]).run_s(n - 1, 1)),
    ];
    let mut key_col = Col::default();
    let mut id_actor = Vec::new();
    let mut id_ctr = Vec::new();
    let mut succ = Vec::new();
    let mut prev = 0i64;
    for (i, k) in keys.iter().enumerate() {
        key_col = key_col.run_str(2, k);
        let (a, b) = (i as i64 + 1, n as i64 + 1 + i as i64);
        id_actor.extend([first, second]);
        id_ctr.extend([a - prev, b - a]);
        prev = b;
        succ.extend([1, 0]);
    }
    let ops = vec![
        (0x15, key_col),
        (0x21, Col::default().lit_u(&id_actor)),
        (0x23, Col::default().lit_s(&id_ctr)),
        (0x34, Col::default().bools(&[2 * n])),
        (0x42, Col::default().run_u(2 * n, 1)),
        (0x56, Col::default().run_u(2 * n, 0)),
        (0x80, Col::default().lit_u(&succ)),
        (0x81, Col::default().run_u(n, second)),
        (0x83, Col::default().lit_s(&[n as i64 + 1]).run_s(n - 1, 1)),
    ];
    document_with(&[long_actor(len), actor(1)], n + 1, changes, ops, 0)
}

/// A document chunk of one change (a list of two nulls) whose actor table
/// also lists `m` other actors of `len` bytes that nothing refers to.
pub fn long_actor_table(m: u64, len: usize) -> Vec<u8> {
    let mut actors = vec![actor(0)];
    for i in 0..m {
        let mut a = vec![0xffu8; len];
        a[..8].copy_from_slice(&i.to_be_bytes());
        a[0] = 0xff;
        a[len - 8..].copy_from_slice(&i.to_be_bytes());
        actors.push(a);
    }
    document_with(&actors, 1, one_change(3), list_ops(2), 0)
}

/// A change chunk by an actor whose id is `len` bytes long (seq 1, no
/// deps) making a list of `n` nulls.
pub fn long_actor_change(n: u64, len: usize, compressed: bool) -> Vec<u8> {
    let data = change_data_full(
        &long_actor(len),
        1,
        1,
        &[],
        "",
        &[],
        &list_change_ops(n),
        &[],
    );
    if compressed {
        compress(&data)
    } else {
        chunk(1, &data)
    }
}

/// A document by an actor whose id is `len` bytes long (a map at root
/// key "m", op 1; a list at "l", op 2, with one element, op 3; root key
/// "k" put, op 4), and a change chunk of actor 0 (16 bytes; seq 1,
/// depending on that document's head) listing the long actor among its
/// other actors, whose `n` ops refer to it from `column`: "obj" (puts
/// into the map), "key" (inserts after the list's element), "pred" (puts
/// of root key "k" overwriting op 4); or `n` other actors of `len` bytes,
/// all different, that no op refers to ("others"; its ops make a list of
/// three nulls). Returns the document's save and the change chunk.
pub fn long_other_actor(column: &str, n: u64, len: usize, compressed: bool) -> (Vec<u8>, Vec<u8>) {
    use automerge::transaction::Transactable;
    let mut base =
        automerge::AutoCommit::new().with_actor(automerge::ActorId::from(long_actor(len)));
    base.put_object(automerge::ROOT, "m", automerge::ObjType::Map)
        .unwrap();
    let l = base
        .put_object(automerge::ROOT, "l", automerge::ObjType::List)
        .unwrap();
    base.insert(&l, 0, 0i64).unwrap();
    base.put(automerge::ROOT, "k", 0i64).unwrap();
    base.commit();
    let head = base.get_heads()[0].0;
    let mut others = vec![long_actor(len)];
    let ops = match column {
        "obj" => vec![
            (0x01, Col::default().run_u(n, 1)),
            (0x02, Col::default().run_u(n, 1)),
            (0x15, Col::default().run_str(n, "k")),
            (0x34, Col::default().bools(&[n])),
            (0x42, Col::default().run_u(n, 1)),
            (0x56, Col::default().run_u(n, 0)),
            (0x70, Col::default().run_u(n, 0)),
        ],
        "key" => vec![
            (0x01, Col::default().run_u(n, 1)),
            (0x02, Col::default().run_u(n, 2)),
            (0x11, Col::default().run_u(n, 1)),
            (0x13, Col::default().lit_s(&[3]).run_s(n - 1, 0)),
            (0x34, Col::default().bools(&[0, n])),
            (0x42, Col::default().run_u(n, 1)),
            (0x56, Col::default().run_u(n, 0)),
            (0x70, Col::default().run_u(n, 0)),
        ],
        "pred" => vec![
            (0x15, Col::default().run_str(n, "k")),
            (0x34, Col::default().bools(&[n])),
            (0x42, Col::default().run_u(n, 1)),
            (0x56, Col::default().run_u(n, 0)),
            (0x70, Col::default().run_u(n, 1)),
            (0x71, Col::default().run_u(n, 1)),
            (0x73, Col::default().lit_s(&[4]).run_s(n - 1, 0)),
        ],
        "others" => {
            others = (0..n)
                .map(|i| {
                    let mut a = vec![0xeeu8; len];
                    a[len - 8..].copy_from_slice(&i.to_be_bytes());
                    a
                })
                .collect();
            list_change_ops(3)
        }
        other => panic!("unknown column {other}"),
    };
    let data = change_data_full(&actor(0), 1, 5, &[head], "", &others, &ops, &[]);
    let change = if compressed {
        compress(&data)
    } else {
        chunk(1, &data)
    };
    (base.document().save_nocompress(), change)
}
