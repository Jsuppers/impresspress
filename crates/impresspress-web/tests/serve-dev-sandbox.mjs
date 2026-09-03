/**
 * Serve a built dev-sandbox bundle for the end-to-end suite.
 *
 * `python3 -m http.server` is enough for every other browser-bundle spec, and
 * it is what `ci-main.yml`'s `e2e-dev-sandbox` job has always used. It is NOT
 * enough once the page starts the compiler, and the reason is a rule that is
 * easy to miss:
 *
 *   **A dedicated worker inherits its creator's cross-origin embedder policy,
 *   and the browser refuses to start one whose own script response does not
 *   carry a compatible `Cross-Origin-Embedder-Policy`.**
 *
 * `/b/dev` sets `Cross-Origin-Opener-Policy: same-origin` and
 * `Cross-Origin-Embedder-Policy: credentialless` on itself (`blocks/dev/
 * page.rs`) because the compiler needs `SharedArrayBuffer`. The compiler
 * worker's script is NOT served by that runtime — `/__impresspress_dev/
 * compiler/` is on the service worker's bypass list (`examples/dev-sandbox/
 * impresspress.toml`), because it is a quarter of a gigabyte of static
 * toolchain. So the STATIC host is what has to say `credentialless` for those
 * files, and a host that says nothing gets
 * `net::ERR_BLOCKED_BY_RESPONSE` and a `Worker` `error` event with no message
 * in it — which is exactly as much as the page can be told.
 *
 * `examples/dev-sandbox/compiler/scripts/serve-probe.mjs` already does this
 * for the compiler's own probe; this is the same rule for the bundle.
 *
 * The header is scoped to `/__impresspress_dev/compiler/` rather than sent on
 * everything, because that is the smallest true statement: a subresource does
 * not need COEP, only a worker script that inherits one does. Sending it on
 * `/` as well would make the boot shell cross-origin isolated and quietly
 * invalidate the "the published site is NOT isolated" half of the design.
 *
 * Usage:
 *   node tests/serve-dev-sandbox.mjs <dist> [port]      # default port 8082
 */

import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';

const dist = path.resolve(process.argv[2] ?? '.');
const port = Number(process.argv[3] ?? process.env.TEST_PORT ?? 8082);

/** The prefix whose files a COEP document loads as worker scripts. */
const COMPILER_PREFIX = '/__impresspress_dev/compiler/';

const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.wasm': 'application/wasm',
  '.br': 'application/octet-stream',
  '.png': 'image/png',
  '.svg': 'image/svg+xml',
  '.ico': 'image/x-icon',
  '.woff2': 'font/woff2',
  '.txt': 'text/plain; charset=utf-8',
};

if (!fs.existsSync(dist) || !fs.statSync(dist).isDirectory()) {
  console.error(`serve-dev-sandbox: ${dist} is not a directory — run examples/dev-sandbox/build.sh`);
  process.exit(1);
}

const server = http.createServer((request, response) => {
  const url = new URL(request.url ?? '/', `http://localhost:${port}`);
  const pathname = decodeURIComponent(url.pathname);

  // `..` cannot escape: the resolved path is checked to still be under dist.
  let file = path.resolve(dist, `.${pathname}`);
  if (!file.startsWith(dist)) {
    response.writeHead(403).end('forbidden\n');
    return;
  }
  if (fs.existsSync(file) && fs.statSync(file).isDirectory()) {
    file = path.join(file, 'index.html');
  }
  if (!fs.existsSync(file) || !fs.statSync(file).isFile()) {
    response.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' });
    response.end(`no such file: ${pathname}\n`);
    return;
  }

  const body = fs.readFileSync(file);
  const headers = {
    'Content-Type': TYPES[path.extname(file)] ?? 'application/octet-stream',
    'Content-Length': body.length,
    // The bundle is rebuilt between runs under the same URLs.
    'Cache-Control': 'no-store',
  };
  if (pathname.startsWith(COMPILER_PREFIX)) {
    headers['Cross-Origin-Embedder-Policy'] = 'credentialless';
  }
  response.writeHead(200, headers);
  response.end(request.method === 'HEAD' ? undefined : body);
});

server.listen(port, '127.0.0.1', () => {
  console.log(`serving ${dist} on http://127.0.0.1:${port}`);
});
