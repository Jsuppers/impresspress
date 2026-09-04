import { defineConfig, devices } from '@playwright/test';

const PORT = process.env.TEST_PORT ? parseInt(process.env.TEST_PORT) : 8080;

// Base config used by smoke.spec.ts (port 8080, browser-WASM static server,
// no admin login API). Visual-baseline tests use a separate config that
// extends this with `globalSetup` — see `playwright.visual-baseline.config.ts`.
export default defineConfig({
  testDir: './e2e',
  // Snapshot baselines live in Playwright's default location next to the
  // specs (`e2e/<spec>-snapshots/`). They must NOT be pointed at
  // `.playwright-mcp/` — that is the Playwright MCP server's scratch dir
  // (page snapshots, console logs) and is gitignored, which silently made
  // the committed baselines unstageable.
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: 0,
  workers: 1,
  // Paths below are resolved relative to THIS config file's directory
  // (`tests/`), not the working directory. CI uploads the report and the
  // actual/diff PNGs from the crate root, so both are lifted one level out
  // of `tests/`. Without this the html report was never written at all and
  // every `Upload Playwright report on failure` step in ci.yml uploaded
  // nothing, leaving screenshot failures with no diff images to inspect.
  reporter: [['list'], ['html', { open: 'never', outputFolder: '../playwright-report' }]],
  outputDir: '../test-results',
  timeout: 60_000,
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    serviceWorkers: 'allow',
  },
  projects: [
    { name: 'desktop-chrome', use: { ...devices['Desktop Chrome'] } },
  ],
});
