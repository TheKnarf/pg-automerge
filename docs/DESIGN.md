# pg_automerge design

A Postgres 18+ extension (Rust, pgrx 0.19, `automerge` crate 0.12) that stores
[Automerge](https://automerge.org) documents in an `automerge` column type,
lets you **query them as `jsonb`**, and **merges** concurrent writes.

## Scope

In scope:

- Storing Automerge documents produced elsewhere.
- Reading them with every `jsonb` operator/function/index.
- `merge(a, b)`: CRDT merge so concurrent writers never overwrite each other.

Out of scope (by decision):

- Server-side edits (no `set`/`splice`/subscripting, no `jsonb -> automerge`).
- Actor IDs. All changes are authored by the application backend, which owns
  actor IDs. The extension never creates changes.
- Sync protocol, history queries, history pruning.

## Intended usage

The application backend syncs with frontends via Automerge and persists
documents in Postgres:

```sql
CREATE TABLE docs (id uuid PRIMARY KEY, doc automerge NOT NULL);

-- persist (bytes from Automerge.save() / save_incremental() passed as bytea)
INSERT INTO docs (id, doc) VALUES ($1, $2)
ON CONFLICT (id) DO UPDATE SET doc = merge(docs.doc, EXCLUDED.doc);

UPDATE docs SET doc = merge(doc, $2::automerge) WHERE id = $1;

-- `$2::automerge` matters when the driver binds $2 as a typed `bytea`:
-- function arguments only use implicit casts, and bytea -> automerge is an
-- assignment cast. (INSERT/UPDATE targets apply it automatically.)

-- read
SELECT doc->>'title' FROM docs WHERE doc @> '{"status": "open"}';
SELECT jsonb_path_query(doc, '$.items[*] ? (@.done == false)') FROM docs;
```

### Why `SET doc = merge(doc, $1)` is concurrency-safe

Under READ COMMITTED, when two transactions update the same row, the second
blocks on the row lock; after the first commits, Postgres re-evaluates the
SET expression against the *newly committed* row version (EvalPlanQual). So
`merge(doc, $1)` merges into the latest state and no write is lost.

Under REPEATABLE READ / SERIALIZABLE the second writer instead fails with a
serialization error and must retry, as with any concurrent update.

## The `automerge` type

- Variable-length (`varlena`), `STORAGE = extended`, alignment `int4`.
- Payload: the raw bytes of `Automerge::save_nocompress()`, no extra header.
  Stored uncompressed so TOAST compression (pglz, or `lz4` via
  `ALTER TABLE .. ALTER COLUMN .. SET COMPRESSION lz4`) is effective, instead of
  compressing Automerge's already-deflated columns.
- Every value entering the type is **validated and normalized**: it is loaded
  with `Automerge::load` (which accepts a document chunk plus trailing change
  chunks, compressed or not) and re-saved with `save_nocompress()`. Stored
  values are therefore always a single compact document chunk. Values whose
  changes have missing dependencies are rejected (no orphaned changes stored).
  Empty input (`''::bytea`, `'\x'`) is the empty document, whose jsonb is `{}`.
- Text I/O is **lossless**, because `pg_dump`/`COPY` use it: output is `\x`
  followed by lowercase hex of the stored bytes (same shape as `bytea`); input
  accepts the same form. (No JSON text input: that would create history.)
- Binary I/O (`send`/`recv`): the raw bytes (recv validates + normalizes).

### Casts

| From → To | Context | Notes |
|---|---|---|
| `bytea → automerge` | assignment | validates + normalizes; lets drivers bind `bytea` params directly |
| `automerge → bytea` | explicit | `WITHOUT FUNCTION` (same varlena layout); stored bytes, loadable by any Automerge implementation |
| `automerge → jsonb` | **implicit** | so every jsonb operator/function applies to `automerge` directly |

No `jsonb → automerge` cast (out of scope, and it would silently fabricate
history).

The implicit `jsonb` cast is deliberate. It is the only implicit cast from
`automerge`, so operator resolution is unambiguous. Do not add an implicit cast
to `json` or `text`.

## Functions and operators

All are `IMMUTABLE STRICT PARALLEL SAFE` unless noted.

- `merge(automerge, automerge) → automerge`: the state after applying every
  change of `b` missing from `a`. Commutative and idempotent in state
  (`merge(a,b)` and `merge(b,a)` have identical heads and jsonb).
  - Fast path: if every head of `b` is already in `a`, return `a` unchanged,
    with no re-save. Symmetrically, if `b` already contains all of `a`,
    return `b` unchanged (the state is the same; this is the common case of a
    backend sending a full, newer save).
  - Errors (SQLSTATE `22P02`/`XX000` with a clear message) if merged changes
    have missing deps. That cannot happen for two valid stored docs.
  - Note: `merge` is an unreserved keyword in PG15+. Verified: unqualified
    `SELECT merge(a, b)` and `SET doc = merge(doc, ...)` work on PG18, so the
    function keeps the name `merge`.
- Operator `automerge || automerge → automerge` = `merge`. Because an
  untyped literal takes the other operand's type, `doc || '{"a": 1}'` resolves
  to this operator (and fails to parse the literal as hex); write
  `doc::jsonb || '{"a": 1}'` for jsonb concatenation.
- Aggregate `merge_agg(automerge) → automerge`: merges all non-null inputs;
  NULL when there are none. The state is an in-memory document (`internal`),
  so each input is loaded once and the result saved once. There is no
  combine function, so it never runs as a parallel partial aggregate.
- `automerge_to_jsonb(automerge) → jsonb`: the cast function.
- `automerge_heads(automerge) → text[]`: current heads as sorted lowercase hex
  change hashes.
- `automerge_contains(a automerge, b automerge) → bool`: whether every change
  in `b` is already in `a`, meaning `merge(a, b)` would be a no-op.

No equality or btree opclass in v1.

## jsonb mapping

The root is always a map, so it maps to a jsonb object. For a map key with
conflicting concurrent values, Automerge's winner is used (what `get` returns).

| Automerge | jsonb |
|---|---|
| Map / Table | object |
| List | array |
| Text | string (`doc.text(obj)`; block markers are included as the characters Automerge's `text()` returns) |
| Str | string |
| (any string or map key containing U+0000) | U+0000 replaced by U+FFFD (Postgres text and jsonb cannot hold NUL) |
| Int, Uint, Counter | number (exact) |
| F64 | number. NaN/±Inf become `null` (jsonb cannot represent them) |
| Boolean | boolean |
| Null | null |
| Timestamp (ms since epoch) | string, ISO 8601 UTC with milliseconds, e.g. `"2024-01-02T03:04:05.678Z"`; years outside 0000–9999 use the six-digit signed form (`"+010000-01-01T00:00:00.000Z"`), like JavaScript's `toISOString` |
| Bytes | string, standard base64 with padding |
| Unknown | null |

The conversion must produce jsonb directly or through `serde_json` with exact
integers (u64 and i64 must not go through f64). The implementation builds a
`serde_json::Value` (exact `Number`s) and hands its text to `jsonb_in`.

The document walk uses an explicit stack, but nesting is capped at 1000
levels (error `XX000`) to bound recursion when serializing/dropping the
`serde_json::Value`.

## Performance notes

- Every `automerge → jsonb` evaluation loads the document. Loading is roughly
  linear in ops and hashes every change. For heavy read workloads the
  recommended pattern is a stored generated column, which the IMMUTABLE cast
  allows:

  ```sql
  ALTER TABLE docs ADD COLUMN data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED;
  CREATE INDEX ON docs USING gin (data jsonb_path_ops);
  ```

- Expression indexes also work: `CREATE INDEX ON docs USING gin ((doc::jsonb));`
- A per-backend cache of loaded docs and converted jsonb is future work.

## Implementation notes (pgrx)

- `automerge` is a custom varlena type. It is not a
  `#[derive(PostgresType)]`, whose serde/CBOR storage we don't want. Define it
  in SQL via `extension_sql!` (shell type, I/O functions, `CREATE TYPE`) and
  give the Rust newtype manual `FromDatum` / `IntoDatum` / `SqlTranslatable`
  impls mapping to SQL `automerge`. Detoast with `pg_detoast_datum_packed` or
  equivalent. Never hold references into a detoasted datum beyond the call.
- Errors: never panic across FFI. Map Automerge errors to `ereport(ERROR)`
  with SQLSTATE `22P02` (invalid_text_representation) for bad input (text,
  binary recv and bytea alike) and `XX000` otherwise.
- Code layout: all Automerge logic (normalize, merge, heads, contains, the
  jsonb mapping, hex/base64/ISO 8601 encoders) is plain Rust in
  `crates/pg_automerge_core` with `#[test]`s; `src/lib.rs` is pgrx glue. The
  I/O functions are declared by hand inside the `CREATE TYPE` block
  (`#[pg_extern(sql = false)]`), and the Rust type `AutomergeDatum` maps to SQL
  `automerge` with `TypeOrigin::ThisExtension`, so pgrx orders every other
  function after the type.
- Tests: `#[pg_test]` for SQL behaviour, plain `#[test]` for the pure
  conversion logic, and `tests/pg_regress` for user-facing examples. Build test
  documents in Rust with `automerge::AutoCommit` and pass them in as `bytea`.
