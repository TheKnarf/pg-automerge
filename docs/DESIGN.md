# pg_automerge design

A Postgres 18+ extension (Rust, pgrx 0.19, `automerge` crate 0.12) that stores
[Automerge](https://automerge.org) documents in an `automerge` column type,
lets you **query them as `jsonb`**, and **merges** concurrent writes.

## Scope

In scope:

- Storing Automerge documents produced elsewhere.
- Reading them with every `jsonb` operator/function/index.
- `merge(a, b)`: CRDT merge so concurrent writers never overwrite each other.
- Read-only history: listing a document's changes, fetching change bytes
  (e.g. everything since a replica's heads), and the state as of earlier
  heads (see "History").

Out of scope (by decision):

- Server-side edits (no `set`/`splice`/subscripting, no `jsonb -> automerge`).
- Actor IDs. All changes are authored by the application backend, which owns
  actor IDs. The extension never creates changes.
- Sync protocol, history pruning, and anything that writes history (the
  history functions only read it).

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

The change count comes from the same place: after the heads come the
change and op column metadata (uleb128 count, then uleb128 spec and
length per column) and then the column data, change columns first. The
change actor column (spec `0x01`) has one RLE-encoded entry per change
(signed LEB128 run count: `n > 0` repeats the next value `n` times, `n < 0`
is followed by `-n` literal values, `0` is a null run), and Automerge's
loader takes the number of changes from its length and rejects documents
whose other change columns disagree. `header::change_count_from_prefix`
counts it; an absent column means no changes; a deflated or repeated
actor column, a null run, or anything that does not add up falls back to
a load. A property test compares it with the loaded change graph over the
same generated documents (and a many-actor document with literal and
repeat runs).

`automerge_heads`, `automerge_contains`, `automerge_change_count` and the
since-functions of "History" take their document without detoasting it
(`LazyAutomerge`): they read the value's size with
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

### History (read-only)

Automerge keeps a document as an op set plus a change graph: hash, actor,
seq, op range, time, message and deps of every change. Change *bytes* are
not stored; Automerge rebuilds them from the op set on request. So the
metadata functions cost one load, while the functions returning change
bytes cost a load plus re-encoding those changes (about as much again for
all changes of a document). Nothing here creates changes.

Two composite types describe a change:

```sql
CREATE TYPE automerge_change AS (
    hash text,          -- 64 lowercase hex digits
    actor text,         -- lowercase hex
    seq bigint,         -- 1, 2, ... per actor
    start_op bigint,    -- counter of the change's first op
    op_count bigint,    -- number of ops (0 for an empty change)
    "time" timestamptz, -- NULL when not set (0)
    message text,       -- NULL when not set
    deps text[],        -- sorted hashes of its dependencies
    change bytea        -- the change chunk (uncompressed, as save_after emits it)
);
-- automerge_change_meta: the same without `change`.
```

- `automerge_changes(doc automerge, since_heads text[] DEFAULT '{}') →
  SETOF automerge_change`: every change not reachable from `since_heads`
  (all changes for `'{}'`), in causal order: each change comes after its
  dependencies, and otherwise in Automerge's own order (which already is
  causal, so it is kept as is). Rebuilds the bytes of the returned changes.
- `automerge_changes_meta(doc, since_heads text[] DEFAULT '{}') → SETOF
  automerge_change_meta`: the same rows from the change graph only
  (`get_changes_meta`); no change is rebuilt. Use it whenever the bytes are
  not needed.
- `automerge_changes_bytes(doc, since_heads text[] DEFAULT '{}') → bytea`:
  those changes' chunks concatenated in the same causal order, i.e.
  Automerge's `save_after(since_heads)`. A replica at `since_heads` loads it
  with `loadIncremental` / `applyChanges`, and `merge(replica,
  automerge_changes_bytes(full, automerge_heads(replica)))` has the heads
  and jsonb of `full` (tested). Empty when there is nothing new.
- `automerge_get_change(doc, hash text) → automerge_change`: one change with
  its bytes, or NULL if the document does not have it.
- `automerge_change_count(doc) → bigint`: number of changes. Read from a
  prefix of the stored value (see "Heads fast path"), without loading it.
- `automerge_to_jsonb(doc automerge, heads text[]) → jsonb`: the state as of
  `heads` (Automerge's `*_at` reads), with the same mapping as the cast.
  `'{}'` is the state before any change (`{}`); the current heads give the
  current state. Every hash must be a change of the document, otherwise
  `22023` (`automerge document does not contain change <hash>`).

All are `IMMUTABLE STRICT PARALLEL SAFE`: the result depends only on the
arguments (a stored value's history is part of its bytes). `STRICT` with
defaults: a NULL argument gives NULL (no rows for the set-returning ones).

"Since heads" semantics are Automerge's `get_changes(have_deps)`: the
changes that are neither in `since_heads` nor ancestors of them.
**Hashes the document does not have are ignored**, as Automerge does: a
replica that is ahead of the stored row (it has changes the row lacks)
gets everything that is not an ancestor of the heads the row knows, a
superset of what it is missing and never less, which it deduplicates on
load. (An error would make that sync case fail; a typo in a hash also
just returns more changes.) Automerge computes the set from the per-actor
sequence numbers of the known heads' ancestors; for histories written by
Automerge (each change depends on its actor's previous change) that is
exactly the set of non-ancestors, which a property test checks against a
walk of the dependency graph. `automerge_to_jsonb(doc, heads)` instead
rejects unknown hashes, since it cannot show a state it does not have.

Hash arguments must be exactly 64 hex digits, either case (`22P02`
`invalid automerge change hash "..."` otherwise); a NULL array element is
`22004` (`since_heads must not contain NULL`).

Time: Automerge change times are Unix **seconds** (as documented for the
Rust crate's `CommitOptions::with_time`; check what your producer writes).
`time` is NULL when the change has none (0) and when Postgres cannot
represent it; a producer writing milliseconds shows up as a date tens of
thousands of years ahead.

Shortcuts: when every current head of `doc` is in `since_heads`, the
since-functions return nothing without loading (heads read from the
header, as in `automerge_heads`). Measured on the 3 MB document of "Heads
fast path" plus 200 small changes (201 changes; release build, warm
cache): `automerge_change_count` 2 ms; `automerge_changes_meta` 2.8 s (one
load); `automerge_changes` and `automerge_changes_bytes` for all changes
5.1 s (load plus rebuilding 3 MB of changes); since the 100th change, or one
`automerge_get_change`, 2.7-2.8 s; `automerge_to_jsonb(doc, old heads)` 3.5
s against 3.0 s for the current state; any since-function given the
current heads 1 ms.

Set-returning functions compute all rows on the first call: the document
is loaded, the rows are copied into a Rust `Vec` and the document is
dropped before the first row is returned. pgrx keeps that `Vec` in the
SRF's multi-call memory context, which Postgres deletes when the scan ends
or is cut short (`LIMIT`, a closed cursor, an error), dropping it; tested
with `LIMIT` on a target-list SRF, closed cursors and errors mid-scan.
Memory: for `automerge_changes` the `Vec` holds the change bytes (about the
size of the document's history, 3 MB above) until the scan ends; in
`FROM`, Postgres additionally materializes the rows in a tuplestore
(spilling to disk past `work_mem`). Rows are built with the function's
declared result type (looked up by the function's OID), so they do not
depend on `search_path`.

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
- `automerge_heads` and `automerge_change_count` never load the document,
  and `merge` / `merge_agg` / `automerge_contains` and the history
  since-functions skip loads where the headers decide (see "Heads fast
  path").
- History: `automerge_changes_meta` costs a load; `automerge_changes`,
  `automerge_changes_bytes` and `automerge_get_change` also rebuild the
  returned changes' bytes (see "History").
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
  binary recv and bytea alike, and malformed change hashes), `22023`
  (invalid_parameter_value) for well-formed heads a document does not
  have, `22004` for NULL array elements, and `XX000` otherwise.
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
  and the header parser, contains, the history functions in `history.rs`,
  the jsonb mapping, hex/base64/ISO 8601 encoders) is plain Rust in
  `crates/pg_automerge_core` with `#[test]`s; `src/lib.rs` is pgrx glue. The
  I/O functions are declared by hand inside the `CREATE TYPE` block
  (`#[pg_extern(sql = false)]`), and the Rust type `AutomergeDatum` maps to SQL
  `automerge` with `TypeOrigin::ThisExtension`, so pgrx orders every other
  function after the type.
- Tests: `#[pg_test]` for SQL behaviour, plain `#[test]` for the pure
  conversion logic (edge cases in `crates/pg_automerge_core/tests/`:
  `edge_cases.rs`, `merge_changes.rs` for `merge(automerge, bytea)`,
  `heads_fast_path.rs` for the header parser property test, `history.rs`
  for the history functions, checked against Automerge (`fork_at`,
  `get_changes`) and a dependency-graph walk; `common/` holds the shared
  random-history generator), and
  `tests/pg_regress` for user-facing examples. `tests/concurrency.sh`
  (`mise run concurrency`, also the last step of `mise run test`) runs two real psql sessions against one row to
  check the EvalPlanQual claim above, the upsert path and REPEATABLE READ,
  two sessions persisting only incremental changes with
  `merge(doc, $1::bytea)` (plus the orphaned-changes rejection), and a
  pg_dump/restore round trip. Build test documents in Rust with
  `automerge::AutoCommit` and pass them in as `bytea`.
- `mise run regress` passes `--resetdb`: a reused regress database would
  keep the extension objects of an earlier build, so new functions would be
  missing.
- Build profile: dev builds compile dependencies optimized and Automerge
  without debug assertions or overflow checks (Cargo.toml). Its debug
  assertions make large documents quadratic, and its overflow checks turned a
  counter past i64::MAX into a panic on read in dev builds only.
