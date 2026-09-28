# pg_automerge

A Postgres 18 extension that stores [Automerge](https://automerge.org)
documents in an `automerge` column, lets you query them with every `jsonb`
operator, function and index, and merges concurrent writes so that two
backends never overwrite each other's changes.

The extension does not edit documents and never creates changes. Your backend
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

UPDATE docs SET doc = merge(doc, $2::automerge) WHERE id = $1;

-- Read: automerge casts implicitly to jsonb.
SELECT doc->>'title' FROM docs WHERE doc @> '{"status": "open"}';
SELECT jsonb_path_query(doc, '$.items[*] ? (@.done == false)') FROM docs;

-- Load it back in the backend.
SELECT doc::bytea FROM docs WHERE id = $1;
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
| `merge(a, b)`, `a \|\| b` | CRDT merge. Commutative and idempotent; returns an input unchanged if it already contains the other. |
| `merge_agg(automerge)` | Aggregate merge of all non-null inputs. |
| `automerge_heads(automerge) → text[]` | Current heads, sorted hex change hashes. |
| `automerge_contains(a, b) → bool` | Whether `a` already has every change of `b`. |

jsonb mapping: maps/tables → objects, lists → arrays, text → strings,
integers and counters → exact numbers, NaN/±Infinity → `null`, timestamps →
ISO 8601 UTC strings with milliseconds, bytes → base64 strings. Conflicting
concurrent values show Automerge's winner.

Gotchas:

- `merge(doc, $1)` with a parameter the driver types as `bytea` needs
  `$1::automerge` (function arguments only use implicit casts).
- `doc || '{"a": 1}'` means `merge`, not jsonb concatenation; write
  `doc::jsonb || '{"a": 1}'`.
- There is no equality or btree opclass, so no `DISTINCT`/`GROUP BY` on
  `automerge` columns; compare `automerge_heads(...)` or `doc::jsonb` instead.

## Development

Tooling runs through [mise](https://mise.jdx.dev):

```sh
mise run pgrx-init   # once: build the Postgres pgrx develops against
mise run test        # core unit tests + #[pg_test] tests
mise run regress     # pg_regress examples in tests/pg_regress
```
