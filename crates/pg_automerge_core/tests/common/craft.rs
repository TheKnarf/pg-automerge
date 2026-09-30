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

/// A document chunk making a list of 3 nulls with `n` empty column
/// metadata entries before its op columns.
pub fn doc_columns(n: u64) -> Vec<u8> {
    document_with(&[actor(0)], 1, one_change(4), list_ops(3), n)
}

/// A change chunk making a list of 3 nulls with `n` empty column metadata
/// entries before its columns.
pub fn change_columns(n: u64) -> Vec<u8> {
    chunk(1, &change_data_with(&list_change_ops(3), n))
}

/// [`change_columns`] as a compressed change chunk.
pub fn compressed_change_columns(n: u64) -> Vec<u8> {
    compress(&change_data_with(&list_change_ops(3), n))
}
