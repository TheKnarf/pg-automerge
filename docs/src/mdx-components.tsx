import type { MDXComponents } from "mdx/types";
import {
	type ComponentProps,
	type ReactNode,
	useEffect,
	useRef,
	useState,
} from "react";
import { Link } from "react-router";
import { usePage } from "./page-context.ts";

// Internal links go through react-router so they carry the base path
// (DOCS_BASE) and navigate client-side. "#anchor" links are resolved
// against the current page's canonical path: a bare "#x" would resolve
// against <base href>, i.e. the home page.
function Anchor({ href, ...props }: ComponentProps<"a">) {
	const page = usePage();
	if (href?.startsWith("#") && page) {
		return <Link to={`${page.path}${href}`} {...props} />;
	}
	if (href?.startsWith("/")) {
		return <Link to={href} {...props} />;
	}
	return <a href={href} {...props} />;
}

// Wide tables scroll horizontally instead of widening the page.
function Table(props: ComponentProps<"table">) {
	return (
		<div className="table-scroll">
			<table {...props} />
		</div>
	);
}

function Pre(props: ComponentProps<"pre">) {
	const ref = useRef<HTMLPreElement>(null);
	const [copied, setCopied] = useState(false);
	useEffect(() => {
		if (!copied) return;
		const t = setTimeout(() => setCopied(false), 1500);
		return () => clearTimeout(t);
	}, [copied]);
	const copy = async () => {
		const text = ref.current?.innerText ?? "";
		try {
			await navigator.clipboard.writeText(text);
			setCopied(true);
		} catch {
			// No clipboard access (insecure context); nothing to do.
		}
	};
	return (
		<div className="code-block">
			<pre ref={ref} {...props} />
			<button type="button" className="copy-button" onClick={copy}>
				{copied ? "Copied" : "Copy"}
			</button>
		</div>
	);
}

const calloutTitles = {
	note: "Note",
	tip: "Tip",
	warning: "Warning",
	danger: "Danger",
};

// <Callout type="warning" title="Optional title">...</Callout>, usable in
// any page without an import.
export function Callout({
	type = "note",
	title,
	children,
}: {
	type?: keyof typeof calloutTitles;
	title?: ReactNode;
	children?: ReactNode;
}) {
	return (
		<aside className={`callout callout-${type}`}>
			<div className="callout-title">{title ?? calloutTitles[type]}</div>
			{children}
		</aside>
	);
}

export const mdxComponents: MDXComponents = {
	a: Anchor,
	table: Table,
	pre: Pre,
	Callout,
};
