/**
 * The compiler worker: `new Worker('<version>/worker.js', { type: 'module' })`.
 *
 * It speaks `src/protocol.ts` to the page and drives Rubrc's virtualised rust
 * toolchain underneath. The split between this file and `vfs-runner.ts` is
 * forced by `@oligami/browser_wasi_shim-threads`: the WASI *farm* services
 * calls for every thread of the guest, and the guest's threads block on
 * `Atomics.wait` until it does, so the farm cannot live on the same thread as
 * the guest that calls it. This worker is the farm (and the protocol); the
 * runner worker it spawns is the guest.
 *
 *   page ── protocol ──▶ worker-entry (farm, sysroot, artifact capture)
 *                             │  wasi_ref + dispatch requests
 *                             ▼
 *                        vfs-runner (WASIFarmAnimal + vfs.core.wasm)
 *                             │  thread-spawn
 *                             ▼
 *                        N thread workers ── WASI calls ──▶ farm
 *
 * What the guest is, is a terminal: rubrc composes rustc, cargo, llvm and a
 * shell into one component, and the only way in is to type at session 0 and
 * read what comes back. So `compile` writes the crate's files through the
 * VFS's write-file event, types a cargo line, waits for the prompt to come
 * back, and asks the shell to `download` the artifact — which is delivered
 * back through the same host bridge as chunks.
 */

/// <reference lib="webworker" />

import {
  Directory,
  Fd,
  File,
  type Inode,
  PreopenDirectory,
} from "@bjorn3/browser_wasi_shim";
import { WASIFarm, wait_async_polyfill } from "@oligami/browser_wasi_shim-threads";
import { createHttpBridge, isHttpBridgeMessage } from "rubrc-lib/http_bridge";
import {
  createChildProcessBridge,
  isChildProcessMessage,
} from "rubrc-lib/child_process_bridge";
import { fetch_compressed_stream } from "rubrc-lib/brotli_stream";
import { parseTar } from "rubrc-lib/parse_tar";
import childProcessWorkerUrl from "rubrc-worker/vfs_bindings/child_process_worker.ts?worker&url";
import VfsRunner from "./vfs-runner.ts?worker";
import type {
  Diagnostic,
  PageMessage,
  ProgressStage,
  ResultMessage,
  Severity,
  WorkerMessage,
} from "./protocol";
import { stripAnsi } from "./ansi";

wait_async_polyfill();

/** The shell's prompt is `<cwd> $ `; a bare `$ ` tail means it is idle. */
const PROMPT = "$ ";
/** `input_string` with this session id is the VFS's write-file event. */
const WRITE_FILE_SESSION = 0xeeeeeeee;
const SESSION = 0;
/** Long enough for a cold `cargo build` of a std-only crate; see README. */
const COMPILE_TIMEOUT_MS = 10 * 60 * 1000;
const SYSROOT_TIMEOUT_MS = 5 * 60 * 1000;
/** The only target the sandbox builds for, and the sysroot we vendor. */
const SYSROOT_TRIPLE = "wasm32-wasip1";

const post = (message: WorkerMessage, transfer: Transferable[] = []) => {
  (globalThis as unknown as Worker).postMessage(message, transfer);
};

// ---------------------------------------------------------------- terminal

/**
 * Everything session 0 has printed since the last `reset()`.
 *
 * Both the runner and the farm see `terminalWrite` — the shell's main thread
 * calls it through the component's own import, its session threads through
 * the farm — so both funnel into this one buffer, in arrival order.
 */
class Transcript {
  private text = "";
  private waiters: { test: (text: string) => boolean; resolve: (text: string) => void }[] = [];

  append(chunk: string) {
    this.text += chunk;
    for (const waiter of [...this.waiters]) {
      if (waiter.test(this.text)) {
        this.waiters.splice(this.waiters.indexOf(waiter), 1);
        waiter.resolve(this.text);
      }
    }
  }

  reset() {
    this.text = "";
  }

