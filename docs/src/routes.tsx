import type { RouteObject } from "react-router";
import { Layout, NotFound, PageView } from "./Layout.tsx";
import { pages } from "./pages.ts";

// Shared route config for both the client (createBrowserRouter) and the
// static site generator (createStaticHandler). Child paths are relative to
// the "/" parent; the home page uses an empty path so ssg-for-vite derives
// "/" for it. "404" is prerendered and copied to dist/404.html (GitHub
// Pages serves it for unknown paths, where the client router then matches
// "*" and renders the same page).
const routes: RouteObject[] = [
	{
		path: "/",
		element: <Layout />,
		children: [
			...pages.map((page) => ({
				path: page.path === "/" ? "" : page.path.slice(1),
				element: <PageView page={page} />,
			})),
			{ path: "404", element: <NotFound /> },
			{ path: "*", element: <NotFound /> },
		],
	},
];

export default routes;
