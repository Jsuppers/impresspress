// A stand-in for `examples/dev-sandbox/compiler/dist/<pin>/worker.js`.
//
// The real worker is 365 MiB of composed toolchain and takes seven seconds to
// reach `ready` and forty to compile anything. `dev-compiler.spec.ts` is about
// the PAGE half — the queue, the cancel, the teardown, the four ways the
// adapter refuses a worker that misbehaves — and none of that is any different
// against real rustc. So this file speaks the same protocol in microseconds,
// and `dev-compile.spec.ts` (Plan 3 Task 6) drives the real one.
//
// The protocol is `examples/dev-sandbox/compiler/src/protocol.ts` and the
// STATE MACHINE below is `worker-entry.ts`'s, deliberately:
//
//   new → initializing → ready ⇄ compiling,  and `broken`, which is terminal
//
//   * `compile` on a `broken` or not-yet-`ready` worker is an `error` — the
//     adapter's signal to terminate and start over.
//   * `compile` on a worker that is merely busy is a failed `result`: that
//     request is refused, the worker is not. This is the refusal that makes
//     the adapter's queue mean something.
//   * `cancel` names the COMPILE's id. One that names anything else — a
//     double click, or one that raced the result it meant to cancel — is an
//     `error` and changes nothing; a healthy worker must not be brickable
//     from the page.
//   * A cancelled build keeps running underneath (nothing can stop it), so
//     its eventual `result` and any late `progress` are dropped: one request,
//     exactly one terminal message.
//
// A fake that skipped any of that would let a broken adapter pass.
//
// It is loaded as `{ type: 'module' }`, like the real worker.
//
// # Making it misbehave
//
// The adapter treats four things as reasons to destroy the worker, and each
// needs a worker willing to do it. `crateName` is the switch, because it
// travels in the compile message the page already sends:
//
//   protocol-wrong-id      answer with an id nobody asked for
//   protocol-not-a-buffer  send the artifact as a Uint8Array, not transferred
//   protocol-oversized     send an artifact one byte over the sandbox's limit
//   protocol-error         report an out-of-band `error` instead of a result
//   slow                   take five seconds, so a `cancel` has something to hit
//
// Anything else compiles: `success: true` with a sixteen-byte artifact, unless
// one of the files contains the marker `FAIL`, which produces one error
// diagnostic and no artifact — a crate that does not compile, which is an
// ordinary answer and not a failure of the protocol.

/** `worker-entry.ts`'s own states, and for the same reasons. */
let state = 'new';

/** The id of the compile that is running, if one is. */
let inFlight;

/** Compiles already answered with `cancelled: true`; their results are dropped. */
const cancelledIds = new Set();

/** The artifact a successful fake build produces: 16 bytes, 0x00…0x0f. */
const ARTIFACT_BYTES = 16;

/** How long a compile takes, unless the crate name asks for longer. */
const COMPILE_MS = 60;

/** Bumped per compile so the page can prove the queue preserved its order. */
let builds = 0;

const post = (message, transfer = []) => {
  self.postMessage(message, transfer);
};

const postProgress = (id, stage, extra = {}) => {
  if (cancelledIds.has(id)) return;
  post({ type: 'progress', id, stage, ...extra });
};

const failed = (id, message) => ({
  type: 'result',
  id,
  success: false,
  stdout: '',
  stderr: message,
  diagnostics: [],
  elapsedMs: 0,
});

/** Answer a compile, unless a `cancel` already answered it. */
const deliver = (id, result, transfer = []) => {
  inFlight = undefined;
  if (cancelledIds.has(id)) return;
  if (state === 'compiling') state = 'ready';
  post(result, transfer);
};

/** The bytes a successful build hands back, as a transferable buffer. */
const artifact = () => Uint8Array.from({ length: ARTIFACT_BYTES }, (_, i) => i).buffer;

/**
 * The first file carrying the `FAIL` marker, as one rustc-shaped diagnostic.
 *
 * Shaped like the real thing (`worker-entry.ts` parses these out of
 * `cargo build --message-format=json`) so the page's rendering of a
 * diagnostic is exercised against the same fields.
 */
const failure = (files) => {
  const file = Object.keys(files).find((path) => files[path].includes('FAIL'));
  return file
    ? [
        {
          file,
          line: 1,
          column: 1,
          severity: 'error',
          message: 'expected `;`, found `value`',
          code: 'E0425',
        },
      ]
    : null;
};

