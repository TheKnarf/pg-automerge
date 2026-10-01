// GitHub Pages serves dist/404.html for unknown paths: the prerendered
// "404" route (the client router renders the same page through "*").
import { copyFileSync } from "node:fs";

const dist = new URL("../dist/", import.meta.url);
copyFileSync(new URL("404/index.html", dist), new URL("404.html", dist));
console.log("dist/404.html written");
