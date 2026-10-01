import { createContext, useContext } from "react";
import type { Page } from "./pages.ts";

// The page being rendered, for links relative to it (#anchors).
export const PageContext = createContext<Page | null>(null);

export const usePage = () => useContext(PageContext);