const init = (id) => {
  state = 'initializing';
  // Two progress messages then `ready`. The real worker sends one more
  // `initializing` (it loads the sysroot); the adapter passes through
  // whatever arrives, so the count is the fake's business, not the page's.
  postProgress(id, 'download', { loaded: 0, total: 75124002 });
  postProgress(id, 'initializing', { detail: 'waiting for the shell' });
  state = 'ready';
  post({ type: 'ready', id, rustcVersion: 'rustc 1.90.0-nightly (fake worker)' });
};

const compile = (message) => {
  const started = Date.now();
  const build = (builds += 1);
  state = 'compiling';
  inFlight = message.id;
  postProgress(message.id, 'compiling', { detail: 'cargo build' });

  const slow = message.crateName === 'slow';
  setTimeout(
    () => {
      const stdout = `fake build #${build}: ${message.crateName}`;
      const elapsedMs = Date.now() - started;

      switch (message.crateName) {
        case 'protocol-wrong-id':
          // A result for a request the page never made. Everything else about
          // it is well formed, so the id is the only thing that can be wrong.
          deliver(message.id, {
            type: 'result',
            id: `${message.id}-bogus`,
            success: true,
            stdout,
            stderr: '',
            diagnostics: [],
            elapsedMs,
          });
          return;
        case 'protocol-not-a-buffer':
          // Structured-cloned rather than transferred, so it arrives as a
          // Uint8Array — which the adapter must refuse rather than quietly
          // reinterpret.
          deliver(message.id, {
            type: 'result',
            id: message.id,
            success: true,
            artifact: new Uint8Array(ARTIFACT_BYTES),
            stdout,
            stderr: '',
            diagnostics: [],
            elapsedMs,
          });
          return;
        case 'protocol-oversized': {
          // One byte over `validation::MAX_ARTIFACT_BYTES`.
          const big = new ArrayBuffer(4194304 + 1);
          deliver(
            message.id,
            {
              type: 'result',
              id: message.id,
              success: true,
              artifact: big,
              stdout,
              stderr: '',
              diagnostics: [],
              elapsedMs,
            },
            [big],
          );
          return;
        }
        case 'protocol-error':
          // What a worker whose image is corrupt reports: not an answer about
          // the crate, an admission that it cannot serve the request at all.
          inFlight = undefined;
          state = 'broken';
          post({ type: 'error', id: message.id, message: 'the compiler image is corrupt' });
          return;
        default:
          break;
      }

      const diagnostics = failure(message.files);
      if (diagnostics) {
        deliver(message.id, {
          type: 'result',
          id: message.id,
          success: false,
          stdout,
          stderr: 'error: expected `;`, found `value`\n',
          diagnostics,
          elapsedMs,
        });
        return;
      }
      const bytes = artifact();
      deliver(
        message.id,
        {
          type: 'result',
          id: message.id,
          success: true,
          artifact: bytes,
          stdout,
          stderr: '',
          diagnostics: [],
          elapsedMs,
        },
        [bytes],
      );
    },
    slow ? 5000 : COMPILE_MS,
  );
};

self.addEventListener('message', (event) => {
  const message = event.data;
  if (!message || typeof message.type !== 'string') return;

  switch (message.type) {
    case 'init':
      if (state !== 'new') {
        post({ type: 'error', id: message.id, message: `init in state ${state}` });
        return;
      }
      init(message.id);
      return;

    case 'compile':
      if (state === 'broken' || state === 'new' || state === 'initializing') {
        post({ type: 'error', id: message.id, message: `compile in state ${state}` });
        return;
      }
      // The refusal that would show up if the adapter's queue ever stopped
      // serialising: the worker answers, but with a failed build.
      if (state === 'compiling') {
        post(failed(message.id, `a compile is already in flight (${inFlight})`));
        return;
      }
      compile(message);
      return;

    case 'cancel':
      if (inFlight === undefined) {
        post({ type: 'error', id: message.id, message: 'nothing in flight' });
        return;
      }
      if (inFlight !== message.id) {
        post({
          type: 'error',
          id: message.id,
          message: `nothing in flight for ${message.id}; ${inFlight} is running`,
        });
        return;
      }
      // The compile is answered as cancelled and the worker is spent. The
      // build keeps running underneath — `deliver` drops what it eventually
      // produces — and the adapter must terminate this worker.
      cancelledIds.add(message.id);
      state = 'broken';
      post({ ...failed(message.id, 'cancelled'), cancelled: true });
      return;
  }
});
