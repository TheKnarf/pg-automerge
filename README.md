# pg_automerge

A Postgres 18 extension that stores [Automerge](https://automerge.org)
documents in an `automerge` column, lets you query them with every `jsonb`
operator, function and index, and merges concurrent writes so that two
backends never overwrite each other's changes. It also exposes a
document's history (changes, and the state as of earlier heads) and the
structure of its rich text (marks and blocks, as Automerge's `spans()`),
and notifies listening backends when a document changes.

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

To self-host, the [Docker image](#docker) is `postgres:18` with the
extension built in. From source, with [mise](https://mise.jdx.dev):

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
it, not a database owner. A few kB of crafted (or merely compressed,
highly repetitive) Automerge input can describe a document that takes
gigabytes to load, and a failed Rust allocation aborts the backend, which
restarts the whole cluster. Every write is priced before it is loaded and
refused over [`pg_automerge.max_load_memory`](#configuration), but that
bound rests on measured costs, memory stays outside Postgres' accounting
and a load cannot be cancelled, so installing it into a database remains
a superuser's decision. Details in
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

## Docker

Managed Postgres services do not load custom C extensions, so for
self-hosting there is an image: the official `postgres:18` image, untouched
(entrypoint, environment variables, volume, signals), plus
`pg_automerge.so`, its control file and SQL scripts, and one init script.
The extension is compiled in a builder stage from the same base image,
against that image's own PGDG Postgres 18 (release build, no assertions),
with Rust and cargo-pgrx as pinned in `mise.toml`. Nothing is pushed
anywhere; the image lives in your local Docker until you push it to a
registry of your own (CI can do that for tags once you
[configure it](#publishing-the-image)).

```sh
mise run docker-build        # pg-automerge:0.1.0 and pg-automerge:dev
mise run docker-test         # builds, then tests the image and compose.yaml (about 4 minutes once built)
```

The image is 3.5 MB larger than `postgres:18`. A first build takes about
8 minutes (Rust, cargo-pgrx and a fat-LTO release build); BuildKit cache
mounts keep the toolchain and compiled dependencies, so a rebuild after a
source change takes about a minute ([measurements](docs/DESIGN.md#docker-image)).

Without mise (from the repository root, BuildKit required; the build
checks the version argument against `Cargo.toml`):

```sh
docker build -f docker/Dockerfile \
  --build-arg PG_AUTOMERGE_VERSION="$(sh scripts/versions.sh | sed -n 's/^CRATE_VERSION=//p')" \
  -t pg-automerge:dev .
```

Run it like `postgres:18`. Mount the volume at `/var/lib/postgresql` (the
18 images keep the data in `/var/lib/postgresql/18/docker` below it):

```sh
docker run -d --name pg -p 127.0.0.1:5432:5432 \
  -e POSTGRES_PASSWORD=secret -e POSTGRES_DB=app \
  -v pgdata:/var/lib/postgresql \
  pg-automerge:0.1.0 \
  postgres -c default_toast_compression=lz4 -c pg_automerge.max_load_memory=1GB
```

For local development, `compose.yaml` does the same with a named volume,
a `pg_isready` healthcheck, the port on `127.0.0.1` only and the suggested
settings below:

```sh
mise run docker-up           # build if needed, start, wait until healthy
psql postgres://postgres:postgres@localhost:5432/app
mise run docker-down         # stop; `docker compose down -v` also deletes the data
```

`PG_AUTOMERGE_PORT`, `POSTGRES_PASSWORD` and `POSTGRES_DB` override its
defaults (5432, `postgres`, `app`).

### Configuration

| Variable | Default | Effect |
|---|---|---|
| `PG_AUTOMERGE_CREATE_EXTENSION` | `1` | `1`: `CREATE EXTENSION IF NOT EXISTS pg_automerge` in `POSTGRES_DB` when the data directory is first initialized. `0`: skip it (install it from your migrations, e.g. `CREATE EXTENSION pg_automerge SCHEMA automerge`, as a superuser). Anything else fails initialization. |

The official image's variables (`POSTGRES_PASSWORD`, `POSTGRES_USER`,
`POSTGRES_DB`, `POSTGRES_INITDB_ARGS`, ...) work as documented for
`postgres`, and your own files in `/docker-entrypoint-initdb.d` run after
ours (`10-pg-automerge.sh`) when their names sort after it. Like all init
scripts, it runs only on an empty data directory, never on an existing
volume.

Add init files one by one, as single-file mounts or `COPY` in an image
of your own; do not mount a whole directory over
`/docker-entrypoint-initdb.d`:

```sh
docker run ... -v ./init/20-app.sql:/docker-entrypoint-initdb.d/20-app.sql:ro pg-automerge:0.1.0
```

A directory mounted there replaces `10-pg-automerge.sh`, and the database
then silently comes up without the extension. If you do mount a
directory, put an executable script in it that runs the image's copy of
ours, `/usr/local/bin/pg-automerge-initdb` (it reads
`PG_AUTOMERGE_CREATE_EXTENSION` too):

```sh
printf '#!/bin/sh\nexec pg-automerge-initdb\n' > init/10-pg-automerge.sh
chmod 755 init/10-pg-automerge.sh
```

The extension goes into `POSTGRES_DB` only, not `template1`: it is not
trusted (see [Install](#install)), and a copy in `template1` would put it
into every database a `CREATEDB` role creates later. Other databases get
it with `CREATE EXTENSION pg_automerge` by a superuser. Your application
should connect as its own non-superuser role, not as `POSTGRES_USER`.

The image changes no Postgres setting. Suggested, as command-line options
(as above) or in a mounted `postgresql.conf`:

- `default_toast_compression=lz4`: stored documents are uncompressed
  Automerge saves, which TOAST compresses; the image's Postgres is built
  with lz4 (`pg_config --configure` shows `--with-lz4`), which compresses
  and decompresses much faster than the default `pglz`. It applies to
  columns created afterwards; `ALTER TABLE .. ALTER COLUMN doc SET
  COMPRESSION lz4` sets it on an existing one (for new values).
- `pg_automerge.max_load_memory`: size it to the container. A backend
  whose Rust allocation fails aborts, and Postgres then restarts every
  session of the cluster; with a container memory limit (`--memory`), the
  kernel's OOM killer does the same. Each session can use up to the limit
  at once, like `work_mem`, so keep it well below the container's memory
  divided by the sessions you expect to load large documents at once (the
  default is 2GB; compose uses 1GB). See [Configuration](#configuration).

### Updating

**A new pg_automerge version** (a newer image on the same volume):
recreate the container from the new image, then in every database that
has the extension:

```sql
ALTER EXTENSION pg_automerge UPDATE;   -- as a superuser
SELECT extversion FROM pg_extension WHERE extname = 'pg_automerge';
```

The init script does not run on an existing volume, so this step is
yours (a migration, say). Until you run it the new library serves the old
SQL definitions, which it keeps supporting (see
[DESIGN.md](docs/DESIGN.md#versioning-and-upgrades)).

**A new Postgres 18 minor release or Debian security fixes**: the base
image is pinned by digest in `docker/Dockerfile` (`ARG PG_IMAGE`, with how
to find the current digest next to it). Update the digest, rebuild,
recreate the container on the same volume. Nothing else to do: minor
releases keep the data format and the extension ABI. Check for new
digests regularly; a pinned image does not get security fixes by itself.

**A new Postgres major version** (19, ...): pg_automerge supports only 18
so far, and an image of it for 19 does not exist yet. When it does, the
18 volume cannot simply be mounted into it; either dump and restore (the
`automerge` text form is the dump format and is stable across versions;
restore with `pg_automerge.max_load_memory=-1` as the
[Configuration](#configuration) section describes), or run `pg_upgrade`
with both majors' binaries and pg_automerge built for both (the
`/var/lib/postgresql` mount lets `pg_upgrade --link` work within one
volume).

### Testing the image

`mise run docker-test` runs everything against throwaway containers of the
image (labelled `pg-automerge-test`, removed afterwards with their
volumes): the init script and its variable, SQL through the container's
own `psql` (casts, merges, jsonb operators with a GIN expression index
and a generated column, the history functions, a real `LISTEN` session
for `automerge_notify()`, the settings and their privileges),
`pg_dump -Fc` in one container restored into a second, a restart on the
same volume, the regress examples and the concurrency, notify, dump and
extension suites, the load memory limit against a container started with
`--memory=1g`, and that no server log shows an assertion failure, panic
or crashed backend. The suites need Postgres 18 client tools and cargo:
those of the pgrx Postgres by default (`mise run pgrx-init`), or PGDG's
`postgresql-client-18` with
`PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config` (what CI uses);
`DOCKER_TEST_SUITES=0` skips them. It also checks the release scripts:
`scripts/docker-archive.sh` and a `--dry-run` of `scripts/docker-push.sh`.

The multi-session suites run against any server of yours, too (it needs
the extension available, a superuser, and room for scratch databases):

```sh
PG_AUTOMERGE_TEST_HOST=127.0.0.1 PG_AUTOMERGE_TEST_PORT=5432 \
PG_AUTOMERGE_TEST_USER=postgres PG_AUTOMERGE_TEST_PASSWORD=... \
  bash tests/concurrency.sh   # or notify.sh, dump.sh, extension.sh, bench_sql.sh
```

`mise run docker-bench-sql` times the everyday SQL paths against the
image; see [Performance](#performance) for how it compares.

### CI

`.github/workflows/ci.yml` has a `docker` job next to the `ci` job, on
every push and pull request: it builds the image with BuildKit, caching
its layers in the GitHub Actions cache (so the Rust toolchain and
cargo-pgrx are built once, not on every run), and runs `tests/docker.sh`
in full against it, with PGDG's client tools. It does not need pgrx.

On a `v*` tag (which must equal the `Cargo.toml` version) the job runs
twice, on an amd64 and on GitHub's native arm64 runner
(`ubuntu-24.04-arm`), each building and testing its own image, and
uploads each as a workflow artifact
(`pg-automerge-<version>-linux-<arch>.tar.gz`, under Actions > the run >
Artifacts). To use one:

```sh
docker load -i pg-automerge-0.1.0-linux-arm64.tar.gz   # loads pg-automerge:0.1.0
```

`docker load` moves your local `pg-automerge:0.1.0` tag to the loaded
image, so loading the other architecture's archive leaves that tag on an
image your host cannot run (`exec format error`) until you
`mise run docker-build` again. `scripts/docker-push.sh` (below) loads
archives too, but puts the tags back as they were.

The arm64 image is built and tested natively, without QEMU or
cross-compilation ([why](docs/DESIGN.md#docker-image-in-ci)). Where the
arm64 runner is not available (it depends on the repository's plan and
visibility), set the repository variable `PG_AUTOMERGE_ARM64` to `false`:
tags then build amd64 only.

### Publishing the image

The workflow's `publish` job pushes a tag's tested images to a registry of
yours as one multi-architecture image, `<image>:<version>` and
`<image>:latest` (plus the per-architecture `<image>:<version>-amd64` and
`-arm64` it is made of). It is prepared but off: without the secret
below it only logs a notice. To turn it on, in the repository's Settings >
Secrets and variables > Actions:

| Kind | Name | Value |
|---|---|---|
| Variable | `PG_AUTOMERGE_REGISTRY_IMAGE` | the image, registry host first, lower case: `ghcr.io/you/pg-automerge`, `docker.io/you/pg-automerge`, ... |
| Variable | `PG_AUTOMERGE_REGISTRY_USERNAME` | the registry user |
| Secret | `PG_AUTOMERGE_REGISTRY_TOKEN` | a token that may push to that image only (for ghcr.io a personal access token with `write:packages`; for Docker Hub an access token with Read & Write) |

It runs only for tags, only after both the `ci` and the `docker` jobs
passed, and pushes exactly the archives those jobs tested
(`scripts/docker-push.sh`, which checks each one's version and
architecture first). Nothing else in the workflow logs in to a registry or
pushes, and its token is read-only (`contents: read`); `tests/check_ci.sh`
fails if that changes. To require an approval for every push, move the
secret into a GitHub environment with required reviewers and add
`environment: <name>` to the job. By hand, the same script pushes archives
you built yourself:

```sh
bash scripts/docker-archive.sh                     # pg-automerge-0.1.0-linux-amd64.tar.gz
docker login ghcr.io
bash scripts/docker-push.sh --dry-run ghcr.io/you/pg-automerge pg-automerge-0.1.0-linux-*.tar.gz  # prints the image IDs and the pushes
bash scripts/docker-push.sh ghcr.io/you/pg-automerge pg-automerge-0.1.0-linux-*.tar.gz
```

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
| `merge(a, b)`, `a \|\| b` | I S P | CRDT merge. Commutative and idempotent in state (heads and jsonb), not byte for byte; returns an input unchanged if it already contains the other, otherwise an in-memory (expanded) result. Two histories with different changes under one actor id cannot be merged (22000, see [Limitations](#limitations-and-gotchas)); a result over `pg_automerge.max_load_memory` is refused (53400). |
| `merge(doc, changes bytea)`, `doc \|\| changes` | I S P | Apply a save or bare change chunks (`save_incremental()` / `save_after()` output, may be concatenated) on top of `doc`. Returns `doc` unchanged if nothing is new; rejects changes with missing dependencies (22P02, naming them in the DETAIL). |
| `merge_agg(automerge)` | immutable, parallel safe (no combine function) | Aggregate merge of all non-null inputs. Loads a document only to merge it: a single row, or a version plus older ones, costs no load or one. A merged state over `pg_automerge.max_load_memory` is refused (53400). |
| `automerge_heads(automerge) → text[]` | I S P | Current heads, sorted hex change hashes. Read from the stored header, without loading the document. |
| `automerge_contains(a, b) → bool` | I S P | Whether `a` already has every change of `b`. Decided without loading when the heads or the change counts can tell (`b` newer than or concurrent with `a`); otherwise loads `a`. |
| `automerge_contains(doc, changes bytea) → bool` | I S P | Whether `merge(doc, changes)` would add nothing (every change in the save or change chunks is already in `doc`). Usually decided without loading the document; when it loads, changes that reuse an actor id of `doc` with different content fail as in `merge` (22000). |
| `automerge_notify('channel', 'key_col' [, ...])` | volatile, parallel unsafe | `AFTER INSERT OR UPDATE OR DELETE FOR EACH ROW` trigger: `NOTIFY channel` with the row key and the new/previous heads of `automerge` columns whose heads changed. |
| `automerge_changes(doc, since_heads text[] DEFAULT '{}')` | I S P | `SETOF automerge_change (hash, actor, seq, start_op, op_count, time, message, deps, change bytea)`: every change not reachable from `since_heads` (all by default), dependencies first. Rebuilds change bytes (costly on big documents). |
| `automerge_changes_meta(doc, since_heads DEFAULT '{}')` | I S P | The same rows without `change` (`SETOF automerge_change_meta`); needs only the change graph. |
| `automerge_changes_bytes(doc, since_heads DEFAULT '{}') → bytea` | I S P | Those changes as concatenated change chunks (`save_after(since_heads)`); `merge(replica, ...)` or `loadIncremental` applies them. |
| `automerge_get_change(doc, hash) → automerge_change` | I S P | One change with its bytes; NULL if absent. |
| `automerge_change_count(doc) → bigint` | I S P | Number of changes, read from the stored bytes without loading the document. |
| `automerge_to_jsonb(doc, heads text[]) → jsonb` | I S P | The state as of `heads` (`'{}'`: before any change). |
| `automerge_spans(doc, path text[] [, heads text[]]) → jsonb` | I S P | The structure of the text object at `path` (as for `#>`: `'{notes,0,body}'`): an array of `{"type":"text","value":…,"marks":{…}}` runs and `{"type":"block","value":{…}}` blocks, the shape of Automerge's JavaScript `spans()`; as of `heads` with the third argument. NULL if nothing is at `path`; 22023 if it is not a text object. See [Rich text](#rich-text). |

Every object has a `COMMENT` (`\df+`, `\dT+`). Error codes are listed in
[DESIGN.md](docs/DESIGN.md#error-codes).

## jsonb mapping

Maps/tables → objects, lists → arrays, text → strings, integers and
counters → exact numbers, NaN/±Infinity → `null`, timestamps → ISO 8601
UTC strings with milliseconds, bytes → base64 strings. Conflicting
concurrent values show Automerge's winner. Details in
[DESIGN.md](docs/DESIGN.md#jsonb-mapping).

## Rich text

The jsonb view shows a text object as a plain string: marks (bold, links,
...) are not in it, and each block marker (a paragraph, heading or list
item inserted with `splitBlock`) is a U+FFFC character. `automerge_spans`
returns the structure, in the shape of `Automerge.spans(doc, path)` in
JavaScript, so a frontend can render it or a backend index it:

```sql
SELECT automerge_spans(doc, '{body}') FROM notes WHERE id = $1;
-- [{"type": "block", "value": {"type": "heading", "parents": [], "attrs": {"level": 1}}},
--  {"type": "text", "value": "Shopping tips"},
--  {"type": "block", "value": {"type": "paragraph", "parents": [], "attrs": {}}},
--  {"type": "text", "value": "Buy "},
--  {"type": "text", "value": "fresh milk", "marks": {"bold": true}},
--  {"type": "text", "value": "."}]

-- One row per span; every link in a document; the text as of earlier heads.
SELECT s->>'type', s->>'value', s->'marks' FROM jsonb_array_elements(automerge_spans(doc, '{body}')) s;
SELECT s->'marks'->>'link' FROM notes, jsonb_array_elements(automerge_spans(doc, '{body}')) s
WHERE s->'marks' ? 'link';
SELECT automerge_spans(doc, '{body}', $2) FROM notes WHERE id = $1;  -- $2: heads (text[])
```

The path works like `#>` (map keys, list indices, NULL when nothing is
there); a path to anything but a text object is an error (22023). Block
values are the maps the writer stored (the `type`/`parents`/`attrs`
fields above are the convention of editors such as automerge-prosemirror).
Mark values and block contents use the jsonb mapping above (timestamps as
ISO strings, bytes as base64, exact integers), where JavaScript gives
`Date`, `Uint8Array` and numbers. There are no positions in the result;
a block counts as one character in the text's indices. Blocks nested more
than 32 levels deep are refused (54000). Details in
[DESIGN.md](docs/DESIGN.md#rich-text-spans).

## Configuration

| Setting | Default | Who can change it | Effect |
|---|---|---|---|
| `pg_automerge.max_load_memory` | `2GB` | superusers, or roles granted `SET` on it; also `ALTER ROLE`/`ALTER DATABASE .. SET` by a superuser | Refuse client input and merge results whose estimated load exceeds this (kB, like `work_mem`; `-1`: no limit) |
| `pg_automerge.verify_writes` | `on` | superusers, or roles granted `SET` on it (`GRANT SET ON PARAMETER pg_automerge.verify_writes TO writer`); also `ALTER ROLE`/`ALTER DATABASE .. SET` by a superuser | Load back the normalized save of every value built from client bytes (text input, binary receive, the `bytea` cast, `merge(automerge, bytea)` results) before it is stored or sent |

`pg_automerge.max_load_memory` protects the server from input that
describes far more than it holds (see
[Limitations](#limitations-and-gotchas)). Every value built from client
bytes (text input, `COPY`, binary parameters, the `bytea` cast,
`merge(doc, $bytes)`, `automerge_contains(doc, $bytes)`) is priced from
its headers before it is loaded, and so is every merge result (`merge`,
`||`, `merge_agg`, PL/pgSQL chains); over the limit it fails with
SQLSTATE `53400`:

```text
ERROR:  estimated memory to load automerge input exceeds "pg_automerge.max_load_memory" (2048 MB)
DETAIL:  Loading it could take up to 9537 MB (10000001 operations, 1 change, 1 actor, 123 bytes uncompressed).
HINT:  A superuser can raise "pg_automerge.max_load_memory".
```

The estimate is deliberately pessimistic (plain text is priced at about
five times its real cost): the 2 GB default admits about 4.4 million
characters of text, whose load really takes about 0.4 GB and up to 16 s.
Each session can use up to the limit at once, so size it like
`work_mem`. Reading stored values is never limited, so lowering the limit
never makes data unreadable, and writes that add nothing (re-sent saves or
changes) are not refused. But merging two documents that each fit can
fail when the result would not, and restoring a dump into a server with a
lower limit refuses the larger rows: restore with
`PGOPTIONS='-c pg_automerge.max_load_memory=-1' pg_restore ...` as a
superuser. On a logical replication subscriber, set it for the
subscription's owner at least as high as on the publisher. Details in
[DESIGN.md](docs/DESIGN.md#resource-limits).

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
  postmaster restarts every session. `pg_automerge.max_load_memory`
  refuses such input before it is loaded (see
  [Configuration](#configuration)); what it cannot do is bound several
  sessions together, or catch a load that costs more than its measured
  model predicts (an Automerge upgrade re-measures it). Keep memory
  overcommit in mind, and `-1` only for trusted writers.
- **Bundle chunks** (Automerge's experimental format that packs many
  changes into one chunk) are refused as input (`0A000`).
- **A load is not interruptible.** The jsonb conversion and the history
  functions check for interrupts as they go (and `merge_agg` between
  rows), but a single Automerge load, merge or save runs to the end
  before a cancel or `statement_timeout` takes effect: 2.5 s for a 3 MB
  document, longer for documents of tens of MB (at most about 16 s for
  input within the default `pg_automerge.max_load_memory`).
- **Every writer needs its own actor id.** Two writers (or two copies of
  a document) that commit with the same actor id produce different changes
  with the same sequence number, and Automerge cannot merge them. Every
  path that meets both histories fails with SQLSTATE `22000` (data_exception,
  not `22P02`; `conflicting automerge changes: actor ... has two different
  changes with seq N`, with a HINT): `merge` / `||` of two documents,
  `merge_agg`, `merge(doc, changes)` / `doc || changes`,
  `automerge_contains(doc, changes)` when it has to load the document, and
  text input or the `bytea → automerge` cast of bytes that hold both
  histories (a save followed by the other writer's change chunk).
  Retrying does not help; the fix is in the writer. Automerge picks a
  random actor id per document instance unless you set one.
- Malformed input is SQLSTATE `22P02` (`invalid automerge document`),
  including input that passes Automerge's checksums but panics its decoder;
  input that is well formed but holds two conflicting histories of one
  actor is `22000` (above), input (or a merge result) over
  `pg_automerge.max_load_memory` is `53400`.
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
    MB document), whichever mode, within the subscriber's
    `pg_automerge.max_load_memory` (set it for the subscription's owner
    at least as high as on the publisher, or the apply worker fails on a
    larger row and retries).
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
  validate every value (one load each, within
  `pg_automerge.max_load_memory`: raise it for a restore into a server
  with a lower limit) and recompute stored generated columns (one more
  conversion each).

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

These were measured on pgrx's development Postgres (assertions on). On
the [Docker image](#docker) (PGDG Postgres 18, `mise run
docker-bench-sql`) the paths that load a document take the same time
(within a few percent: the time is Automerge's), and the millisecond
paths (heads, no-op merges, containment checks) about half
([comparison](docs/DESIGN.md#the-docker-image-against-the-pgrx-postgres-2026-09-30)).

## Development

Tooling runs through [mise](https://mise.jdx.dev):

```sh
mise run pgrx-init   # once: build the Postgres pgrx develops against
mise run test        # core tests + #[pg_test] tests + the concurrency, notify, dump, upgrade, extension and limits scripts
mise run regress     # pg_regress examples in tests/pg_regress (checks their fixtures first)
mise run lint        # CI/packaging checks (actionlint, shellcheck, workflow invariants), rustfmt, clippy -D warnings (all build configurations), rustdoc
mise run ci          # lint + test + regress: what CI's ci job runs (its docker job: docker-test)
mise run concurrency # only: two real psql sessions merging into one row
mise run notify      # only: a real LISTEN session receiving automerge_notify() payloads
mise run dump        # only: pg_dump/pg_restore and COPY round trips of every object kind
mise run upgrade     # only: ALTER EXTENSION UPDATE from every released version
mise run extension   # only: relocation (SCHEMA, SET SCHEMA, dump), install and setting privileges
mise run limits      # only: the load memory limit against real crashes in a memory-capped scratch cluster
mise run replication # logical replication in a scratch cluster (not part of test)
mise run fuzz        # a long mutation-fuzzing session of the core (not part of test)
mise run bench-sql   # median timings of the everyday SQL paths on a release build (minutes)
mise run bench-expanded  # SQL timings of merge chains and PL/pgSQL loops on a release build (minutes)
mise run bench-core  # Rust timings of load, normalize and the jsonb walk
mise run package     # release package for the Postgres of $PG_CONFIG (required)
mise run docker-build   # the Docker image (see Docker)
mise run docker-test    # build it, then test it against containers: SQL, dump/restore, suites, memory limit, compose.yaml (not part of test)
mise run docker-bench-sql  # bench-sql against the image (release build on PGDG Postgres, no assertions)
mise run docker-up      # compose.yaml's development Postgres; docker-down stops it
# scripts/docker-archive.sh and scripts/docker-push.sh: release archives and pushing (see Publishing the image)
mise run run         # install and open psql against the pgrx-managed Postgres
```

`crates/pg_automerge_core` holds all Automerge logic as plain Rust; `src/`
is the pgrx glue. See [DESIGN.md](docs/DESIGN.md#architecture).

## License

MIT — see [LICENSE](LICENSE).
