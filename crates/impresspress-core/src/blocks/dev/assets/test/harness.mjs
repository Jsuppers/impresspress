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
export function instantiate({ hasModelContext = false } = {}) {
  const fetchCalls = [];
  const fakeElement = () => ({
    value: '',
    textContent: '',
    innerHTML: '',
    disabled: false,
    scrollTop: 0,
    scrollHeight: 0,
    setAttribute() {},
    getAttribute() {
      return null;
    },
    appendChild() {},
    addEventListener() {}
  });

  const sandbox = {
    document: {
      getElementById: fakeElement,
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
    window: { addEventListener() {} },
    fetch(...args) {
      fetchCalls.push(args);
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
  isFileConflict
};`
  );
  const handle = factory(...Object.values(sandbox));
  return { handle, fetchCalls };
}
