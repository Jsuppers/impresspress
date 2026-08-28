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
//   npx playwright test --config=tests/playwright.live.config.ts \
//     tests/e2e/webmcp.spec.ts --grep-invert "seeded product|inspector"
//
// The grep-invert matters: the "seeded product" describe creates a product
// through the admin API on every run (fine on a throwaway local database,
// not on a live catalog), and the inspector block is not part of a Worker
// build. Same globalSetup admin login as the visual-baseline config.
export default defineConfig({
  ...baseConfig,
  use: {
    ...baseConfig.use,
    baseURL: process.env.LIVE_BASE_URL,
  },
  globalSetup: join(HERE, 'e2e/fixtures/global-setup.ts'),
});
