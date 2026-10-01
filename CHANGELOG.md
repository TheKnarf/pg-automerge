# Changelog

Versions are the extension's (`ALTER EXTENSION pg_automerge UPDATE`
takes an installed one to the library's version; see
[Updating](docs/src/pages/guide/updating.mdx) and
[Versioning and upgrades](docs/src/pages/design/versioning.mdx) in the
documentation). Each entry
says whether an update needs more than recreating the container (or
installing the package) and `ALTER EXTENSION pg_automerge UPDATE`.

## 0.3.0 (unreleased)

Upgrade: `ALTER EXTENSION pg_automerge UPDATE` from 0.2.0
(`sql/pg_automerge--0.2.0--0.3.0.sql`; from 0.1.0 it runs both scripts in
one step). It only adds the two functions below (the new setting needs
no SQL): no `REINDEX`, no rewrite
of generated columns, every function returns what it returned in 0.2.0,
and the stored format is unchanged. The fixes take effect as soon as a
backend loads the new library, before the `UPDATE`. With compose, set a
`build:` section's `PG_AUTOMERGE_VERSION` arg to 0.3.0 (see
[Updating](docs/src/pages/guide/updating.mdx)).

Added:

- **`automerge_memory_usage()`**: the memory pg_automerge holds in the
  current backend outside Postgres' memory contexts, which
  `pg_backend_memory_contexts` and `work_mem` do not see: bytes held now
  and at peak (exact, from a counting allocator), loaded documents alive,
  and the number and total time of Automerge loads. And
  `automerge_memory_reset()`, which restarts the peak and the load
  counters, to measure one statement. Per backend; see
  [Monitoring](docs/src/pages/operations/monitoring.mdx). The allocator costs a few ns per
  allocation (`mise run bench-alloc`, new: +6.5 ns per allocation and
  free on an Atom C3758R): not measurable in `mise run bench-sql`
  (reads, writes, merges, `merge_agg`), and at most about 4% in the
  Rust-level benchmarks (the load of a 3 MB text, which allocates
  hundreds of MB).
- **`pg_automerge.trim_threshold`** (default `64MB`, superuser-only like
  the other settings): at the end of a transaction in which
  pg_automerge's allocation fell by at least this much, return the memory
  `malloc` kept for reuse to the operating system (glibc's
  `malloc_trim`). Found by the new soak test: every long-lived connection
  used to keep the most it had ever needed (200 to 355 MB per connection
  for documents of up to 1 MB, 2.4 GB of a 4 GB container for eight
  connections, busy or idle); with the trim the container's median was
  1.3 to 1.7 GB. It costs the next load of a document that big about
  15% (the memory is faulted in again), and nothing for documents whose
  loads stay below the threshold; `-1` restores the old behaviour. `0`
  trims after every transaction in which anything was freed, and never
  after one that freed nothing (so other applications' transactions in
  the same backend do not pay about 50 µs each for it). See
  [Configuration](docs/src/pages/reference/configuration.mdx).
- **Soak test** (`mise run soak`, `tests/soak.sh`): an hour of
  concurrent writes, reads, history, `merge_agg`, notifications and
  VACUUM against the Docker image in a memory-capped container, sampled
  and checked; its findings are in the new
  [Operations](docs/src/pages/operations/index.mdx) section (memory sizing, WAL and
  TOAST, and GIN indexes on documents: with Postgres' default
  `fastupdate = on` the planner stops using them while writes keep the
  pending list full, so create them with `fastupdate = off`, which costs
  more WAL). A 60-second run is part of `mise run docker-test`.

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

- **Load memory estimate: repeated mark names.** A document chunk whose
  ops share one mark name through a run of its column (2,000 changes
  with one 100 kB name) peaked at 0.996 of its estimate: below it, but
  without the margin of every other measured input (0.80 at most). Mark
  names are now priced like change messages (5 bytes per byte each
  rebuilt change copies, not 3 like keys): 0.60. Documents that do not
  repeat a mark name by a run are priced exactly as before; one that
  does, near `pg_automerge.max_load_memory`, can now be refused
  (`53400`) where it was accepted.

Known issue:

- Changes with many overlapping rich-text marks take time quadratic in
  their number to apply (Automerge's mark bookkeeping), so a small
  crafted change can keep a backend busy and uncancellable for minutes
  while staying far below `pg_automerge.max_load_memory` (32,000 marks
  in 122 bytes: 3 s; a million: about 50 minutes, extrapolated). Saves
  are not affected. Not priced yet: the cheap bound would refuse
  ordinary rich text (see [What the limit cannot
  do](docs/src/pages/design/resource-limits.mdx#what-the-limit-cannot-do)).

Changed:

- `mise run docker-upgrade-test` also rehearses the upgrade from a 0.2.0
  deployment (`PG_AUTOMERGE_OLD_VERSION=0.2.0`, with
  `PG_AUTOMERGE_OLD_IMAGE` or an image built from the 0.2.0 release),
  the path of an app already on 0.2.0.
- The core tests measure the load memory estimate on the shapes next to
  the ones it was fitted to (every string-valued column as a run,
  different strings and runs between literals; actor ids of 17 bytes to
  100 kB from every actor column; documents written by Automerge), 592
  more measurements, worst 0.80 of the estimate; and a test fails when
  the lock file has another Automerge (or hexane, its column store) than
  the estimate was measured with, so an upgrade re-measures it first.
- A flat (stored) `automerge` argument is detoasted once and read in
  place, and the detoasted copy is freed as soon as the function is done
  with it. It used to be copied again into Rust memory, with the first
  copy kept until the end of the call, so a large document held about
  twice its size in raw bytes while it was loaded or read (a 1 MB value:
  1,016,576 bytes left allocated in the call's memory context, now none).
- The documentation is a static site (`docs/`: Vite, React, MDX,
  prerendered for GitHub Pages; `mise run docs-dev`). README.md and
  the design document were split into its pages, under `docs/src/pages`
  (Guide, Reference, Operations, Design); README.md is now a short
  landing page, and this changelog is the site's Changelog page.

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
