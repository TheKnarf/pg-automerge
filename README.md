# pg_automerge

A Postgres 18 extension that stores [Automerge](https://automerge.org)
documents in an `automerge` column, lets you query them with every `jsonb`
operator, function and index, and merges concurrent writes so that two
backends never overwrite each other's changes. It also exposes a
document's history (changes, and the state as of earlier heads) and
notifies listening backends when a document changes.

The extension does not edit documents and never creates changes. Your
backend syncs with its frontends, owns the actor IDs, and persists
Automerge saves; Postgres stores them, merges them and makes them
queryable. [docs/DESIGN.md](docs/DESIGN.md) is the full specification.

## Requirements

- PostgreSQL 18 (the only supported version).
- To build: Rust 1.98 and `cargo-pgrx` 0.19.3 (the exact version of the
  `pgrx` crate), both pinned in `mise.toml`, plus what pgrx needs to build
  against Postgres (a C toolchain, libclang).
- `CREATE EXTENSION` needs a superuser: the extension is not trusted, see
  [Install](#install).

## Install

From source, with [mise](https://mise.jdx.dev):

```sh
mise install                 # Rust and cargo-pgrx as pinned in mise.toml
mise run pgrx-init           # once: download and build the Postgres 18 pgrx develops against
# into your Postgres 18 (the one whose pg_config you pass):
cargo pgrx install --release --pg-config /usr/lib/postgresql/18/bin/pg_config
```

Without mise, install Rust 1.98 and `cargo install --locked cargo-pgrx
--version 0.19.3`, run `cargo pgrx init --pg18 /path/to/pg_config`, then
the same `cargo pgrx install`. No `shared_preload_libraries` entry is
needed. Then, in each database:

```sql
CREATE EXTENSION pg_automerge;               -- as a superuser
-- or into a schema of its own (then qualify, or add it to search_path):
CREATE EXTENSION pg_automerge SCHEMA automerge;
```

The extension is relocatable: `ALTER EXTENSION pg_automerge SET SCHEMA x`
moves it, and tables, domains, views, indexes, generated columns, check
constraints, triggers and `BEGIN ATOMIC` SQL functions that use it keep
working (they refer to it by OID). What resolves names at run time does
not follow: queries, string-bodied SQL and PL/pgSQL functions, and
`search_path` settings must name the new schema. The `||` operator is
found only through `search_path` (or as `OPERATOR(x.||)`); the casts work
regardless.

It is **not trusted** (`trusted = false`): only a superuser can install
it, not a database owner. The reason is a denial of service: a few kB of
crafted (or merely compressed, highly repetitive) Automerge input can make
a single load allocate gigabytes, and a failed Rust allocation aborts the
backend, which restarts the whole cluster. Anyone who can write to an
`automerge` column can send such input (see
[Limitations](#limitations-and-gotchas)), so installing it into a
database is a superuser's decision. Details in
[DESIGN.md](docs/DESIGN.md#installation-schema-and-privileges).

There are no published packages yet (tag builds in CI attach a tarball,
built against the PGDG Postgres 18 on Ubuntu, to the workflow run as an
artifact; they are not GitHub releases). To build one for another
machine, use that server's `pg_config` (same Postgres major version, same
platform). `PG_CONFIG` is required, and pgrx's own development Postgres
(under `~/.pgrx`) is refused: a package built for it would unpack into
`~/.pgrx/...` where no real server looks.

```sh
PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config mise run package
# target/release/pg_automerge-pg18.tar.gz mirrors that server's paths
# (pg_config --pkglibdir and --sharedir/extension), so on the server:
sudo tar -C / -xzf pg_automerge-pg18.tar.gz
```

The tarball holds `pg_automerge.so`, `pg_automerge.control`, the install
script `pg_automerge--<version>.sql` and any upgrade scripts. After
installing a newer version, run `ALTER EXTENSION pg_automerge UPDATE` in
each database.

## Quick start

```sql
CREATE TABLE docs (id uuid PRIMARY KEY, doc automerge NOT NULL);

-- Persist: $2 is the output of Automerge save() (or save + incremental saves),
-- bound as bytea. Concurrent writers merge instead of overwriting.
INSERT INTO docs (id, doc) VALUES ($1, $2)
ON CONFLICT (id) DO UPDATE SET doc = merge(docs.doc, EXCLUDED.doc);

-- Or persist incrementally: $2 is only the new changes (save_incremental()
-- or save_after(heads)), bound as bytea, applied on top of the stored row.
-- A full save works here too, and when the row exists this costs one load
-- of the save against the upsert's two.
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

Every read through `jsonb` loads the document. For read-heavy tables, keep
a jsonb copy and index it (the cast is IMMUTABLE):

```sql
ALTER TABLE docs ADD COLUMN data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED;
CREATE INDEX ON docs USING gin (data jsonb_path_ops);
-- or an expression index on the document itself:
CREATE INDEX ON docs USING gin ((doc::jsonb));
```

## Keeping backends in sync

Each backend holds replicas of the documents its clients use. To learn
when another backend persisted changes, attach the notification trigger and
`LISTEN`:

```sql
CREATE TRIGGER docs_notify AFTER INSERT OR UPDATE OR DELETE ON docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('docs_changed', 'id');

LISTEN docs_changed;
-- payloads look like
-- {"table":"public.docs","op":"UPDATE","seq":17,"key":{"id":"…"},
--  "columns":{"doc":{"heads":["79df…"],"prev_heads":["891e…"]}}}
```

On a notification for a document you hold, skip it if you already have
every hash in `heads` (you probably wrote them), otherwise fetch exactly
what your replica lacks and apply it with `loadIncremental` /
`applyChanges`:

```sql
SELECT automerge_changes_bytes(doc, $2) FROM docs WHERE id = $1;  -- $2: your replica's heads
```

Notifications are sent at commit and only to connected listeners, so after
(re)connecting, `LISTEN` first and then run the same query for every
document you hold. INSERT and DELETE always notify; an UPDATE notifies only
when a document's heads (or the key) change, so no-op merges are silent.
Payloads over NOTIFY's 8000-byte limit (about 115 heads) drop the heads and
carry `"truncated":true`; then just fetch. `seq` (a per-backend counter)
keeps payloads distinct, since `NOTIFY` would otherwise collapse identical
events of one transaction (e.g. INSERT, DELETE, INSERT of one row). Details in
[DESIGN.md](docs/DESIGN.md#change-notifications).

A no-op merge still rewrites the row. When re-sends are common, skip them:

```sql
UPDATE docs SET doc = merge(doc, $1) WHERE id = $2 AND NOT automerge_contains(doc, $1);
```

This is safe with concurrent writers in READ COMMITTED: a writer that had
to wait for another's row lock re-checks the whole `WHERE` against the
committed row (EvalPlanQual), so it skips the row if the other writer
already stored the same changes and merges into the new version otherwise.
For bare changes (`$1` bytea) the check usually needs no load of the
document; for a full save it needs none when the save is the stored
document's own, and costs one extra load of the stored document when there
is something new.

## Merging in PL/pgSQL and nested merges

A merge result stays loaded in memory (an expanded value) until it is
stored, sent or cast to `bytea`, so chains of merges do not save and
re-load the document at every step:

```sql
-- d is merged into in place; the document is saved once, at the UPDATE.
DECLARE d automerge; ch bytea;
BEGIN
    SELECT doc INTO d FROM docs WHERE id = $1 FOR UPDATE;
    FOREACH ch IN ARRAY change_sets LOOP
        d := merge(d, ch);
    END LOOP;
    UPDATE docs SET doc = d WHERE id = $1;
END;
```

On a 3 MB document, merging 20 change sets this way and storing the result
takes 5.4 s, about two loads of the document in total;
`merge(merge(merge(doc, a), b), c)`, `merge(...)::jsonb` and
`merge_agg(...)::jsonb` also skip the intermediate saves and loads. A
single `UPDATE .. SET doc = merge(doc, $1)` gains nothing (the column
arrives flat). A failed `merge` leaves the variable unchanged, also inside
a `BEGIN .. EXCEPTION` block. Details in
[DESIGN.md](docs/DESIGN.md#expanded-values).

## SQL API

Functions are `IMMUTABLE STRICT PARALLEL SAFE` (I S P below) unless noted.

| | Volatility | |
|---|---|---|
| `automerge` | | Type. Stores `save_nocompress()` bytes; every input is validated and normalized. Text form is `\x` + hex (lossless, used by `pg_dump`/`COPY`). |
| `bytea → automerge` | I S P | Assignment cast (validates). |
| `automerge → bytea` | | Explicit binary-coercible cast: the stored Automerge bytes. |
| `automerge → jsonb` | I S P | Implicit cast / `automerge_to_jsonb(automerge)`: the current state. |
| `merge(a, b)`, `a \|\| b` | I S P | CRDT merge. Commutative and idempotent in state (heads and jsonb), not byte for byte; returns an input unchanged if it already contains the other, otherwise an in-memory (expanded) result. Two histories with different changes under one actor id cannot be merged (22000, see [Limitations](#limitations-and-gotchas)). |
| `merge(doc, changes bytea)`, `doc \|\| changes` | I S P | Apply a save or bare change chunks (`save_incremental()` / `save_after()` output, may be concatenated) on top of `doc`. Returns `doc` unchanged if nothing is new; rejects changes with missing dependencies (22P02, naming them in the DETAIL). |
| `merge_agg(automerge)` | immutable, parallel safe (no combine function) | Aggregate merge of all non-null inputs. Loads a document only to merge it: a single row, or a version plus older ones, costs no load or one. |
| `automerge_heads(automerge) → text[]` | I S P | Current heads, sorted hex change hashes. Read from the stored header, without loading the document. |
| `automerge_contains(a, b) → bool` | I S P | Whether `a` already has every change of `b`. Decided without loading when the heads or the change counts can tell (`b` newer than or concurrent with `a`); otherwise loads `a`. |
| `automerge_contains(doc, changes bytea) → bool` | I S P | Whether `merge(doc, changes)` would add nothing (every change in the save or change chunks is already in `doc`). Usually decided without loading the document. |
| `automerge_notify('channel', 'key_col' [, ...])` | volatile, parallel unsafe | `AFTER INSERT OR UPDATE OR DELETE FOR EACH ROW` trigger: `NOTIFY channel` with the row key and the new/previous heads of `automerge` columns whose heads changed. |
| `automerge_changes(doc, since_heads text[] DEFAULT '{}')` | I S P | `SETOF automerge_change (hash, actor, seq, start_op, op_count, time, message, deps, change bytea)`: every change not reachable from `since_heads` (all by default), dependencies first. Rebuilds change bytes (costly on big documents). |
| `automerge_changes_meta(doc, since_heads DEFAULT '{}')` | I S P | The same rows without `change` (`SETOF automerge_change_meta`); needs only the change graph. |
| `automerge_changes_bytes(doc, since_heads DEFAULT '{}') → bytea` | I S P | Those changes as concatenated change chunks (`save_after(since_heads)`); `merge(replica, ...)` or `loadIncremental` applies them. |
| `automerge_get_change(doc, hash) → automerge_change` | I S P | One change with its bytes; NULL if absent. |
| `automerge_change_count(doc) → bigint` | I S P | Number of changes, read from the stored bytes without loading the document. |
| `automerge_to_jsonb(doc, heads text[]) → jsonb` | I S P | The state as of `heads` (`'{}'`: before any change). |

Every object has a `COMMENT` (`\df+`, `\dT+`). Error codes are listed in
[DESIGN.md](docs/DESIGN.md#error-codes).

## jsonb mapping

Maps/tables → objects, lists → arrays, text → strings, integers and
counters → exact numbers, NaN/±Infinity → `null`, timestamps → ISO 8601
UTC strings with milliseconds, bytes → base64 strings. Conflicting
concurrent values show Automerge's winner. Details in
[DESIGN.md](docs/DESIGN.md#jsonb-mapping).

## Configuration

| Setting | Default | Who can change it | Effect |
|---|---|---|---|
| `pg_automerge.verify_writes` | `on` | superusers, or roles granted `SET` on it (`GRANT SET ON PARAMETER pg_automerge.verify_writes TO writer`); also `ALTER ROLE`/`ALTER DATABASE .. SET` by a superuser | Load back the normalized save of every value built from client bytes (text input, binary receive, the `bytea` cast, `merge(automerge, bytea)` results) before it is stored or sent |

With `pg_automerge.verify_writes = off`, incremental writes
(`merge(doc, $changes)`) cost one load instead of two, and input that is not
already a canonical or compressed save skips its second load. Parsing,
checksums, missing-dependency checks and panic guards still run. The risk:
malformed input that loads, but whose re-save does not (so far seen only in
fuzzing), is stored instead of rejected with `22P02`, and every later read
of that row fails with `XX000` until it is overwritten. It is
superuser-only because one writer turning it off could store a value that
breaks reads for everyone. Turn it off only for trusted writers, e.g.
`ALTER ROLE app_backend SET pg_automerge.verify_writes = off` for the role
of a backend that sends what its own Automerge produced. It never changes
a result's bytes, so functions stay `IMMUTABLE` and dumps are unaffected.
Details in
[DESIGN.md](docs/DESIGN.md#the-pg_automergeverify_writes-setting).

## Limitations and gotchas

- **Each jsonb access is a load.** `SELECT doc->>'a', doc->>'b', doc->>'c'`
  converts the document three times (1275 ms against 414 ms for one
  access in a measurement). Convert once with
  `FROM docs, LATERAL (SELECT doc::jsonb AS j OFFSET 0) x` and read
  `x.j->>'a'` (the `OFFSET 0` stops the planner from inlining the cast
  back), or keep the generated jsonb column shown above.
- **Some writes cost two loads.** A full save, compressed
  (`Automerge.save()`) or not (`saveNoCompress()`, `doc::bytea`), is loaded
  once to validate it, and `merge(doc, $save)` with a newer save loads only
  the save. Other input (a save followed by change chunks, a document saved
  by a different Automerge implementation or version whose encoding
  differs) and every merged result of incremental changes is loaded,
  re-saved and loaded again to verify it; the upsert of a full save loads
  it twice (Postgres hands `EXCLUDED` over flat).
- **`pg_automerge.verify_writes`** is that verification load; see
  [Configuration](#configuration).
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
- **No equality or btree opclass**, so no `DISTINCT`/`GROUP BY` on
  `automerge` columns. But `a = b`, `a <> b`, `<` etc. do *not* fail:
  through the implicit cast they become **jsonb** comparisons of the
  current state. Two documents with different histories but the same
  content compare equal. For "same document/history" compare
  `automerge_heads(a) = automerge_heads(b)` (or `automerge_contains` both
  ways).
- `merge(a, b)` and `merge(b, a)` have the same heads and jsonb but can differ
  in bytes when neither contains the other. Don't dedupe or cache on
  `doc::bytea` / `md5(doc::bytea)`; use `automerge_heads(doc)`.
- **Memory outside Postgres' accounting.** `merge_agg` keeps a fully
  loaded document per group in backend memory (once a group needs a
  merge; before that, the stored bytes); HashAgg cannot spill it,
  and `work_mem`/`hash_mem_multiplier` do not limit it. The aggregate
  declares a 1 MB state size so the planner favours sorted grouping, but
  for a grouped `merge_agg` over many large documents check `EXPLAIN` and
  use `SET enable_hashagg = off` if needed. In-memory merge results (the
  PL/pgSQL example above) are likewise not counted.
- **Small input, large load.** Automerge input is compressed and
  run-length encoded: a 4 kB save of a 4,000,000-character text takes
  4.3 s and 390 MB to load, and memory grows linearly with what the input
  describes, not with its size. That memory is outside Postgres'
  accounting, and when an allocation fails the backend aborts and the
  postmaster restarts every session. There is no size limit yet; let only
  roles you trust write `automerge` values (input functions and casts
  cannot be revoked) and keep memory overcommit in mind.
- **A load is not interruptible.** The jsonb conversion and the history
  functions check for interrupts as they go (and `merge_agg` between
  rows), but a single Automerge load, merge or save runs to the end
  before a cancel or `statement_timeout` takes effect: 2.5 s for a 3 MB
  document, longer for documents of tens of MB.
- **Every writer needs its own actor id.** Two writers (or two copies of
  a document) that commit with the same actor id produce different changes
  with the same sequence number, and Automerge cannot merge them: `merge`,
  `merge_agg` and `merge(doc, changes)` fail with SQLSTATE `22000`
  (`conflicting automerge changes: actor ... has two different changes
  with seq N`, with a HINT). Retrying does not help; the fix is in the
  writer. Automerge picks a random actor id per document instance unless
  you set one.
- Malformed input is always SQLSTATE `22P02` (`invalid automerge document`),
  including input that passes Automerge's checksums but panics its decoder.
  One rare case is reported late: a result of `merge(doc, bytea)` is checked
  to survive a save and load when it is first stored, sent or cast, not
  inside `merge` (a `BEGIN .. EXCEPTION` around just the `merge` does not
  catch it). Nothing that fails the check is ever stored.
- In `since_heads`, hashes the document does not have are **ignored** (as
  in Automerge's `getChanges`): a replica that is ahead of the stored row
  gets every change that is not an ancestor of the heads the row knows,
  which is more than it needs but never less. A mistyped hash therefore
  returns more changes, not an error. `automerge_to_jsonb(doc, heads)`
  instead fails with `22023` for an unknown hash. Malformed hashes (not 64
  hex digits) are `22P02` everywhere.
- `time` is Automerge's commit time in Unix seconds, NULL when unset.
- `automerge_notify()` checks its arguments when it fires, not at
  `CREATE TRIGGER`: a wrong key column or a BEFORE / statement-level
  trigger fails the first write. Its channel is used verbatim, like
  `pg_notify`, while `LISTEN` lower-cases unquoted names: use a lower-case
  channel. On a partitioned table, `table` is the partition.
- `automerge_changes`, `automerge_changes_bytes` and `automerge_get_change`
  rebuild change bytes from the document; prefer `automerge_changes_meta`
  for listings. The set-returning functions compute all rows up front, so
  `LIMIT` does not make them cheaper.
- **Replication.** Physical replication and backups carry the values as
  ordinary bytes, and a hot standby answers every read, but `LISTEN` is not
  available on a standby: listeners connect to the primary. Logical
  replication works in text and binary mode (`tests/replication.sh`) with
  these caveats:
  - The subscriber needs the extension, and validates every replicated
    value again on the way in: one load per row and column (2.5 s for a 3
    MB document), whichever mode.
  - Use a primary key (or unique index) as the replica identity. With
    `REPLICA IDENTITY FULL` the subscriber cannot match rows for `UPDATE`
    and `DELETE` ("could not identify an equality operator for type
    automerge": the type has none), and the apply worker fails and retries
    until the subscription is fixed.
  - `automerge_notify()` does not fire on the subscriber for replicated
    rows unless the trigger is enabled with `ALTER TABLE .. ENABLE ALWAYS
    TRIGGER` (or `ENABLE REPLICA`).
- **Dump and restore** (`tests/dump.sh`): plain and custom-format dumps
  restore with the extension in any schema; a restore and `COPY FROM`
  validate every value (one load each) and recompute stored generated
  columns (one more conversion each).

## Performance

Loading a document dominates everything; the rest is cheap. On a 3 MB
document (3,000,000-character text; release build, warm cache):

| Operation | Time |
|---|---|
| `Automerge::load` (what `doc::jsonb`, history and most merges pay) | 2.5 s |
| Writing a save, compressed or not (`$1::automerge`: validation) | 2.6 s (one load) |
| `doc->>'status'` (load, then jsonb) | 2.8 s |
| `automerge_heads`, `automerge_change_count` (header only) | 0.3 ms, 2 ms |
| `merge(doc, x)` when `x` adds nothing | 1 ms (heads from a prefix; an `UPDATE` keeps the TOAST value) |
| `automerge_contains(doc, newer save or version)` (false) | 1-9 ms (heads and change counts from a prefix; the other way round, one load) |
| `UPDATE .. SET doc = merge(doc, one change set)` | 5.2 s (load, save, verification load); 2.7 s with `pg_automerge.verify_writes = off` |
| `UPDATE .. SET doc = merge(doc, newer full save)` | 2.8 s (one load, of the save) |
| `INSERT .. ON CONFLICT DO UPDATE SET doc = merge(docs.doc, EXCLUDED.doc)`, newer save | 5.3 s (two loads) |
| PL/pgSQL: merge 20 change sets into a variable, then store | 5.4 s |
| `automerge_changes_meta(doc)` / `automerge_changes(doc)` (201 changes) | 2.8 s / 5.1 s |

On an 83 kB document (2,000 list items) the same single-change `UPDATE`
takes 41 ms, and `doc->>'status'` 33 ms. More numbers, and what each function loads, in
[DESIGN.md](docs/DESIGN.md#performance).

## Development

Tooling runs through [mise](https://mise.jdx.dev):

```sh
mise run pgrx-init   # once: build the Postgres pgrx develops against
mise run test        # core tests + #[pg_test] tests + the concurrency, notify, dump, upgrade and extension scripts
mise run regress     # pg_regress examples in tests/pg_regress (checks their fixtures first)
mise run lint        # CI/packaging checks, rustfmt, clippy -D warnings (all build configurations), rustdoc
mise run ci          # lint + test + regress: what CI runs
mise run concurrency # only: two real psql sessions merging into one row
mise run notify      # only: a real LISTEN session receiving automerge_notify() payloads
mise run dump        # only: pg_dump/pg_restore and COPY round trips of every object kind
mise run upgrade     # only: ALTER EXTENSION UPDATE from every released version
mise run extension   # only: relocation (SCHEMA, SET SCHEMA, dump), install and setting privileges
mise run replication # logical replication in a scratch cluster (not part of test)
mise run fuzz        # a long mutation-fuzzing session of the core (not part of test)
mise run bench-sql   # median timings of the everyday SQL paths on a release build (minutes)
mise run bench-expanded  # SQL timings of merge chains and PL/pgSQL loops on a release build (minutes)
mise run bench-core  # Rust timings of load, normalize and the jsonb walk
mise run package     # release package for the Postgres of $PG_CONFIG (required)
mise run run         # install and open psql against the pgrx-managed Postgres
```

`crates/pg_automerge_core` holds all Automerge logic as plain Rust; `src/`
is the pgrx glue. See [DESIGN.md](docs/DESIGN.md#architecture).

## License

MIT — see [LICENSE](LICENSE).
