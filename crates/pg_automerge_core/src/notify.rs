//! Payloads for the `automerge_notify()` trigger (see docs/DESIGN.md,
//! "Change notifications").
//!
//! A payload is compact JSON:
//!
//! ```json
//! {"table":"public.docs","op":"UPDATE","seq":7,"key":{"id":1},
//!  "columns":{"doc":{"heads":["ab.."],"prev_heads":["cd.."]}}}
//! ```
//!
//! `NOTIFY` rejects payloads of [`MAX_PAYLOAD`] + 1 bytes or more, and the
//! trigger must never fail a write because of that, so [`Event::payload`]
//! drops parts until the payload fits and then adds `"truncated":true`:
//! first the heads (column names stay), then the column list, then the key.
//! `seq` is always kept: it makes every payload distinct, since `NOTIFY`
//! silently drops a notification whose channel and payload equal an earlier
//! one of the same transaction.

use std::fmt::Write as _;

/// Longest payload `NOTIFY` accepts, in bytes: it must be shorter than
/// `NOTIFY_PAYLOAD_MAX_LENGTH` (`BLCKSZ - NAMEDATALEN - 128` = 8000 with
/// default build options).
pub const MAX_PAYLOAD: usize = 7999;

/// The row operation that fired the trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Insert,
    Update,
    Delete,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Insert => "INSERT",
            Op::Update => "UPDATE",
            Op::Delete => "DELETE",
        }
    }
}

/// Heads of one side (old or new row) of an `automerge` column: `None`
/// when the column is SQL NULL, otherwise sorted hex hashes.
pub type Heads = Option<Vec<String>>;

/// One `automerge` column in a payload. `heads` is present for INSERT and
/// UPDATE (the new row), `prev_heads` for UPDATE and DELETE (the old row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub heads: Option<Heads>,
    pub prev_heads: Option<Heads>,
}

/// A notification: the key values are JSON texts produced by Postgres
/// (`to_json` of the column value), embedded verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Schema-qualified, quoted as needed (`quote_qualified_identifier`).
    pub table: String,
    pub op: Op,
    /// Per-backend event number, increasing with every notification the
    /// backend sends. Kept in every rendered form so that two events of one
    /// transaction never have identical payloads (which `NOTIFY` would
    /// collapse into one).
    pub seq: u64,
    /// Key column name and JSON value text, in trigger argument order (new
    /// row for INSERT/UPDATE, old row for DELETE).
    pub key: Vec<(String, String)>,
    /// The old row's key, for an UPDATE that changed it.
    pub old_key: Option<Vec<(String, String)>>,
    pub columns: Vec<Column>,
}

/// How much of an [`Event`] to render.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Detail {
    Full,
    NoHeads,
    NoColumns,
    NoKey,
}

fn push_str(out: &mut String, s: &str) {
    // serde_json escapes exactly what JSON requires (quotes, backslashes,
    // control characters) and cannot fail for a &str.
    out.push_str(&serde_json::to_string(s).expect("string serializes"));
}

fn push_key(out: &mut String, key: &[(String, String)]) {
    out.push('{');
    for (i, (name, value)) in key.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_str(out, name);
        out.push(':');
        out.push_str(value);
    }
    out.push('}');
}

fn push_heads(out: &mut String, heads: &Heads) {
    match heads {
        None => out.push_str("null"),
        Some(heads) => {
            out.push('[');
            for (i, h) in heads.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_str(out, h);
            }
            out.push(']');
        }
    }
}

