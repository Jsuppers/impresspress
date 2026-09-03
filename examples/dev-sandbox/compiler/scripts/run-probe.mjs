/**
 * Run `src/probe.html` headlessly and print what it measured.
 *
 * Starts `serve-probe.mjs`, opens the page in Playwright's chromium, waits for
 * `window.__PROBE__.done`, prints the JSON and exits non-zero if any step
 * failed. A compile of the `hello` template takes minutes, so the timeout is
 * generous and the console is mirrored — a run that is going nowhere says so
 * rather than sitting silent until it is killed.
 *
 * Playwright is not a dependency of this package: it comes from
 * `crates/impresspress-web`, which is where the repo's browser tests live.
 * Point `PLAYWRIGHT_MODULE_DIR` at any directory whose `node_modules` has it
 * if that tree has not been installed.
 *
 * Usage:
 *   node scripts/run-probe.mjs [port]
 */

import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const repo = path.resolve(here, "../../..");
const port = Number(process.argv[2] ?? process.env.PROBE_PORT ?? 8099);
const TIMEOUT_MS = Number(process.env.PROBE_TIMEOUT_MS ?? 20 * 60 * 1000);

const playwrightFrom = process.env.PLAYWRIGHT_MODULE_DIR ?? path.join(repo, "crates/impresspress-web");
let chromium;
try {
  // `require`, not `import`: playwright is CommonJS and node does not always
  // detect its named exports through a dynamic import.
  const require = createRequire(pathToFileURL(path.join(playwrightFrom, "package.json")));
  ({ chromium } = require(require.resolve("playwright")));
} catch (error) {
  console.error(
    `run-probe.mjs: playwright is not installed under ${playwrightFrom} ` +
      "(set PLAYWRIGHT_MODULE_DIR to a tree that has it)\n" +
      `  ${error?.message ?? error}`,
  );
  process.exit(1);
}

const server = spawn(process.execPath, [path.join(here, "scripts/serve-probe.mjs"), String(port)], {
  stdio: ["ignore", "pipe", "inherit"],
});
await new Promise((resolve, reject) => {
  server.stdout.once("data", resolve);
  server.once("exit", (code) => reject(new Error(`serve-probe.mjs exited with ${code}`)));
});

const browser = await chromium.launch();
let failed = false;
try {
  const page = await browser.newPage();
  page.on("console", (message) => console.log(`  [page] ${message.text()}`));
  page.on("pageerror", (error) => console.log(`  [page error] ${error.message}`));
  await page.goto(`http://localhost:${port}/`, { waitUntil: "domcontentloaded" });
  await page.waitForFunction(() => window.__PROBE__?.done === true, null, { timeout: TIMEOUT_MS });

  const results = await page.evaluate(() => window.__PROBE__);
  console.log(JSON.stringify(results, null, 2));
  failed = results.steps.some((step) => !step.ok) || Boolean(results.error);
  for (const step of results.steps) {
    console.log(`${step.ok ? "PASS" : "FAIL"}  ${step.text}`);
  }
} catch (error) {
  console.error(`run-probe.mjs: ${error?.stack ?? error}`);
  failed = true;
} finally {
  await browser.close();
  server.kill();
}

process.exit(failed ? 1 : 0);
