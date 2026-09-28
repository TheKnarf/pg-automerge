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
- Change notifications: a trigger that tells listening backends (`LISTEN`)
  which rows' documents changed and their new heads (see "Change
  notifications").

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
  - A new document is returned as an expanded value (see "Expanded
    values"): kept loaded in memory and saved only when it is stored,
    sent or cast to `bytea`. When `a` is a read-write expanded pointer
    (a PL/pgSQL variable in `d := merge(d, x)`, or the result of an inner
    `merge`), the merge happens in place.
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
  - Strict, unlike `load_incremental`: a chunk that fails to parse or has
    a bad checksum fails the whole call instead of being skipped. Bare
    uncompressed change chunks (the usual input) are split and
    checksum-checked by `header::change_chunks`, each parsed with
    `Change::try_from` (the parser `Automerge::load` uses for change
    chunks) and applied to `a`'s document with `apply_changes`: the same
    steps as a load of `a ++ changes`, without re-loading `a` when it is
    already loaded (an expanded value). Anything else (a save, compressed
    chunks) is loaded as `a ++ changes` (for an expanded `a`, a fresh save
    of it ++ `changes`).
  - Changes whose dependencies are in neither `a` nor `changes` raise
    `22P02` naming them (`invalid automerge changes: missing 1 dependency
    that neither the document nor the input contains: <hash>`; at most five
    hashes, then "and N more"). Nothing orphaned is ever stored.
  - Empty `changes`, or nothing new (heads unchanged): returns `a` unchanged,
    no re-save. When `changes` is bare change chunks that are all heads of
    `a` (a re-send of the latest changes), this is seen from the chunks'
    hashes without loading anything (see `automerge_contains(a, bytea)`).
  - Malformed input, including decoder panics, is `22P02`. As in
    normalization, a changed result is loaded back once and must keep its
    heads, so a value that loads but whose save does not is never stored.
    Since expanded values, that check runs when the result is first
    flattened (stored, sent, cast to `bytea`), not inside `merge`: see
    "Expanded values". A real change on a stored `a` costs one load of
    `a`, one save and one verification load (about 2x a load of `a`); a
    no-op costs one load; on an expanded `a`, a clone plus applying the
    changes (milliseconds), with the save and the check deferred to the
    flattening.
  - A document chunk inside `changes` whose heads `a` already has is skipped
    by Automerge after the checksum check, without decoding its columns.
- Operators `automerge || automerge` and `automerge || bytea` → `automerge`
  are the two `merge`s. There is no `bytea || automerge`, so the second has
  no commutator.

### Overload resolution

Adding `merge(automerge, bytea)` / `automerge || bytea` keeps every form
unambiguous (verified per expression in the `merge_overloads_resolve_unambiguously`
and `contains_bytea_overload` pg_tests, through the view dependencies and
result type). `automerge_contains(automerge, bytea)` follows the same
rules:

| Expression | Resolves to |
|---|---|
| `merge(doc, doc)`, `doc \|\| doc` | `(automerge, automerge)` |
| `merge(doc, $1::bytea)`, `doc \|\| $1::bytea`, typed bytea parameter `$1` | `(automerge, bytea)` |
| `merge(doc, '\x..')`, `doc \|\| '\x..'` (untyped literal) | `(automerge, automerge)` |
| `merge(doc, NULL)`, untyped parameter (`PREPARE p AS ... merge(doc, $1)`) | `(automerge, automerge)` |
| `doc::jsonb \|\| '{..}'` | `jsonb \|\| jsonb` |
| `automerge_contains(doc, doc)`, `automerge_contains(doc, '\x..')`, `automerge_contains(doc, NULL)` | `(automerge, automerge)` |
| `automerge_contains(doc, $1::bytea)`, typed bytea parameter `$1` | `(automerge, bytea)` |

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
  `CHECK_FOR_INTERRUPTS` before each input. Expanded inputs are used
  without a load. When no single input contains all others, the result is
  an expanded value holding a copy of the state (the final function may
  run more than once, e.g. as a window function), so
  `merge_agg(doc)::jsonb` needs no save and re-load.
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
- `automerge_contains(a automerge, changes bytea) → bool`: whether `a`
  already has every change in `changes` (the same inputs as
  `merge(automerge, bytea)`), i.e. whether `merge(a, changes)` returns `a`
  unchanged. Empty `changes` is contained. Changes whose dependencies are
  in neither `a` nor `changes` are not in `a`: `false` (where `merge`
  raises `22P02`). Malformed bytes are `22P02` when they have to be loaded.
  - No-load path, when `changes` is only uncompressed change chunks: each
    chunk's hash is `sha256(type ‖ uleb128 length ‖ data)` (its checksum
    is the first 4 bytes, and must match) and its data starts with its
    dependency hashes (`header::change_chunks`). `a` is a complete
    document, so a chunk whose hash is a head of `a` is in `a`, and a chunk
    with a dependency that is a head of `a` (or a chunk of the input
    already known to be missing) is not in `a`: a head has no successor in
    `a`. Any change known missing gives `false`, all known present `true`.
    That decides the two cases a backend produces: new changes made on top
    of the heads it last saw (false), and a re-send of the changes that
    made the current heads (true).
  - Otherwise (older changes, full saves, compressed chunks) `a ++
    changes` is loaded once (no save) and the heads compared, which is the
    no-op check of `merge(automerge, bytea)`.
  - On the no-load path only the framing, checksums and dependency lists
    are read, so `false` does not promise that `merge` accepts the input.
  - A property test compares it with a direct lookup of every change in
    the loaded document over generated histories and arbitrary slices of
    their change lists, and checks that `merge` agrees (a no-op exactly
    when contained).

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
(`AutomergeArg`; an expanded value is read in memory): they read the value's size with
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

