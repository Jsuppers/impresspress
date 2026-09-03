// Shared harness for `dev.js` unit tests (`node --test
// crates/impresspress-core/src/blocks/dev/assets/test/*.test.mjs`).
//
// `dev.js` is authored as a TAIL with nothing on `window` by design (see the
// file's own header comment) — there is no export to import here the way
// `impresspress-browser/js/test` imports `bridge.js`'s real functions. So
// this harness does the same thing `blocks/dev/assets.rs::dev_js()` does —
// read the tail's source and wrap it in an IIFE — except this wrapper is
// test-only and closes over a returned handle instead of discarding the
// closure, which is how the tail's internal state and functions are reached
// without adding a single test hook to the shipped file.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const tail = fs.readFileSync(path.join(here, '..', 'dev.js'), 'utf8');

// Builds one fresh, isolated instance of the tail's closure. Each instance
// gets its own stub `document`/`window`/`fetch` so tests can't leak state
// (in particular `outstanding`/`polling`) into one another.
/**
 * @param {object} [options]
 * @param {boolean} [options.hasModelContext]  give the stub document a WebMCP
 *   registrar, as a browser that supports it would.
 * @param {object|null} [options.compilerManifest]  what
 *   `/__impresspress_dev/compiler/manifest.json` answers with: an object for a
 *   bundle that shipped the browser toolchain, `null` for one that did not
 *   (the host 404s, which is a normal build — see `discoverCompiler`).
 */
export function instantiate({ hasModelContext = false, compilerManifest = null } = {}) {
  const fetchCalls = [];
  // Every `window.addEventListener` the tail makes, so a test can fire the
  // real handler — `pagehide`'s `event.persisted` branch is a decision the
  // handler owns, and there is no other way to reach it.
  const windowListeners = [];
  const fakeElement = () => ({
    value: '',
    textContent: '',
    innerHTML: '',
    disabled: false,
    title: '',
    scrollTop: 0,
    scrollHeight: 0,
    setAttribute() {},
    getAttribute() {
      return null;
    },
    appendChild() {},
    addEventListener() {}
  });

  // The attributes `blocks/dev/page.rs` actually ships on an element, for the
  // ids where the INITIAL state is load-bearing rather than incidental.
  // `#dev-compile` is `disabled` in the markup and only the compiler manifest
  // may clear it, so a stub that started it enabled would let a
  // `discoverCompiler` that did nothing at all pass.
  const MARKUP = {
    'dev-compile': { disabled: true },
    'dev-export': { disabled: true }
  };

  // Memoised by id, unlike `createElement`: the tail looks an element up once
  // and keeps it, so a stub that handed out a fresh object per call would let
  // a test read an element the code under test never touched. The map is
  // returned so a test can assert on what the tail did to the document.
  const elements = new Map();
  const elementById = (id) => {
    if (!elements.has(id)) {
      elements.set(id, Object.assign(fakeElement(), MARKUP[id]));
    }
    return elements.get(id);
  };

  const sandbox = {
    document: {
      getElementById: elementById,
      createElement: fakeElement,
      addEventListener() {},
      ...(hasModelContext
        ? {
            modelContext: {
              registerTool() {},
              unregisterTool() {}
            }
          }
        : {})
    },
    window: {
      addEventListener(type, listener) {
        windowListeners.push({ type, listener });
      }
    },
    fetch(...args) {
      fetchCalls.push(args);
      // The tail makes two kinds of request on load and they cannot share one
      // canned answer, so the stub routes by URL — the same split the real
      // page has: `api.*` calls go to the block's JSON API and read
      // `response.text()`, while the compiler manifest is a STATIC file read
      // with `response.json()` whose absence (404) is a normal build.
      const url = String(args[0]);
      if (url === '/__impresspress_dev/compiler/manifest.json') {
        if (compilerManifest === null) {
          return Promise.resolve({ ok: false, status: 404 });
        }
        return Promise.resolve({
          ok: true,
          status: 200,
          json: async () => compilerManifest
        });
      }
      // `loadFiles()` (bottom of the tail) awaits this on load; an empty
      // file list is enough for every test using this harness, none of
      // which assert on the file pane.
      return Promise.resolve({ ok: true, status: 200, text: async () => '{"files":[]}' });
    },
    AbortController,
    console,
    Promise,
    setInterval,
    clearInterval,
    Date,
    Array,
    Math,
    Error,
    JSON
  };

  // `vm` would be the conventional tool here, but the tail's top level
  // already runs long enough (`loadFiles().catch(...)`, event listener
  // registration) that a plain `Function` constructor closing over the
  // stub globals above is simpler than wiring a full `vm.Context`, and
  // nothing in the tail needs real module semantics.
  const factory = new Function(
    ...Object.keys(sandbox),
    `${tail}
return {
  withProgress,
  get outstanding() { return outstanding },
  get isPolling() { return polling !== null },
  abort,
  unregisterPageTools,
  get registered() { return registered.slice() },
  isFileConflict,
  refusalBody,
  refusalMessage,
  describeCompiler,
  get compilerManifest() { return compilerManifest }
};`
  );
  const handle = factory(...Object.values(sandbox));
  // Fires every `window` listener the tail registered for `type`, with
  // `event` as the argument — the shape a browser would deliver.
  const fireWindow = (type, event) => {
    windowListeners
      .filter((l) => l.type === type)
      .forEach((l) => l.listener(event));
  };
  return { handle, fetchCalls, fireWindow, elements };
}
