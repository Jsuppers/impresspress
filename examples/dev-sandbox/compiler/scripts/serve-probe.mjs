/**
 * Serve `src/probe.html` over `dist/` with the headers the sandbox uses.
 *
 * The compiler needs `SharedArrayBuffer`, so the page has to be
 * cross-origin isolated, and the value that matters is the one the sandbox
 * actually deploys: `Cross-Origin-Embedder-Policy: credentialless`, not the
 * `require-corp` rubrc's own build emits. Proving the worker starts under
 * `credentialless` is the point of running the probe at all — under
 * `require-corp` every same-origin asset would need a CORP header we do not
 * set, and the failure would only show up in production.
 *
 * Usage:
 *   node scripts/serve-probe.mjs [port]     # default 8099
 *
 * Routes:
 *   /                     src/probe.html
 *   /manifest.json, /<version>/**   dist/
 *   /template/hello.json  the `hello` block template, read from the crate
 *                         (`crates/impresspress-core/src/blocks/dev/templates/hello`)
 *                         so the probe compiles the real thing rather than a
 *                         copy that can drift.
 */

import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const repo = path.resolve(here, "../../..");
const dist = path.join(here, "dist");
const template = path.join(repo, "crates/impresspress-core/src/blocks/dev/templates/hello");
const port = Number(process.argv[2] ?? process.env.PROBE_PORT ?? 8099);

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".wasm": "application/wasm",
  ".br": "application/octet-stream",
};

const send = (response, status, body, type) => {
  response.writeHead(status, {
    "Content-Type": type,
    "Content-Length": Buffer.byteLength(body),
    // The two headers that make `crossOriginIsolated` true.
    "Cross-Origin-Opener-Policy": "same-origin",
    "Cross-Origin-Embedder-Policy": "credentialless",
    "Cache-Control": "no-store",
  });
  response.end(body);
};

const server = http.createServer((request, response) => {
  const url = new URL(request.url ?? "/", `http://localhost:${port}`);
  const pathname = decodeURIComponent(url.pathname);

  try {
    if (pathname === "/" || pathname === "/probe.html") {
      send(response, 200, fs.readFileSync(path.join(here, "src/probe.html")), TYPES[".html"]);
      return;
    }

    if (pathname === "/template/hello.json") {
      const files = {
        "Cargo.toml": fs.readFileSync(path.join(template, "Cargo.toml"), "utf8"),
        "src/lib.rs": fs.readFileSync(path.join(template, "src/lib.rs"), "utf8"),
        "src/wafer_guest.rs": fs.readFileSync(path.join(template, "src/wafer_guest.rs"), "utf8"),
      };
      send(response, 200, JSON.stringify({ crateName: "hello", files }), TYPES[".json"]);
      return;
    }

    // Everything else is a static asset out of dist/. `..` cannot escape,
    // because the resolved path is checked to still be under dist/.
    const file = path.resolve(dist, `.${pathname}`);
    if (!file.startsWith(dist) || !fs.existsSync(file) || !fs.statSync(file).isFile()) {
      send(response, 404, `no such file: ${pathname}\n`, "text/plain; charset=utf-8");
      return;
    }
    const extension = /\.br\.part-\d+$/.test(file) ? ".br" : path.extname(file);
    send(response, 200, fs.readFileSync(file), TYPES[extension] ?? "application/octet-stream");
  } catch (error) {
    send(response, 500, `${error?.stack ?? error}\n`, "text/plain; charset=utf-8");
  }
});

server.listen(port, () => {
  console.log(`probe: http://localhost:${port}/  (serving ${dist})`);
});