## Change notifications

Backends keep their in-memory replicas in sync by listening for changes
other backends persisted, then fetching only what they lack:

```sql
CREATE TRIGGER docs_notify AFTER INSERT OR UPDATE OR DELETE ON docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('docs_changed', 'id' [, more key columns]);
```

`automerge_notify()` runs `NOTIFY <channel>` (`Async_Notify`, what
`pg_notify` calls) with a compact JSON payload:

```json
{"table":"public.docs","op":"UPDATE","seq":17,"key":{"id":1},
 "columns":{"doc":{"heads":["79df.."],"prev_heads":["891e.."]}}}
```

- `table`: schema-qualified, each part quoted as needed
  (`public."Odd Name"`). For a partitioned table this is the partition
  holding the row (row triggers run on the partitions).
- `op`: `INSERT`, `UPDATE` or `DELETE`.
- `seq`: a per-backend counter, incremented for every notification the
  sending backend's trigger sends (all channels and tables; numbers of
  rolled-back work are skipped, so there are gaps). It exists to make every
  payload distinct: `NOTIFY` silently drops a notification whose channel
  and payload equal an earlier one of the same transaction, which without
  `seq` would lose events a listener needs, e.g. the second INSERT of
  INSERT, DELETE, INSERT of the same row in one transaction (the listener
  would drop a replica of a row that exists), or the third UPDATE of a key
  changing 1 → 2 → 1 → 2. Within a transaction `seq` increases in firing
  order. It is not a global order: another backend (or this one after a
  reconnect, as a new process) counts separately, and the numbers are not
  comparable across senders. Kept in every truncated form.
- `key`: the key columns named in the trigger arguments, in that order,
  each as `to_json(value)` (numbers stay numbers, uuids and text are
  strings, NULL is `null`); from the new row, or the old row for DELETE.
  `old_key`: the old row's key, only for an UPDATE that changed it.
- `columns`: every column of type `automerge` (or a domain over it):
  INSERT lists all with `heads`, DELETE all with `prev_heads`, UPDATE only
  those whose heads changed, with both. Heads are sorted hex hashes as in
  `automerge_heads`; `null` for a NULL value (`[]` is the empty document).
- `truncated: true`: see below.

