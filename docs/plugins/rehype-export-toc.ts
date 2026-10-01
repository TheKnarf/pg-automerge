// Exports the page's table of contents from every MDX module:
//
//   export const toc = [{ id: "install", title: "Install", depth: 2 }, ...]
//
// Runs after rehype-slug (so headings carry their final ids) and before
// rehype-autolink-headings (so the anchor link is not part of the title).

import type { Program } from "estree";
import { valueToEstree } from "estree-util-value-to-estree";
import type { Element, Root } from "hast";
import { headingRank } from "hast-util-heading-rank";
import { toString as hastToString } from "hast-util-to-string";
import { visit } from "unist-util-visit";

export type TocEntry = { id: string; title: string; depth: number };

export default function rehypeExportToc({ minDepth = 2, maxDepth = 3 } = {}) {
	return (tree: Root) => {
		const toc: TocEntry[] = [];
		visit(tree, "element", (node: Element) => {
			const depth = headingRank(node);
			if (!depth || depth < minDepth || depth > maxDepth) return;
			const id = node.properties.id;
			if (typeof id !== "string") return;
			toc.push({ id, title: hastToString(node).trim(), depth });
		});

		const program: Program = {
			type: "Program",
			sourceType: "module",
			body: [
				{
					type: "ExportNamedDeclaration",
					specifiers: [],
					attributes: [],
					declaration: {
						type: "VariableDeclaration",
						kind: "const",
						declarations: [
							{
								type: "VariableDeclarator",
								id: { type: "Identifier", name: "toc" },
								init: valueToEstree(toc),
							},
						],
					},
				},
			],
		};
		// An ESM node, as remark-mdx-frontmatter adds for `frontmatter`
		// (mdxjsEsm is MDX's extension of hast, not in @types/hast).
		const esm = { type: "mdxjsEsm", value: "", data: { estree: program } };
		tree.children.unshift(esm as unknown as Root["children"][number]);
	};
}
