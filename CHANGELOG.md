# Changelog

Versions are the extension's (`ALTER EXTENSION pg_automerge UPDATE`
takes an installed one to the library's version; see
[Updating](docs/src/pages/guide/updating.mdx) and
[Versioning and upgrades](docs/src/pages/design/versioning.mdx) in the
documentation). Each entry
says whether an update needs more than recreating the container (or
installing the package) and `ALTER EXTENSION pg_automerge UPDATE`.

## 0.1.0 (2026-10-07)

First release: PostgreSQL 18 only, Automerge 0.12.0 (pinned). As a source
build (`cargo pgrx install`, or a package with `mise run package`) and as
two Docker images. Later versions update from it with `ALTER EXTENSION
pg_automerge UPDATE` (see [Updating](docs/src/pages/guide/updating.mdx)).

The SQL surface:

- The `automerge` type: full Automerge saves, validated and normalized on
  input; text form `\x` + hex (the dump format), bytea casts both ways,
  and an implicit cast to `jsonb`, so every jsonb operator, function and
  index (GIN, B-tree expressions, STORED generated columns) works on it.
- Merging: `merge(doc, changes)` and `||` for documents and bare changes,
  the `merge_agg` aggregate; concurrency-safe `UPDATE .. SET doc =
  merge(doc, $1)`; expanded in-memory values for merges in PL/pgSQL.
- Reading: the jsonb mapping, `automerge_to_jsonb(doc, heads)` as of
  earlier heads, `automerge_heads`, `automerge_contains`.
- Rich text: `automerge_spans(doc automerge, path text[]) RETURNS jsonb`
  and `automerge_spans(doc, path, heads text[])`: a text object's
  structure (text runs with their marks, and blocks), as Automerge's
  JavaScript `spans()` returns it; NULL if nothing is at the path, 22023
  if it is not a text object, 54000 for blocks nested more than 32 levels
  deep. The jsonb view never makes Automerge render blocks (a text shows
  each block as U+FFFC), so a document with blocks nested hundreds of
  levels deep reads as jsonb like any other, instead of overflowing the
  stack inside Automerge (see
  [Deep blocks](docs/src/pages/design/deep-blocks.mdx)).
- History (read-only): `automerge_changes`, `automerge_changes_meta`,
  `automerge_changes_bytes`, `automerge_get_change`,
  `automerge_change_count`. If the owner of `automerge_change` or
  `automerge_change_meta` alters, drops or adds an attribute, these fail
  with `XX000` (`type automerge_change has been altered: ...`).
- Change notifications: the `automerge_notify()` trigger (NOTIFY with the
  row key and the heads of changed columns). A virtual generated column
  (PG18) is refused as a key column (`22023`, with a HINT): a trigger
  sees NULL in it. STORED generated columns work.
- Memory observability: `automerge_memory_usage()`, the memory
  pg_automerge holds in the current backend outside Postgres' memory
  contexts (which `pg_backend_memory_contexts` and `work_mem` do not
  see): bytes held now and at peak (exact, from a counting allocator),
  loaded documents alive, and the number and total time of Automerge
  loads; `automerge_memory_reset()` restarts the peak and the load
  counters, to measure one statement. Per backend; see
  [Monitoring](docs/src/pages/operations/monitoring.mdx). The allocator
  costs a few ns per allocation (`mise run bench-alloc`: +6.5 ns per
  allocation and free on an Atom C3758R), not measurable in
  `mise run bench-sql`.
- Settings, all superuser-only (see
  [Configuration](docs/src/pages/reference/configuration.mdx)):
  `pg_automerge.verify_writes`; `pg_automerge.max_load_memory`, a load
  memory limit: every load is priced before Automerge allocates and
  refused with `53400` above it (measured inputs peak at 0.80 of the
  estimate at most); `pg_automerge.trim_threshold` (default `64MB`): at
  the end of a transaction in which pg_automerge's allocation fell by at
  least this much, the memory `malloc` kept is returned to the operating
  system (`malloc_trim`), so a long-lived connection does not keep the
  most it ever needed (`-1` turns it off). The library reserves the
  `pg_automerge` prefix: a misspelled setting is an error (`42602`) once
  it is loaded, and one from `postgresql.conf`, `ALTER SYSTEM` or
  `ALTER ROLE/DATABASE .. SET` is removed with a WARNING.
- The extension is relocatable (`CREATE EXTENSION .. SCHEMA`, `ALTER
  EXTENSION .. SET SCHEMA`) and not trusted: only a superuser installs it.

Images (built locally with `mise run docker-build`; see
[Docker](docs/src/pages/guide/docker.mdx)):

- `pg-automerge:0.1.0`: the official `postgres:18` image plus the
  extension, created in `POSTGRES_DB` on first start
  (`PG_AUTOMERGE_CREATE_EXTENSION=0` skips it), and a `compose.yaml` for
  local development.
- `pg-automerge-cnpg:0.1.0-18-trixie`: the extension's files in the
  layout of a CloudNativePG image volume extension (`FROM scratch`), from
  the same compile. List it in a `Cluster`'s `spec.postgresql.extensions`
  with a PostgreSQL 18 trixie operand (CNPG 1.27 or later, Kubernetes
  image volumes); see
  [CloudNativePG](docs/src/pages/guide/cloudnativepg.mdx).
- Releases: publishing a GitHub release (tag `v<version>`) runs
  `.github/workflows/release.yml`, which tests the tag's commit like CI
  (plus arm64 and the CloudNativePG end-to-end test) and only then pushes
  both images, each as one amd64 + arm64 image, to
  `ghcr.io/theknarf/pg-automerge:<version>` (and `latest`) and
  `ghcr.io/theknarf/pg-automerge-cnpg:<version>-18-trixie`, attaches the
  package tarball, the image archives and `SHA256SUMS` to the release, and
  adds the pull commands and a CNPG snippet to its notes; see
  [Releasing](docs/src/pages/operations/releasing.mdx).

Notable behaviour:

- A flat (stored) `automerge` argument is detoasted once and read in
  place; a loaded document takes far more memory than its stored size (a
  3 MB text about 400 MB), outside Postgres' accounting: size containers
  and pods as [Operations](docs/src/pages/operations/index.mdx) describes.
- GIN indexes on documents: with Postgres' default `fastupdate = on` the
  planner stops using them while writes keep the pending list full, so
  create them with `fastupdate = off` (more WAL); found by the soak test
  (`mise run soak`).
- Reading documents with blocks as jsonb skips rendering the blocks
  altogether, which also makes it about three times faster than
  rendering them.

Known issue:

- Changes with many overlapping rich-text marks take time quadratic in
  their number to apply (Automerge's mark bookkeeping), so a small
  crafted change can keep a backend busy and uncancellable for minutes
  while staying far below `pg_automerge.max_load_memory` (32,000 marks
  in 122 bytes: 3 s; a million: about 50 minutes, extrapolated). Saves
  are not affected. Not priced yet: the cheap bound would refuse
  ordinary rich text (see [What the limit cannot
  do](docs/src/pages/design/resource-limits.mdx#what-the-limit-cannot-do)).

Tested by `mise run ci` (unit, `#[pg_test]`, regress and multi-session
suites, the upgrade test of the install script against
`sql/snapshots/pg_automerge--0.1.0.sql`), `mise run docker-test` (both
images, a 60-second soak run) and `mise run cnpg-e2e` (the CNPG operator
on kind). The documentation is a static site built from `docs/`.
