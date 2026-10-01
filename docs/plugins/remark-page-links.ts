// Links between pages are written as relative paths to the page sources
// (`[Resource limits](../design/resource-limits.mdx#x)`), so they also work
// when the .mdx files are read on GitHub. This turns them into site routes
// (`/design/resource-limits#x`) at build time. A root-relative link
// (`/design/resource-limits`) fails the build: on GitHub it would lead to
// github.com/design/..., a 404. So does a relative link to anything but a
// page: on the site it would point nowhere.
//
// Runs before remark-include, whose spliced-in links are already routes.
import type { Definition, Link, Root } from "mdast";
import { visit } from "unist-util-visit";
import type { VFile } from "vfile";
import { siteLink } from "./remark-include.ts";

export default function remarkPageLinks() {
	return (tree: Root, file: VFile) => {
		visit(tree, ["link", "definition"], (node) => {
			const link = node as Link | Definition;
			const where = `${file.path}:${link.position?.start.line ?? "?"}`;
			if (link.url.startsWith("/")) {
				throw new Error(
					`${where}: link ${link.url} is root-relative; link to the page's source instead (relative to this file, e.g. ../design/x.mdx#y) so it also works on GitHub`,
				);
			}
			link.url = siteLink(link.url, file.dirname ?? ".", where);
		});
	};
}
