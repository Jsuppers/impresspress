import { test, expect } from '@playwright/test';
import { MODEL_CONTEXT_POLYFILL } from './fixtures/model-context-polyfill';

/**
 * Lightweight smoke test that doesn't rebuild mid-test. Catches regressions
 * like the `/manifest.json` bypass bug and the `/sql-wasm-esm.js` import
 * path bug (both of which silently prevented SW registration).
 */
test('service worker registers and controls the page', async ({ page }) => {
  // `commit` is the right waitUntil here: this test exercises SW registration,
  // which `loader.js` triggers as soon as it parses (registration is async on
  // top of that). The downstream `waitForFunction(() => navigator.serviceWorker
  // .controller)` provides the actual assertion timing. Default `load` blocks
  // on every subresource; even `domcontentloaded` is delayed by deferred and
  // module scripts. Neither fires reliably here because the loader page imports
  // `/webllm-engine.js` and `/embed-engine.js` (type="module"), and a slow
  // jsdelivr CDN response for either one used to push the goto past the 60s
  // test timeout. Lazy-loading the WebLLM ESM (see webllm-engine.js) removed
  // most of the slowness, but `commit` is still the semantically correct
  // waitUntil for an SW-registration smoke and survives future regressions.
  await page.goto('/', { waitUntil: 'commit' });
  // Read the controller scriptURL inside the waitForFunction predicate so the
  // value is captured atomically. impresspress-web's loader.js redirects to
  // `boot_redirect` as soon as the SW takes control, which would otherwise
  // destroy the execution context between a separate `waitForFunction` +
  // `evaluate` pair.
  const handle = await page.waitForFunction(
    () => navigator.serviceWorker.controller?.scriptURL ?? null,
    null,
    { timeout: 20_000 },
  );
  const controllerURL = (await handle.jsonValue()) as string | null;
  expect(controllerURL).toMatch(/\/sw\.js$/);
});

test('boot redirect lands on the auth login page', async ({ page }) => {
  // boot_redirect is "/" (intercepted by SW → wasm router → 302 →
  // /b/auth/login for anonymous visitors). loader.js sets
  // `window.location.href = boot_redirect` once the SW takes control;
  // waiting for the resulting URL match avoids the
  // `net::ERR_ABORTED; maybe frame was detached?` race that an explicit
  // second goto would hit.
  //
  // Asserting on the rendered Sign In form rather than a non-empty body
  // catches the regression where boot_redirect pointed at /b/system/ —
  // an unclaimed path that returned a non-empty 404 page and silently
  // passed the smoke.
  await page.goto('/', { waitUntil: 'commit' });
  await page.waitForURL(/\/b\/auth\/login/, { timeout: 30_000 });
  await expect(page.locator('input#email')).toBeVisible();
  await expect(page.locator('input#password')).toBeVisible();
});

