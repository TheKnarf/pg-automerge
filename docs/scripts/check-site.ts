// Checks the built site in dist/ (run after `pnpm build`, with the same
// DOCS_BASE):
//   - every page in src/pages/**/*.mdx was prerendered to
//     dist/<path>/index.html, and 404.html exists;
//   - every internal link (href/src under the base path) in every HTML file
//     resolves to an emitted page or asset, and its #anchor, if any, to an
//     id on that page;
//   - no id is defined twice on a page.
// Links outside the base path (other sites) are not checked: the check
// runs offline.
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { pagePath } from "../src/page-path.ts";

const root = fileURLToPath(new URL("..", import.meta.url));
const dist = join(root, "dist");
const base = (process.env.DOCS_BASE || "/").replace(/\/*$/, "/");
const errors: string[] = [];

function walk(dir: string): string[] {
	return readdirSync(dir).flatMap((name) => {
		const path = join(dir, name);
		return statSync(path).isDirectory() ? walk(path) : [path];
	});
}

if (!existsSync(dist)) {
	console.error(`check-site: ${dist} does not exist; run pnpm build first`);
	process.exit(1);
}

// The pages the registry knows, and where the build must have put them.
const pageFiles = walk(join(root, "src", "pages")).filter((f) =>
	f.endsWith(".mdx"),
);
const htmlFor = (path: string) =>
	join(dist, path.replace(/^\/+/, ""), "index.html");
for (const file of pageFiles) {
	const path = pagePath(relative(join(root, "src"), file).split(sep).join("/"));
	if (!existsSync(htmlFor(path))) {
		errors.push(`${relative(root, file)}: not emitted (${htmlFor(path)})`);
	}
}
if (!existsSync(join(dist, "404.html"))) errors.push("dist/404.html missing");

// Ids per HTML file, and the links in it.
const htmlFiles = walk(dist).filter((f) => f.endsWith(".html"));
const idsOf = new Map<string, Set<string>>();
const decode = (s: string) =>
	s
		.replace(/&quot;/g, '"')
		.replace(/&#x27;/g, "'")
		.replace(/&lt;/g, "<")
		.replace(/&gt;/g, ">")
		.replace(/&amp;/g, "&");
for (const file of htmlFiles) {
	const html = readFileSync(file, "utf8");
	const ids = new Set<string>();
	for (const m of html.matchAll(/\sid="([^"]*)"/g)) {
		const id = decode(m[1]);
		if (ids.has(id))
			errors.push(`${relative(dist, file)}: duplicate id "${id}"`);
		ids.add(id);
	}
	idsOf.set(file, ids);
}

let checked = 0;
for (const file of htmlFiles) {
	const html = readFileSync(file, "utf8");
	const where = relative(dist, file);
	if (!html.includes(`<base href="${base}"/>`)) {
		errors.push(
			`${where}: no <base href="${base}"/> (built with another DOCS_BASE?)`,
		);
	}
	for (const m of html.matchAll(/\s(?:href|src)="([^"]*)"/g)) {
		const raw = decode(m[1]);
		if (/^[a-z][a-z0-9+.-]*:/i.test(raw) || raw.startsWith("//")) continue;
		checked++;
		// Relative URLs resolve against <base href>, absolute ones must be
		// under it.
		const url = new URL(raw, `http://site${base}`);
		if (!url.pathname.startsWith(base) && `${url.pathname}/` !== base) {
			errors.push(`${where}: ${raw} is outside the base path ${base}`);
			continue;
		}
		const rel = decodeURIComponent(url.pathname.slice(base.length));
		const candidates = [join(dist, rel), join(dist, rel, "index.html")];
		const target = candidates.find(
			(c) => existsSync(c) && statSync(c).isFile(),
		);
		if (!target) {
			errors.push(`${where}: broken link ${raw}`);
			continue;
		}
		if (url.hash) {
			const id = decodeURIComponent(url.hash.slice(1));
			if (!idsOf.get(target)?.has(id)) {
				errors.push(
					`${where}: ${raw}: no id "${id}" on ${relative(dist, target)}`,
				);
			}
		}
	}
}

if (errors.length > 0) {
	for (const e of errors) console.error(`check-site: ${e}`);
	process.exit(1);
}
console.log(
	`check-site: ok (${pageFiles.length} pages, ${htmlFiles.length} HTML files, ${checked} internal links)`,
);