When it fires: INSERT and DELETE always notify. An UPDATE notifies only if
the heads of some `automerge` column changed, or the key did (a listener
tracks rows by key). No-op merges (`merge` returning the stored value),
`SET doc = doc`, and updates of other columns do not notify. Change is
decided by heads, never by bytes: `merge(a, b)` and `merge(b, a)` may
differ in bytes with the same heads. Cost: an unchanged value whose raw
datum is identical (same inline bytes or same TOAST pointer, e.g. an
update of another column) is skipped without reading it; otherwise both
heads are read from a prefix of the stored values as in `automerge_heads`
(4 kB, only the TOAST chunks covering it), never a full load. An UPDATE
that changed no heads and whose key columns are raw-identical returns
before building any JSON. Measured on 20,000 small rows (release build):
`UPDATE t SET n = n + 1` 451 ms without the trigger, 746 ms with it (no
notifications); merging a new change into every row 7.2 s without, 7.8 s
with (20,000 notifications); re-sending that change 709 ms with a plain
`merge` (a no-op rewrite of every row, no notifications) and 46 ms with
the `WHERE NOT automerge_contains` pattern below.

Delivery is Postgres' `NOTIFY`: sent at commit (nothing for a rolled-back
transaction), in commit order, only to sessions connected and listening at
the time; notifications with identical channel and payload within one
transaction are collapsed, which `seq` prevents for this trigger's
notifications (so every row event of a committed transaction arrives, in
the order it fired). Still, a listener must treat notifications as hints:

1. On connect (and reconnect), `LISTEN docs_changed` first, then resync
   every replica it holds: `SELECT automerge_changes_bytes(doc,
   $replica_heads) FROM docs WHERE id = $1` and apply the result.
2. On a notification for a row it holds: if the payload's `heads` are all
   in its replica already (e.g. it wrote them), skip; otherwise fetch
   `automerge_changes_bytes(doc, $replica_heads)` for that key. The row
   may have moved on since the notification; that just returns more.
   `automerge_changes_bytes` with heads the row already has returns
   nothing without loading the document, so fetching on every
   notification (and on `truncated` ones) is cheap when there is nothing
   new.
3. DELETE: drop the replica. `old_key`: re-key it. Apply events in the
   order they arrive; a DELETE later followed by an INSERT of the same key
   arrives as both.

Payload size: `NOTIFY` rejects payloads of 8000 bytes or more
(`NOTIFY_PAYLOAD_MAX_LENGTH`); at 67 bytes per hash that is about 115 heads,
counting `heads` and `prev_heads` of all columns. The trigger
never fails a write because of that: it renders the full payload, and if
it has more than 7999 bytes drops, in this order, until it fits: the heads
(`"columns":{"doc":{}}`, column names kept), the `columns` object, the
`key`/`old_key`. Any dropped part adds `"truncated":true`; listeners then
fetch by key (or resync the table when the key is gone too, which needs a
key of several kB). Tested with a 150-head document.

Argument checks, when the trigger fires (Postgres does not validate
trigger arguments at `CREATE TRIGGER`): not `AFTER` or not `FOR EACH ROW` →
`39P01` (trigger protocol violated); fewer than two arguments, an empty
channel or one of 64 bytes or more, a key column listed twice or of type
`automerge` → `22023`; an unknown key column → `42703`; calling
`automerge_notify()` outside a trigger → `0A000`. These errors fail the
write, as trigger errors do. The channel is used verbatim, like
`pg_notify`: `LISTEN` folds unquoted names to lower case, so use a
lower-case channel (or `LISTEN "Name"`). The `automerge` type is looked up
in the function's own schema, so the trigger does not depend on
`search_path`.

Labels: `VOLATILE`, not `STRICT`, `PARALLEL UNSAFE` (the defaults; it sends
notifications and triggers never run in parallel workers).