test('admin can log in and reach the dashboard', async ({ page }) => {
  // Regression guard for the browser-only JWT bug: the pipeline verified
  // access tokens against a secret snapshotted at build time. In the browser
  // target `WAFER_RUN__AUTH__JWT_SECRET` is auto-generated AFTER the runtime
  // is built, so that snapshot was the empty string while login signed with
  // the real seeded secret. Login returned a token, but every authenticated
  // request then 403'd and the user was bounced back to /b/auth/login.
  //
  // This is the only browser-WASM test that exercises a *protected* route
  // post-login — the anonymous boot smoke above can't catch a verify bug.
  await page.goto('/', { waitUntil: 'commit' });
  await page.waitForURL(/\/b\/auth\/login/, { timeout: 30_000 });

  await page.locator('input#email').fill('admin@example.com');
  await page.locator('input#password').fill('admin123');
  await page.getByRole('button', { name: /sign in/i }).click();

  // A usable session lands on the admin dashboard; the regression instead
  // redirected back to /b/auth/login?redirect=%2Fb%2Fadmin%2F.
  await page.waitForURL(/\/b\/admin\//, { timeout: 30_000 });
  await expect(page).toHaveURL(/\/b\/admin\//);
});

test('the default bundle has no dev block', async ({ page }) => {
  // The sandbox's security model (design §13) is that `impresspress/dev` is
  // ABSENT from a normal deployment, not merely disabled in one. `pkg/` is
  // built without `browser-devtools` and without `[dev] enabled`, so both
  // halves of that must hold: no route, and no relaxed header for the
  // compiler worker / preview iframe the sandbox would need.
  //
  // Wait for the SW-served boot redirect to land rather than for
  // `navigator.serviceWorker.controller`: loader.js navigates to
  // `boot_redirect` the moment the SW takes control, which destroys the
  // execution context out from under an `evaluate()` that raced it. Landing
  // on /b/auth/login means the wasm router has already answered a request,
  // which is the precondition both fetches below need anyway.
  await page.goto('/', { waitUntil: 'commit' });
  await page.waitForURL(/\/b\/auth\/login/, { timeout: 30_000 });

  const status = await page.evaluate(async () => (await fetch('/b/dev/api/status')).status);
  expect(status).toBe(404);

  const csp = await page.evaluate(
    async () => (await fetch('/b/auth/login')).headers.get('content-security-policy'),
  );
  expect(csp).not.toBeNull();
  // `worker-src` exists ONLY when the sandbox is active — nothing else in the
  // bundle spawns a worker.
  expect(csp).not.toContain('worker-src');
  // The sandbox previews the live site in a same-origin iframe and relaxes
  // this to `'self'` (`runtime_factory.rs`); a normal deployment refuses
  // framing outright.
  expect(csp).toContain("frame-ancestors 'none'");
  // Deliberately NOT `expect(csp).not.toContain('frame-src')`, which is what
  // this line used to say. The products block declares
  // `frame-src https://js.stripe.com https://hooks.stripe.com
  // https://checkout.stripe.com` for Stripe Checkout, so a feature-OFF bundle
  // has a `frame-src` too — and this assertion failed the first time it was
  // run against a real built `pkg/` (Plan 1 Task 10's fix round).
  //
  // Both lines above really do discriminate: `dev-foundations.spec.ts`
  // asserts the opposite of each on a `browser-devtools` + `[dev] enabled`
  // bundle. They did not until `flows::register_site_main` stopped replacing
  // the factory's whole `wafer-run/security-headers` config with the shared
  // CSP directives — until then both bundles served the same policy and this
  // test passed for the wrong reason.
});

test('a cold visitor gets WebMCP tools without a reload', async ({ page }) => {
  // The race this guards: `webmcp.js`'s deferred script runs while
  // `navigator.serviceWorker.controller` is still null (the SW hasn't taken
  // control of THIS document yet) — a cold visit to a production host with
  // SPA-fallback routing lands here (see
  // `docs/2026-08-28-browser-demo-design-note.md` §3). It is not
  // reproducible by simply `goto`-ing a deep link on this harness:
  // `python3 -m http.server -d pkg` (what this spec and CI's e2e-smoke job
  // both serve `pkg/` with) has no SPA fallback, so a direct `goto` at
  // `/b/auth/login` 404s before any script runs. And reaching the real page
  // the only way this server allows — through `/`, like every other test
  // above — proves nothing either: `impresspress-bundle`'s `loader.js`
  // already reloads the tab once on first registration, and by the time
  // that reload's navigation lands on `/b/auth/login` the SW is already
  // active and controlling, so `controller` is non-null before `webmcp.js`
  // ever runs (verified empirically — this harness's redirect chain closes
  // the race on its own, fix or no fix).
  //
  // So: reach the real page normally, then shadow `navigator.serviceWorker`
  // on it ONLY (the loader shell at `/` is untouched, so registration still
  // happens for real) to force exactly the state the fix handles —
  // `controller` reads null and `.ready` is a promise this test holds open
  // — and prove `webmcp.js` waits for it instead of registering nothing.
  await page.addInitScript(MODEL_CONTEXT_POLYFILL);
  await page.addInitScript(() => {
    if (location.pathname !== '/b/auth/login') return;
    const real = navigator.serviceWorker;
    let release = () => {};
    const heldReady = new Promise((resolve) => {
      release = () => resolve(real.controller);
    });
    (window as unknown as { __releaseWebmcpReady: () => void }).__releaseWebmcpReady = release;
    Object.defineProperty(navigator, 'serviceWorker', {
      configurable: true,
      get() {
        return {
          get controller() { return null; },
          get ready() { return heldReady; },
          getRegistration: real.getRegistration.bind(real),
        };
      },
    });
  });
  await page.goto('/', { waitUntil: 'commit' });
  await page.waitForURL(/\/b\/auth\/login/, { timeout: 30_000 });

  // While `.ready` is held open, webmcp.js must not have registered
  // anything — this is the assertion that fails (immediately, not a
  // timeout) against the pre-fix script, which fetches the manifest
  // unconditionally and ignores `navigator.serviceWorker` entirely.
  await page.waitForTimeout(500);
  expect(await page.evaluate(() => document.modelContext.__tools().length)).toBe(0);

  // Releasing `.ready` (the SW finishing activation) lets it proceed —
  // without a reload, and without the test navigating again.
  await page.evaluate(() => (window as unknown as { __releaseWebmcpReady: () => void }).__releaseWebmcpReady());
  const names = await page.waitForFunction(() => {
    const t = document.modelContext.__tools();
    return t.length > 0 ? t.map((x) => x.name) : null;
  }, null, { timeout: 10_000 });
  expect(await names.jsonValue()).toContain('list_products');
});
