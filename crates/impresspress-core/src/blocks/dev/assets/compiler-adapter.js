// The page half of the dev sandbox's compiler protocol.
//
// `examples/dev-sandbox/compiler/` packages Rubrc — rustc, cargo and LLVM
// built to wasm and composed into one component — as a module worker that
// speaks the message protocol in `compiler/src/protocol.ts`. This file is the
// other end of that protocol: the class `/b/dev` uses to turn "compile this
// crate" into a worker session, and a worker session back into a
// `CompileResult`.
//
// # Why this is a module and not part of `dev.js`
//
// `dev.js` is a TAIL composed into one IIFE with `webmcp-core.js`
// (`blocks/dev/assets.rs`), and everything in it is deliberately unreachable
// from outside that closure. The compiler is the one piece of the workspace
// that has a surface worth testing on its own — a protocol with a queue, a
// timeout and four failure modes — and a class sealed inside an IIFE cannot
// be driven by a test. So it ships as an ES module with a named export, and
// `dev.js` becomes a module script that imports it. The class is never hung
// off the page's global object: a global would put the compiler on the same
// footing as the page's own tools, which anything running in this document —
// including the site the preview iframe renders — could then reach.
//
// It imports nothing. `webmcp-core.js`'s `buildRequest`/`toolOptions` are
// about calling HTTP tools over `fetch`; a worker session has no HTTP in it.
//
// # The shape of a session (compiler/README.md is the long version)
//
//     new BrowserRustCompiler(manifest)   // from /__impresspress_dev/compiler/manifest.json
//     await c.initialize(onProgress)      // 'download' … 'initializing' … ready (~7 s)
//     await c.compile({ crateName, files, onProgress })   // 'compiling' … result
//     await c.cancel()                    // the in-flight compile resolves cancelled
//     await c.dispose()
//
// Two properties of the worker drive most of what is below:
//
//  * **It runs one compile at a time, and refuses a second rather than
//    queueing it.** The queue therefore lives here — see `compile`.
//  * **`cancel` spends the worker.** Rubrc's shell runs a command on a
//    session thread that nothing outside it can unwind, so a cancelled worker
//    answers the compile with `{ cancelled: true }` and marks itself broken.
//    The adapter must `terminate()` it and `init` a fresh one, which costs a
//    re-instantiation (~7 s), not a re-download. That is why `cancel` and the
//    compile timeout share one code path: both end in a destroyed worker.
//    Note that a `cancel` carries the COMPILE's id, not one of its own, and a
//    cancel naming anything else is answered `error` and changes nothing —
//    which is the worker refusing to be bricked by a stray click, and the
//    reason the adapter must never invent an id for it.
//
// The 120 s budget is the adapter's policy, not the worker's: the worker's own
// ten-minute ceiling is a backstop for a wedged shell. A worker that answers
// slowly is not misbehaving, it is compiling — so the timeout does not treat
// it as a protocol violation, it cancels it.

/**
 * The largest artifact this page will carry.
 *
 * `impresspress-core`'s `blocks::dev::validation::MAX_ARTIFACT_BYTES` — the
 * limit `POST /b/dev/api/builds/stage` enforces on the other side. Written as
 * the byte count rather than `4 * 1024 * 1024` so a test in `dev_page.rs` can
 * assert the two halves still agree.
 */
const MAX_ARTIFACT_BYTES = 4194304;

/**
 * How long a single `compile` may run before the adapter gives up on it.
 *
 * The probe's own figure for the `hello` template is ~38 s
 * (`compiler/README.md`), and the worker has its own ten-minute ceiling per
 * shell command. This is the *page's* patience, not the compiler's: past two
 * minutes the human has no way to tell a slow build from a wedged one, and a
 * wedged worker cannot be unwedged — only replaced. So the timeout does what
 * `cancel` does.
 */
const COMPILE_TIMEOUT_MS = 120000;

/**
 * How long `cancel` waits for the worker to answer before killing it.
 *
 * The answer is the cancelled compile's own terminal message — one `result`
 * per request, cancel included — so this is a real wait, not a courtesy: it is
 * what lets the page report the compile as the WORKER saw it rather than as
 * the adapter guessed. Bounded, because a worker wedged badly enough not to
 * answer is exactly what a cancel is for. The worker's message loop is the
 * WASI farm, not the blocked session thread, so it answers immediately or not
 * at all; two seconds is generous.
 */
const CANCEL_GRACE_MS = 2000;

/**
 * A diagnostic, exactly as `protocol.ts` defines it.
 *
 * @typedef {{ file: string, line: number, column: number,
 *             severity: 'error' | 'warning', message: string, code?: string }} Diagnostic
 */

