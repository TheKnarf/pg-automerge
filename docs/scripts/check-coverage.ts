// Checks that the site still says everything README.md, docs/DESIGN.md and
// CHANGELOG.md said before they were ported into pages (run after
// `pnpm build`): every heading, paragraph, list item, table cell and code
// block of those files, as they were before the port, must appear in the
// text of some built page, after whitespace is normalized.
//
// The pre-port documents are frozen in coverage-sources/ (<name>.orig,
// byte for byte as they were at commit 6996273, the last commit before
// the port), so the check needs no git history and survives rebases.
//
// Intentional differences are listed in coverage-deviations.json: each
// replaces `from` with `to` in the source blocks that contain it (both
// normalized text, as this script prints them) before they are looked up,
// with the reason. A deviation that no longer applies is an error too, so
// the list stays exact. When a page is edited on purpose, add a deviation
// (`node scripts/check-coverage.ts --suggest` proposes one per missing
// block).
import { readdirSync, readFileSync, statSync } from "node:fs";
import { basename, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import type { Element, Nodes as HastNodes, Root as HastRoot } from "hast";
import { fromHtml } from "hast-util-from-html";
import { toText } from "hast-util-to-text";
import type { Nodes as MdastNodes } from "mdast";
import { fromMarkdown } from "mdast-util-from-markdown";
import { gfmFromMarkdown } from "mdast-util-gfm";
import { toString as mdastToString } from "mdast-util-to-string";
import { gfm } from "micromark-extension-gfm";
import { visit } from "unist-util-visit";

// Named by their pre-port paths (as in coverage-deviations.json).
const SOURCES = ["README.md", "docs/DESIGN.md", "CHANGELOG.md"];

const root = fileURLToPath(new URL("..", import.meta.url));
const dist = join(root, "dist");
const suggest = process.argv.includes("--suggest");

type Deviation = { source: string; from: string; to: string; reason: string };
const deviations: Deviation[] = JSON.parse(
	readFileSync(join(root, "scripts", "coverage-deviations.json"), "utf8"),
);
const used = new Array<number>(deviations.length).fill(0);

const norm = (s: string) => s.normalize("NFC").replace(/\s+/g, " ").trim();

// ---- The source blocks ----
function sourceBlocks(path: string): string[] {
	const md = readFileSync(
		join(root, "scripts", "coverage-sources", `${basename(path)}.orig`),
		"utf8",
	);
	const tree = fromMarkdown(md, {
		extensions: [gfm()],
		mdastExtensions: [gfmFromMarkdown()],
	});
	const blocks: string[] = [];
	visit(tree, (node: MdastNodes) => {
		switch (node.type) {
			case "heading":
			case "paragraph":
			case "tableCell":
				blocks.push(norm(mdastToString(node)));
				return "skip";
			case "code":
			case "html":
				blocks.push(norm(node.value));
				return "skip";
		}
	});
	return blocks.filter((b) => b !== "");
}

// ---- The site's text, per page ----
function walk(dir: string): string[] {
	return readdirSync(dir).flatMap((name) => {
		const path = join(dir, name);
		return statSync(path).isDirectory() ? walk(path) : [path];
	});
}
const classes = (el: Element) => {
	const c = el.properties.className;
	return Array.isArray(c) ? c.map(String) : [];
};
function findArticle(tree: HastRoot): Element | undefined {
	let found: Element | undefined;
	visit(tree, "element", (el: Element) => {
		if (!found && el.tagName === "article") found = el;
	});
	return found;
}
// Not page content: heading anchors ("#"), copy buttons, prev/next links.
function strip(el: Element) {
	el.children = el.children.filter(
		(c) =>
			c.type !== "element" ||
			!(
				c.tagName === "button" ||
				c.tagName === "nav" ||
				classes(c).includes("heading-anchor")
			),
	);
	for (const c of el.children) if (c.type === "element") strip(c);
}
const pages: { path: string; text: string; blocks: string[] }[] = [];
for (const file of walk(dist).filter((f) => f.endsWith("index.html"))) {
	const path = `/${relative(dist, file).replace(/\/?index\.html$/, "")}`;
	if (path === "/404") continue;
	const tree = fromHtml(readFileSync(file, "utf8"));
	const article = findArticle(tree);
	if (!article) continue;
	strip(article);
	const blocks: string[] = [];
	if (suggest) {
		visit(article, "element", (el: Element) => {
			if (/^(p|li|td|th|h[1-6]|pre)$/.test(el.tagName)) {
				blocks.push(norm(toText(el as HastNodes)));
			}
		});
	}
	pages.push({ path, text: norm(toText(article as HastNodes)), blocks });
}
if (pages.length === 0) {
	console.error(`check-coverage: no pages in ${dist}; run pnpm build first`);
	process.exit(1);
}
const found = (block: string) => pages.some((p) => p.text.includes(block));

// The site block most like `block` (longest common prefix + suffix), and
// a deviation that turns one into the other, with a few words of context.
function suggestion(source: string, block: string): Deviation | undefined {
	let best: { score: number; to: string } | undefined;
	for (const p of pages)
		for (const b of p.blocks) {
			let pre = 0;
			while (pre < b.length && b[pre] === block[pre]) pre++;
			let suf = 0;
			while (
				suf < b.length - pre &&
				suf < block.length - pre &&
				b[b.length - 1 - suf] === block[block.length - 1 - suf]
			)
				suf++;
			if (!best || pre + suf > best.score) best = { score: pre + suf, to: b };
		}
	if (!best || best.score < 20) return undefined;
	const b = best.to;
	let pre = 0;
	while (pre < b.length && b[pre] === block[pre]) pre++;
	let suf = 0;
	while (
		suf < b.length - pre &&
		suf < block.length - pre &&
		b[b.length - 1 - suf] === block[block.length - 1 - suf]
	)
		suf++;
	// Widen to a few words either side, so `from` is specific.
	const left = Math.max(0, block.lastIndexOf(" ", Math.max(0, pre - 25)));
	const rightFrom = block.indexOf(" ", block.length - suf + 25);
	const rightCut = rightFrom === -1 ? 0 : block.length - rightFrom;
	return {
		source,
		from: block.slice(left, block.length - rightCut).trim(),
		to: b.slice(left, b.length - rightCut).trim(),
		reason: "",
	};
}

let total = 0;
let viaDeviation = 0;
const missing: { source: string; block: string }[] = [];
const counts: string[] = [];
for (const source of SOURCES) {
	const blocks = sourceBlocks(source);
	counts.push(`${source} ${blocks.length}`);
	for (const original of blocks) {
		total++;
		let block = original;
		deviations.forEach((d, i) => {
			if (d.source === source && block.includes(d.from)) {
				block = block.split(d.from).join(d.to);
				used[i]++;
			}
		});
		if (found(block)) {
			if (block !== original) viaDeviation++;
		} else {
			missing.push({ source, block });
		}
	}
}

let failed = false;
for (const m of missing) {
	failed = true;
	console.error(`check-coverage: ${m.source}: not on any page:\n  ${m.block}`);
	if (suggest) {
		const s = suggestion(m.source, m.block);
		console.error(
			s
				? `  suggested deviation: ${JSON.stringify(s)}`
				: "  (no similar block on the site)",
		);
	}
}
deviations.forEach((d, i) => {
	if (used[i] === 0) {
		failed = true;
		console.error(
			`check-coverage: deviation no longer applies: ${JSON.stringify(d)}`,
		);
	}
	if (!d.reason) {
		failed = true;
		console.error(`check-coverage: deviation without a reason: ${d.from}`);
	}
});
if (failed) process.exit(1);
console.log(
	`check-coverage: ok, ${total}/${total} blocks of the pre-port ${SOURCES.join(", ")} (${counts.join(", ")}) found on ${pages.length} pages; ${viaDeviation} through ${deviations.length} listed deviations`,
);
