// The page registry: every src/pages/**/*.mdx is a page. Its route comes
// from its file path (page-path.ts), its title, section and position in
// the sidebar from its frontmatter. Adding a page = adding one .mdx file.
import type { MDXContent } from "mdx/types";
import type { TocEntry } from "../plugins/rehype-export-toc.ts";
import { pagePath, type Section, sections } from "./page-path.ts";

export type { TocEntry };

type Frontmatter = {
	title?: unknown;
	section?: unknown;
	order?: unknown;
	description?: unknown;
};

type MdxModule = {
	default: MDXContent;
	frontmatter?: Frontmatter;
	toc: TocEntry[];
};

export type Page = {
	path: string;
	file: string;
	title: string;
	section: Section;
	order: number;
	description?: string;
	toc: TocEntry[];
	Content: MDXContent;
};

const modules = import.meta.glob<MdxModule>("./pages/**/*.mdx", {
	eager: true,
});

function toPage(file: string, mod: MdxModule): Page {
	const fm = mod.frontmatter ?? {};
	const fail = (msg: string) => {
		throw new Error(`${file}: frontmatter ${msg}`);
	};
	if (typeof fm.title !== "string" || fm.title === "") fail("needs a title");
	if (!sections.includes(fm.section as Section)) {
		fail(`section must be one of ${sections.join(", ")}`);
	}
	if (fm.order !== undefined && typeof fm.order !== "number") {
		fail("order must be a number");
	}
	if (fm.description !== undefined && typeof fm.description !== "string") {
		fail("description must be a string");
	}
	return {
		path: pagePath(file),
		file,
		title: fm.title as string,
		section: fm.section as Section,
		order: (fm.order as number | undefined) ?? 0,
		description: fm.description as string | undefined,
		toc: mod.toc,
		Content: mod.default,
	};
}

// In sidebar order: by section, then order, then title. Prev/next links
// follow this order too.
export const pages: Page[] = Object.entries(modules)
	.map(([file, mod]) => toPage(file, mod))
	.sort(
		(a, b) =>
			sections.indexOf(a.section) - sections.indexOf(b.section) ||
			a.order - b.order ||
			a.title.localeCompare(b.title),
	);

const seen = new Map<string, string>();
for (const page of pages) {
	const other = seen.get(page.path);
	if (other) throw new Error(`${page.file} and ${other} are both ${page.path}`);
	seen.set(page.path, page.file);
}

export const nav = sections
	.map((section) => ({
		section,
		pages: pages.filter((p) => p.section === section),
	}))
	.filter((group) => group.pages.length > 0);
