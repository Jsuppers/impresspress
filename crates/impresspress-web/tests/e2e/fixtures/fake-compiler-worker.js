// A stand-in for `examples/dev-sandbox/compiler/dist/<pin>/worker.js`.
//
// The real worker is 365 MiB of composed toolchain and takes seven seconds to
// reach `ready` and forty to compile anything. `dev-compiler.spec.ts` is about
// the PAGE half — the queue, the timeout, the teardown, the four ways the
// adapter refuses a worker that misbehaves — and none of that is any different
// against real rustc. So this file speaks the same protocol in microseconds,
// and `dev-compile.spec.ts` (Plan 3 Task 6) drives the real one.
//
// The protocol is `examples/dev-sandbox/compiler/src/protocol.ts` and the
// STATE MACHINE below is `worker-entry.ts`'s, deliberately: a fake that
// answered a second `compile` while one was in flight, or that kept working
// after a `cancel`, would let the adapter's queue and its terminate-and-
// re-`init` rule pass while being wrong. The refusals are the assertions.
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

/** The artifact a successful fake build produces: 16 bytes, 0x00…0x0f. */
const ARTIFACT_BYTES = 16;

/** How long a compile takes, unless the crate name asks for longer. */
const COMPILE_MS = 60;

/** Bumped per compile so the page can prove the queue preserved its order. */
let builds = 0;

const post = (message, transfer = []) => {
  self.postMessage(message, transfer);
};

const fail = (id, message, extra = {}) => {
  post({
    type: 'result',
    id,
    success: false,
    stdout: '',
    stderr: message,
    diagnostics: [],
    elapsedMs: 0,
    ...extra,
  });
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
  // Two progress messages then `ready` — the real worker's own sequence:
  // the component download, then the wait for the shell to come up.
  post({ type: 'progress', id, stage: 'download', loaded: 0, total: 75124002 });
  post({ type: 'progress', id, stage: 'initializing', detail: 'waiting for the shell' });
  state = 'ready';
  post({ type: 'ready', id, rustcVersion: 'rustc 1.90.0-nightly (fake worker)' });
};

const compile = (message) => {
  const started = Date.now();
  const build = (builds += 1);
  state = 'compiling';
  post({ type: 'progress', id: message.id, stage: 'compiling', detail: 'cargo build' });

  const slow = message.crateName === 'slow';
  setTimeout(() => {
    state = 'ready';
    const stdout = `fake build #${build}: ${message.crateName}`;
    const elapsedMs = Date.now() - started;

    switch (message.crateName) {
      case 'protocol-wrong-id':
        // A result for a request the page never made. Everything else about
        // it is well formed, so the id is the only thing that can be wrong.
        post({
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
        post({
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
        post(
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
        state = 'broken';
        post({ type: 'error', id: message.id, message: 'the compiler image is corrupt' });
        return;
      default:
        break;
    }

    const diagnostics = failure(message.files);
    if (diagnostics) {
      post({
        type: 'result',
        id: message.id,
        success: false,
        stdout,
        stderr: '',
        diagnostics,
        elapsedMs,
      });
      return;
    }
    const bytes = artifact();
    post(
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
  }, slow ? 5000 : COMPILE_MS);
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
      // The real worker answers a concurrent compile with a failed result
      // rather than queueing it. If the adapter's queue ever stopped
      // serialising, this is what the page would see.
      if (state === 'compiling') {
        fail(message.id, 'a compile is already in flight');
        return;
      }
      if (state !== 'ready') {
        fail(message.id, `compile in state ${state}`);
        return;
      }
      compile(message);
      return;

    case 'cancel':
      // Answered with the CANCEL's id, not the compile's — the compile it
      // abandoned is on a thread nothing can unwind, so it is never answered
      // at all. The worker is spent; the adapter must terminate it.
      state = 'broken';
      fail(message.id, 'cancelled', { cancelled: true });
      return;
  }
});
