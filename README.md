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
queryable.

## Documentation

The documentation is a static site built from [docs/](docs/), published
to GitHub Pages under `/pg-automerge/` by
`.github/workflows/deploy-docs.yml`. Its pages are the MDX files in
[docs/src/pages](docs/src/pages), which read fine on GitHub too:

- Guide: [Install](docs/src/pages/guide/install.mdx),
  [Quick start](docs/src/pages/guide/quick-start.mdx),
  [Keeping backends in sync](docs/src/pages/guide/keeping-backends-in-sync.mdx),
  [Docker](docs/src/pages/guide/docker.mdx),
  [Updating](docs/src/pages/guide/updating.mdx),
  [Development](docs/src/pages/guide/development.mdx).
- Reference: [SQL API](docs/src/pages/reference/index.mdx),
  [Configuration](docs/src/pages/reference/configuration.mdx),
  [Error codes](docs/src/pages/reference/error-codes.mdx).
- Operations: [Sizing, WAL and indexes](docs/src/pages/operations/index.mdx),
  [Monitoring](docs/src/pages/operations/monitoring.mdx),
  [Limitations and gotchas](docs/src/pages/operations/limitations.mdx).
- [Design](docs/src/pages/design/index.mdx): the full specification.
- [CHANGELOG.md](CHANGELOG.md): what changed in each version.

To run the site locally (Node and pnpm are pinned in `mise.toml`):

```sh
mise run docs-dev      # dev server with hot reload
mise run docs-check    # what CI checks: typecheck, lint, build, links, coverage
```

See [docs/Readme.md](docs/Readme.md) for how the site is put together.

## Quick start

PostgreSQL 18 only. With Docker, the image is `postgres:18` plus the
extension, built locally ([Docker](docs/src/pages/guide/docker.mdx)):

```sh
mise run docker-up           # build the image, start compose.yaml's Postgres
psql postgres://postgres:postgres@localhost:5432/app
```

From source, with [mise](https://mise.jdx.dev)
([Install](docs/src/pages/guide/install.mdx)):

```sh
mise install && mise run pgrx-init
cargo pgrx install --release --pg-config /usr/lib/postgresql/18/bin/pg_config
psql -c 'CREATE EXTENSION pg_automerge'   # as a superuser: it is not trusted
```

Then:

```sql
CREATE TABLE docs (id uuid PRIMARY KEY, doc automerge NOT NULL);

-- Persist: $2 is the output of Automerge save() (or only new changes), as
-- bytea. Concurrent writers merge instead of overwriting.
INSERT INTO docs (id, doc) VALUES ($1, $2)
ON CONFLICT (id) DO UPDATE SET doc = merge(docs.doc, EXCLUDED.doc);
UPDATE docs SET doc = merge(doc, $2) WHERE id = $1;

-- Read: automerge casts implicitly to jsonb.
SELECT doc->>'title' FROM docs WHERE doc @> '{"status": "open"}';

-- What a replica at heads $2 (text[]) is missing, for loadIncremental.
SELECT automerge_changes_bytes(doc, $2) FROM docs WHERE id = $1;
```

## License

MIT — see [LICENSE](LICENSE).
