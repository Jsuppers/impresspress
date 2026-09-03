import { expect, type Page } from '@playwright/test';

/**
 * Getting a page onto the browser development sandbox.
 *
 * Every sandbox spec (`dev-foundations.spec.ts`, `dev-workspace.spec.ts`,
 * `dev-compiler.spec.ts`) starts the same way — boot the service worker, sign
 * in as the seeded admin, and for most of them land on `/b/dev` — and none of
 * it is what any of them is testing. The boot wait alone is subtle enough
 * (three separate conditions, each for its own reason) that a second copy of
 * it would be a second thing to get wrong.
 *
 * These helpers are for the SANDBOX bundle (`examples/dev-sandbox/build.sh`,
 * served on `TEST_PORT` by a plain static host), not for the native server the
 * rest of the suite uses. That is why they log in through the form instead of
 * reusing `fixtures/auth.ts`'s saved `storageState`: `global-setup.ts` posts to
 * a server that is not running in this job, and the sandbox's credentials are
 * the seeded ones every fresh origin creates for itself.
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

/** The credentials `impresspress-core`'s auth block seeds into a new instance. */
export const ADMIN_EMAIL = 'admin@example.com';
export const ADMIN_PASSWORD = 'admin123';

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

/** Sign in as the seeded admin and land anywhere but the login page. */
export async function loginAdmin(page: Page) {
  await page.goto('/b/auth/login?redirect=/b/dev/api/status', { waitUntil: 'commit' });
  await page.locator('input#email').fill(ADMIN_EMAIL);
  await page.locator('input#password').fill(ADMIN_PASSWORD);
  await page.getByRole('button', { name: /sign in/i }).click();
  // The login script honours `?redirect=`, but falls back to the role-aware
  // `default_redirect` the API computed. Either lands a session; which one it
  // is says nothing about the sandbox, so this waits only for "off the login
  // page" and reads the API through `fetch` afterwards.
  await page.waitForURL((url) => !url.pathname.startsWith('/b/auth/login'), {
    timeout: 60_000,
  });
}

/**
 * Boot, sign in, and land on the workspace page with its script running.
 *
 * The last wait is what makes this more than a `goto`. `/b/dev/static/dev.js`
 * is an ES **module** — it imports `BrowserRustCompiler` from
 * `/b/dev/static/compiler-adapter.js` — and a module that fails to parse, or
 * whose import 404s, is dropped by the browser with nothing but a console
 * message: the page would still render, every pane would stay empty, and a
 * spec that only checked for `#dev-workspace` would pass. The progress ladder
 * is drawn by the script's own first status poll, so an element inside it is
 * proof the module loaded, resolved its import and ran.
 */
export async function loginToWorkspace(page: Page) {
  await bootServiceWorker(page);
  await loginAdmin(page);
  await page.goto('/b/dev', { waitUntil: 'commit' });
  await expect(page.locator('#dev-progress-steps li').first()).toBeAttached({ timeout: 60_000 });
}