Why a C (Rust) trigger and not PL/pgSQL: PL/pgSQL would have to find the
`automerge` columns in the catalog and read each through dynamic SQL
(`EXECUTE format('SELECT automerge_heads($1.%I)', col) USING NEW`) per row;
its `to_jsonb(NEW)` would convert every document to jsonb (a full load
each). The Rust trigger reads the raw datums (so an unchanged TOAST pointer
costs nothing and heads come from a prefix), builds the payload in
`pg_automerge_core::notify` (plain Rust, unit-tested including the size
limit and escaping) and gets the key values' JSON from Postgres' own
`to_json` machinery (`json_categorize_type` / `datum_to_json`, exported by
the server though not in pgrx's bindings). It is written like pgrx's
`#[pg_trigger]` expansion (V1 info record, guarded entry point, SQL in an
`extension_sql!` block) so that a call outside a trigger is a clean error.

### Skipping no-op writes

A no-op `UPDATE docs SET doc = merge(doc, $1)` still writes a new row
version (and a new copy of a TOASTed document, plus WAL) and fires
triggers (`automerge_notify` stays silent, since the heads are unchanged).
To skip it:

```sql
UPDATE docs SET doc = merge(doc, $1) WHERE id = $2 AND NOT automerge_contains(doc, $1);
```

`$1` is the same bytea parameter in both places (`automerge_contains(automerge,
bytea)`; for a full save cast to `automerge` in both). Under concurrency this
stays correct in READ COMMITTED: the row qualifies against the statement's
snapshot; if a concurrent transaction updated it, the UPDATE waits for its
row lock, and once that transaction commits Postgres re-evaluates the whole
`WHERE` clause (EvalPlanQual) and the `SET` expression against the newly
committed row version. If that version already contains the changes, the
row is skipped (UPDATE 0, nothing lost: the changes are there); otherwise
`merge` applies them to that version. If the row fails the check against
the snapshot, it is skipped without waiting: history only grows under
`merge`, so a later version contains the changes too. That argument
assumes every writer merges; a plain `SET doc = ...` that replaces history
breaks it (and loses updates anyway). Tested with two blocking sessions in
`tests/concurrency.sh` (same changes: B updates no row; different changes:
B merges into A's version). REPEATABLE READ / SERIALIZABLE raise a
serialization error instead, as for any concurrent update.

UPDATE 0 means "row missing or nothing new"; check the row's existence
separately if the difference matters. Cost: for bare changes the check
usually needs no load (new changes built on the current heads, and
re-sends of the latest changes, are decided from the chunk hashes); for a
full save it loads the stored document, so a real change then costs one
more load than the plain `merge`.

## Expanded values

Postgres lets a varlena type have an in-memory "expanded" form
(`utils/expandeddatum.h`): a function returns a pointer to an object that
stays in memory, and the flat bytes are produced only when the value is
stored, sent or copied. For `automerge` the expanded form is the loaded
Automerge document, so chains of operations stop paying a save and a
re-load between steps.

### Where it helps, and where it does not

Cost of the primitives (Rust, release build; `load` is `Automerge::load`
of the stored bytes, `apply` one small change set to a clone):

| Document | Stored | load | save_nocompress | clone | clone + apply | to_json (loaded) |
|---|---|---|---|---|---|---|
| 3,000,000-character text, 1 change | 3.0 MB | 2591 ms | 7.7 ms | 0.7 ms | 1.6 ms | 197 ms |
| 20,000 list items, 401 changes | 877 kB | 155 ms | 3.6 ms | 1.3 ms | 1.8 ms | 165 ms |
| 2,000 list items, 41 changes | 83 kB | 15 ms | 0.3 ms | 0.1 ms | 0.3 ms | 15 ms |

Loading dominates everything: it rebuilds and hashes every change. The
flat path of `merge(doc, changes)` costs a load, a save and a verification
load (2x load); an in-memory document costs a clone plus the new changes.
So the win is exactly the loads that sit *between* operations:

- Table columns always arrive flat. `UPDATE docs SET doc = merge(doc, $1)`
  still loads the stored value, applies, saves and verifies once: no
  change (and none is possible without a cross-statement cache).
- `merge(merge(a, b), c)`, `a || b || c`: the inner result stays loaded; the
  outer merge applies into it in place.
- `merge(...)::jsonb`, `automerge_heads(merge(...))` and the history
  functions on a merge result read the document in memory (no save, no
  re-load).
- PL/pgSQL `d := merge(d, x)` in a loop: the variable holds the expanded
  document, merged into in place; the save (and its verification) happens
  once, when `d` is stored.
- Repeated reads of such a variable (`d->>'a'`, `d->>'b'`) convert from
  memory instead of loading each time. A variable filled from a table
  (`SELECT doc INTO d`) is flat until the first `merge` into it (PL/pgSQL
  expands only arrays by itself).
- `merge_agg(...)::jsonb` skips the final save and re-load (the inputs
  still need a load each).

Measured with `mise run bench-expanded` (release build, warm cache, mean of
two runs, milliseconds; fixtures as in the table above, change sets are
one small commit each, `merge_agg` over the document and 8 concurrent
forks):

| Workload | 3.0 MB before | after | 877 kB before | after | 83 kB before | after |
|---|---|---|---|---|---|---|
| `UPDATE .. SET doc = merge(doc, c1)` | 5435 | 5452 | 388 | 379 | 43 | 42 |
| `automerge_heads(merge(doc, c1))` | 5258 | 2642 | 358 | 180 | 40 | 23 |
| `merge(doc, c1)::jsonb->>'status'` | 8141 | 2895 | 784 | 448 | 81 | 48 |
| `automerge_heads(merge(merge(merge(doc, c1), c2), c3))` | 15758 | 2645 | 1034 | 187 | 112 | 24 |
| PL/pgSQL `d := merge(d, c)`, 20 change sets | 105772 | 2697 | 6852 | 217 | 691 | 31 |
| the same, each merge in a `BEGIN .. EXCEPTION` block | 105827 | 2693 | 6868 | 229 | 686 | 30 |
| the same, then `UPDATE .. SET doc = d` | 106136 | 5584 | 6890 | 425 | 699 | 52 |
| 2 merges into `d`, then 10 reads `d->>'status'` | 38960 | 5154 | 4987 | 2805 | 494 | 271 |
| `merge_agg(doc)::jsonb` over 9 versions | 26488 | 24111 | 2006 | 1759 | 210 | 179 |

The loop goes from one save plus two loads per change set to one load
(plus one save and one verification load when stored). The statements
that never store their result (`automerge_heads(merge(..))`) also skip the
save and verification entirely. The single `UPDATE` is unchanged, as
expected.

### Implementation

- Object: `ExpandedAutomerge` (`src/expanded.rs`), the standard
  `ExpandedObjectHeader` followed by a pointer to a Rust
  `pg_automerge_core::loaded::LoadedDoc` (the `Automerge` document, its
  sorted heads, and its stored bytes once computed). It lives in its own
  memory context (`automerge expanded document`, a child of the calling
  context), so Postgres frees or re-parents it with the value
  (`TransferExpandedObject` when PL/pgSQL keeps it in a variable). The
  Rust document is dropped by a reset callback registered on that
  context, so it lives exactly as long as the object. The Rust heap
  memory is not counted in the context (as for `merge_agg`'s state).
- Flattening: `get_flat_size` computes the stored bytes
  (`save_nocompress()`), caches them in the `LoadedDoc` and returns their
  size; `flatten_into` copies the cached bytes (and checks the size it is
  given). The bytes are the same as the flat path's, byte for byte (a
  property test runs chains of merges both ways over generated histories:
  a document chunk lists changes in the order they were applied, and both
  paths apply them in the same order).
- The cache cannot go stale: a `LoadedDoc` is never modified. "In place"
  means the object's `LoadedDoc` is *replaced* by a new one (built from a
  clone of the old document plus the new changes, with an empty cache).
- Read-only vs read-write: only a read-write pointer
  (`VARTAG_EXPANDED_RW`) allows the replacement. With a read-only pointer
  (a variable passed to another target, a value in a slot, the other
  arguments of an in-place call) the function returns a new object and
  never touches the old one. New objects are returned read-write, as the
  protocol requires.
- Failure atomicity: the new document is complete before it replaces the
  old one, and nothing after the replacement can fail. An error (bad
  changes, missing dependencies, a duplicate seq, a decoder panic) leaves
  the argument's document as it was. This is condition 1 of
  `SupportRequestModifyInPlace`, and it holds for every read-write call,
  not just PL/pgSQL's.
- `merge(x, x)` through a read-write and a read-only pointer to the same
  object (`d := merge(d, d)`, `d := d || d`): recognized by object identity
  and returned unchanged, before any document is borrowed.
  `d := merge(d, d::bytea)` reads a flattened copy (argument unboxing
  happens before the call). This is condition 2 of the support request.
- Support function: `automerge_merge_support(internal)` is attached to both
  `merge`s (and so to both `||`) with `SUPPORT`. For
  `SupportRequestModifyInPlace` it names the first argument when it is the
  assignment target's `PARAM_EXTERN` Param, otherwise NULL (also for every
  other request). PL/pgSQL then passes the variable read-write even when
  it is declared outside a `BEGIN .. EXCEPTION` block, where its value must
  survive an error; for a local variable referenced once PL/pgSQL 18
  already transfers ownership into the expression by itself. Both paths
  are tested (a test-only counter of in-place replacements).
