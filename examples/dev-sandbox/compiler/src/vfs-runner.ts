/**
 * The guest half of the compiler worker: downloads `vfs.core-*.wasm`,
 * instantiates it against the farm `worker-entry.ts` runs, and turns protocol
 * requests into the component's session events.
 *
 * This is rubrc's `page/src/worker_process/util_cmd.ts` with its UI removed:
 * no `@oligami/shared-object` proxies to a solid app, no xterm, no LSP
 * routing. What is kept is the part that is not UI — the split/brotli/
 * IndexedDB image loader and the `WASIFarmAnimal` + `custom_instantiate`
 * handshake, which is subtle enough that reimplementing it would only be a
 * way to get it wrong.
 */

/// <reference lib="webworker" />

import { WASIFarmAnimal } from "@oligami/browser_wasi_shim-threads";
import { custom_instantiate } from "rubrc-worker/vfs_bindings/inst";
import { set_fake_worker } from "rubrc-worker/vfs_bindings/common";
import { get_brotli_decompress_stream } from "rubrc-lib/brotli_stream";
import threadSpawnUrl from "rubrc-worker/vfs_bindings/thread_spawn.ts?worker&url";
import workerBackgroundUrl from "rubrc-worker/vfs_bindings/worker_background_worker.ts?worker&url";
// The composed component. `build-compiler.sh` puts it here; vite emits it as
// `vfs.core-<hash>.wasm` and `prepare-vfs-asset.mjs` then replaces it with its
// brotli parts, which is what the loader below reassembles.
import vfsCoreUrl from "../.rubrc/page/src/worker_process/vfs_bindings/vfs.core.wasm?url";

await set_fake_worker();

const WRITE_FILE_SESSION = 0xeeeeeeee;
const EVENT_INPUT_CHAR = 0;
const EVENT_RESIZE = 1;
const EVENT_CREATE_SESSION = 3;
const EVENT_WRITE_FILE = 7;
/** The component sizes its terminal from this; nothing renders it. */
const COLS = 200;
const ROWS = 50;
const VFS_THREADS = 8;

const post = (message: Record<string, unknown>) => {
  (globalThis as unknown as Worker).postMessage(message);
};

// ------------------------------------------------------- the compiler image

const CACHE_DB = "wasm_cache";
const CACHE_STORE = "modules";

const openCache = () =>
  new Promise<IDBDatabase | null>((resolve) => {
    if (typeof indexedDB === "undefined") return resolve(null);
    try {
      const request = indexedDB.open(CACHE_DB, 1);
      request.onupgradeneeded = () => request.result.createObjectStore(CACHE_STORE);
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => resolve(null);
    } catch {
      resolve(null);
    }
  });

const getCached = async (key: string): Promise<WebAssembly.Module | null> => {
  const db = await openCache();
  if (!db || !db.objectStoreNames.contains(CACHE_STORE)) return null;
  return new Promise((resolve) => {
    try {
      const request = db.transaction(CACHE_STORE, "readonly").objectStore(CACHE_STORE).get(key);
      request.onsuccess = () => resolve(request.result ?? null);
      request.onerror = () => resolve(null);
    } catch {
      resolve(null);
    }
  });
};

const putCached = async (key: string, module: WebAssembly.Module): Promise<void> => {
  const db = await openCache();
  if (!db) return;
  return new Promise((resolve) => {
    try {
      const tx = db.transaction(CACHE_STORE, "readwrite");
      tx.objectStore(CACHE_STORE).put(module, key);
      tx.oncomplete = () => resolve();
      tx.onerror = () => resolve();
    } catch {
      resolve();
    }
  });
};

type PartManifest = {
  version: number;
  encoding: string;
  originalFile: string;
  originalSize: number;
  compressedSize: number;
  parts: { file: string; size: number }[];
};

/**
 * Fetch the split, brotli-compressed component and compile it.
 *
 * Every field of the manifest is checked before a byte is trusted, and the
 * decompressed length is checked against `originalSize` as it streams: the
 * parts are served from our own origin, but a truncated part would otherwise
 * surface as an unintelligible `CompileError` half a minute in.
 */
