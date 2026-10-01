// Maps an MDX file under src/pages to its route. Shared by the page
// registry (src/pages.ts) and scripts/check-site.ts, so the checker knows
// which pages the build must emit.
//
//   pages/index.mdx              -> /
//   pages/guide/index.mdx        -> /guide
//   pages/guide/quick-start.mdx  -> /guide/quick-start
export function pagePath(file: string): string {
	const rel = file
		.replace(/\\/g, "/")
		.replace(/^.*?\/?pages\//, "")
		.replace(/\.mdx$/, "");
	const path = `/${rel}`.replace(/(^|\/)index$/, "");
	return path === "" ? "/" : path;
}

// The sidebar's sections, in order. A page's frontmatter `section` must be
// one of these.
export const sections = [
	"Guide",
	"Reference",
	"Operations",
	"Design",
	"Changelog",
] as const;

export type Section = (typeof sections)[number];