  /** Resolves once `test` holds; rejects after `timeoutMs`. */
  wait(test: (text: string) => boolean, timeoutMs: number, what: string): Promise<string> {
    if (test(this.text)) return Promise.resolve(this.text);
    return new Promise((resolve, reject) => {
      const waiter = { test, resolve };
      this.waiters.push(waiter);
      setTimeout(() => {
        const at = this.waiters.indexOf(waiter);
        if (at === -1) return;
        this.waiters.splice(at, 1);
        reject(new Error(`timed out after ${timeoutMs}ms waiting for ${what}`));
      }, timeoutMs);
    });
  }
}

const transcript = new Transcript();
/** fd 2 of the guest, which is not the shell's own session stream. */
let stderrText = "";

const idle = (text: string) => stripAnsi(text).endsWith(PROMPT);

// -------------------------------------------------------------- the runner

let runner: Worker | undefined;
let vfsReady: { resolve: () => void; reject: (e: Error) => void } | undefined;

const runnerReady = new Promise<void>((resolve, reject) => {
  vfsReady = { resolve, reject };
});

const startRunner = (wasiRef: unknown) => {
  runner = new VfsRunner();
  runner.addEventListener("message", (event: MessageEvent) => {
    const data = event.data;
    switch (data?.type) {
      case "terminal":
        transcript.append(new TextDecoder().decode(toBytes(data.data)));
        break;
      case "progress":
        post({
          type: "progress",
          id: currentId,
          stage: data.stage as ProgressStage,
          loaded: data.loaded,
          total: data.total,
          detail: data.detail,
        });
        break;
      case "ready":
        vfsReady?.resolve();
        break;
      case "error":
        vfsReady?.reject(new Error(String(data.message)));
        break;
    }
  });
  runner.postMessage({ type: "start", wasi_ref: wasiRef });
};

const send = (line: string) => {
  runner?.postMessage({ type: "input", sessionId: SESSION, data: line });
};

const writeFile = (path: string, content: string) => {
  runner?.postMessage({
    type: "input",
    sessionId: WRITE_FILE_SESSION,
    data: JSON.stringify({ path, content }),
  });
};

/**
 * Type a line at session 0 and hand back what it printed.
 *
 * The prompt is the only completion signal the shell offers, so the transcript
 * is cleared first: a stale prompt in the buffer would otherwise read as "this
 * command is already done".
 */
const runCommand = async (line: string, timeoutMs: number, what: string): Promise<string> => {
  await transcript.wait(idle, timeoutMs, `the shell prompt before \`${line}\``);
  transcript.reset();
  send(`${line}\r`);
  const raw = await transcript.wait(idle, timeoutMs, what);
  const text = stripAnsi(raw);
  // Drop the echoed command line and the prompt that closes the output.
  const body = text.slice(text.indexOf("\n") + 1);
  return body.slice(0, body.lastIndexOf("\n") + 1);
};

// ------------------------------------------------------------ the WASI farm

/** A file descriptor that collects what the guest writes into a string. */
class CapturedFd extends Fd {
  constructor(private readonly sink: (text: string) => void) {
    super();
  }
  fd_write(data: Uint8Array) {
    this.sink(new TextDecoder().decode(data));
    return { ret: 0, nwritten: data.byteLength };
  }
  fd_seek() {
    return { ret: 8, offset: 0n }; // ERRNO_BADF
  }
  fd_filestat_get() {
    return { ret: 8, filestat: null };
  }
}

const toBytes = (data: unknown): Uint8Array => {
  if (data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) {
    return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  }
  if (Array.isArray(data)) return new Uint8Array(data);
  if (data && typeof data === "object") {
    return new Uint8Array(Object.values(data as Record<string, number>));
  }
  return new Uint8Array();
};

const toMap = (entries: [string, Inode][]) => new Map<string, Inode>(entries);

/**
 * The filesystem the guest starts from.
 *
 * `/sysroot` is filled by `load_sysroot`, and the crate's own files arrive
 * through the write-file event, so this is only the skeleton: an empty cargo
 * config (cargo insists on one) and the two directories the rest hangs off.
 */
const rootDir = new PreopenDirectory(
  "/",
  toMap([
    ["sysroot", new Directory([])],
    ["src", new Directory([])],
    [".cargo", new Directory(toMap([["config.toml", new File(new Uint8Array())]]))],
  ]),
);

