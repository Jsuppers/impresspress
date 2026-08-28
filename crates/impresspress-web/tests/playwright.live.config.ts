import { defineConfig } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import baseConfig from './playwright.config.ts';

const HERE = dirname(fileURLToPath(import.meta.url));

// Run the WebMCP spec against a DEPLOYED impresspress instead of a local
// server:
//
//   LIVE_BASE_URL=https://<host> \
//   WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL=<admin> \
//   WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_PASSWORD=<password> \
//   npx playwright test --config=tests/playwright.live.config.ts
//
// No positional filter and no --grep-invert: pointing a test runner at a
// deployment is not the place for safety that depends on the operator
// re-typing a command out of a comment. Everything below is enforced by the
// config.
//
// What is excluded, and why:
//   * every spec except webmcp.spec.ts — the rest (products-lifecycle,
//     products-catalog-admin, products-wizard, vector, visual-baseline)
//     create, mutate and delete real rows.
//   * the "seeded product" describe — it creates a product through the admin
//     API on every run, which is fine on a throwaway local database and not
//     on a live catalog.
//   * the storage seed in globalSetup, which would otherwise create a
//     `photos` bucket and upload two objects into the deployment.
//
// The inspector describe is NOT excluded: `wafer-block-inspector` is an
// unconditional dependency and is registered for wasm32 too, so
// `/b/inspector/webmcp` is served by a Worker build. A deployment is exactly
// where confirming declared-vs-served auth is worth doing.
const liveBaseURL = process.env.LIVE_BASE_URL;
if (!liveBaseURL) {
  throw new Error(
    'playwright.live.config.ts: LIVE_BASE_URL is required. Without it the ' +
      'admin login silently falls back to http://127.0.0.1:8080 and the ' +
      'default bootstrap credentials are sent to whatever is listening there.',
  );
}

process.env.IMPRESSPRESS_E2E_NO_SEED = '1';

export default defineConfig({
  ...baseConfig,
  testMatch: /webmcp\.spec\.ts$/,
  grepInvert: /seeded product/,
  use: {
    ...baseConfig.use,
    baseURL: liveBaseURL,
  },
  globalSetup: join(HERE, 'e2e/fixtures/global-setup.ts'),
});
