import { useEffect, useRef, useState } from "react";
import { Link, Outlet, useLocation } from "react-router";
import { mdxComponents } from "./mdx-components.tsx";
import { PageContext } from "./page-context.ts";
import { nav, type Page, pages } from "./pages.ts";
import "./styles.css";

const siteName = "pg_automerge";

// After a client-side navigation: to the #anchor if there is one, else to
// the top. Not on the first render, where the browser restores the
// position (or jumps to the anchor) itself.
function useScrollOnNavigate() {
	const location = useLocation();
	const first = useRef(true);
	useEffect(() => {
		if (first.current) {
			first.current = false;
			return;
		}
		if (location.hash) {
			const id = decodeURIComponent(location.hash.slice(1));
			document.getElementById(id)?.scrollIntoView();
		} else {
			window.scrollTo(0, 0);
		}
	}, [location]);
}

export function Layout() {
	const location = useLocation();
	const [navOpen, setNavOpen] = useState(false);
	useScrollOnNavigate();
	// The current page's path without a trailing slash: GitHub Pages serves
	// /x/ (dist/x/index.html) where the prerender saw /x, and NavLink's
	// `end` would not match both, so hydration would disagree.
	const here = location.pathname.replace(/(.)\/+$/, "$1");
	// Close the mobile menu when a link in it is followed.
	// biome-ignore lint/correctness/useExhaustiveDependencies: runs on navigation
	useEffect(() => setNavOpen(false), [location.pathname]);

	return (
		<div className="layout">
			<header className="topbar">
				<button
					type="button"
					className="nav-toggle"
					aria-controls="sidebar"
					aria-expanded={navOpen}
					onClick={() => setNavOpen((open) => !open)}
				>
					Menu
				</button>
				<Link to="/" className="brand">
					{siteName}
				</Link>
				<span className="version">v{__PG_AUTOMERGE_VERSION__}</span>
			</header>
			<nav
				id="sidebar"
				className={navOpen ? "sidebar open" : "sidebar"}
				aria-label="Documentation"
			>
				{nav.map((group) => (
					<section key={group.section}>
						<h2>{group.section}</h2>
						<ul>
							{group.pages.map((page) => (
								<li key={page.path}>
									<Link
										to={page.path}
										className={here === page.path ? "active" : undefined}
										aria-current={here === page.path ? "page" : undefined}
									>
										{page.title}
									</Link>
								</li>
							))}
						</ul>
					</section>
				))}
			</nav>
			<main className="main">
				<Outlet />
			</main>
		</div>
	);
}

function Toc({ page }: { page: Page }) {
	if (page.toc.length === 0) return null;
	return (
		<nav className="toc" aria-label="On this page">
			<h2>On this page</h2>
			<ul>
				{page.toc.map((entry) => (
					<li key={entry.id} className={`toc-depth-${entry.depth}`}>
						<Link to={`${page.path}#${entry.id}`}>{entry.title}</Link>
					</li>
				))}
			</ul>
		</nav>
	);
}

function PrevNext({ page }: { page: Page }) {
	const i = pages.indexOf(page);
	const prev = pages[i - 1];
	const next = pages[i + 1];
	return (
		<nav className="prev-next" aria-label="Previous and next page">
			{prev ? (
				<Link to={prev.path} className="prev" rel="prev">
					<span>Previous</span>
					{prev.title}
				</Link>
			) : (
				<span />
			)}
			{next && (
				<Link to={next.path} className="next" rel="next">
					<span>Next</span>
					{next.title}
				</Link>
			)}
		</nav>
	);
}

export function PageView({ page }: { page: Page }) {
	const title = page.path === "/" ? siteName : `${page.title} · ${siteName}`;
	return (
		<PageContext.Provider value={page}>
			<title>{title}</title>
			{page.description && (
				<meta name="description" content={page.description} />
			)}
			<div className="page">
				<article className="content">
					<h1>{page.title}</h1>
					<page.Content components={mdxComponents} />
					<PrevNext page={page} />
				</article>
				<Toc page={page} />
			</div>
		</PageContext.Provider>
	);
}

export function NotFound() {
	return (
		<div className="page">
			<title>{`Page not found · ${siteName}`}</title>
			<article className="content">
				<h1>Page not found</h1>
				<p>
					There is no page at this address. Try the{" "}
					<Link to="/">start page</Link> or the menu.
				</p>
			</article>
		</div>
	);
}
