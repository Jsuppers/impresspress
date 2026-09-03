import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

// The pinned rubrc checkout `build-compiler.sh` produces. Only two trees under
// it are ever imported: `page/src/worker_process/**` (the VFS bindings and the
// thread-spawn plumbing) and `lib/src/**` (brotli, tar, and the http /
// child-process bridges the composed component calls back into). Nothing from
// rubrc's UI — Monaco, xterm, solid, `@oligami/shared-object` — is reachable
// from `src/worker-entry.ts`, which is what keeps this bundle at a few hundred
// kilobytes next to a page that ships an editor.
const rubrc = (path: string) =>
  fileURLToPath(new URL(`./.rubrc/${path}`, import.meta.url));

const version = process.env.COMPILER_VERSION;
if (!version) {
  throw new Error("COMPILER_VERSION is not set — run build-compiler.sh, not vite directly");
}

/**
 * Emit only what the worker actually loads: JavaScript and wasm.
 *
 * `vfs.js` — the loader `jco` generated for the component — ends with a
 * fallback `getCoreModule = (name) => fetchCompile(new URL(`./${name}`,
 * import.meta.url))`. We never take it (`custom_instantiate` is always handed
 * the compiled module), but a `new URL` with an interpolated path makes vite
 * emit *every* file in that directory as an asset: rubrc's `package.json`,
 * `bun.lock`, `tsconfig.json`, its own `index.html` and a dozen `.ts` sources,
 * all of them served from `/__impresspress_dev/compiler/<version>/`. None of
 * it is reachable from `worker.js`, and `dist/` is a deploy artefact, so the
 * emitted set is trimmed to the two extensions the worker can load.
 */
const onlyLoadableAssets = () => ({
  name: "dev-sandbox:only-loadable-assets",
  generateBundle(
    _options: unknown,
    bundle: Record<string, { type: string; fileName: string }>,
  ) {
    for (const [key, item] of Object.entries(bundle)) {
      if (item.type !== "asset") continue;
      if (/\.(js|wasm)$/.test(item.fileName)) continue;
      delete bundle[key];
    }
  },
});

export default defineConfig({
  plugins: [onlyLoadableAssets()],
  // Everything is addressed relative to the module that references it, so the
  // same `dist/<version>/` tree works under `/__impresspress_dev/compiler/`
  // here and under `/` in the probe server.
  base: "./",
  resolve: {
    alias: {
      "rubrc-worker": rubrc("page/src/worker_process"),
      "rubrc-lib": rubrc("lib/src"),
    },
    // The WASI shim is the one thing both halves of the bundle must agree on:
    // our farm and rubrc's thread-spawn code talk to each other over shared
    // memory, so two copies at two versions would fail as a memory-layout bug
    // rather than as a version error. `package.json` pins the versions the
    // pinned rubrc resolves, and this makes every import of them — theirs
    // included — come from this package's `node_modules`.
    dedupe: ["@bjorn3/browser_wasi_shim", "@oligami/browser_wasi_shim-threads"],
  },
  worker: {
    format: "es",
    rollupOptions: {
      output: {
        entryFileNames: "[name]-[hash].js",
        chunkFileNames: "[name]-[hash].js",
        assetFileNames: "[name]-[hash][extname]",
      },
    },
  },
  build: {
    outDir: `dist/${version}`,
    emptyOutDir: true,
    target: "es2022",
    // `vfs.core.wasm` is ~250 MB and the brotli decoder is 200 KB: nothing
    // here should ever be inlined as a data: URI.
    assetsInlineLimit: 0,
    // Flat, because `dist/<version>/` is served as-is and the split parts of
    // `vfs.core-*.wasm` have to sit next to their `.br.json`.
    assetsDir: ".",
    rollupOptions: {
      input: { worker: fileURLToPath(new URL("./src/worker-entry.ts", import.meta.url)) },
      output: {
        format: "es",
        entryFileNames: "[name].js",
        chunkFileNames: "[name]-[hash].js",
        assetFileNames: "[name]-[hash][extname]",
      },
    },
  },
});
