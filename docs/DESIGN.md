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

-- persist a full save: $2 is Automerge.save() (optionally followed by later
-- save_incremental() chunks), bound as bytea. Every writer must use its own
-- actor id: two different histories claiming the same (actor, seq) cannot be
-- merged (ERROR "duplicate seq").
INSERT INTO docs (id, doc) VALUES ($1, $2)
ON CONFLICT (id) DO UPDATE SET doc = merge(docs.doc, EXCLUDED.doc);

-- persist incrementally: $2 is only the new changes (save_incremental() /
-- save_after(heads) output, one or more change chunks) bound as bytea, or a
-- full save. A typed bytea parameter selects merge(automerge, bytea), which
-- applies the chunks on top of the stored document; no cast needed.
UPDATE docs SET doc = merge(doc, $2) WHERE id = $1;

-- (The row must exist for incremental changes: an INSERT's VALUES are cast
-- to automerge on their own, and bare changes are not a complete document.)

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
  - No-op fast path: if `b`'s history is already in `a`, return `a`
    unchanged, with no re-save; symmetrically, if `b` already contains all
    of `a`, return `b` unchanged (the state is the same; this is the common
    case of a backend sending a full, newer save). Checked cheapest first:
    identical bytes; the heads read from both headers (no load, see
    "Heads fast path"): `heads(b) ⊆ heads(a)` → `a`, `heads(a) ⊊ heads(b)` →
    `b`; then the larger input is loaded and checked against the other's
    heads (a containing document is usually the larger one, so one load
    decides the common linear case); only then the other.
  - Errors (SQLSTATE `22P02`/`XX000` with a clear message) if merged changes
    have missing deps. That cannot happen for two valid stored docs.
  - Note: `merge` is an unreserved keyword in PG15+. Verified: unqualified
    `SELECT merge(a, b)` and `SET doc = merge(doc, ...)` work on PG18, so the
    function keeps the name `merge`.
- `merge(a automerge, changes bytea) → automerge`: applies external bytes on
  top of `a`, like Automerge's `load_incremental`. `changes` may be a full
  save (compressed or not, optionally followed by change chunks) or bare
  change chunks (`save_incremental()` / `save_after()` output, several may
  be concatenated) whose dependencies are in `a` or earlier in `changes`.
  - Implemented as a strict `Automerge::load(a ++ changes)`: `a` is a
    document chunk, so the rest is applied incrementally on top of it, but
    unlike `load_incremental` a chunk that fails to parse or has a bad
    checksum fails the whole call instead of being skipped.
  - Changes whose dependencies are in neither `a` nor `changes` raise
    `22P02` naming them (`invalid automerge changes: missing 1 dependency
    that neither the document nor the input contains: <hash>`; at most five
    hashes, then "and N more"). Nothing orphaned is ever stored.
  - Empty `changes`, or nothing new (heads unchanged): returns `a` unchanged,
    no re-save.
  - Malformed input, including decoder panics, is `22P02`. As in
    normalization, a changed result is loaded back once and must keep its
    heads, so a value that loads but whose save does not is never stored.
    So a real change costs one load of `a ++ changes`, one save and one
    verification load (about 2x a load of `a`); a no-op costs one load.
  - A document chunk inside `changes` whose heads `a` already has is skipped
    by Automerge after the checksum check, without decoding its columns.
- Operators `automerge || automerge` and `automerge || bytea` → `automerge`
  are the two `merge`s. There is no `bytea || automerge`, so the second has
  no commutator.

### Overload resolution

Adding `merge(automerge, bytea)` / `automerge || bytea` keeps every form
unambiguous (verified per expression in the `merge_overloads_resolve_unambiguously`
pg_test, through the view dependencies and result type):

| Expression | Resolves to |
|---|---|
| `merge(doc, doc)`, `doc \|\| doc` | `(automerge, automerge)` |
| `merge(doc, $1::bytea)`, `doc \|\| $1::bytea`, typed bytea parameter `$1` | `(automerge, bytea)` |
| `merge(doc, '\x..')`, `doc \|\| '\x..'` (untyped literal) | `(automerge, automerge)` |
| `merge(doc, NULL)`, untyped parameter (`PREPARE p AS ... merge(doc, $1)`) | `(automerge, automerge)` |
| `doc::jsonb \|\| '{..}'` | `jsonb \|\| jsonb` |

