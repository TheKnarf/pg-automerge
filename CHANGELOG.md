# Changelog

Versions are the extension's (`ALTER EXTENSION pg_automerge UPDATE`
takes an installed one to the library's version; see the README's
[Updating](README.md#updating) and
[docs/DESIGN.md](docs/DESIGN.md#versioning-and-upgrades)). Each entry
says whether an update needs more than recreating the container (or
installing the package) and `ALTER EXTENSION pg_automerge UPDATE`.

## Unreleased

Upgrade: none beyond installing the new library (or recreating the
container); no SQL object changes, so no `ALTER EXTENSION .. UPDATE`. The
fixes take effect as soon as a backend loads the new library.

Fixed:

- **Server crash after altering the change types.** If the owner of
  `automerge_change` or `automerge_change_meta` (the superuser who ran
  `CREATE EXTENSION`) changed an attribute's type
  (`ALTER TYPE automerge_change ALTER ATTRIBUTE seq TYPE text`),
  `automerge_changes`, `automerge_changes_meta` and `automerge_get_change`
  crashed the backend (SIGSEGV), and Postgres restarted every session.
  They now fail with `XX000` (`type automerge_change has been altered:
  ...`) for any altered, dropped or added attribute.
- **Misspelled settings were accepted silently.** The library now
  reserves the `pg_automerge` prefix: `SET pg_automerge.max_load_memroy`
  is an error (`42602`) once the library is loaded, and such a setting
  from `postgresql.conf`, `ALTER SYSTEM` or `ALTER ROLE/DATABASE .. SET`
  is removed with a WARNING when it loads, instead of leaving the real
  setting at its default without a word.
- `automerge_notify()` refuses a virtual generated column (PG18) as a key
  column (`22023`, with a HINT). A trigger sees NULL in such a column,
  so the key used to be reported as `null`. STORED generated columns work
  as before.

Changed:

- A flat (stored) `automerge` argument is detoasted once and read in
  place, and the detoasted copy is freed as soon as the function is done
  with it. It used to be copied again into Rust memory, with the first
  copy kept until the end of the call, so a large document held about
  twice its size in raw bytes while it was loaded or read (a 1 MB value:
  1,016,576 bytes left allocated in the call's memory context, now none).

## 0.2.0 (2026-09-30)

Upgrade: `ALTER EXTENSION pg_automerge UPDATE` from 0.1.0
(`sql/pg_automerge--0.1.0--0.2.0.sql`). No `REINDEX`, no rewrite of
generated columns: every function returns what it returned in 0.1.0, the
jsonb view of every document included. Automerge is still 0.12.0 and the
stored format is unchanged.

Fixed:

- **Server crash on deeply nested blocks.** Reading a document whose rich
  text has a block nested a few hundred levels deep (about 700 with an
  8 MB stack; a few kB of document) as jsonb (the
  `doc::jsonb` cast, a STORED generated column or expression index on it,
  `automerge_to_jsonb`, a jsonb operator on the column) overflowed the
  stack inside Automerge; the backend died with SIGSEGV and Postgres
  restarted every session. The jsonb view never makes Automerge render
  blocks now (a text shows each block as U+FFFC, as before), whatever
  their depth. The fix is in the library: it takes effect as soon as the
  new library is loaded, before the `ALTER EXTENSION .. UPDATE`.

Added:

- `automerge_spans(doc automerge, path text[]) RETURNS jsonb` and
  `automerge_spans(doc, path, heads text[])`: a text object's rich-text
  structure (text runs with their marks, and blocks), as Automerge's
  JavaScript `spans()` returns it; NULL if nothing is at the path, 22023
  if it is not a text object, 54000 for blocks nested more than 32 levels
  deep.

Changed:

- Reading documents with blocks as jsonb is about three times faster
  (their blocks are no longer rendered only to be dropped).

Note on 0.1.0 images: Docker images built from this repository between
the addition of `automerge_spans` and this release were still labelled
0.1.0 and created `automerge_spans` at `CREATE EXTENSION` time. Whether
they carry the crash above depends on the tree they were built from:
those built from a commit up to 555110e do, while one built from a
working tree that already had the uncommitted fix does not (for example
image 7c6d11f23a6f, labelled revision `555110e-dirty`). The
`org.opencontainers.image.revision` label alone does not tell; reading
the deep-block document of `tests/pg_regress/sql/automerge.sql` as jsonb
in a throwaway container does. Their catalog is the same either way, and
their databases update to 0.2.0 the same way (the upgrade script
replaces the two functions in place with identical definitions), as does
a database of the released 0.1.0 image, which has the crash.

## 0.1.0 (2026-09-30)

First release, PostgreSQL 18 only, Automerge 0.12.0; as a source build
and as a Docker image (`postgres:18` plus the extension).

- The `automerge` type: full Automerge saves, validated and normalized on
  input; text form `\x` + hex (the dump format), bytea casts both ways,
  and an implicit cast to `jsonb`, so every jsonb operator, function and
  index (GIN, B-tree expressions, STORED generated columns) works on it.
- Merging: `merge(doc, changes)` and `||` for documents and bare changes,
  the `merge_agg` aggregate; concurrency-safe `UPDATE .. SET doc =
  merge(doc, $1)`; expanded in-memory values for merges in PL/pgSQL.
- Reading: the jsonb mapping, `automerge_to_jsonb(doc, heads)` as of
  earlier heads, `automerge_heads`, `automerge_contains`.
- History (read-only): `automerge_changes`, `automerge_changes_meta`,
  `automerge_changes_bytes`, `automerge_get_change`,
  `automerge_change_count`.
- Change notifications: the `automerge_notify()` trigger (NOTIFY with the
  row key and the heads of changed columns).
- Settings: `pg_automerge.verify_writes`, `pg_automerge.max_load_memory`
  (a load memory limit priced before Automerge allocates); both
  superuser-only. The extension is relocatable and not trusted.
