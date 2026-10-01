// `import meta from "./pages/x.mdx?meta"`: a page's frontmatter and table
// of contents, without its content. The page registry (src/pages.ts) loads
// every page's meta up front (sidebar, titles, prev/next) and each page's
// content as a chunk of its own, loaded when the page is shown.
//
// The table of contents is computed the way the MDX compile builds the
// page: the same remark plugins up to rehype-slug (vite.config.ts passes
// them in), so its ids are the page's heading ids.
import { readFileSync } from "node:fs";
import type { Element, Root as HastRoot } from "hast";
import { headingRank } from "hast-util-heading-rank";
import { toString as hastToString } from "hast-util-to-string";
import type { Root as MdastRoot, Yaml } from "mdast";
import remarkMdx from "remark-mdx";
import remarkParse from "remark-parse";
import remarkRehype from "remark-rehype";
import { type PluggableList, unified } from "unified";
import { visit } from "unist-util-visit";
import { VFile } from "vfile";
import type { Plugin } from "vite";
import { parse as parseYaml } from "yaml";

export type TocEntry = { id: string; title: string; depth: number };

export type PageMeta = {
	frontmatter: Record<string, unknown>;
	toc: TocEntry[];
};

// MDX's own node types, carried through to hast untouched.
const mdxNodeTypes = [
	"mdxjsEsm",
	"mdxFlowExpression",
	"mdxJsxFlowElement",
	"mdxJsxTextElement",
	"mdxTextExpression",
];

export function pageMetaPlugin(options: {
	remarkPlugins: PluggableList;
	rehypePlugins: PluggableList;
	minDepth?: number;
	maxDepth?: number;
}): Plugin {
	const { minDepth = 2, maxDepth = 3 } = options;
	const processor = unified()
		.use(remarkParse)
		.use(remarkMdx)
		.use(options.remarkPlugins)
		.use(remarkRehype, { passThrough: mdxNodeTypes } as never)
		.use(options.rehypePlugins);

	async function meta(path: string): Promise<PageMeta> {
		const file = new VFile({ path, value: readFileSync(path, "utf8") });
		const mdast = processor.parse(file) as unknown as MdastRoot;
		let frontmatter: Record<string, unknown> = {};
		const yaml = mdast.children.find((n): n is Yaml => n.type === "yaml");
		if (yaml) frontmatter = parseYaml(yaml.value) ?? {};
		const hast = (await processor.run(
			mdast as never,
			file,
		)) as unknown as HastRoot;
		const toc: TocEntry[] = [];
		visit(hast, "element", (node: Element) => {
			const depth = headingRank(node);
			if (!depth || depth < minDepth || depth > maxDepth) return;
			const id = node.properties.id;
			if (typeof id !== "string") return;
			toc.push({ id, title: hastToString(node).trim(), depth });
		});
		return { frontmatter, toc };
	}

	return {
		name: "page-meta",
		enforce: "pre",
		async load(id) {
			const [path, query] = id.split("?", 2);
			if (query !== "meta" || !path.endsWith(".mdx")) return;
			this.addWatchFile(path);
			const { frontmatter, toc } = await meta(path);
			return [
				`export const frontmatter = ${JSON.stringify(frontmatter)};`,
				`export const toc = ${JSON.stringify(toc)};`,
			].join("\n");
		},
	};
}
