import { readFileSync } from "node:fs";
import mdx from "@mdx-js/rollup";
import rehypeShiki from "@shikijs/rehype";
import react from "@vitejs/plugin-react";
import rehypeAutolinkHeadings from "rehype-autolink-headings";
import rehypeSlug from "rehype-slug";
import remarkFrontmatter from "remark-frontmatter";
import remarkGfm from "remark-gfm";
import type { PluggableList } from "unified";
import { defineConfig, type Plugin } from "vite";
import { pageMetaPlugin } from "./plugins/page-meta.ts";
import remarkInclude, { includedFiles } from "./plugins/remark-include.ts";

// Languages highlighted at build time (shiki runs only in the MDX compile,
// never in the browser). A fence with any other language fails the build.
const langs = [
	"sql",
	"rust",
	"shellscript", // also bash, sh, shell, zsh
	"json",
	"toml",
	"yaml",
	"dockerfile",
	"diff",
	"javascript",
	"typescript",
];

// The extension's version, from the crate (one source of truth).
const cargoToml = readFileSync(
	new URL("../Cargo.toml", import.meta.url),
	"utf8",
);
const version = /^version = "([^"]+)"/m.exec(cargoToml)?.[1];
if (!version) throw new Error("no version in ../Cargo.toml");

// Shared by the MDX compile and the page-meta pipeline (plugins/page-meta.ts),
// so a page's table of contents has the ids its headings get.
const remarkPlugins: PluggableList = [
	remarkFrontmatter,
	remarkGfm,
	remarkInclude,
];
const slugPlugins: PluggableList = [rehypeSlug];

// Files spliced into pages with <Include> are not modules; reload the
// pages when one changes (dev server).
const reloadIncluded: Plugin = {
	name: "reload-included",
	handleHotUpdate({ file, server }) {
		if (!includedFiles.has(file)) return;
		server.moduleGraph.invalidateAll();
		server.ws.send({ type: "full-reload" });
		return [];
	},
};

export default defineConfig(() => ({
	root: "src",
	// Defaults to "/"; the GitHub Pages workflow sets DOCS_BASE to the repo
	// subpath so relative asset URLs resolve when deployed under /<repo>/.
	base: process.env.DOCS_BASE || "/",
	build: {
		outDir: "../dist",
		emptyOutDir: true,
		rollupOptions: {
			output: {
				manualChunks(id: string) {
					if (!id.includes("node_modules")) return;
					if (/[\\/](react|react-dom|react-router|scheduler)[\\/]/.test(id)) {
						return "react-vendor";
					}
				},
			},
		},
	},
	// ssg.tsx runs under vite-node (VITE_NODE=true), which transforms .mdx
	// files in "web" mode: without this their React imports would point at
	// the browser's pre-bundled deps (node_modules/.vite/deps), which a
	// Node import cannot load.
	optimizeDeps:
		process.env.VITE_NODE === "true"
			? { noDiscovery: true, include: [] }
			: undefined,
	define: {
		__PG_AUTOMERGE_VERSION__: JSON.stringify(version),
	},
	plugins: [
		// Before react(), so the compiled MDX goes through its JSX transform.
		{
			enforce: "pre" as const,
			...mdx({
				// The plugin would derive this from Vite's mode, which is
				// "development" under vite-node (ssg.tsx) even for production
				// builds; React's production jsx-dev-runtime has no jsxDEV.
				development: process.env.NODE_ENV !== "production",
				remarkPlugins,
				rehypePlugins: [
					...slugPlugins,
					[
						rehypeAutolinkHeadings,
						{
							behavior: "append",
							properties: {
								className: ["heading-anchor"],
								ariaLabel: "Link to this section",
							},
							content: { type: "text", value: "#" },
						},
					],
					[
						rehypeShiki,
						{
							themes: { light: "github-light", dark: "github-dark" },
							// Colours as CSS light-dark(), following color-scheme.
							defaultColor: "light-dark()",
							langs,
							defaultLanguage: "text",
						},
					],
				],
			}),
		},
		pageMetaPlugin({ remarkPlugins, rehypePlugins: slugPlugins }),
		reloadIncluded,
		react({ include: /\.(mdx|tsx?)$/ }),
	],
}));
