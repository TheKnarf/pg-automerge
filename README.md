# pg_automerge

A Postgres 18 extension that stores [Automerge](https://automerge.org)
documents in an `automerge` column, lets you query them with every `jsonb`
operator, function and index, and merges concurrent writes so that two
backends never overwrite each other's changes.

The extension does not edit documents and never creates changes (it can
list and return the changes a document already has). Your backend
syncs with its frontends, owns the actor IDs, and persists Automerge saves;
Postgres stores them, merges them and makes them queryable. See
[docs/DESIGN.md](docs/DESIGN.md) for the full specification.

## Usage

```sql
CREATE EXTENSION pg_automerge;

CREATE TABLE docs (id uuid PRIMARY KEY, doc automerge NOT NULL);

-- Persist: $2 is the output of Automerge save() (or save + incremental saves),
-- bound as bytea. Concurrent writers merge instead of overwriting.
INSERT INTO docs (id, doc) VALUES ($1, $2)
ON CONFLICT (id) DO UPDATE SET doc = merge(docs.doc, EXCLUDED.doc);

-- Or persist incrementally: $2 is only the new changes (save_incremental()
-- or save_after(heads)), bound as bytea, applied on top of the stored row.
UPDATE docs SET doc = merge(doc, $2) WHERE id = $1;

-- Read: automerge casts implicitly to jsonb.
SELECT doc->>'title' FROM docs WHERE doc @> '{"status": "open"}';
SELECT jsonb_path_query(doc, '$.items[*] ? (@.done == false)') FROM docs;

-- Load it back in the backend.
SELECT doc::bytea FROM docs WHERE id = $1;

-- Or only what a replica at heads $2 (text[]) is missing, as change chunks
-- for loadIncremental / applyChanges (Automerge's save_after).
SELECT automerge_changes_bytes(doc, $2) FROM docs WHERE id = $1;

-- History: who changed what, and the state as of any change.
SELECT seq, actor, time, message FROM docs, automerge_changes_meta(doc) WHERE id = $1;
SELECT automerge_to_jsonb(doc, ARRAY[$2]) FROM docs WHERE id = $1;
```

For read-heavy tables, keep a jsonb copy and index it (the cast is IMMUTABLE):

```sql
ALTER TABLE docs ADD COLUMN data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED;
CREATE INDEX ON docs USING gin (data jsonb_path_ops);
-- or an expression index on the document itself:
CREATE INDEX ON docs USING gin ((doc::jsonb));
```

### SQL API

| | |
|---|---|
| `automerge` | Type. Stores `save_nocompress()` bytes; every input is validated and normalized. Text form is `\x` + hex (lossless, used by `pg_dump`/`COPY`). |
| `bytea → automerge` | Assignment cast (validates). |
| `automerge → bytea` | Explicit cast: the stored Automerge bytes. |
| `automerge → jsonb` | Implicit cast / `automerge_to_jsonb(automerge)`: the current state. |
| `merge(a, b)`, `a \|\| b` | CRDT merge. Commutative and idempotent in state (heads and jsonb), not byte for byte; returns an input unchanged if it already contains the other. |
| `merge(doc, changes bytea)`, `doc \|\| changes` | Apply a save or bare change chunks (`save_incremental()` / `save_after()` output, may be concatenated) on top of `doc`. Returns `doc` unchanged if nothing is new; rejects changes with missing dependencies (22P02, naming them). |
| `merge_agg(automerge)` | Aggregate merge of all non-null inputs. |
| `automerge_heads(automerge) → text[]` | Current heads, sorted hex change hashes. Read from the stored header, without loading the document. |
| `automerge_contains(a, b) → bool` | Whether `a` already has every change of `b`. |
| `automerge_changes(doc, since_heads text[] DEFAULT '{}')` | `SETOF automerge_change (hash, actor, seq, start_op, op_count, time, message, deps, change bytea)`: every change not reachable from `since_heads` (all by default), dependencies first. Rebuilds change bytes (costly on big documents). |
| `automerge_changes_meta(doc, since_heads DEFAULT '{}')` | The same rows without `change` (`SETOF automerge_change_meta`); needs only the change graph. |
| `automerge_changes_bytes(doc, since_heads DEFAULT '{}') → bytea` | Those changes as concatenated change chunks (`save_after(since_heads)`); `merge(replica, ...)` or `loadIncremental` applies them. |
| `automerge_get_change(doc, hash) → automerge_change` | One change with its bytes; NULL if absent. |
| `automerge_change_count(doc) → bigint` | Number of changes, read from the stored bytes without loading the document. |
| `automerge_to_jsonb(doc, heads text[]) → jsonb` | The state as of `heads` (`'{}'`: before any change). |

jsonb mapping: maps/tables → objects, lists → arrays, text → strings,
integers and counters → exact numbers, NaN/±Infinity → `null`, timestamps →
ISO 8601 UTC strings with milliseconds, bytes → base64 strings. Conflicting
concurrent values show Automerge's winner.

Gotchas:

- `merge(doc, $1)` with a parameter the driver types as `bytea` uses
  `merge(automerge, bytea)`, which accepts full saves and bare change
  chunks. An *untyped* literal or parameter (`merge(doc, '\x..')`) resolves
  to `merge(automerge, automerge)` and must be a complete document; write
  `'\x..'::bytea` for change chunks.
- Incremental changes need an existing row: in
  `INSERT .. VALUES ($1, $2) ON CONFLICT ..` the value is cast to
  `automerge` on its own first, and bare changes are not a document.
- `doc || '{"a": 1}'` means `merge`, not jsonb concatenation; write
  `doc::jsonb || '{"a": 1}'`.
- There is no `automerge` equality or btree opclass, so no
  `DISTINCT`/`GROUP BY` on `automerge` columns. But `a = b`, `a <> b`, `<`
  etc. do *not* fail: through the implicit cast they become **jsonb**
  comparisons of the current state. Two documents with different histories
  but the same content compare equal. For "same document/history" compare
  `automerge_heads(a) = automerge_heads(b)` (or `automerge_contains` both
  ways).
- `merge(a, b)` and `merge(b, a)` have the same heads and jsonb but can differ
  in bytes when neither contains the other. Don't dedupe or cache on
  `doc::bytea` / `md5(doc::bytea)`; use `automerge_heads(doc)`.
- `merge_agg` keeps a fully loaded document per group in backend memory
  outside Postgres' memory accounting; HashAgg cannot spill it, and
  `work_mem`/`hash_mem_multiplier` do not limit it. The aggregate declares a
  1 MB state size so the planner favours sorted grouping, but for a grouped
  `merge_agg` over many large documents check `EXPLAIN` and use
  `SET enable_hashagg = off` if needed.
- Loading, merging and converting a single document runs without interrupt
  checks, so a cancel or `statement_timeout` takes effect only once that call
  returns (for `merge_agg`, at the next input row). With documents of tens of
  MB a single call can take a noticeable time.
- Malformed input is always SQLSTATE `22P02` (`invalid automerge document`),
  including input that passes Automerge's checksums but panics its decoder.
- In `since_heads`, hashes the document does not have are **ignored** (as
  in Automerge's `getChanges`): a replica that is ahead of the stored row
  gets every change that is not an ancestor of the heads the row knows,
  which is more than it needs but never less. A mistyped hash therefore
  returns more changes, not an error. `automerge_to_jsonb(doc, heads)`
  instead fails with `22023` for an unknown hash. Malformed hashes (not 64
  hex digits) are `22P02` everywhere.
- `time` is Automerge's commit time in Unix seconds, NULL when unset.
- `automerge_changes`, `automerge_changes_bytes` and `automerge_get_change`
  rebuild change bytes from the document (on a 3 MB document, all changes
  took 5 s against 2.8 s for `automerge_changes_meta`); prefer
  `automerge_changes_meta` for listings. The set-returning functions
  compute all rows up front, so `LIMIT` does not make them cheaper.

## Development

Tooling runs through [mise](https://mise.jdx.dev):

```sh
mise run pgrx-init   # once: build the Postgres pgrx develops against
mise run test        # core unit tests + #[pg_test] tests + the concurrency test
mise run regress     # pg_regress examples in tests/pg_regress
mise run concurrency # only: two real psql sessions merging into one row, pg_dump round trip
```
