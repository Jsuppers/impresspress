import type { Page } from '@playwright/test';

/**
 * Getting a page onto the browser development sandbox.
 *
 * Both sandbox specs (`dev-foundations.spec.ts`, `dev-workspace.spec.ts`) run
 * against the same bundle — `examples/dev-sandbox/build.sh`'s `dist/`, served
 * by a plain static host — and both have to get past the same two-stage boot
 * before they can assert anything. That wait is subtle enough (three separate
 * conditions, each for its own reason) that a second copy of it would be a
 * second thing to get wrong.
 */

/**
 * A stable phrase from the real welcome page
 * (`examples/dev-sandbox/seed/site/index.html`) — the "Open workspace" link
 * text, present on every render regardless of copy edits elsewhere on the
 * page.
 */
export const WELCOME_PHRASE = 'Open workspace';

/**
 * That page's heading. Used where the assertion has to be about what
 * RENDERED rather than what the document contains — an `<h1>` inside the
 * `/b/dev` preview iframe, say, which only has text if the frame really
 * loaded the site.
 */
export const WELCOME_HEADING = 'ImpressPress dev sandbox';

/**
 * Load `/` and wait until the service worker is serving it.
 *
 * The first load of an origin gets the static boot shell: `loader.js`
 * registers `sw.js`, the worker claims the page and the loader reloads, and
 * only the *second* document comes from the wasm runtime. The worker
 * initialises lazily on its first fetch, so by the time it controls a page it
 * has already run `initialize()` — wasm boot, migrations, seed import and
 * boot convergence included.
 *
 * Deliberately a *neutral* signal, not "the page renders X": what `/` serves
 * is itself under test in `dev-foundations.spec.ts`, and a boot helper that
 * asserted it would abort every test on one defect instead of reporting each
 * on its own line.
 *
 * `waitUntil: 'commit'` for the same reason `smoke.spec.ts` uses it — the boot
 * shell pulls `/webllm-engine.js` and `/embed-engine.js` as modules, and
 * neither `load` nor `domcontentloaded` fires promptly behind them.
 */
export async function bootServiceWorker(page: Page) {
  await page.goto('/', { waitUntil: 'commit' });
  await page.waitForFunction(() => navigator.serviceWorker.controller !== null, null, {
    timeout: 120_000,
  });
  // `controller !== null` says a worker claimed the page; it does not say the
  // worker has finished `initialize()`, because `sw.js` initialises lazily on
  // its first fetch. One round trip through a route only the runtime can
  // answer (the static host would 404 it) is what makes the wait mean "the
  // sandbox is up" — and it is what the timings the specs print measure.
  await page.waitForFunction(async () => (await fetch('/b/auth/login')).status === 200, null, {
    timeout: 120_000,
  });
  // …and wait for the boot shell to be replaced. `loader.js` reloads the page
  // once the worker controls it, on a `setTimeout(…, 0)` this function cannot
  // see; navigating on top of that pending reload is the
  // `net::ERR_ABORTED at /` a caller would otherwise hit. `#status` is the
  // shell's own progress line (`index.html.tmpl`) and exists on no page the
  // runtime serves, so its absence means the reload has landed.
  await page.waitForFunction(() => document.getElementById('status') === null, null, {
    timeout: 120_000,
  });
}