/**
 * What one `compile` call answers with.
 *
 * `success: false` is an ordinary answer — a crate that does not compile is
 * the normal case in a sandbox — so `compile` resolves with it rather than
 * rejecting. It rejects only when the adapter itself cannot go on: a protocol
 * violation, a worker that failed to start, a disposed compiler.
 *
 * @typedef {{
 *   buildId: string,
 *   success: boolean,
 *   cancelled: boolean,
 *   artifact: Uint8Array | null,
 *   artifactSha256: string | null,
 *   stdout: string,
 *   stderr: string,
 *   diagnostics: Diagnostic[],
 *   elapsedMs: number,
 *   compilerVersion: string | null
 * }} CompileResult
 */

/** Lowercase hex SHA-256 of a buffer, the form `builds/stage` records. */
async function sha256Hex(buffer) {
  const digest = await crypto.subtle.digest('SHA-256', buffer);
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, '0')).join('');
}

/**
 * The page's compiler session.
 *
 * One instance owns at most one worker at a time. The worker is created
 * lazily by `initialize` and destroyed by `cancel`, `dispose`, a compile
 * timeout, or any protocol violation — and every one of those leaves the
 * instance usable: the next `initialize` (or the `initialize` a queued
 * `compile` does for itself) builds a fresh one.
 */
export class BrowserRustCompiler {
  /** The manifest this session was constructed from. */
  #manifest;

  /** The live worker, or `null` when there is none. */
  #worker = null;

  /**
   * The in-flight-or-settled `initialize` for `#worker`, or `null`.
   *
   * Doubles as the idempotence latch: a second `initialize` returns this
   * promise rather than starting a second toolchain, and a rejection clears
   * it so the call after that starts over.
   */
  #ready = null;

  /** Request id → `{ resolve, reject, onProgress, timer, startedAt }`. */
  #pending = new Map();

  /** Tail of the promise chain that serialises `compile` calls. */
  #queue = Promise.resolve();

  /** The compile the worker is running, or `null`. */
  #inFlight = null;

  /** Monotonic source of request ids. */
  #seq = 0;

  /** What `rustc --version` printed inside the VFS, once `ready` said so. */
  #rustcVersion = null;

  #disposed = false;

  /**
   * @param {{ version: string, entry: string, target: string }} manifest
   *   `/__impresspress_dev/compiler/manifest.json`, parsed.
   */
  constructor(manifest) {
    if (!manifest || typeof manifest !== 'object') {
      throw new TypeError('BrowserRustCompiler: the compiler manifest is required');
    }
    // `entry` becomes the URL of a script this page runs. It arrives over the
    // network, so it is checked here rather than trusted: a root-relative
    // path and nothing else. `//host/x.js` is a protocol-relative URL — it
    // starts with `/` and is cross-origin — which is exactly the shape a
    // tampered manifest would take, so it is excluded by name.
    if (
      typeof manifest.entry !== 'string' ||
      !manifest.entry.startsWith('/') ||
      manifest.entry.startsWith('//')
    ) {
      throw new TypeError(
        'BrowserRustCompiler: manifest.entry must be a same-origin absolute path, got ' +
          JSON.stringify(manifest.entry)
      );
    }
    if (typeof manifest.version !== 'string' || manifest.version === '') {
      throw new TypeError('BrowserRustCompiler: manifest.version must be a non-empty string');
    }
    // The compile message cannot be formed without it, and the sandbox builds
    // for exactly one triple — the one the vendored sysroot is for.
    if (typeof manifest.target !== 'string' || manifest.target === '') {
      throw new TypeError('BrowserRustCompiler: manifest.target must be a non-empty string');
    }
    this.#manifest = manifest;
  }

  /** The pinned compiler bundle's version — the rubrc sha the URLs carry. */
  get version() {
    return this.#manifest.version;
  }

  /**
   * Start the toolchain, or join the start already under way.
   *
   * Resolves with the `rustc --version` string the worker reported. Idempotent:
   * calling it again after it resolved is free, and only the FIRST caller's
   * `onProgress` sees the download — a later caller has nothing left to
   * watch. A rejection is not sticky: the failed worker is destroyed and the
   * next call builds another.
   *
   * @param {(progress: { stage: string, loaded?: number, total?: number, detail?: string }) => void} [onProgress]
   * @returns {Promise<string>}
   */
  async initialize(onProgress) {
    if (this.#disposed) {
      throw new Error('the compiler has been disposed');
    }
    if (this.#ready) {
      return this.#ready;
    }
    const ready = this.#start(onProgress);
    this.#ready = ready;
    // Clear the latch on failure so the next call starts a fresh worker.
    // Guarded on identity because a `dispose` or a violation in between has
    // already replaced (or nulled) it, and this handler must not resurrect
    // a state that moved on without it.
    ready.catch(() => {
      if (this.#ready === ready) {
        this.#ready = null;
      }
    });
    return ready;
  }

