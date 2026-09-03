// Node module-customization hook used ONLY by `node --test` (see
// `storage_paths.test.mjs` in this directory and the `browser-wasm-test`
// CI step). bridge.js is a wasm-bindgen snippet module — it has a
// top-level `import initSqlJs from '/vendor/sql-wasm-esm.js'` that a
// browser/Service Worker resolves from the site root, but which plain
// Node can't resolve at all (no such path exists on disk). bridge.js's
// storage-key helpers (`splitKey`/`joinKey`/`validateSegments`/etc.) are
// exported from bridge.js itself now — not a separate pure module — so
// this hook exists to let `node --test` import bridge.js directly without
// dragging in sql.js or a real OPFS/Service Worker environment.
//
// Registered as the loader for the process via
// `node --import ./js/test/node-hooks.mjs --test ...`: on --import this
// file runs once in the main thread, where it self-registers (pointing
// `register()` at its own URL) so Node also loads it as the hooks module
// in the internal loader thread; `isMainThread` guards against
// re-registering when that second load happens. The `resolve`/`load` pair
// below then intercepts only the one specifier bridge.js can't resolve
// under Node and replaces it with an in-memory stub — every other
// specifier passes through to the default resolver/loader untouched.
import { register } from 'node:module';
import { isMainThread } from 'node:worker_threads';

const STUBBED_SPECIFIER = '/vendor/sql-wasm-esm.js';
const STUB_URL = 'node-hooks-stub:sql-wasm-esm';
const STUB_SOURCE = `export default async function initSqlJs() {
    throw new Error('sql.js stub (node-hooks.mjs): dbInit() is not exercised by node --test');
}
`;

if (isMainThread) {
    register(new URL(import.meta.url), import.meta.url);
}

export async function resolve(specifier, context, nextResolve) {
    if (specifier === STUBBED_SPECIFIER) {
        return { url: STUB_URL, shortCircuit: true };
    }
    return nextResolve(specifier, context);
}

export async function load(url, context, nextLoad) {
    if (url === STUB_URL) {
        return { format: 'module', shortCircuit: true, source: STUB_SOURCE };
    }
    return nextLoad(url, context);
}