/** Chunks of the file the shell's `download` command is streaming out. */
let downloadChunks: Uint8Array[] = [];
let downloadName = "";

/** The sysroot tar, one entry at a time, as the guest pulls it. */
type SysrootEntry = { name: Uint8Array; data: Uint8Array; is_directory: boolean };
let sysrootQueue: SysrootEntry[] = [];
let sysrootCurrent: SysrootEntry | null = null;

/**
 * The vendored sysroot, NOT `oligamiq.github.io`.
 *
 * Rubrc fetches `https://oligamiq.github.io/rust_wasm/v0.2.0/<triple>.tar.br`
 * at runtime (`lib/src/sysroot.ts`). A sandbox that compiles code cannot have
 * its standard library come from a host we do not control, so
 * `build-compiler.sh` vendors the tarball into `<version>/sysroot/` with its
 * sha256 in `PIN.json`, and this resolves it from there. `import.meta.url` is
 * the worker chunk, which lives in `<version>/`.
 */
//
// `assetBase` is not a pointless alias. Vite rewrites a `new URL` whose first
// argument is a template literal into a lookup in a glob map built at BUILD
// time — and `sysroot/` does not exist until `build-compiler.sh` vendors the
// tarball, so that map is empty and the URL resolves to `undefined`. Reading
// `import.meta.url` through a variable keeps the resolution at runtime, where
// the file exists. Do not inline it.
const assetBase = import.meta.url;
const sysrootUrl = (triple: string) => new URL(`./sysroot/${triple}.tar.br`, assetBase).href;

const loadSysrootQueue = async (triple: string) => {
  sysrootQueue = [];
  const stream = await fetch_compressed_stream(sysrootUrl(triple));
  await parseTar(stream, (file: { name: string; data?: Uint8Array; type: string }) => {
    sysrootQueue.push({
      name: new TextEncoder().encode(file.name),
      data: file.data ?? new Uint8Array(),
      is_directory: file.type === "directory",
    });
  });
};

let farm: WASIFarm;

// No registry. A block's `Cargo.toml` has an empty `[dependencies]` table (see
// the templates), so an outbound request from the toolchain is a block doing
// something it cannot do, not a fetch to proxy: refusing it keeps the sandbox
// offline by construction rather than by policy. Rubrc's own page points this
// at a crates.io proxy worker instead.
const httpBridge = createHttpBridge((input: RequestInfo | URL) =>
  Promise.reject(new Error(`the sandbox toolchain has no network access: ${String(input)}`)),
);

const childBridge = createChildProcessBridge({
  getWasiRef: () => farm.get_ref(),
  workerUrl: childProcessWorkerUrl,
  filesystemRoot: rootDir.dir,
  uploadTimeoutMs: 30_000,
  executionTimeoutMs: 120_000,
});

farm = new WASIFarm(
  new CapturedFd(() => {}),
  new CapturedFd((text) => transcript.append(text)),
  new CapturedFd((text) => {
    stderrText += text;
  }),
  [rootDir],
  {
    allocator_size: 100 * 1024 * 1024,
    base_call_allocator_size: 64 * 1024 * 1024,
    unknown_fn: async (unknown: { name: string; args: Record<string, unknown> }) => {
      if (isHttpBridgeMessage(unknown)) return await httpBridge(unknown);
      if (isChildProcessMessage(unknown)) return await childBridge(unknown);

      switch (unknown.name) {
        case "terminalWrite":
          transcript.append(new TextDecoder().decode(toBytes(unknown.args.data)));
          return;

        case "downloadFileStart":
          downloadName = String(unknown.args.name ?? "");
          downloadChunks = [];
          return;
        case "downloadFileChunk":
          downloadChunks.push(toBytes(unknown.args.data));
          return;
        case "downloadFileEnd":
          return;

        case "sysrootStartFetch":
          await loadSysrootQueue(String(unknown.args.triple));
          return {};
        case "sysrootGetNextFileMeta": {
          sysrootCurrent = sysrootQueue.shift() ?? null;
          if (!sysrootCurrent) return { has_file: false, name_len: 0, data_len: 0 };
          return {
            has_file: true,
            name_len: sysrootCurrent.name.length,
            data_len: sysrootCurrent.is_directory ? -1 : sysrootCurrent.data.length,
          };
        }
        case "sysrootReadFileName":
          if (!sysrootCurrent) throw new Error("sysrootReadFileName without a current file");
          return { name: Array.from(sysrootCurrent.name) };
        case "sysrootReadFileChunk": {
          if (!sysrootCurrent) return { chunk: [] };
          const want = Number(unknown.args.chunk_len ?? 0);
          const chunk = sysrootCurrent.data.slice(0, want);
          sysrootCurrent.data = sysrootCurrent.data.slice(want);
          return { chunk: Array.from(chunk) };
        }

        default:
          console.warn("[compiler] unhandled host call", unknown);
          return;
      }
    },
  },
);

