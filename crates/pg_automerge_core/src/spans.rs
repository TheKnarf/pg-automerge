//! The structure of a text object (text runs with their marks, and block
//! markers) as JSON, in the shape of the Automerge JavaScript API's
//! `spans()` (see docs/DESIGN.md, "Rich text spans").
//!
//! The result is an array of
//!
//! - `{"type": "text", "value": "...", "marks": {...}}` for a run of text
//!   with the same marks (`"marks"` absent when the run has none), and
//! - `{"type": "block", "value": {...}}` for a block marker, whose value is
//!   the block's map,
//!
//! in document order, exactly as `ReadDoc::spans` splits them. Mark values
//! and block contents use the scalar mapping of [`crate::json`].
//!
//! Automerge renders a block's value with `hydrate`, which recurses once
//! per level of the block's content and takes about 13 kB of stack per
//! level: a text holding a block nested a few hundred levels deep (a
//! document of a few kB) would overflow the backend's stack, which no
//! guard catches. So before `spans()` runs, every block of the text is
//! found (from `text()` and `get()`, which do not hydrate) and its depth
//! measured with an iterative walk; deeper than [`MAX_BLOCK_DEPTH`] is
//! [`Error::LimitExceeded`]. The block values in the result come from this
//! crate's own (iterative) walk, not from the hydrated map.

use automerge::iter::Span;
use automerge::{
    Automerge, ChangeHash, ObjId, ObjType, ROOT, ReadDoc, ScalarValue, TextEncoding, Value,
};

use crate::json::{JsonSink, ValueSink, sanitize, write_object, write_owned_scalar};
use crate::loaded::{Input, with_doc};
use crate::{Error, Ticker};

/// Maximum nesting of a block's value (the block's map is level 1). Far
/// above what editors store in a block (a type, a list of parent types and
/// a map of attributes: 2 or 3 levels), and far below what overflows the
/// stack in Automerge's recursive `hydrate` (about 13 kB per level in a
/// release build; 32 levels take about 0.4 MB).
pub const MAX_BLOCK_DEPTH: usize = 32;

/// The spans of the text object at `path` in `input`'s state as of `heads`
/// (`None`: the current state) as JSON, or `None` when nothing is at
/// `path`. See [`write_spans`].
///
/// # Errors
///
/// As [`write_spans`].
pub fn spans_to_json(
    input: Input<'_>,
    path: &[&str],
    heads: Option<&[ChangeHash]>,
) -> Result<Option<serde_json::Value>, Error> {
    let mut sink = ValueSink::default();
    if write_spans(input, path, heads, &mut sink)? {
        Ok(Some(sink.into_value().expect("the walk emits one array")))
    } else {
        Ok(None)
    }
}

/// Write the spans of the text object at `path` (map keys, and list
/// indices as decimal strings, from the root, as jsonb's `#>` takes them)
/// into `sink`, in `input`'s state as of `heads` (`None`: the current
/// state; every hash must be a change of the document, `[]` is the state
/// before any change). Returns `false`, having written nothing, when
/// nothing is at `path` (see [`resolve`]).
///
/// # Errors
///
/// [`Error::InvalidParameter`] when `path` leads to something other than a
/// text object, or `heads` has a hash the document lacks;
/// [`Error::LimitExceeded`] for a block nested deeper than
/// [`MAX_BLOCK_DEPTH`]; [`Error::Internal`] if a stored `input` does not
/// load or Automerge fails. On an error the sink may have received an
/// incomplete array.
pub fn write_spans<S: JsonSink + ?Sized>(
    input: Input<'_>,
    path: &[&str],
    heads: Option<&[ChangeHash]>,
    sink: &mut S,
) -> Result<bool, Error> {
    with_doc(input, |doc| {
        let heads = match heads {
            Some(heads) => crate::history::read_heads(doc, heads)?,
            None => None,
        };
        write_doc_spans(doc, path, heads.as_deref(), sink)
    })
}

