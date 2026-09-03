import { test, expect } from '@playwright/test';
import { createHash } from 'node:crypto';
import { copyFileSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { loginToWorkspace } from './fixtures/dev-helpers';

/**
 * `BrowserRustCompiler` — the page half of the compiler protocol.
 *
 * The class under test is
 * `crates/impresspress-core/src/blocks/dev/assets/compiler-adapter.js`, served
 * at `/b/dev/static/compiler-adapter.js`. It only exists inside a document: it
 * creates a `Worker`, moves an `ArrayBuffer` across a `postMessage` boundary
 * and digests it with `crypto.subtle`, none of which a host test can reach. So
 * this file is the only place it runs.
 *
 * # Why the worker is a fake
 *
 * The real one (`examples/dev-sandbox/compiler/`) is a 365 MiB composed
 * toolchain: seven seconds to `ready`, thirty-eight to compile the `hello`
 * template. Everything asserted here — the queue, the cancel, the four
 * protocol violations — behaves the same against real rustc, and a spec
 * paying forty seconds a case could not afford to cover them.
 * `fixtures/fake-compiler-worker.js` speaks `compiler/src/protocol.ts`
 * exactly, including the two refusals that make the adapter's queue mean
 * something (a second `compile` while one is in flight, and anything after a
 * `cancel`). Plan 3 Task 6 drives the real compiler end to end.
 *
 * # How the fake gets served
 *
 * The manifest the page reads names the worker's URL, so pointing the page at
 * the fake is a matter of writing a second manifest beside the real one:
 * `/__impresspress_dev/compiler/test/manifest.json`, whose `entry` is
 * `/__impresspress_dev/compiler/test/worker.js`. Both files are written into
 * the *served* `dist/` below rather than committed — `examples/dev-sandbox/
 * dist/` is build output, and a fixture inside the deployed compiler tree
 * would ship to dev.impresspress.org. The real `manifest.json` is never
 * touched: the deployed tree stays exactly what `build.sh` produced, plus one
 * directory this file creates and removes again.
 */

/**
 * The bundle being served on `TEST_PORT`.
 *
 * CI exports `DEV_DIST` from `build.sh`'s last line; a local run defaults to
 * the same path that script writes to.
 */
const DEV_DIST =
  process.env.DEV_DIST ??
  fileURLToPath(new URL('../../../../examples/dev-sandbox/dist', import.meta.url));

/** Where the test-only manifest and worker go, and the URLs they get. */
const TEST_COMPILER_DIR = path.join(DEV_DIST, '__impresspress_dev', 'compiler', 'test');
const TEST_MANIFEST_URL = '/__impresspress_dev/compiler/test/manifest.json';
const TEST_WORKER_URL = '/__impresspress_dev/compiler/test/worker.js';

const FAKE_WORKER = fileURLToPath(new URL('./fixtures/fake-compiler-worker.js', import.meta.url));

/** The sixteen bytes `fake-compiler-worker.js` hands back, and their digest. */
const ARTIFACT = Buffer.from(Array.from({ length: 16 }, (_, i) => i));
const ARTIFACT_SHA256 = createHash('sha256').update(ARTIFACT).digest('hex');

/** `validation::MAX_ARTIFACT_BYTES` — the ceiling both halves enforce. */
const MAX_ARTIFACT_BYTES = 4194304;

/** What `fake-compiler-worker.js` answers `rustc --version` with. */
const FAKE_RUSTC_VERSION = 'rustc 1.90.0-nightly (fake worker)';

test.beforeAll(() => {
  mkdirSync(TEST_COMPILER_DIR, { recursive: true });
  copyFileSync(FAKE_WORKER, path.join(TEST_COMPILER_DIR, 'worker.js'));
  // The same shape `scripts/write-manifest.mjs` writes, minus the asset
  // inventory nothing on the page reads: the adapter takes `entry`, `version`
  // and `target`, and refuses a manifest missing any of them.
  writeFileSync(
    path.join(TEST_COMPILER_DIR, 'manifest.json'),
    `${JSON.stringify(
      {
        schema_version: 1,
        version: 'test',
        entry: TEST_WORKER_URL,
        total_bytes: 0,
        assets: [],
        license: 'MIT OR Apache-2.0',
        target: 'wasm32-wasip1',
      },
      null,
      2,
    )}\n`,
  );
});

test.afterAll(() => {
  // The static server serves whatever is on disk, so the fixture must not
  // outlive the run that needed it — least of all in a tree a deploy uploads.
  rmSync(TEST_COMPILER_DIR, { recursive: true, force: true });
});

test('the compiler adapter reports progress, results and diagnostics', async ({ page }) => {
  // A cold boot of the sandbox (wasm compile, OPFS create, migrations, seed
  // import) is minutes on a cold cache; the adapter's own work is milliseconds.
  test.setTimeout(300_000);
  await loginToWorkspace(page);

  const result = await page.evaluate(async (manifestUrl) => {
    // Dynamic, and from the page — the module is the unit under test, so
    // loading it by the URL `dev.js` itself imports is the point. A
    // non-literal specifier keeps the transpiler from trying to resolve a
    // path that only exists at runtime.
    const modulePath = '/b/dev/static/compiler-adapter.js';
    const { BrowserRustCompiler } = await import(modulePath);
    const manifest = await (await fetch(manifestUrl)).json();
    const compiler = new BrowserRustCompiler(manifest);

    const stages: string[] = [];
    const version = compiler.version;
    await compiler.initialize((progress: any) => stages.push(progress.stage));
    const ok = await compiler.compile({
      crateName: 'hello',
      files: { 'src/lib.rs': '// ok' },
      onProgress: (progress: any) => stages.push(progress.stage),
    });
    const bad = await compiler.compile({
      crateName: 'hello',
      files: { 'src/lib.rs': '// FAIL' },
    });
    await compiler.dispose();
    // A `compile` after `dispose` is a caller error, not a compiler error, so
    // it must reject rather than resolve with a failed build.
    const afterDispose = await compiler
      .compile({ crateName: 'hello', files: { 'src/lib.rs': '// ok' } })
      .then(
        () => null,
        (error: Error) => error.message,
      );

    return {
      version,
      stages,
      ok: {
        success: ok.success,
        cancelled: ok.cancelled,
        bytes: Array.from(ok.artifact as Uint8Array),
        sha: ok.artifactSha256,
        stdout: ok.stdout,
        compilerVersion: ok.compilerVersion,
        buildId: ok.buildId,
      },
      bad: {
        success: bad.success,
        artifact: bad.artifact,
        sha: bad.artifactSha256,
        diagnostics: bad.diagnostics,
      },
      afterDispose,
    };
  }, TEST_MANIFEST_URL);

  // The manifest's version is the pinned bundle's, straight through.
  expect(result.version).toBe('test');
  // `initialize` reports the toolchain coming up and `compile` reports the
  // build, each to the callback its own caller passed, in order.
  expect(result.stages).toEqual(['download', 'initializing', 'compiling']);

  expect(result.ok.success).toBe(true);
  expect(result.ok.cancelled).toBe(false);
  // The artifact is the transferred buffer, viewed — not a copy, and not the
  // bytes of some earlier build.
  expect(result.ok.bytes).toEqual(Array.from(ARTIFACT));
  expect(result.ok.sha).toMatch(/^[0-9a-f]{64}$/);
  // …and the digest is over those bytes, which a `sha` that merely looked
  // like hex would not prove.
  expect(result.ok.sha).toBe(ARTIFACT_SHA256);
  expect(result.ok.stdout).toContain('hello');
  // What `POST /b/dev/api/builds/stage` will record as `compiler_version`:
  // the string the worker reported at `ready`.
  expect(result.ok.compilerVersion).toBe(FAKE_RUSTC_VERSION);
  expect(result.ok.buildId).toMatch(/^build-\d+$/);

  // A crate that does not compile is an ANSWER. `compile` resolves with it.
  expect(result.bad.success).toBe(false);
  expect(result.bad.artifact).toBeNull();
  expect(result.bad.sha).toBeNull();
  expect(result.bad.diagnostics[0]).toMatchObject({
    file: 'src/lib.rs',
    line: 1,
    column: 1,
    severity: 'error',
    code: 'E0425',
  });

  expect(result.afterDispose).toContain('disposed');
});

test('the compiler adapter queues compiles, cancels one, and recovers from a broken worker', async ({
  page,
}) => {
  test.setTimeout(300_000);
  await loginToWorkspace(page);

  const result = await page.evaluate(async (manifestUrl) => {
    const modulePath = '/b/dev/static/compiler-adapter.js';
    const { BrowserRustCompiler } = await import(modulePath);
    const manifest = await (await fetch(manifestUrl)).json();
    const compiler = new BrowserRustCompiler(manifest);

    const settle = (promise: Promise<any>): Promise<any> =>
      promise.then(
        (value: any) => ({ ok: true, value }),
        (error: Error) => ({ ok: false, message: error.message }),
      );

    await compiler.initialize();

    // --- the queue --------------------------------------------------------
    // Both calls are made before either resolves. The worker refuses a second
    // `compile` while one is in flight (`protocol.ts`: "a queue would only
    // hide a bug in the page"), so if the adapter did not serialise them, the
    // second would come back failed with that refusal as its stderr instead
    // of a build.
    const first = compiler.compile({ crateName: 'queued-a', files: { 'src/lib.rs': '// ok' } });
    const second = compiler.compile({ crateName: 'queued-b', files: { 'src/lib.rs': '// ok' } });
    const queued = await Promise.all([first, second]);

    // --- cancel -----------------------------------------------------------
    // `slow` keeps the fake worker busy for five seconds, so there is
    // something in flight to cancel.
    const slow = compiler.compile({ crateName: 'slow', files: { 'src/lib.rs': '// ok' } });
    await new Promise((resolve) => setTimeout(resolve, 200));
    await compiler.cancel();
    const cancelled = await slow;
    // The worker is spent, so the adapter must have terminated it and be
    // willing to build another. Nothing here calls `initialize` again: the
    // point is that `compile` alone is enough.
    const afterCancel = await compiler.compile({
      crateName: 'after-cancel',
      files: { 'src/lib.rs': '// ok' },
    });

    // --- the four protocol violations -------------------------------------
    // Each must REJECT — the compiler said nothing about the crate, so there
    // is no result to resolve with — and leave the instance usable.
    const violations: Record<string, any> = {};
    for (const crateName of [
      'protocol-wrong-id',
      'protocol-not-a-buffer',
      'protocol-oversized',
      'protocol-error',
    ]) {
      const broke = await settle(compiler.compile({ crateName, files: { 'src/lib.rs': '// ok' } }));
      const next = await settle(
        compiler.compile({ crateName: 'recovered', files: { 'src/lib.rs': '// ok' } }),
      );
      violations[crateName] = {
        rejected: broke.ok === false,
        message: broke.message ?? null,
        recovered: next.ok === true && next.value.success === true,
      };
    }

    await compiler.dispose();
    return {
      queued: queued.map((r: any) => ({ success: r.success, stdout: r.stdout })),
      cancelled: {
        success: cancelled.success,
        cancelled: cancelled.cancelled,
        artifact: cancelled.artifact,
        stderr: cancelled.stderr,
        elapsedMs: cancelled.elapsedMs,
        compilerVersion: cancelled.compilerVersion,
      },
      afterCancel: { success: afterCancel.success, stdout: afterCancel.stdout },
      violations,
    };
  }, TEST_MANIFEST_URL);

  // Both compiles ran, in the order they were requested — the fake worker
  // numbers its builds, so the sequence is its own account of what happened.
  expect(result.queued.map((r) => r.success)).toEqual([true, true]);
  expect(result.queued[0].stdout).toBe('fake build #1: queued-a');
  expect(result.queued[1].stdout).toBe('fake build #2: queued-b');

  // The cancelled compile RESOLVES: the page asked for this.
  expect(result.cancelled.success).toBe(false);
  expect(result.cancelled.cancelled).toBe(true);
  expect(result.cancelled.artifact).toBeNull();
  // The WORKER's own account of the cancelled build, not one the adapter
  // made up: `cancel` names the compile's id, so the answer to it *is* that
  // compile's one terminal message. If the adapter had invented an id, the
  // worker would have answered `error: nothing in flight` instead and the
  // page would be reading a synthesized result here.
  expect(result.cancelled.stderr).toBe('cancelled');
  expect(result.cancelled.compilerVersion).toBe(FAKE_RUSTC_VERSION);
  // A fresh worker was built on demand: this is build #1 of the new one.
  expect(result.afterCancel.success).toBe(true);
  expect(result.afterCancel.stdout).toBe('fake build #1: after-cancel');

  const violations = result.violations as Record<
    string,
    { rejected: boolean; message: string | null; recovered: boolean }
  >;
  expect(violations['protocol-wrong-id'].message).toContain(
    'answered result for a request this page did not make',
  );
  expect(violations['protocol-not-a-buffer'].message).toContain('not an ArrayBuffer');
  expect(violations['protocol-oversized'].message).toContain(
    `over the sandbox limit of ${MAX_ARTIFACT_BYTES}`,
  );
  expect(violations['protocol-error'].message).toContain('the compiler image is corrupt');
  // Every one of them rejected, and every one left a compiler that still works.
  for (const crateName of Object.keys(violations)) {
    expect(violations[crateName].rejected, crateName).toBe(true);
    expect(violations[crateName].recovered, crateName).toBe(true);
  }
});
