# pg_automerge documentation site

The docs as a static site: Vite + React + React Router, pages written in
MDX, every route prerendered to HTML (`ssg.tsx` + `ssg-for-vite.tsx`) and
hydrated in the browser. Published to GitHub Pages by
`.github/workflows/deploy-docs.yml`.

## Running it

Node and pnpm are pinned in the repository's `mise.toml` (`mise install`).
From the repository root:

```sh
mise run docs-install   # pnpm install --frozen-lockfile
mise run docs-dev       # dev server with hot reload (client-rendered)
mise run docs-build     # static site into docs/dist
mise run docs-check     # typecheck, biome, build, check-site, check-coverage
```

or, in `docs/`, `pnpm install` then `pnpm dev`, `pnpm build`,
`pnpm check`, `pnpm preview` (serves `dist/`; give it the `DOCS_BASE` the
build had), `pnpm check-site`, `pnpm check-coverage`.

`DOCS_BASE` sets the path the site is served under (default `/`; GitHub
Pages serves a project site under `/<repo>/`, and `mise run docs-check`
builds with `/pg-automerge/` to exercise that).

## Adding a page

Add one `.mdx` file under `src/pages/`. Its path is its URL
(`src/pages/guide/quick-start.mdx` is `/guide/quick-start`, `index.mdx` is
the directory's own page), and its frontmatter places it in the sidebar:

```mdx
---
title: Quick start          # required: sidebar, <h1>, <title>
section: Guide              # required: Guide, Reference, Operations, Design or Changelog
order: 10                   # position within the section (default 0, then by title)
description: One sentence.  # optional: <meta name="description">
---

## A heading

Text, GFM tables, fenced code (sql, rust, sh, json, toml, yaml,
dockerfile, diff, js, ts), [links](../guide/quick-start.mdx).

<Callout type="warning" title="Optional title">note, tip, warning or danger</Callout>
```

MDX is not Markdown: `<` and `{` outside code start JSX and expressions,
so write `\<` and `\{` (or put them in backticks), and comments are
`{/* ... */}`.

Pages are also read on GitHub, from code comments
(`docs/src/pages/design/resource-limits.mdx, "The estimate"`) and from
CHANGELOG.md, so keep a page's file name and its headings stable, or
update what refers to them (`tests/check_ci.sh` fails on a reference to a
page that does not exist). `tests/docker_upgrade.sh` runs the compose
commands of `guide/updating.mdx`.

The page's title is its `<h1>`, so content starts at `##`. `##` and `###`
headings form the page's table of contents; every heading gets a stable
id (GitHub's slug rules) and an anchor link. Link to other pages by
their source file, relative to the page (`../section/page.mdx#anchor`), so
the link works on GitHub too; `plugins/remark-page-links.ts` turns it into
a route at build time, and fails the build on a root-relative link
(`/section/page`, a 404 on GitHub). Link within the page with `#anchor`;
`pnpm check-site` fails on links that do not resolve.

## Where the content came from

The pages were ported from the repository's README.md and docs/DESIGN.md
(which was then removed); CHANGELOG.md stays at the repository root and is
spliced into the changelog page at build time
(`<Include file="../../../CHANGELOG.md" />`, see below), so it is edited
in one place. Its links point at page sources
(`docs/src/pages/guide/updating.mdx`), which work on GitHub and become
routes on the site.

`scripts/check-coverage.ts` (part of `pnpm check`) proves nothing was
lost: it reads README.md, docs/DESIGN.md and CHANGELOG.md as they were
before the port (frozen, byte for byte as at commit 6996273, in
`scripts/coverage-sources/*.orig`, so no git history is needed), and checks that every
heading, paragraph, list item, table cell and code block of them appears,
whitespace-normalized, in the text of some built page. The intentional
differences (links that were titled "DESIGN.md" now name their page, and
prose that said "the README" names the page it meant) are listed with
their reasons in `scripts/coverage-deviations.json`. When you change a
ported sentence on purpose, add a deviation for it:
`node scripts/check-coverage.ts --suggest` proposes one per block it no
longer finds.

## How it fits together

- `src/pages.ts`: the page registry, validating frontmatter. Every page's
  frontmatter and table of contents (`page.mdx?meta`, from
  `plugins/page-meta.ts`) is in the main bundle; its content is a chunk of
  its own, loaded with `React.lazy` (the prerender waits for it, so the
  HTML has the full page). `src/page-path.ts`: file path to route, and
  the section order.
- `src/routes.tsx`: routes shared by the dev SPA (`src/main.tsx`), the
  prerender (`ssg-for-vite.tsx`) and hydration (`src/ssg-main.tsx`).
- `src/Layout.tsx`: sidebar, table of contents, previous/next links;
  `src/mdx-components.tsx`: links, tables, code blocks, `<Callout>`;
  `src/styles.css`: all styling (light/dark from `prefers-color-scheme`).
- `vite.config.ts`: MDX with remark-gfm, frontmatter,
  `plugins/remark-page-links.ts` (page links as `.mdx` paths become
  routes), `plugins/remark-include.ts` (`<Include file="..." />` splices a Markdown
  file into a page), rehype-slug, autolinked headings and shiki
  (highlighting happens at build time; no highlighter in the browser).
  `plugins/page-meta.ts` runs the same remark plugins and rehype-slug to
  compute each page's table of contents.
- `scripts/copy-404.ts`: `dist/404.html` for GitHub Pages;
  `scripts/check-site.ts`: every page emitted, every internal link and
  anchor resolves; `scripts/check-coverage.ts`: see above.

`ssg.tsx`, `ssg-for-vite.tsx`, `src/ssg-main.tsx` and `src/main.tsx` are
copied unchanged from
[modular-svg](https://github.com/theknarf-experiments/modular-svg)'s docs
package (MIT).
