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
mise run docs-check     # typecheck, biome, build, check-site
```

or, in `docs/`, `pnpm install` then `pnpm dev`, `pnpm build`,
`pnpm check`, `pnpm preview` (serves `dist/`; give it the `DOCS_BASE` the
build had).

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
dockerfile, diff, js, ts), [links](/guide/quick-start#indexing-reads).

<Callout type="warning" title="Optional title">note, tip, warning or danger</Callout>
```

The page's title is its `<h1>`, so content starts at `##`. `##` and `###`
headings form the page's table of contents; every heading gets a stable
id (GitHub's slug rules) and an anchor link. Link to other pages with
absolute paths (`/section/page#anchor`, without the base path) and within
the page with `#anchor`; `pnpm check-site` fails on links that do not
resolve.

## How it fits together

- `src/pages.ts`: the page registry (`import.meta.glob` of the MDX files),
  validating frontmatter; `src/page-path.ts`: file path to route, and the
  section order.
- `src/routes.tsx`: routes shared by the dev SPA (`src/main.tsx`), the
  prerender (`ssg-for-vite.tsx`) and hydration (`src/ssg-main.tsx`).
- `src/Layout.tsx`: sidebar, table of contents, previous/next links;
  `src/mdx-components.tsx`: links, tables, code blocks, `<Callout>`;
  `src/styles.css`: all styling (light/dark from `prefers-color-scheme`).
- `vite.config.ts`: MDX with remark-gfm, frontmatter, rehype-slug,
  autolinked headings, `plugins/rehype-export-toc.ts` and shiki
  (highlighting happens at build time; no highlighter in the browser).
- `scripts/copy-404.ts`: `dist/404.html` for GitHub Pages;
  `scripts/check-site.ts`: every page emitted, every internal link and
  anchor resolves.

`ssg.tsx`, `ssg-for-vite.tsx`, `src/ssg-main.tsx` and `src/main.tsx` are
copied unchanged from
[modular-svg](https://github.com/theknarf-experiments/modular-svg)'s docs
package (MIT).