const loadVfsCore = async (): Promise<WebAssembly.Module> => {
  const manifestUrl = new URL(`${vfsCoreUrl}.br.json`, import.meta.url);
  const response = await fetch(manifestUrl.href);
  if (!response.ok) {
    throw new Error(`${manifestUrl.href}: ${response.status} ${response.statusText}`);
  }
  const manifest = (await response.json()) as PartManifest;
  if (manifest.version !== 1 || manifest.encoding !== "br" || !Array.isArray(manifest.parts)) {
    throw new Error(`${manifestUrl.href}: not a version 1 brotli part manifest`);
  }
  let expected = 0;
  for (const [index, part] of manifest.parts.entries()) {
    const want = `${manifest.originalFile}.br.part-${index.toString().padStart(3, "0")}`;
    if (part.file !== want) throw new Error(`part ${index} is ${part.file}, expected ${want}`);
    expected += part.size;
  }
  if (expected !== manifest.compressedSize) {
    throw new Error("the manifest's part sizes do not add up to compressedSize");
  }

  const key = `${manifestUrl.href}?size=${manifest.compressedSize}`;
  const cached = await getCached(key);
  if (cached) {
    post({ type: "progress", stage: "download", loaded: manifest.compressedSize, total: manifest.compressedSize, detail: "cached" });
    return cached;
  }

  const { readable, writable } = new TransformStream<Uint8Array, Uint8Array>();
  void (async () => {
    const writer = writable.getWriter();
    try {
      let loaded = 0;
      for (const part of manifest.parts) {
        const partResponse = await fetch(new URL(part.file, manifestUrl.href).href);
        if (!partResponse.ok || !partResponse.body) {
          throw new Error(`${part.file}: ${partResponse.status} ${partResponse.statusText}`);
        }
        const reader = partResponse.body.getReader();
        let partLoaded = 0;
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          partLoaded += value.byteLength;
          loaded += value.byteLength;
          post({ type: "progress", stage: "download", loaded, total: manifest.compressedSize });
          await writer.write(value);
        }
        if (partLoaded !== part.size) {
          throw new Error(`${part.file}: got ${partLoaded} bytes, manifest says ${part.size}`);
        }
      }
      await writer.close();
    } catch (error) {
      await writer.abort(error);
    }
  })();

  let inflated = 0;
  const checkLength = new TransformStream<Uint8Array, Uint8Array>({
    transform(chunk, controller) {
      inflated += chunk.byteLength;
      if (inflated > manifest.originalSize) {
        controller.error(new Error("the image inflates past originalSize"));
        return;
      }
      controller.enqueue(chunk);
    },
    flush(controller) {
      if (inflated !== manifest.originalSize) {
        controller.error(
          new Error(`the image inflated to ${inflated} bytes, manifest says ${manifest.originalSize}`),
        );
      }
    },
  });

  const stream = readable.pipeThrough(await get_brotli_decompress_stream()).pipeThrough(checkLength);
  const module = await WebAssembly.compileStreaming(
    new Response(stream, { headers: { "Content-Type": "application/wasm" } }),
  );
  await putCached(key, module);
  return module;
};

// ----------------------------------------------------------- instantiation

type VfsRoot = {
  dispatch: (session: number, event: number, arg1: number, arg2: number) => void;
  allocBuf: (len: number) => number;
  freeBuf: (ptr: number, len: number) => void;
};

let vfsRoot: VfsRoot | undefined;
let sharedMemory: WebAssembly.Memory | undefined;

const start = async (wasiRef: unknown) => {
  const module = await loadVfsCore();
  post({ type: "progress", stage: "initializing", detail: "instantiating" });

  const animal = new WASIFarmAnimal(
    [wasiRef],
    [],
    [`VFS_THREADS=${VFS_THREADS}`],
    {
      can_thread_spawn: true,
      thread_spawn_worker_url: new URL(threadSpawnUrl, import.meta.url).href,
      thread_spawn_wasm: module,
      worker_background_worker_url: new URL(workerBackgroundUrl, import.meta.url).href,
      share_memory: {
        memory: new WebAssembly.Memory({ initial: 1032, maximum: 32775, shared: true }),
      },
    },
  );

  await animal.wait_worker_background_worker();

  const root = (await custom_instantiate(
    module,
    animal.wasiImport as never,
    animal.wasiThreadImport as never,
    animal.get_share_memory(),
    (idx: number, unknown: { name: string; args: Record<string, unknown> }) => {
      // The shell's own thread writes here; its session threads reach the
      // farm instead, and `worker-entry.ts` merges both into one transcript.
      if (unknown.name === "terminalWrite") {
        post({ type: "terminal", sessionId: unknown.args.session_id, data: unknown.args.data });
        return;
      }
      return animal.call_unknown_fn(idx, unknown);
    },
  )) as unknown as VfsRoot;

  sharedMemory = animal.get_share_memory().memory;
  vfsRoot = root;

  animal.start(root as never);
  root.dispatch(0, EVENT_CREATE_SESSION, 0, 0);
  root.dispatch(0, EVENT_RESIZE, COLS, ROWS);
  post({ type: "ready" });
};

// --------------------------------------------------------------- the input

/**
 * Session 0 is a terminal, so a command is typed into it one code point at a
 * time; the write-file event takes its JSON through the component's own
 * allocator instead. Both are what rubrc's `input_string` proxy does.
 */
const input = (sessionId: number, data: string) => {
  const root = vfsRoot;
  const memory = sharedMemory;
  if (!root || !memory) return;

  if (sessionId !== WRITE_FILE_SESSION) {
    for (const character of data) {
      const codePoint = character.codePointAt(0);
      if (codePoint !== undefined) root.dispatch(sessionId, EVENT_INPUT_CHAR, codePoint, 0);
    }
    return;
  }

  const bytes = new TextEncoder().encode(data);
  const pointer = root.allocBuf(bytes.length);
  new Uint8Array(memory.buffer).set(bytes, pointer);
  root.dispatch(sessionId, EVENT_WRITE_FILE, pointer, bytes.length);
  root.freeBuf(pointer, bytes.length);
};

globalThis.addEventListener("message", (event: MessageEvent) => {
  const message = event.data;
  if (message?.type === "start") {
    start(message.wasi_ref).catch((error) => {
      post({ type: "error", message: String(error?.stack ?? error) });
    });
    return;
  }
  if (message?.type === "input") {
    try {
      input(message.sessionId, message.data);
    } catch (error) {
      post({ type: "error", message: String(error) });
    }
  }
});