- Inputs: every function taking `automerge` accepts both forms
  (`AutomergeArg`); the read functions (heads, jsonb, history,
  containment, change count) use an expanded argument's document in
  memory and never flatten it. Output, `send` and casts flatten it (once;
  the bytes are cached). Functions that
  receive `automerge` as `bytea` (`doc::bytea` is binary-coercible) get the
  flattened bytes through the normal detoast.
- No-op results: an input that already contains the other is returned as
  is: a flat input as a copy of its bytes (as before), an expanded input
  as the same pointer, the way `COALESCE` passes an argument through
  (whoever keeps the result beyond the expression copies it: slots,
  PL/pgSQL assignment and SQL function results flatten a read-only
  pointer).
- `merge(automerge, bytea)` on an expanded document parses the change
  chunks (`Change::try_from`, the parser `Automerge::load` uses) and
  applies them to a clone, so it needs no load; a save or compressed
  chunks go through a strict load of a fresh save of the document plus
  the input (the old path).

### The deferred verification

A result that contains changes from a `bytea` is marked unverified. Its
save is loaded back and must keep the heads (the safeguard `normalize` and
`merge(automerge, bytea)` always had) when it is first flattened, not
inside `merge`. So such a value can never be stored unless it loads back,
as before, but the rare input that loads, applies, and then does not
survive a save and load (seen only in fuzzing, as malformed input with
recomputed checksums) now fails with the same `22P02` where the value is
stored, sent or cast, e.g. at the `UPDATE`, not at the `merge` call; a
`BEGIN .. EXCEPTION` block around only the `merge` does not catch it.
Every other bad input (framing, checksums, change columns, missing
dependencies, duplicate seq, decoder panics) still fails inside `merge`.
Verifying eagerly would cost a load per merge and undo most of the gain.