An untyped literal is assumed to have the type of the other, known argument
(function resolution step 4.f; for operators, the exact-match check with the
unknown side taking the other side's type), so literals and `NULL` keep
their v1 meaning; `automerge` and `bytea` are both in type category `U`, so
no category preference applies earlier. A consequence: bare change chunks
written as a literal need `'\x..'::bytea`, otherwise the automerge input
function rejects them as orphaned. `doc || '{"a": 1}'` still means merge
(and fails to parse the literal as hex); write `doc::jsonb || '{"a": 1}'`
for jsonb concatenation. `merge($1, $2)` with two typed bytea parameters
matches nothing (bytea → automerge is not implicit).

- Aggregate `merge_agg(automerge) → automerge`: merges all non-null inputs;
  NULL when there are none. The state is an in-memory document (`internal`),
  so each input is loaded once and the result saved once. There is no
  combine function, so it never runs as a parallel partial aggregate.
  The loaded document lives in the Rust heap, outside memory-context
  accounting, and without a serialfunc HashAgg cannot spill it; the
  aggregate declares `SSPACE = 1048576` so the planner's per-group estimate
  is realistic and it prefers sorted grouping. `merge_agg_trans` runs
  `CHECK_FOR_INTERRUPTS` before each input.
- `automerge_to_jsonb(automerge) → jsonb`: the cast function.
- `automerge_heads(automerge) → text[]`: current heads as sorted lowercase hex
  change hashes. Read from the header without loading the document (see
  "Heads fast path").
- `automerge_contains(a automerge, b automerge) → bool`: whether every change
  in `b` is already in `a`, meaning `merge(a, b)` would be a no-op. Decided
  from the two headers when `heads(b) ⊆ heads(a)` (true) or
  `heads(a) ⊊ heads(b)` (false: a head of `b` missing from `a`'s heads has
  no successor in `b`, so it cannot be an ancestor of a head of `a`);
  otherwise `a` is loaded and `b` never is.

### Heads fast path

A stored value is one uncompressed document chunk, whose header lists the
heads (automerge `src/storage/document.rs`):

```text
magic 85 6f 4a 83 | checksum (4) | chunk type (1, 0 = document) | uleb128 data length
data: uleb128 actor count, per actor uleb128 length + bytes
      uleb128 head count, 32 bytes per head
      change/op column metadata and data, head indices
```

`pg_automerge_core::header` parses just that far. It is exact for stored
values: loading a document chunk (`VerificationMode::Check`, the default)
fails with "mismatching heads" unless the listed heads equal the ones
derived from its changes, and every stored value was loaded that way on its
way in. A property test compares the fast path with
`Automerge::load().get_heads()` over hundreds of generated documents (empty,
up to 12 actors, random forks and merges, merge and `merge_agg` outputs,
arbitrary prefixes). The checksum is not recomputed (that would hash the
whole value): stored values were checksum-verified when written. Anything
that is not exactly one document chunk (wrong magic or type, trailing
chunks, lengths that do not add up to the value's size) falls back to a
full load.

`automerge_heads` and `automerge_contains` take their arguments without
detoasting them (`LazyAutomerge`): they read the value's size with
`toast_raw_datum_size` and fetch only a prefix (4 kB, grown as needed) with
`pg_detoast_datum_slice`, which for an out-of-line value reads only the
TOAST chunks covering it and for a compressed value decompresses only that
far. `merge` and `merge_agg` use the fast path on the fully detoasted bytes
for their no-op checks (`merge_agg` checks every input after the first
against the loaded state by its header heads, so inputs that add nothing
are never loaded).

Measured on a 3 MB stored document (3,000,000-character text plus 200
changes, one head; release build, warm cache): `automerge_heads` 2745 ms →
0.3 ms; `automerge_contains(doc, ''::bytea::automerge)` 2738 ms → 0.5 ms; `merge(doc, ''::bytea::automerge)`
2743 ms → 16 ms (the remaining time is detoasting the 3 MB argument).
`merge` of that document with a newer version of it, in either argument
order, now needs one load (2.6 s) instead of two (5.3 s), and so does a
`merge_agg` over both. Deciding containment for a linear history (newer
contains older) still needs a load of the newer document: the headers only
list heads, and change hashes are only available after reconstructing the
changes.

No equality or btree opclass in v1. Note that `=`, `<>`, `<` etc. on two
`automerge` values still resolve, via the implicit cast, to jsonb
comparisons of the current state (not history); history equality is
`automerge_heads(a) = automerge_heads(b)`.

`merge` is commutative in state only: when neither input contains the other
the re-saved bytes depend on argument order, so `doc::bytea` is not an
identity; `automerge_heads` is.

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
| Int, Uint, Counter | number (exact). A counter summing past the i64 range wraps, as in Automerge release builds |
| F64 | number. NaN/±Inf become `null` (jsonb cannot represent them); -0.0 becomes `0` (jsonb numeric has no negative zero) |
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
- `automerge_heads` never loads the document, and `merge` / `merge_agg` /
  `automerge_contains` skip loads where the headers decide (see "Heads fast
  path").
- Persisting incrementally with `merge(doc, $changes::bytea)` sends and
  parses only the new changes, but still loads the stored document, saves
  the result and loads it once more to verify it (on the 3 MB document
  above: 5.3 s for a one-change update, 2.6 s when the changes are already
  there). Persisting a full save with `merge(doc, $save::automerge)` costs
  the same two loads on the server (normalizing the input and loading the
  newer side) plus the upload.
- A per-backend cache of loaded docs and converted jsonb is future work.

## Implementation notes (pgrx)

- `automerge` is a custom varlena type. It is not a
  `#[derive(PostgresType)]`, whose serde/CBOR storage we don't want. Define it
  in SQL via `extension_sql!` (shell type, I/O functions, `CREATE TYPE`) and
  give the Rust newtype manual `FromDatum` / `IntoDatum` / `SqlTranslatable`
  impls mapping to SQL `automerge`. Detoast with `pg_detoast_datum_packed` or
  equivalent. Never hold references into a detoasted datum beyond the call.
  A second Rust type, `LazyAutomerge`, maps to the same SQL type for
  arguments that are detoasted on demand (only a prefix for the heads); it
  holds the raw argument datum and exists only for the duration of a call.
  Both are listed in the `creates` of the type's `extension_sql!` block.
- Errors: never panic across FFI. Map Automerge errors to `ereport(ERROR)`
  with SQLSTATE `22P02` (invalid_text_representation) for bad input (text,
  binary recv and bytea alike) and `XX000` otherwise.
  The automerge decoder is not panic-free: input whose chunk checksums are
  valid but whose column data is malformed can panic inside
  `Automerge::load` (found by fuzzing with recomputed checksums). The core
  crate therefore runs every Automerge call under `catch_unwind` and maps a
  panic to `22P02` for external input and `XX000` for stored values. For the
  same reason `normalize` re-loads its output (when it differs from the
  input) and requires identical heads, since fuzzing also found malformed
  input that loads but whose re-save does not; storing it would leave an
  unreadable value.
- Interrupts: single Automerge calls (load, merge, save, jsonb conversion)
  don't check for interrupts, so cancel/`statement_timeout` wait for the call
  to return. Only `merge_agg_trans` checks between inputs.
- Code layout: all Automerge logic (normalize, merge, merge_changes, heads
  and the header parser, contains, the jsonb mapping, hex/base64/ISO 8601
  encoders) is plain Rust in
  `crates/pg_automerge_core` with `#[test]`s; `src/lib.rs` is pgrx glue. The
  I/O functions are declared by hand inside the `CREATE TYPE` block
  (`#[pg_extern(sql = false)]`), and the Rust type `AutomergeDatum` maps to SQL
  `automerge` with `TypeOrigin::ThisExtension`, so pgrx orders every other
  function after the type.
- Tests: `#[pg_test]` for SQL behaviour, plain `#[test]` for the pure
  conversion logic (edge cases in `crates/pg_automerge_core/tests/`:
  `edge_cases.rs`, `merge_changes.rs` for `merge(automerge, bytea)`,
  `heads_fast_path.rs` for the header parser property test), and
  `tests/pg_regress` for user-facing examples. `tests/concurrency.sh`
  (`mise run concurrency`, also the last step of `mise run test`) runs two real psql sessions against one row to
  check the EvalPlanQual claim above, the upsert path and REPEATABLE READ,
  two sessions persisting only incremental changes with
  `merge(doc, $1::bytea)` (plus the orphaned-changes rejection), and a
  pg_dump/restore round trip. Build test documents in Rust with
  `automerge::AutoCommit` and pass them in as `bytea`.
- Build profile: dev builds compile dependencies optimized and Automerge
  without debug assertions or overflow checks (Cargo.toml). Its debug
  assertions make large documents quadratic, and its overflow checks turned a
  counter past i64::MAX into a panic on read in dev builds only.
