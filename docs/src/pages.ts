// The page registry: every src/pages/**/*.mdx is a page. Its route comes
// from its file path (page-path.ts), its title, section and position in
// the sidebar from its frontmatter. Adding a page = adding one .mdx file.
//
// Every page's meta (frontmatter and table of contents, from the `?meta`
// query of plugins/page-meta.ts) is in the main bundle; its content is a
// chunk of its own, loaded when the page is shown (React.lazy: the
// prerender waits for it, hydration waits for the page's chunk).
import type { MDXContent } from "mdx/types";
import { type LazyExoticComponent, lazy } from "react";
import type { PageMeta, TocEntry } from "../plugins/page-meta.ts";
import { pagePath, type Section, sections } from "./page-path.ts";

export type { TocEntry };

type Frontmatter = {
	title?: unknown;
	section?: unknown;
	order?: unknown;
	description?: unknown;
};

type MdxModule = { default: MDXContent };

export type Page = {
	path: string;
	file: string;
	title: string;
	section: Section;
	order: number;
	description?: string;
	toc: TocEntry[];
	Content: LazyExoticComponent<MDXContent>;
};

const metas = import.meta.glob<PageMeta>("./pages/**/*.mdx", {
	eager: true,
	query: "?meta",
});
const contents = import.meta.glob<MdxModule>("./pages/**/*.mdx");

function toPage(file: string, meta: PageMeta): Page {
	const fm: Frontmatter = meta.frontmatter;
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
		toc: meta.toc,
		Content: lazy(contents[file]),
	};
}

// In sidebar order: by section, then order, then title. Prev/next links
// follow this order too.
export const pages: Page[] = Object.entries(metas)
	.map(([file, meta]) => toPage(file, meta))
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