### Pitfalls avoided (supabase/pg_crdt's expanded automerge)

1. Mutating through a read-only pointer: only `VARTAG_EXPANDED_RW` allows
   replacement; tested with a PL/pgSQL variable passed read-only to two
   merges and with an alias (`x := d; d := merge(d, y)` leaves `x`).
2. Stale flat bytes: the cache belongs to an immutable `LoadedDoc`, and a
   merge replaces the whole `LoadedDoc`; tested by storing, merging in
   place and storing again (and in the core tests).
3. Support function not attached: `SUPPORT automerge_merge_support` is in
   the generated `CREATE FUNCTION`s; a pg_test checks `pg_proc.prosupport`
   and that PL/pgSQL's in-place path is taken inside an `EXCEPTION` block.
4. Document memory outside the object's lifecycle: the document is owned
   by the object and dropped by its context's reset callback; a pg_test
   runs about 2,400 merges in PL/pgSQL loops and checks that
   `pg_backend_memory_contexts` shows at most the live variables' objects
   during the loop and none after, and that the number of live Rust
   documents returns to where it was.

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
- `automerge_contains(doc, changes bytea)` and the no-op check of
  `merge(doc, changes bytea)` need no load for re-sent latest changes and
  (for `automerge_contains`) new changes on top of the current heads.
- `automerge_notify()` reads at most a prefix of each changed `automerge`
  value per row, never loads a document.