impl Event {
    fn render(&self, detail: Detail) -> String {
        let mut out = String::with_capacity(256);
        out.push_str("{\"table\":");
        push_str(&mut out, &self.table);
        let _ = write!(out, ",\"op\":\"{}\",\"seq\":{}", self.op.as_str(), self.seq);
        if detail < Detail::NoKey {
            out.push_str(",\"key\":");
            push_key(&mut out, &self.key);
            if let Some(old_key) = &self.old_key {
                out.push_str(",\"old_key\":");
                push_key(&mut out, old_key);
            }
        }
        if detail < Detail::NoColumns {
            out.push_str(",\"columns\":{");
            for (i, col) in self.columns.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_str(&mut out, &col.name);
                out.push_str(":{");
                if detail == Detail::Full {
                    let mut first = true;
                    for (field, heads) in [("heads", &col.heads), ("prev_heads", &col.prev_heads)] {
                        if let Some(heads) = heads {
                            if !first {
                                out.push(',');
                            }
                            first = false;
                            let _ = write!(out, "\"{field}\":");
                            push_heads(&mut out, heads);
                        }
                    }
                }
                out.push('}');
            }
            out.push('}');
        }
        if detail != Detail::Full {
            out.push_str(",\"truncated\":true");
        }
        out.push('}');
        out
    }

    /// The payload, at most `max_len` bytes if at all possible: the full
    /// form, or with the heads left out, then also the column list, then
    /// also the key (each with `"truncated":true`). If even the last form
    /// is too long (a `max_len` below a few hundred bytes), only
    /// `{"op":..,"seq":..,"truncated":true}`.
    pub fn payload(&self, max_len: usize) -> String {
        for detail in [
            Detail::Full,
            Detail::NoHeads,
            Detail::NoColumns,
            Detail::NoKey,
        ] {
            let out = self.render(detail);
            if out.len() <= max_len {
                return out;
            }
        }
        format!(
            "{{\"op\":\"{}\",\"seq\":{},\"truncated\":true}}",
            self.op.as_str(),
            self.seq
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn hash(i: usize) -> String {
        format!("{i:064x}")
    }

    fn event(heads: usize) -> Event {
        Event {
            table: "public.docs".into(),
            op: Op::Update,
            seq: 7,
            key: vec![
                ("id".into(), "42".into()),
                ("tenant".into(), "\"a\\\"b\"".into()),
            ],
            old_key: None,
            columns: vec![
                Column {
                    name: "doc".into(),
                    heads: Some(Some((0..heads).map(hash).collect())),
                    prev_heads: Some(Some(vec![hash(999)])),
                },
                Column {
                    name: "we\"ird".into(),
                    heads: Some(None),
                    prev_heads: Some(Some(vec![])),
                },
            ],
        }
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("invalid JSON {s}: {e}"))
    }

    #[test]
    fn full_payload() {
        let p = event(2).payload(MAX_PAYLOAD);
        assert_eq!(
            parse(&p),
            json!({
                "table": "public.docs",
                "op": "UPDATE",
                "seq": 7,
                "key": {"id": 42, "tenant": "a\"b"},
                "columns": {
                    "doc": {"heads": [hash(0), hash(1)], "prev_heads": [hash(999)]},
                    "we\"ird": {"heads": null, "prev_heads": []},
                },
            })
        );
        // Compact: no whitespace outside strings.
        assert!(!p.contains(' '), "{p}");
    }

    #[test]
    fn insert_delete_and_old_key() {
        let mut e = event(1);
        e.op = Op::Insert;
        e.columns = vec![Column {
            name: "doc".into(),
            heads: Some(Some(vec![hash(1)])),
            prev_heads: None,
        }];
        assert_eq!(
            parse(&e.payload(MAX_PAYLOAD))["columns"],
            json!({"doc": {"heads": [hash(1)]}})
        );
        e.op = Op::Delete;
        e.columns[0].heads = None;
        e.columns[0].prev_heads = Some(Some(vec![]));
        e.old_key = Some(vec![("id".into(), "7".into())]);
        let v = parse(&e.payload(MAX_PAYLOAD));
        assert_eq!(v["op"], "DELETE");
        assert_eq!(v["columns"], json!({"doc": {"prev_heads": []}}));
        assert_eq!(v["old_key"], json!({"id": 7}));
    }

    #[test]
    fn degrades_to_fit() {
        // 67 bytes per head; 200 heads do not fit into 7999 bytes.
        let e = event(200);
        let p = e.payload(MAX_PAYLOAD);
        assert!(p.len() <= MAX_PAYLOAD);
        assert_eq!(
            parse(&p),
            json!({
                "table": "public.docs",
                "op": "UPDATE",
                "seq": 7,
                "key": {"id": 42, "tenant": "a\"b"},
                "columns": {"doc": {}, "we\"ird": {}},
                "truncated": true,
            })
        );
        // The largest payload that still fits keeps its heads.
        let mut n = 0;
        while event(n + 1).payload(MAX_PAYLOAD).len() <= MAX_PAYLOAD
            && !event(n + 1).payload(MAX_PAYLOAD).contains("truncated")
        {
            n += 1;
        }
        assert!(n > 100, "{n}");
        assert_eq!(
            parse(&event(n).payload(MAX_PAYLOAD))["columns"]["doc"]["heads"]
                .as_array()
                .unwrap()
                .len(),
            n
        );

        // Huge key: columns and then the key go.
        let mut e = event(1);
        e.key = vec![("id".into(), format!("\"{}\"", "x".repeat(7990)))];
        let v = parse(&e.payload(MAX_PAYLOAD));
        assert_eq!(
            v,
            json!({"table": "public.docs", "op": "UPDATE", "seq": 7, "truncated": true})
        );
        // Many columns but a small key: the key stays.
        let mut e = event(1);
        e.columns = (0..200)
            .map(|i| Column {
                name: format!("column_with_a_long_name_{i:040}"),
                heads: Some(None),
                prev_heads: Some(None),
            })
            .collect();
        let v = parse(&e.payload(MAX_PAYLOAD));
        assert_eq!(v["key"], json!({"id": 42, "tenant": "a\"b"}));
        assert_eq!(v.get("columns"), None);
        assert_eq!(v["truncated"], true);
        // Every limit gives valid JSON within the limit, down to the
        // minimal form.
        let e = event(3);
        for max in (0..600).chain([MAX_PAYLOAD]) {
            let p = e.payload(max);
            let v = parse(&p);
            assert_eq!(v["op"], "UPDATE");
            if p.len() > max {
                assert_eq!(v, json!({"op": "UPDATE", "seq": 7, "truncated": true}));
            }
        }
    }

    #[test]
    fn seq_distinguishes_otherwise_identical_events() {
        // NOTIFY drops a notification whose payload equals an earlier one of
        // the same transaction; INSERT, DELETE, INSERT of the same row must
        // still give three distinct payloads, at every level of detail.
        let mut e = event(1);
        e.op = Op::Insert;
        for max in [MAX_PAYLOAD, 300, 120, 60, 0] {
            let a = Event {
                seq: 1,
                ..e.clone()
            }
            .payload(max);
            let b = Event {
                seq: 3,
                ..e.clone()
            }
            .payload(max);
            assert_ne!(a, b, "max {max}");
            assert_eq!(parse(&a)["seq"], 1);
            assert_eq!(parse(&b)["seq"], 3);
        }
        e.seq = u64::MAX;
        assert_eq!(parse(&e.payload(MAX_PAYLOAD))["seq"], json!(u64::MAX));
    }

    #[test]
    fn non_ascii_names_count_bytes() {
        let mut e = event(1);
        e.table = "public.\"dökumente\"".into();
        e.columns[0].name = "ü".repeat(4000);
        let p = e.payload(MAX_PAYLOAD);
        assert!(p.len() <= MAX_PAYLOAD, "{}", p.len());
        assert_eq!(parse(&p)["truncated"], true);
    }
}
