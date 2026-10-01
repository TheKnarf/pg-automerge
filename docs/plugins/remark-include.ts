// <Include file="../../../CHANGELOG.md" /> in an MDX page: splices that
// Markdown file (CommonMark + GFM, not MDX, so it stays plain Markdown for
// GitHub) into the page at build time, as if it were written there. Its
// first `#` heading is dropped (the page has its own title), and its links
// to page sources (`docs/src/pages/guide/updating.mdx#x`, relative to the
// included file, as they work on GitHub) become site routes
// (`/guide/updating#x`). Any other relative link fails the build: on the
// site it would point nowhere.
//
// Runs in both the MDX compile and the page-meta pipeline, so the page's
// table of contents includes the spliced headings.
import { readFileSync } from "node:fs";
import { dirname, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import type { Link, Root, RootContent } from "mdast";
import { fromMarkdown } from "mdast-util-from-markdown";
import { gfmFromMarkdown } from "mdast-util-gfm";
import { gfm } from "micromark-extension-gfm";
import { visit } from "unist-util-visit";
import type { VFile } from "vfile";
import { pagePath } from "../src/page-path.ts";

const pagesDir = fileURLToPath(new URL("../src/pages/", import.meta.url));

type JsxNode = {
	type: "mdxJsxFlowElement";
	name: string | null;
	attributes: { type: string; name?: string; value?: unknown }[];
};

// Every file spliced in so far, for the dev server's file watcher.
export const includedFiles = new Set<string>();

export function siteLink(url: string, fromDir: string, where: string): string {
	if (/^[a-z][a-z0-9+.-]*:/i.test(url) || url.startsWith("#")) return url;
	const [path, hash = ""] = url.split(/(?=#)/);
	const abs = resolve(fromDir, path);
	const rel = relative(pagesDir, abs).split(sep).join("/");
	if (!rel.startsWith("..") && rel.endsWith(".mdx")) {
		return `${pagePath(`pages/${rel}`)}${hash}`;
	}
	throw new Error(
		`${where}: link ${url} is not a page (docs/src/pages/**/*.mdx) or an absolute URL`,
	);
}

export default function remarkInclude() {
	return (tree: Root, file: VFile) => {
		visit(tree, (node, index, parent) => {
			const jsx = node as unknown as JsxNode;
			if (jsx.type !== "mdxJsxFlowElement" || jsx.name !== "Include") return;
			if (!parent || index === undefined) return;
			const attr = jsx.attributes.find((a) => a.name === "file");
			if (typeof attr?.value !== "string") {
				throw new Error(`${file.path}: <Include> needs file="..."`);
			}
			const path = resolve(file.dirname ?? ".", attr.value);
			includedFiles.add(path);
			const md = fromMarkdown(readFileSync(path, "utf8"), {
				extensions: [gfm()],
				mdastExtensions: [gfmFromMarkdown()],
			});
			const children = md.children.filter(
				(n, i) => !(i === 0 && n.type === "heading" && n.depth === 1),
			);
			visit(md, "link", (link: Link) => {
				link.url = siteLink(link.url, dirname(path), path);
			});
			parent.children.splice(index, 1, ...(children as RootContent[]));
			return index + children.length;
		});
	};
}
