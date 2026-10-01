import { readFileSync } from "node:fs";
import mdx from "@mdx-js/rollup";
import rehypeShiki from "@shikijs/rehype";
import react from "@vitejs/plugin-react";
import rehypeAutolinkHeadings from "rehype-autolink-headings";
import rehypeSlug from "rehype-slug";
import remarkFrontmatter from "remark-frontmatter";
import remarkGfm from "remark-gfm";
import remarkMdxFrontmatter from "remark-mdx-frontmatter";
import { defineConfig } from "vite";
import rehypeExportToc from "./plugins/rehype-export-toc.ts";

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
				remarkPlugins: [
					remarkFrontmatter,
					[remarkMdxFrontmatter, { name: "frontmatter" }],
					remarkGfm,
				],
				rehypePlugins: [
					rehypeSlug,
					rehypeExportToc,
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
		react({ include: /\.(mdx|tsx?)$/ }),
	],
}));