// -------------------------------------------------------------- diagnostics

/**
 * `--message-format=json` first, the text fallback second.
 *
 * Cargo's JSON lines share the session stream with its human output, so the
 * parse is per line and tolerant: a line that is not a JSON object is left to
 * the regex, which is the only thing available when a build fails before
 * cargo starts (a malformed `Cargo.toml`, say).
 */
const TEXT_DIAGNOSTIC =
  /^(error|warning)(?:\[(E\d+)\])?: (.*)\n\s+--> ([^:\n]+):(\d+):(\d+)/gm;

const parseDiagnostics = (
  text: string,
): { diagnostics: Diagnostic[]; fromJson: boolean; buildFinished?: boolean } => {
  const diagnostics: Diagnostic[] = [];
  let fromJson = false;
  let buildFinished: boolean | undefined;

  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed.startsWith("{")) continue;
    let value: Record<string, unknown>;
    try {
      value = JSON.parse(trimmed);
    } catch {
      continue;
    }
    if (value.reason === "build-finished") {
      fromJson = true;
      buildFinished = Boolean(value.success);
      continue;
    }
    if (value.reason !== "compiler-message") continue;
    fromJson = true;
    const message = value.message as {
      level?: string;
      message?: string;
      code?: { code?: string } | null;
      spans?: { file_name?: string; line_start?: number; column_start?: number; is_primary?: boolean }[];
    };
    const level = message?.level;
    if (level !== "error" && level !== "warning") continue;
    const span = message.spans?.find((s) => s.is_primary) ?? message.spans?.[0];
    diagnostics.push({
      file: span?.file_name ?? "",
      line: span?.line_start ?? 0,
      column: span?.column_start ?? 0,
      severity: level as Severity,
      message: message.message ?? "",
      ...(message.code?.code ? { code: message.code.code } : {}),
    });
  }

  if (fromJson) return { diagnostics, fromJson, buildFinished };

  TEXT_DIAGNOSTIC.lastIndex = 0;
  for (const match of text.matchAll(TEXT_DIAGNOSTIC)) {
    diagnostics.push({
      file: match[4],
      line: Number(match[5]),
      column: Number(match[6]),
      severity: match[1] as Severity,
      message: match[3],
      ...(match[2] ? { code: match[2] } : {}),
    });
  }
  return { diagnostics, fromJson };
};

// ------------------------------------------------------------ the protocol

type State = "new" | "initializing" | "ready" | "compiling" | "broken";
let state: State = "new";
let currentId = "";
let rustcVersion = "";

const fail = (id: string, message: string, extra: Partial<ResultMessage> = {}) => {
  post({
    type: "result",
    id,
    success: false,
    stdout: "",
    stderr: message,
    diagnostics: [],
    elapsedMs: 0,
    ...extra,
  });
};

const init = async (id: string) => {
  state = "initializing";
  currentId = id;
  post({ type: "progress", id, stage: "download", loaded: 0, total: 0 });
  startRunner(farm.get_ref());
  await runnerReady;

  post({ type: "progress", id, stage: "initializing", detail: "waiting for the shell" });
  await transcript.wait(idle, SYSROOT_TIMEOUT_MS, "the shell's first prompt");

  post({ type: "progress", id, stage: "initializing", detail: "loading the sysroot" });
  await runCommand(`load_sysroot ${SYSROOT_TRIPLE}`, SYSROOT_TIMEOUT_MS, "load_sysroot");

  rustcVersion = (await runCommand("rustc --version", SYSROOT_TIMEOUT_MS, "rustc --version")).trim();
  state = "ready";
  post({ type: "ready", id, rustcVersion });
};

