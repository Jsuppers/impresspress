/**
 * The message protocol between the page-side adapter and the compiler worker.
 *
 * This file is the contract. `worker-entry.ts` implements the worker half;
 * the adapter (`/b/dev`) implements the page half. Both halves are in this
 * repo, so the protocol is versioned by `PIN.json`'s `version` — the URL the
 * page loads the worker from already carries it.
 *
 * One compile at a time. The worker answers `compile` with exactly one
 * `result` carrying that request's `id`, and a `compile` that arrives while
 * another is in flight is answered with a failed `result` rather than queued:
 * the sandbox has one editor and one build button, and a queue would only
 * hide a bug in the page.
 */

/** `init` starts the toolchain: download, instantiate, load the sysroot. */
export type InitMessage = { type: "init"; id: string };

/** `compile` writes `files` into the VFS and runs cargo over them. */
export type CompileMessage = {
  type: "compile";
  id: string;
  crateName: string;
  /** Paths relative to the crate root, e.g. `Cargo.toml`, `src/lib.rs`. */
  files: Record<string, string>;
  target: string;
  release: boolean;
};

/**
 * `cancel` abandons the in-flight compile.
 *
 * Rubrc's shell has no way to interrupt a running command from outside the
 * session thread that is blocked in it, so the worker cannot unwind a compile
 * in progress. It answers with `{ success: false, cancelled: true }` and is
 * then UNUSABLE: the adapter must `terminate()` it and `init` a fresh one.
 * The `cancelled` result is the signal to do that, not a resumption point.
 */
export type CancelMessage = { type: "cancel"; id: string };

export type PageMessage = InitMessage | CompileMessage | CancelMessage;

export type ProgressStage = "download" | "initializing" | "compiling";

export type ProgressMessage = {
  type: "progress";
  id: string;
  stage: ProgressStage;
  /** Bytes of the compiler image fetched so far (`download` only). */
  loaded?: number;
  total?: number;
  /** A line of compiler output, or the step being run (`compiling`). */
  detail?: string;
};

export type ReadyMessage = {
  type: "ready";
  id: string;
  /** Whatever `rustc --version` printed inside the VFS. */
  rustcVersion: string;
};

export type Severity = "error" | "warning";

export type Diagnostic = {
  file: string;
  line: number;
  column: number;
  severity: Severity;
  message: string;
  /** `E0425` and friends, when rustc gave one. */
  code?: string;
};

export type ResultMessage = {
  type: "result";
  id: string;
  success: boolean;
  /** Transferred, not copied — the adapter owns the buffer after this. */
  artifact?: ArrayBuffer;
  /** The shell transcript: cargo's own output, ANSI stripped. */
  stdout: string;
  /** What the guest wrote to fd 2 outside the shell's own stream. */
  stderr: string;
  diagnostics: Diagnostic[];
  elapsedMs: number;
  cancelled?: boolean;
};

/** Sent when the worker fails outside a request, e.g. the image is corrupt. */
export type ErrorMessage = { type: "error"; id: string; message: string };

export type WorkerMessage =
  | ProgressMessage
  | ReadyMessage
  | ResultMessage
  | ErrorMessage;