- Merge results are expanded values (see "Expanded values"): nested
  merges, `merge(...)::jsonb`, `merge_agg(...)::jsonb` and PL/pgSQL loops
  doing `d := merge(d, x)` keep the document loaded between steps. A
  per-backend cache of documents loaded from tables (and of converted
  jsonb) is future work: a table column always arrives flat.

## Implementation notes (pgrx)

- `automerge` is a custom varlena type. It is not a
  `#[derive(PostgresType)]`, whose serde/CBOR storage we don't want. Define it
  in SQL via `extension_sql!` (shell type, I/O functions, `CREATE TYPE`) and
  give the Rust newtype manual `FromDatum` / `IntoDatum` / `SqlTranslatable`
  impls mapping to SQL `automerge`. Detoast with `pg_detoast_datum_packed` or
  equivalent. Never hold references into a detoasted datum beyond the call.
  That newtype (`AutomergeDatum`) is only a result type (input functions,
  `bytea` cast). Arguments are `AutomergeArg`: flat (the raw datum,
  detoasted on demand, only a prefix for the heads) or a pointer to one of
  our expanded objects (read-only or read-write); it exists only for the
  duration of a call. Other results are `AutomergeValue`: new flat bytes,
  or a datum passed through (an unchanged argument, a new expanded
  object). All three are listed in the `creates` of the type's
  `extension_sql!` block.
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
  and the header parser, the change-chunk splitter, contains and
  contains_changes, the history functions in `history.rs`, the notification
  payload builder in `notify.rs`, the in-memory documents behind expanded
  values in `loaded.rs`, the jsonb mapping, hex/base64/ISO 8601
  encoders) is plain Rust in
  `crates/pg_automerge_core` with `#[test]`s; `src/lib.rs` is pgrx glue. The
  I/O functions are declared by hand inside the `CREATE TYPE` block
  (`#[pg_extern(sql = false)]`), and the Rust type `AutomergeDatum` maps to SQL
  `automerge` with `TypeOrigin::ThisExtension`, so pgrx orders every other
  function after the type.
- Tests: `#[pg_test]` for SQL behaviour, plain `#[test]` for the pure
  conversion logic (edge cases in `crates/pg_automerge_core/tests/`:
  `edge_cases.rs`, `merge_changes.rs` for `merge(automerge, bytea)` and
  `automerge_contains(automerge, bytea)`,
  `heads_fast_path.rs` for the header parser property test, `history.rs`
  for the history functions, checked against Automerge (`fork_at`,
  `get_changes`) and a dependency-graph walk, `loaded.rs` for loaded
  documents (byte-identical to the flat path); `common/` holds the shared
  random-history generator), and
  `tests/pg_regress` for user-facing examples. `tests/concurrency.sh`
  (`mise run concurrency`, also the last step of `mise run test`) runs two real psql sessions against one row to
  check the EvalPlanQual claim above, the upsert path and REPEATABLE READ,
  two sessions persisting only incremental changes with
  `merge(doc, $1::bytea)` (plus the orphaned-changes rejection), the
  `WHERE NOT automerge_contains(doc, $1)` pattern under EvalPlanQual, and a
  pg_dump/restore round trip. `tests/notify.sh` (`mise run notify`, also
  part of `mise run test`) checks `automerge_notify()` with a real `LISTEN`
  session: pg_tests run inside one transaction that is rolled back, so
  NOTIFY never delivers there (the pg_tests instead read what the trigger
  sent from a test-only per-backend list). Build test documents in Rust with
  `automerge::AutoCommit` and pass them in as `bytea`.
- `mise run bench-expanded` (`tests/bench_expanded.sh`, not part of `mise
  run test`) installs a release build and times the merge workloads of
  "Expanded values" on three generated documents.
- `mise run regress` passes `--resetdb`: a reused regress database would
  keep the extension objects of an earlier build, so new functions would be
  missing.
- Build profile: dev builds compile dependencies optimized and Automerge
  without debug assertions or overflow checks (Cargo.toml). Its debug
  assertions make large documents quadratic, and its overflow checks turned a
  counter past i64::MAX into a panic on read in dev builds only.