/// [`write_spans`] on a document, with `heads` already checked (`None`:
/// current state).
fn write_doc_spans<S: JsonSink + ?Sized>(
    doc: &Automerge,
    path: &[&str],
    heads: Option<&[ChangeHash]>,
    sink: &mut S,
) -> Result<bool, Error> {
    let Some(text) = resolve(doc, path, heads)? else {
        return Ok(false);
    };
    let blocks = checked_blocks(doc, &text, heads)?;
    let spans = match heads {
        Some(heads) => doc.spans_at(&text, heads),
        None => doc.spans(&text),
    }
    .map_err(|e| Error::Internal(format!("could not read automerge spans: {e}")))?;
    let mut blocks = blocks.into_iter();
    let mut ticker = Ticker::default();
    sink.begin_array(0);
    for span in spans {
        ticker.tick();
        sink.begin_object();
        match span {
            Span::Text { text, marks } => {
                sink.key("type");
                sink.string("text");
                sink.key("value");
                sink.string(&sanitize(&text));
                if let Some(marks) = marks.filter(|m| !m.is_empty()) {
                    sink.key("marks");
                    sink.begin_object();
                    for (name, value) in marks.iter() {
                        sink.key(&sanitize(name));
                        write_owned_scalar(value, sink);
                    }
                    sink.end_object();
                }
            }
            Span::Block(_) => {
                let id = blocks.next().ok_or_else(|| {
                    Error::Internal("automerge spans hold more blocks than the text".into())
                })?;
                sink.key("type");
                sink.string("block");
                sink.key("value");
                write_object(doc, heads, &id, ObjType::Map, MAX_BLOCK_DEPTH, sink)?;
            }
        }
        sink.end_object();
    }
    sink.end_array();
    if blocks.next().is_some() {
        return Err(Error::Internal(
            "automerge spans hold fewer blocks than the text".into(),
        ));
    }
    Ok(true)
}

/// The text object at `path`, following jsonb's `#>`: from the root, a
/// map (or legacy table) is entered by key, a list by index (a decimal
/// integer as `#>` parses it; negative counts from the end). `None` when
/// nothing is there: a missing key, an index out of range or not an
/// integer, or a step into a scalar or a text (which jsonb shows as a
/// string, a scalar).
///
/// # Errors
///
/// [`Error::InvalidParameter`] when the value at `path` exists but is not
/// a text object (the root, for an empty path, is a map).
pub fn resolve(
    doc: &Automerge,
    path: &[&str],
    heads: Option<&[ChangeHash]>,
) -> Result<Option<ObjId>, Error> {
    let internal = |e: automerge::AutomergeError| {
        Error::Internal(format!("could not read automerge document: {e}"))
    };
    let mut obj = ROOT;
    let mut typ = ObjType::Map;
    for (i, step) in path.iter().enumerate() {
        let got = match typ {
            ObjType::Map | ObjType::Table => match heads {
                Some(heads) => doc.get_at(&obj, *step, heads),
                None => doc.get(&obj, *step),
            },
            ObjType::List => {
                let Some(index) = parse_index(step) else {
                    return Ok(None);
                };
                let index = if index < 0 {
                    let len = match heads {
                        Some(heads) => doc.length_at(&obj, heads),
                        None => doc.length(&obj),
                    };
                    match len.checked_sub(index.unsigned_abs() as usize) {
                        Some(index) => index,
                        None => return Ok(None),
                    }
                } else {
                    index as usize
                };
                match heads {
                    Some(heads) => doc.get_at(&obj, index, heads),
                    None => doc.get(&obj, index),
                }
            }
            ObjType::Text => return Ok(None),
        }
        .map_err(internal)?;
        match got {
            None => return Ok(None),
            Some((Value::Object(t), id)) => {
                obj = id;
                typ = t;
            }
            Some((Value::Scalar(scalar), _)) => {
                if i + 1 < path.len() {
                    return Ok(None);
                }
                return Err(not_text(path, scalar_kind(&scalar)));
            }
        }
    }
    match typ {
        ObjType::Text => Ok(Some(obj)),
        ObjType::Map => Err(not_text(path, "a map")),
        ObjType::Table => Err(not_text(path, "a table")),
        ObjType::List => Err(not_text(path, "a list")),
    }
}

fn not_text(path: &[&str], what: &str) -> Error {
    Error::InvalidParameter(format!(
        "automerge value at path {} is {what}, not a text object",
        format_path(path)
    ))
}

fn scalar_kind(scalar: &ScalarValue) -> &'static str {
    match scalar {
        ScalarValue::Str(_) => "a string scalar",
        ScalarValue::Int(_) => "an integer",
        ScalarValue::Uint(_) => "an unsigned integer",
        ScalarValue::F64(_) => "a float",
        ScalarValue::Counter(_) => "a counter",
        ScalarValue::Timestamp(_) => "a timestamp",
        ScalarValue::Boolean(_) => "a boolean",
        ScalarValue::Bytes(_) => "bytes",
        ScalarValue::Null => "null",
        ScalarValue::Unknown { .. } => "a value of an unknown type",
    }
}

/// `path` as the text of a Postgres `text[]` (`{notes,0,body}`), elements
/// quoted as `array_out` quotes them; cut after 200 characters.
pub fn format_path(path: &[&str]) -> String {
    let mut out = String::from("{");
    for (i, step) in path.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let quote = step.is_empty()
            || step.eq_ignore_ascii_case("null")
            || step.chars().any(|c| {
                matches!(c, '{' | '}' | ',' | '"' | '\\')
                    || c.is_ascii_whitespace()
                    || c == '\u{0B}'
            });
        if quote {
            out.push('"');
            for c in step.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        } else {
            out.push_str(step);
        }
    }
    out.push('}');
    if out.chars().count() > 200 {
        let cut: String = out.chars().take(200).collect();
        return format!("{cut}...");
    }
    out
}