const compile = async (message: Extract<PageMessage, { type: "compile" }>) => {
  const started = Date.now();
  state = "compiling";
  currentId = message.id;
  stderrText = "";
  downloadChunks = [];
  downloadName = "";

  for (const [path, content] of Object.entries(message.files)) {
    writeFile(path.startsWith("/") ? path : `/${path}`, content);
  }

  // Cargo decides what to rebuild from file mtimes, and the VFS's write-file
  // event replaces a file's contents without moving its mtime — so a second
  // compile of an edited crate comes back `"fresh": true` with the FIRST
  // build's artifact, which is the worst possible failure here: a green build
  // of code nobody wrote. `cargo clean` is what makes each compile mean what
  // it says. It costs nothing to speak of: a block has no dependencies (the
  // toolchain has no registry), so there is no dependency graph to keep warm
  // — the only thing being rebuilt is the block itself, which has to be
  // rebuilt anyway.
  await runCommand("cargo clean", COMPILE_TIMEOUT_MS, "cargo clean");

  const profile = message.release ? " --release" : "";
  const output = await runCommand(
    `cargo build${profile} --target ${message.target} --message-format=json`,
    COMPILE_TIMEOUT_MS,
    "cargo build",
  );
  const { diagnostics, buildFinished } = parseDiagnostics(output);
  const errored = diagnostics.some((d) => d.severity === "error");
  const built = buildFinished ?? !errored;

  let artifact: ArrayBuffer | undefined;
  if (built) {
    const crateFile = `${message.crateName.replace(/-/g, "_")}.wasm`;
    const artifactPath =
      `/target/${message.target}/${message.release ? "release" : "debug"}/${crateFile}`;
    post({ type: "progress", id: message.id, stage: "compiling", detail: `reading ${artifactPath}` });
    await runCommand(`download ${artifactPath}`, COMPILE_TIMEOUT_MS, "download");
    // `download` prints "File not found" and streams nothing when the path is
    // wrong, so the name the bridge reported is checked rather than assumed:
    // chunks left over from an earlier request must never be served as this
    // request's artifact.
    if (downloadChunks.length > 0 && downloadName === artifactPath) {
      const total = downloadChunks.reduce((n, c) => n + c.byteLength, 0);
      const bytes = new Uint8Array(total);
      let at = 0;
      for (const chunk of downloadChunks) {
        bytes.set(chunk, at);
        at += chunk.byteLength;
      }
      artifact = bytes.buffer;
    }
  }

  const success = built && artifact !== undefined;
  state = "ready";
  post(
    {
      type: "result",
      id: message.id,
      success,
      ...(artifact ? { artifact } : {}),
      stdout: output,
      stderr: stderrText,
      diagnostics,
      elapsedMs: Date.now() - started,
    },
    artifact ? [artifact] : [],
  );
  downloadChunks = [];
  downloadName = "";
};

globalThis.addEventListener("message", (event: MessageEvent) => {
  const message = event.data as PageMessage;
  if (!message || typeof message.type !== "string") return;

  switch (message.type) {
    case "init":
      if (state !== "new") {
        post({ type: "error", id: message.id, message: `init in state ${state}` });
        return;
      }
      init(message.id).catch((error) => {
        state = "broken";
        post({ type: "error", id: message.id, message: String(error) });
      });
      return;

    case "compile":
      if (state === "compiling") {
        fail(message.id, "a compile is already in flight");
        return;
      }
      if (state !== "ready") {
        fail(message.id, `compile in state ${state}`);
        return;
      }
      compile(message).catch((error) => {
        state = "broken";
        fail(message.id, String(error), { elapsedMs: 0 });
      });
      return;

    case "cancel":
      // The session thread is inside cargo and nothing outside it can unwind
      // that, so this reports the compile as cancelled and marks the worker
      // spent. The adapter terminates it and starts a fresh one — see
      // `CancelMessage` in protocol.ts.
      state = "broken";
      fail(message.id, "cancelled", { cancelled: true });
      return;
  }
});