  /**
   * Compile one crate.
   *
   * Resolves with a `CompileResult` — including for a crate that does not
   * compile, which is an answer and not an error. Rejects only on an adapter
   * failure: a worker that would not start, a protocol violation, a disposed
   * compiler.
   *
   * Calls are serialised. The worker refuses a second `compile` while one is
   * in flight (`protocol.ts`: "a queue would only hide a bug in the page"),
   * so the queue is here, where the page can see it: two calls run in the
   * order they were made, and the second one's `initialize` picks up whatever
   * worker the first one left behind — a fresh one, if the first was
   * cancelled.
   *
   * @param {{ crateName: string, files: Record<string, string>,
   *           onProgress?: (progress: { stage: string, detail?: string }) => void }} request
   * @returns {Promise<CompileResult>}
   */
  compile(request) {
    // Chained on both settlements: one compile's failure must not strand
    // every compile queued behind it.
    const run = this.#queue.then(
      () => this.#runCompile(request),
      () => this.#runCompile(request)
    );
    this.#queue = run.then(
      () => {},
      () => {}
    );
    return run;
  }

  /**
   * Abandon the compile in flight, if there is one.
   *
   * The in-flight `compile` resolves with `{ success: false, cancelled: true }`
   * — it does not reject; the page asked for this. The worker is spent
   * afterwards whatever it answers, so it is destroyed and the next `compile`
   * pays for a fresh `init`.
   *
   * A no-op when nothing is compiling.
   */
  async cancel() {
    await this.#abandonInFlight('cancelled by the workspace');
  }

  /**
   * Terminate the worker and refuse further use of this instance.
   *
   * Anything still in flight rejects: a disposed compiler has no answer to
   * give, and a pending promise that never settles would hang the caller.
   */
  async dispose() {
    this.#disposed = true;
    this.#destroy(new Error('the compiler was disposed'));
  }

  // ---- the worker ---------------------------------------------------------

  /** Create the worker and drive it to `ready`. */
  async #start(onProgress) {
    const worker = new Worker(this.#manifest.entry, { type: 'module' });
    // Bound to `worker`, not to `this.#worker`, so a listener that survives
    // a teardown (the event was already queued when `terminate()` ran) can
    // tell that it belongs to a worker this instance no longer owns.
    worker.addEventListener('message', (event) => this.#onMessage(worker, event));
    // A module worker whose script 404s, or whose top level throws, reports
    // it here and nowhere else — without this the `init` request would simply
    // never be answered and `initialize` would hang for ever.
    worker.addEventListener('error', (event) => {
      event.preventDefault();
      this.#failWorker(worker, 'the compiler worker failed to start: ' + describeErrorEvent(event));
    });
    // A message the page cannot structured-clone back. Nothing in the
    // protocol should ever produce one, which is why it is a failure and not
    // a warning.
    worker.addEventListener('messageerror', () => {
      this.#failWorker(worker, 'the compiler worker sent a message this page could not read');
    });
    this.#worker = worker;

    const id = this.#nextId('init');
    // No timeout. The variable part of `init` is a 75 MB download whose
    // progress the worker reports as it goes (`stage: 'download'`), so a
    // stall is visible in the panel; a ceiling here would refuse slow
    // connections for a compiler that is still on its way.
    const ready = await this.#request(id, { type: 'init', id }, { onProgress });
    this.#rustcVersion = typeof ready.rustcVersion === 'string' ? ready.rustcVersion : null;
    return this.#rustcVersion;
  }

  /** One compile, once the queue has let it through. */
  async #runCompile({ crateName, files, onProgress }) {
    if (this.#disposed) {
      throw new Error('the compiler has been disposed');
    }
    if (typeof crateName !== 'string' || crateName === '') {
      throw new TypeError('compile: crateName must be a non-empty string');
    }
    if (!files || typeof files !== 'object') {
      throw new TypeError('compile: files must be an object of path → contents');
    }
    // The same callback receives the toolchain's start-up when this compile
    // is the one that has to pay for it — after a cancel, or on the very
    // first build if the page never called `initialize` itself. The stages
    // are the protocol's own, so the caller cannot tell (and need not care)
    // which half of the work it is watching.
    await this.initialize(onProgress);

    const id = this.#nextId('build');
    this.#inFlight = id;
    try {
      return await this.#request(
        id,
        {
          type: 'compile',
          id,
          crateName,
          files,
          target: this.#manifest.target,
          // Always release. The sandbox's artifact ceiling is 4 MiB and a
          // debug build of the same crate is several times the size of the
          // release one, so a debug switch would only offer the page a way
          // to produce artifacts the server refuses.
          release: true
        },
        {
          onProgress,
          timeoutMs: COMPILE_TIMEOUT_MS,
          onTimeout: () =>
            this.#abandonInFlight('the compile did not finish within ' + COMPILE_TIMEOUT_MS + ' ms')
        }
      );
    } finally {
      if (this.#inFlight === id) {
        this.#inFlight = null;
      }
    }
  }

  /**
   * Send one request and wait for the message that settles it.
   *
   * @param {string} id
   * @param {object} message
   * @param {{ onProgress?: Function, timeoutMs?: number, onTimeout?: Function }} options
   */
  #request(id, message, options = {}) {
    const worker = this.#worker;
    if (!worker) {
      return Promise.reject(new Error('the compiler worker is gone'));
    }
    let entry;
    const answered = new Promise((resolve, reject) => {
      entry = {
        resolve,
        reject,
        onProgress: options.onProgress,
        startedAt: Date.now(),
        timer: options.timeoutMs
          ? setTimeout(() => {
              options.onTimeout();
            }, options.timeoutMs)
          : null
      };
      this.#pending.set(id, entry);
    });
    // A settlement signal `#abandonInFlight` can wait on without competing
    // for the answer itself — whoever asked for the compile still gets it.
    entry.finished = answered.then(
      () => {},
      () => {}
    );
    worker.postMessage(message);
    return answered;
  }

  /** Pop a pending request, clearing its timer. */
  #take(id) {
    const entry = this.#pending.get(id);
    if (!entry) {
      return null;
    }
    if (entry.timer !== null) {
      clearTimeout(entry.timer);
    }
    this.#pending.delete(id);
    return entry;
  }

  #nextId(kind) {
    this.#seq += 1;
    return kind + '-' + this.#seq;
  }

  // ---- the protocol -------------------------------------------------------

  #onMessage(worker, event) {
    // A message from a worker this instance has already replaced. It cannot
    // settle anything — everything that worker owed was rejected when it was
    // destroyed — so it is dropped rather than mistaken for the live one.
    if (worker !== this.#worker) {
      return;
    }
    const message = event.data;
    if (!message || typeof message.type !== 'string') {
      this.#violation('the compiler worker sent a message with no type');
      return;
    }

    switch (message.type) {
      case 'progress': {
        const entry = this.#pending.get(message.id);
        // Progress is advisory: it settles nothing, and a late one for a
        // build the page abandoned (or for a request that has just been
        // answered) is a race, not a defect. Only messages that claim to
        // ANSWER something the page never asked are treated as violations.
        if (entry && entry.onProgress) {
          // Called last in the branch on purpose: a callback the page
          // supplied is the page's to get right, and if it throws, the
          // exception belongs in the console — not swallowed here, and with
          // nothing left in this handler for it to skip.
          entry.onProgress({
            stage: message.stage,
            loaded: message.loaded,
            total: message.total,
            detail: message.detail
          });
        }
        return;
      }

      case 'ready': {
        const entry = this.#take(message.id);
        if (!entry) {
          this.#unexpectedId('ready', message.id);
          return;
        }
        entry.resolve(message);
        return;
      }

      case 'result': {
        const entry = this.#take(message.id);
        if (!entry) {
          this.#unexpectedId('result', message.id);
          return;
        }
        // Validated here, synchronously, and not in the async settle below:
        // a violation has to destroy the worker and reject everything it
        // owed, and by the time an `await` had resumed, the queue could
        // already have posted the next compile into a worker that is about
        // to be killed.
        const artifact = message.artifact;
        if (artifact !== undefined) {
          if (!(artifact instanceof ArrayBuffer)) {
            entry.reject(new Error('the compiler worker sent an artifact that is not an ArrayBuffer'));
            this.#violation('the compiler worker sent an artifact that is not an ArrayBuffer');
            return;
          }
          if (artifact.byteLength > MAX_ARTIFACT_BYTES) {
            const tooBig =
              'the compiler produced ' +
              artifact.byteLength +
              ' bytes, over the sandbox limit of ' +
              MAX_ARTIFACT_BYTES;
            entry.reject(new Error(tooBig));
            this.#violation(tooBig);
            return;
          }
        }
        // The digest is the only asynchronous part, and it cannot fail in a
        // way the protocol cares about.
        this.#settleResult(entry, message).catch((error) => entry.reject(error));
        return;
      }

      case 'error': {
        // The worker sets `state = "broken"` before it posts this, so there
        // is nothing left to talk to whether or not the id is one we know.
        const entry = this.#take(message.id);
        const error = new Error('the compiler worker failed: ' + message.message);
        if (entry) {
          entry.reject(error);
        }
        this.#destroy(error);
        return;
      }

      default:
        this.#violation('the compiler worker sent an unknown message type ' + message.type);
    }
  }

  /** Turn a `result` into the page's `CompileResult`. */
  async #settleResult(entry, message) {
    let artifact = null;
    let artifactSha256 = null;
    if (message.artifact !== undefined) {
      // The buffer was TRANSFERRED, not copied: it is the page's now, and
      // the view below is over the same memory rather than a second copy of
      // a multi-megabyte module.
      artifact = new Uint8Array(message.artifact);
      artifactSha256 = await sha256Hex(message.artifact);
    }
    entry.resolve({
      buildId: message.id,
      success: message.success === true,
      cancelled: message.cancelled === true,
      artifact,
      artifactSha256,
      stdout: message.stdout,
      stderr: message.stderr,
      diagnostics: message.diagnostics,
      elapsedMs: message.elapsedMs,
      compilerVersion: this.#rustcVersion
    });
  }

  #unexpectedId(kind, id) {
    this.#violation(
      'the compiler worker answered ' + kind + ' for a request this page did not make: ' + id
    );
  }

  // ---- teardown -----------------------------------------------------------

  /**
   * Give up on the compile in flight and replace the worker.
   *
   * The single path behind `cancel()` and the compile timeout, because the
   * protocol gives them the same ending: the worker cannot unwind a running
   * compile, so the only way to stop one is to stop the worker.
   */
  async #abandonInFlight(reason) {
    const buildId = this.#inFlight;
    const build = buildId === null ? null : this.#pending.get(buildId);
    if (!build) {
      return;
    }
    this.#inFlight = null;
    // Whatever happens from here this build is over, so the 120 s timer must
    // not fire again on the way out — a timeout that cancelled a compile
    // would otherwise be free to cancel it a second time.
    if (build.timer !== null) {
      clearTimeout(build.timer);
      build.timer = null;
    }

    const worker = this.#worker;
    if (worker) {
      // The COMPILE's id, not one of ours: a `cancel` naming anything else is
      // answered `error` and changes nothing, which would leave the build
      // running under a worker about to be killed and the page holding a
      // promise nobody will settle.
      worker.postMessage({ type: 'cancel', id: buildId });
      await Promise.race([
        build.finished,
        new Promise((resolve) => setTimeout(resolve, CANCEL_GRACE_MS))
      ]);
    }

    // Still unanswered after the grace — or never asked, because there was no
    // worker left to ask. The adapter says what the worker would not.
    const stranded = this.#take(buildId);
    if (stranded) {
      stranded.resolve({
        buildId,
        success: false,
        cancelled: true,
        artifact: null,
        artifactSha256: null,
        stdout: '',
        stderr: reason,
        diagnostics: [],
        elapsedMs: Date.now() - stranded.startedAt,
        compilerVersion: this.#rustcVersion
      });
    }
    this.#destroy(new Error('the compiler worker was replaced: ' + reason));
  }

  /**
   * The worker broke its own protocol.
   *
   * Nothing it says afterwards can be trusted — not this answer, and not the
   * artifact bytes it might send next — so it is destroyed and everything it
   * owed is rejected. The instance stays usable: the next call starts a new
   * worker, which is the only recovery there is.
   */
  #violation(what) {
    this.#destroy(new Error('compiler protocol violation: ' + what));
  }

  /** A worker that failed as a worker, rather than as a protocol speaker. */
  #failWorker(worker, message) {
    if (worker !== this.#worker) {
      return;
    }
    this.#destroy(new Error(message));
  }

  /** Terminate the worker and reject everything it still owed. */
  #destroy(error) {
    const worker = this.#worker;
    this.#worker = null;
    this.#ready = null;
    this.#inFlight = null;
    const owed = Array.from(this.#pending.values());
    this.#pending.clear();
    if (worker) {
      worker.terminate();
    }
    for (const entry of owed) {
      if (entry.timer !== null) {
        clearTimeout(entry.timer);
      }
      entry.reject(error);
    }
  }
}

/** Whatever a worker `ErrorEvent` will tell us about why it died. */
function describeErrorEvent(event) {
  if (event.message) {
    return event.message;
  }
  if (event.error) {
    return String(event.error);
  }
  return 'no reason given';
}