/// A list index as jsonb's `#>` reads a path element (`strtoint(s, &end,
/// 10)` with nothing left over): optional leading whitespace, an optional
/// sign, decimal digits, within `int4`. `None` otherwise.
pub fn parse_index(s: &str) -> Option<i64> {
    let s = s.trim_start_matches([' ', '\t', '\n', '\u{0B}', '\u{0C}', '\r']);
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Leading zeros are fine; anything past int4 is not.
    let digits = digits.trim_start_matches('0');
    if digits.len() > 10 {
        return None;
    }
    let magnitude: i64 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    let value = if negative { -magnitude } else { magnitude };
    (i64::from(i32::MIN)..=i64::from(i32::MAX))
        .contains(&value)
        .then_some(value)
}

/// The blocks of `text` in order (the map objects among its elements),
/// each checked to nest at most [`MAX_BLOCK_DEPTH`] levels deep; see the
/// module documentation. Found without hydrating: every U+FFFC of
/// `text()` (a block marker, another non-string element, or a literal
/// U+FFFC) is looked up with `get()` at its index in the document's text
/// encoding.
fn checked_blocks(
    doc: &Automerge,
    text: &ObjId,
    heads: Option<&[ChangeHash]>,
) -> Result<Vec<ObjId>, Error> {
    let internal = |e: automerge::AutomergeError| {
        Error::Internal(format!("could not read automerge text: {e}"))
    };
    let chars = match heads {
        Some(heads) => doc.text_at(text, heads),
        None => doc.text(text),
    }
    .map_err(internal)?;
    let encoding = doc.text_encoding();
    let mut blocks = Vec::new();
    let mut ticker = Ticker::default();
    let (mut index, mut scanned) = (0usize, 0usize);
    for (at, _) in chars.match_indices('\u{FFFC}') {
        ticker.tick();
        index += width(encoding, &chars[scanned..at])?;
        scanned = at;
        let got = match heads {
            Some(heads) => doc.get_at(text, index, heads),
            None => doc.get(text, index),
        }
        .map_err(internal)?;
        if let Some((Value::Object(ObjType::Map), id)) = got {
            check_depth(doc, heads, &id, &mut ticker)?;
            blocks.push(id);
        }
    }
    Ok(blocks)
}

/// The width of `s` in `encoding`'s units, as Automerge counts indices.
fn width(encoding: TextEncoding, s: &str) -> Result<usize, Error> {
    match encoding {
        TextEncoding::UnicodeCodePoint => Ok(s.chars().count()),
        TextEncoding::Utf8CodeUnit => Ok(s.len()),
        TextEncoding::Utf16CodeUnit => Ok(s.encode_utf16().count()),
        // Documents are loaded with the platform default (code points).
        TextEncoding::GraphemeCluster => Err(Error::Internal(
            "automerge spans: unsupported text encoding".into(),
        )),
    }
}

/// [`Error::LimitExceeded`] if the maps and lists in the block `block`
/// nest deeper than [`MAX_BLOCK_DEPTH`] (the block is level 1). An
/// iterative walk of `map_range` / `list_range`; texts inside are leaves
/// (Automerge renders them with `text()`).
fn check_depth(
    doc: &Automerge,
    heads: Option<&[ChangeHash]>,
    block: &ObjId,
    ticker: &mut Ticker,
) -> Result<(), Error> {
    let mut stack = vec![(block.clone(), ObjType::Map, 1usize)];
    while let Some((obj, typ, depth)) = stack.pop() {
        ticker.tick();
        let mut children = Vec::new();
        match typ {
            ObjType::List => {
                let items = match heads {
                    Some(heads) => doc.list_range_at(&obj, .., heads),
                    None => doc.list_range(&obj, ..),
                };
                for item in items {
                    if let automerge::ValueRef::Object(t) = item.value {
                        children.push((item.id(), t));
                    }
                }
            }
            _ => {
                let items = match heads {
                    Some(heads) => doc.map_range_at(&obj, .., heads),
                    None => doc.map_range(&obj, ..),
                };
                for item in items {
                    if let automerge::ValueRef::Object(t) = item.value {
                        children.push((item.id(), t));
                    }
                }
            }
        }
        for (id, t) in children {
            if t == ObjType::Text {
                continue;
            }
            if depth >= MAX_BLOCK_DEPTH {
                return Err(Error::LimitExceeded(format!(
                    "automerge text block is nested more than {MAX_BLOCK_DEPTH} levels deep"
                )));
            }
            stack.push((id, t, depth + 1));
        }
    }
    Ok(())
}
