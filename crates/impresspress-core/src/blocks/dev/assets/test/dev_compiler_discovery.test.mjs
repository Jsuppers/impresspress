// Run with: node --test crates/impresspress-core/src/blocks/dev/assets/test/dev_compiler_discovery.test.mjs
//
// `discoverCompiler` — the branch that decides whether `/b/dev` can compile
// anything.
//
// Whether a bundle carries the browser Rust toolchain is a property of the
// BUNDLE, not of this crate: `examples/dev-sandbox/impresspress.toml` overlays
// `compiler/dist/` onto `/__impresspress_dev/compiler/`, and a build without
// that overlay is a legitimate build (CI's foundations job serves one). So the
// page ships a disabled Compile button and asks the host at load time, and
// BOTH answers are behaviour: a manifest enables the button and names the
// toolchain, and a 404 leaves it disabled with a reason on it.
//
// The e2e (`dev-workspace.spec.ts`) can only ever drive one of those, because
// `build.sh` refuses to produce a bundle without the compiler. This is where
// the other one is covered.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { instantiate } from './harness.mjs';

/** The real manifest's shape, trimmed to the fields the page reads. */
const MANIFEST = {
  schema_version: 1,
  version: '807ace9e',
  entry: '/__impresspress_dev/compiler/807ace9e/worker.js',
  total_bytes: 75124831,
  target: 'wasm32-wasip1'
};

/**
 * One macrotask drains every microtask the harness's already-resolved
 * promises queued, so the tail's load-time work has finished by the time this
 * returns. No test hook in the shipped file, which is the point of the
 * harness.
 */
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

test('a manifest enables the Compile button and names the toolchain', async () => {
  const { handle, elements, fetchCalls } = instantiate({ compilerManifest: MANIFEST });
  await settle();

  // Fetched once, from the path the bundle overlays the compiler to, and
  // never from a cache — the manifest is the only file in that tree whose URL
  // does not carry the version.
  const [url, init] = fetchCalls.find(
    ([u]) => String(u) === '/__impresspress_dev/compiler/manifest.json'
  );
  assert.equal(url, '/__impresspress_dev/compiler/manifest.json');
  assert.deepEqual(init, { cache: 'no-store' });

  assert.equal(elements.get('dev-compile').disabled, false);
  // The whole manifest is kept, not just its version: `manifest.entry` is the
  // only path that names the pinned worker, and the button will hand this
  // object to `new BrowserRustCompiler(...)`.
  assert.deepEqual(handle.compilerManifest, MANIFEST);
  // 75124831 / 1048576 = 71.64… — MiB, matching every other figure published
  // about this toolchain.
  assert.equal(
    elements.get('dev-compiler-version').textContent,
    'Compiler v807ace9e · 71.6 MiB'
  );
});

test('a 404 leaves the button disabled with a reason on it', async () => {
  const { handle, elements } = instantiate({ compilerManifest: null });
  await settle();

  // Still disabled, exactly as `page.rs` shipped it.
  assert.equal(elements.get('dev-compile').disabled, true);
  assert.equal(elements.get('dev-compile').title, 'No compiler in this build');
  assert.equal(handle.compilerManifest, null);
  assert.equal(elements.get('dev-compiler-version').textContent, '');
});

test('the version line is the manifest, formatted — not a hardcoded string', () => {
  const { handle } = instantiate();
  // A second, differently-sized toolchain: a `describeCompiler` that ignored
  // its argument would pass the test above and fail here.
  assert.equal(
    handle.describeCompiler({ version: 'deadbeef', total_bytes: 1048576 }),
    'Compiler vdeadbeef · 1.0 MiB'
  );
});
