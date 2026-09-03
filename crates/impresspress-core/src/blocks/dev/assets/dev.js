// The `/b/dev` workspace page: registers this page's own agent tools, drives
// the file/editor/preview/progress panes over the sandbox's JSON API, and
// keeps the live-site iframe in step with what the agent publishes.
//
// Authored as a TAIL, not a script. `blocks/dev/assets.rs` composes it with
// `ui/assets/webmcp-core.js` into a single IIFE, so `buildRequest` and
// `toolOptions` are in scope here exactly as they are in `webmcp.js`, and
// nothing declared below reaches `window`. No outer IIFE and no
// `'use strict'` — the wrapper supplies both.
//
// The composed script is an ES MODULE (`<script type="module">`), because it
// imports `BrowserRustCompiler` from `/b/dev/static/compiler-adapter.js` —
// the class that drives the in-browser Rust toolchain. The import declaration
// lives in `assets.rs` (`DEV_JS_IMPORTS`), since an `import` may only stand
// at a module's top level and everything in this file is inside the wrapper's
// function body. The binding is in scope here; the Compile button is what
// will use it. Two consequences for anything written below: a module's top
// level is already strict (the wrapper's directive is now belt and braces),
// and a module has no currentScript — the `document` property that names the
// running script tag is null in one — which is why nothing here reads it.
//
// Every tool registered here is PAGE-scoped: the list comes from
// `/b/dev/api/tools.json` (the curated `dev_*` / `shop_*` allowlist), never
// from the deployment-wide manifest `webmcp.js` owns, and it is torn down
// when this page goes away. An agent on any other page of the site sees the
// site's tools and none of these.

// ---- the API client -------------------------------------------------------

// The lifetime of everything this page registers. Aborting it is the single
// teardown path: a `pagehide` the document does not come back from fires it,
// and so does the first sign that the session behind the page is gone
// (below).
var abort = new AbortController();

// A 401/403 means this document is still on screen but its session is not.
// Tearing the tools down is the honest response — an agent left holding
// tools whose every call now fails would keep retrying against a page that
// cannot answer, and the human would see no reason why.
function check(response) {
  if (response.status === 401 || response.status === 403) {
    abort.abort();
  }
  return response;
}

var api = {
  get: function (path) {
    return fetch(path, { credentials: 'same-origin' }).then(check);
  },
  post: function (path, body) {
    return fetch(path, {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body || {})
    }).then(check);
  }
};

// Parse a response the caller expects to have succeeded. Reads the body as
// text first so a failure can carry the server's own message into the log
// instead of a bare status — the sandbox's refusals (a path outside the
// workspace, a quota, a stale hash) are all things the human needs to read.
async function json(response) {
  var text = await response.text();
  if (!response.ok) {
    throw new Error('HTTP ' + response.status + ': ' + text);
  }
  return JSON.parse(text);
}

// ---- the log --------------------------------------------------------------

var logEl = document.getElementById('dev-log');
var logLines = [];
var LOG_LIMIT = 200;

function log(line) {
  // Wall-clock time-of-day: the panel's job is to let a human line up what
  // the page did with what they asked the agent for, so the absolute time
  // is more use than a relative one.
  logLines.push(new Date().toISOString().slice(11, 19) + '  ' + line);
  if (logLines.length > LOG_LIMIT) {
    logLines.splice(0, logLines.length - LOG_LIMIT);
  }
  logEl.textContent = logLines.join('\n');
  logEl.scrollTop = logEl.scrollHeight;
}

function logError(error) {
  log('error: ' + (error && error.message ? error.message : String(error)));
}

// ---- progress -------------------------------------------------------------

var steps = document.getElementById('dev-progress-steps');

// The activation ladder, in `ActivationPhase`'s own order and spelling (the
// serde form is what `/b/dev/api/status` sends). `idle` — nothing in flight
// — and `failed` — abandoned, the previous generation still live — are
// states of the whole activation rather than steps within it, so they are
// rendered on the list itself via `data-phase` and never as extra steps.
var PHASES = [
  ['validating', 'Validating'],
  ['building_runtime', 'Building runtime'],
  ['publishing', 'Publishing'],
  ['active', 'Active']
];

var lastPhase = null;
var lastDetail = null;
var lastActiveGeneration = null;

function renderStatus(status) {
  var activation = status.activation;
  var phase = activation ? activation.phase : 'idle';
  var reached = -1;
  PHASES.forEach(function (entry, index) {
    if (entry[0] === phase) {
      reached = index;
    }
  });

  steps.setAttribute('data-phase', phase);
  steps.innerHTML = '';
  PHASES.forEach(function (entry, index) {
    var li = document.createElement('li');
    li.className = 'dev-step';
    li.setAttribute(
      'data-state',
      reached < 0 ? 'pending' : index < reached ? 'done' : index === reached ? 'current' : 'pending'
    );
    li.textContent = entry[1];
    steps.appendChild(li);
  });

  if (phase !== lastPhase) {
    log('activation: ' + phase + (activation ? ' (' + activation.generation_id + ')' : ''));
    lastPhase = phase;
  }
  // `detail` is free text the activation may carry for exactly this panel.
  // Nothing sets it yet, so it is logged only when there is something to
  // log rather than emitting a blank line on every poll.
  if (activation && activation.detail && activation.detail !== lastDetail) {
    log(activation.detail);
    lastDetail = activation.detail;
  }
  var activeId = status.active_generation ? status.active_generation.id : null;
  if (activeId !== lastActiveGeneration) {
    log('live generation: ' + (activeId || 'none'));
    lastActiveGeneration = activeId;
  }
}

var polling = null;
var lastRuntimeGeneration = null;

// How many mutating calls are outstanding. A COUNT, not a flag: two of them
// can overlap — an agent firing `shop_create_product` three times without
// awaiting each, or a human saving while a tool call runs — and a flag would
// let the first one to finish stop the interval and run the catch-up while
// the others are still in flight, blanking the panel mid-activation and
// reading a workspace that is about to change again.
var outstanding = 0;

// ~300 ms while a mutating call is outstanding (design §7.5). There is no
// push channel: the block answers `no-store` precisely so this poll always
// sees the journal as it stands.
function startPolling() {
  if (polling !== null) {
    return;
  }
  polling = setInterval(function () {
    api.get('/b/dev/api/status').then(json).then(observe).catch(logError);
  }, 300);
}

function stopPolling() {
  if (polling === null) {
    return;
  }
  clearInterval(polling);
  polling = null;
}

// One status response, applied. When the runtime underneath has been rebuilt
// the deployment-wide tool set has changed with it — a compiled block brings
// its own tools, a removed one takes them away — so the site's registrations
// are refreshed. The page-scoped tools registered here are untouched by that
// refresh (`webmcp.js` only drops what `webmcp.js` itself registered).
function observe(status) {
  renderStatus(status);
  if (lastRuntimeGeneration !== null && status.runtime_generation !== lastRuntimeGeneration) {
    log('runtime rebuilt (generation ' + status.runtime_generation + ') — refreshing site tools');
    refreshSiteTools();
  }
  lastRuntimeGeneration = status.runtime_generation;
  return status;
}

// `webmcp.js` is the last script in the body (`ui/layout.rs`) while this one
// sits inside the page body, so on a `defer` load this script runs FIRST and
// `window.__impresspressWebmcp` may not exist yet. Every use is therefore
// guarded rather than assumed — and a browser without WebMCP never gets the
// object at all.
function refreshSiteTools() {
  if (window.__impresspressWebmcp) {
    return window.__impresspressWebmcp.refresh();
  }
  return Promise.resolve();
}

// Wrap a tool's `execute` so the panel is live for the duration of the call
// and the page catches up with what the call changed. The catch-up is in a
// `finally`: a refused write still moved the workspace's mtime nowhere, but
// a PARTIALLY applied one (a site write that published and then failed to
// activate) leaves the page showing a workspace that no longer exists.
function withProgress(execute) {
  return async function (args) {
    // Once the session is gone (`abort.signal.aborted`), the abort handler
    // has already zeroed `outstanding` and stopped the interval — this is a
    // straggler call reaching the wrapper after that (a browser that ignored
    // `registerTool`'s `signal`, or a call already queued on the event
    // loop). Joining the count back in would reopen the exact race this
    // guard exists to close, so run it plain instead of tracking it.
    if (abort.signal.aborted) {
      return execute(args);
    }
    outstanding += 1;
    startPolling();
    try {
      return await execute(args);
    } finally {
      // Clamp rather than trust the increment/decrement to stay paired: the
      // abort handler can reset `outstanding` to 0 out from under a call
      // that is still in flight (it was never given `abort.signal`, so it
      // always reaches this `finally`). Without the clamp that decrement
      // drives the count negative, `outstanding === 0` becomes unreachable,
      // and the next call would start a poll interval nothing could stop.
      outstanding = Math.max(0, outstanding - 1);
      // The LAST call out of the room turns the lights off and does the
      // catch-up once, rather than every call racing to redraw a workspace
      // its siblings are still changing. Skip it once aborted — the handler
      // already stopped polling, and the endpoints below would just 403.
      if (outstanding === 0 && !abort.signal.aborted) {
        stopPolling();
        await refreshAfterChange();
      }
    }
  };
}

async function refreshAfterChange() {
  try {
    observe(await json(await api.get('/b/dev/api/status')));
    reloadPreview();
    await loadFiles();
  } catch (error) {
    // Never let the catch-up replace the tool's own result: the agent asked
    // for a write, and whether the panel could redraw afterwards is this
    // page's problem, not the answer to that call.
    logError(error);
  }
}

function reloadPreview() {
  var frame = document.getElementById('dev-preview-frame');
  try {
    // The frame keeps `allow-same-origin`, so the parent may drive its
    // location directly. Reassigning `src` would work too but pushes a
    // history entry onto the frame; `reload()` replaces what it shows.
    frame.contentWindow.location.reload();
  } catch (error) {
    frame.setAttribute('src', frame.getAttribute('src'));
  }
}

// ---- tools ----------------------------------------------------------------

// Which tools change the site, and therefore want the progress panel live
// and the panes refreshed afterwards. Everything else is a read, and a read
// must NOT reload the preview: an agent listing the catalog between two
// writes would otherwise flicker the iframe and re-fetch the file tree for
// nothing.
//
// The five `dev_` names are the mutating half of the control plane — its
// reads (`dev_status`, `dev_list_files`, `dev_read_file`,
// `dev_list_generations`, `dev_get_generation`, `dev_read_reference`) change
// nothing. `shop_` covers the products family except its three listers
// (`shop_list_products`, `shop_list_groups`, `shop_list_offers`), which the
// negative lookahead excludes. `dev_compile_block` joins the list when it
// lands; it mutates too.
var MUTATING = /^(dev_write_file|dev_delete_file|dev_create_block|dev_rollback|dev_remove_block|shop_(?!list_))/;

// Every name this page registered. `registerTool`'s options bag takes an
// `AbortSignal`, but a browser (or a polyfill) that ignores it would leave
// this page's tools live on the agent after the page is gone — with the
// document's session cookie no longer riding along, so every call 403s. The
// list is the fallback: on abort, unregister exactly these by name.
var registered = [];

function registerPageTool(options) {
  document.modelContext.registerTool(options, { signal: abort.signal });
  registered.push(options.name);
}

function unregisterPageTools() {
  // `unregisterPageTools` runs on every `pagehide` and on every 401/403,
  // regardless of whether registration ever happened — a browser with no
  // WebMCP support at all has no `document.modelContext` (`registered` is
  // already `[]` in that case, from the guard below), so `document
  // .modelContext` must be checked for existence before its own methods
  // are, or this throws on unload in exactly the browsers the top-level
  // guard (`'modelContext' in document`) was written to tolerate.
  if (!document.modelContext || typeof document.modelContext.unregisterTool !== 'function') {
    registered = [];
    return;
  }
  registered.forEach(function (name) {
    try {
      document.modelContext.unregisterTool(name);
    } catch (error) {
      // Already gone — the browser honoured the signal. Both paths are
      // correct; only one of them runs on any given browser.
    }
  });
  registered = [];
}

// The session check for the manifest tools' own requests.
//
// Their `execute` comes from `toolOptions` (webmcp-core.js) and fetches
// without going through this file's `api`, so `check` never sees the
// response — and a refusal arrives as a RESULT (`isError` plus
// `Request failed (403): …`), never as a rejection. Reading that text back
// is what lets the same "the session is gone, take the tools away" rule
// apply to a tool call as to a pane refresh; without it the page would keep
// offering tools that 403 for the rest of the session.
var SESSION_GONE = /^Request failed \((401|403)\)/;

function withSessionCheck(execute) {
  return async function (args) {
    var result = await execute(args);
    var first = result && result.isError && result.content && result.content[0];
    if (first && typeof first.text === 'string' && SESSION_GONE.test(first.text)) {
      abort.abort();
    }
    return result;
  };
}

function registerFromManifest(manifest) {
  if (!manifest || !Array.isArray(manifest.tools)) {
    return;
  }
  manifest.tools.forEach(function (tool) {
    try {
      // `toolOptions` (webmcp-core.js) turns one manifest entry into the
      // registration options, `execute` included — the request it builds is
      // the same same-origin call `webmcp.js` makes for a site tool.
      var options = toolOptions(tool);
      // Session check innermost, so it sees the raw result; progress
      // outermost, so the panel is live for the whole call.
      options.execute = withSessionCheck(options.execute);
      if (MUTATING.test(tool.name)) {
        options.execute = withProgress(options.execute);
      }
      registerPageTool(options);
    } catch (error) {
      // One tool the browser rejected is not a reason to lose the rest —
      // the same per-tool guard `webmcp.js` applies.
      logError(error);
    }
  });
  log('registered ' + registered.length + ' workspace tools');
}

// `dev_compile_block` and `dev_export` have no HTTP endpoint behind them:
// compiling happens in a worker on this page, and exporting writes a bundle
// from it — neither exists in this build yet. They are registered anyway, as
// honest refusals: an agent that discovers them gets `isError` and a reason,
// which is what stops it inventing its own way to compile or export.
function notAvailable(name) {
  return {
    isError: true,
    content: [{ type: 'text', text: name + ' is not available in this build.' }]
  };
}

function registerPageLocal() {
  registerPageTool({
    name: 'dev_compile_block',
    description: 'Compile a Rust block in the browser. Not available in this build yet.',
    inputSchema: {
      type: 'object',
      properties: { name: { type: 'string' } },
      required: ['name']
    },
    execute: async function () {
      return notAvailable('dev_compile_block');
    }
  });
  registerPageTool({
    name: 'dev_export',
    description: 'Export the site as a runnable static bundle. Not available in this build yet.',
    inputSchema: { type: 'object', properties: {} },
    execute: async function () {
      return notAvailable('dev_export');
    }
  });
}

if ('modelContext' in document && typeof document.modelContext.registerTool === 'function') {
  api
    .get('/b/dev/api/tools.json')
    .then(json)
    .then(function (manifest) {
      registerFromManifest(manifest);
      registerPageLocal();
    })
    .catch(logError);
} else {
  log('this browser has no WebMCP support — the panes below still work');
}

// `pagehide`, not `unload`: it is the event a bfcache-eligible navigation
// actually fires, and it fires on the tab being closed as well.
//
// `event.persisted` is the half that matters. It is `true` when the document
// is going INTO the back/forward cache rather than away for good — it can
// come back, on Back, with its JS state intact and its session cookie still
// valid. Aborting there would be permanent (an `AbortController` cannot be
// reset), so the restored page would look alive while every tool it
// registered had been unregistered, `withProgress` short-circuited on
// `abort.signal.aborted`, and the log claiming the session expired. Nothing
// but a manual reload would bring it back. A frozen document's tools are not
// reachable by an agent on whatever page replaced it, so there is nothing to
// tear down while it sits there; if it is evicted instead of restored, the
// document is discarded and `document.modelContext` goes with it.
window.addEventListener('pagehide', function (event) {
  if (event.persisted) {
    return;
  }
  abort.abort();
});

abort.signal.addEventListener('abort', function () {
  // Nothing in flight is handed `abort.signal` — `api.get`/`api.post` only
  // ever pass `credentials` — so every mutating call still in flight WILL
  // reach its own `finally` and decrement `outstanding`. Resetting it here
  // is therefore just the interval's teardown, not a substitute for those
  // decrements; `withProgress` (above) is what keeps them from taking the
  // count negative or restarting the interval afterwards.
  outstanding = 0;
  stopPolling();
  unregisterPageTools();
  log('session expired — workspace tools removed; sign in again');
});

// ---- files and the editor -------------------------------------------------

// The file in the textarea: its path and the hash it had when it was read.
// The hash is what every write and delete sends back as `expected_sha256`,
// so a change the agent made under the human's feet is refused with a 409
// rather than silently overwritten.
var current = null;

var list = document.getElementById('dev-file-list');
var text = document.getElementById('dev-editor-text');
var title = document.getElementById('dev-editor-title');
var saveButton = document.getElementById('dev-save');

// The textarea and Save move together, and that pairing is load-bearing. A
// binary file leaves the box holding a PLACEHOLDER, not the file; saving it
// would write `(binary file, N bytes)` back as utf8 — with a matching
// `expected_sha256`, so the conflict check waves it through — destroying the
// file and publishing a generation for the loss. Locking the box without
// locking the button leaves exactly that one click available.
function setEditorEnabled(enabled) {
  text.disabled = !enabled;
  saveButton.disabled = !enabled;
}

async function loadFiles() {
  var files = (await json(await api.get('/b/dev/api/files'))).files;
  list.innerHTML = '';
  files.forEach(function (file) {
    var li = document.createElement('li');
    var link = document.createElement('a');
    link.href = '#';
    link.textContent = file.path;
    link.setAttribute('data-path', file.path);
    if (current && current.path === file.path) {
      link.setAttribute('data-open', 'true');
    }
    link.addEventListener('click', function (event) {
      event.preventDefault();
      openFile(file.path).catch(logError);
    });
    li.appendChild(link);
    list.appendChild(li);
  });
}

async function openFile(path) {
  var file = await json(await api.post('/b/dev/api/files/read', { path: path }));
  current = { path: file.path, sha256: file.sha256 };
  title.textContent = file.path;
  // A binary file has no text to edit. Say what it is and lock the box,
  // rather than dropping base64 into an editor that would write it back as
  // literal text on the next save.
  text.value =
    file.encoding === 'utf8' ? file.content : '(binary file, ' + file.size + ' bytes)';
  setEditorEnabled(file.encoding === 'utf8');
  await loadFiles();
}

// A refusal's parsed body, or `null` when it has no shape either reader
// below can use.
//
// Every `/b/dev` refusal answers JSON: a `FileConflict` for a stale hash,
// and `{error, message}` for everything else (`no_store_error` /
// `no_store_error_status`, `blocks/dev/mod.rs`). What is NOT guaranteed is
// that the response came from the block at all — an intermediary's HTML
// error page, or a truncated body, parses to nothing or to a bare string,
// and neither `'current_sha256' in body` nor `body.message` may be asked of
// those (`in` throws a `TypeError` on a non-object, which would escape as a
// raw JS error into the log in place of the server's own explanation).
//
// This covers the 401/403 too, which `check` has already turned into a
// teardown by the time a caller gets here. The log line that teardown writes
// says the tools are gone; it does not say the click that provoked it did
// nothing. Both are worth saying, and the human asked for one of them.
async function refusalBody(response) {
  var text = await response.text();
  try {
    var body = JSON.parse(text);
    return body && typeof body === 'object' ? body : null;
  } catch (error) {
    return null;
  }
}

// `/b/dev/api/files/write` answers 409 for two UNRELATED reasons that
// happen to share a status: a real hash conflict (`FileConflict` —
// `path`/`current_sha256`/`current_size`, and `current_sha256` is present
// even when its value is `null`) and the block-count quota refusal
// (`QuotaError::TooManyBlocks`, `files.rs` — a bare `{error, message}`, no
// `current_sha256` at all, because "the workspace's shape conflicts with a
// limit" is not a hash conflict). The status alone cannot tell them apart —
// only the body's shape can.
function isFileConflict(body) {
  return body !== null && 'current_sha256' in body;
}

// The server's own words for a refusal, or `fallback` when the body carries
// none. The refusals the human sees most — a path outside the workspace, a
// quota, a file that is not there — all say something specific and useful,
// and none of it survives being replaced by a status code.
function refusalMessage(body, fallback) {
  return (body && typeof body.message === 'string' && body.message) || fallback;
}

var save = withProgress(async function () {
  // `text.disabled` is the second half of the guard, not a UI detail: a
  // disabled box holds a placeholder rather than the file's content (see
  // `setEditorEnabled`), and the button being disabled too is not something
  // this function may assume — a caller could reach it another way.
  if (!current || text.disabled) {
    return;
  }
  var response = await api.post('/b/dev/api/files/write', {
    path: current.path,
    content: text.value,
    expected_sha256: current.sha256
  });
  // A refusal is not a failure of the request — it is the answer to it.
  // Read before `json()`, which would turn it into a thrown error and lose
  // the body.
  //
  // EVERY refusal, not just the 409. The human clicked Save; whether the
  // answer is a hash conflict (409), a path the workspace will not take
  // (400) or a quota (413), it is a reply to what they just did, and a line
  // in the log they have to go looking for reads as "nothing happened".
  if (!response.ok) {
    var body = await refusalBody(response);
    if (isFileConflict(body)) {
      window.alert('Changed elsewhere (now ' + body.current_sha256 + '). Reopen the file.');
    } else {
      // Not a hash conflict — show the server's own explanation rather
      // than a `current_sha256` this body never carries.
      window.alert(refusalMessage(body, 'Save refused (' + response.status + ').'));
    }
    return;
  }
  var written = await json(response);
  current.sha256 = written.sha256;
  log(
    'saved ' +
      written.path +
      (written.generation ? ' — generation ' + written.generation.id : ' (staged, not published)')
  );
});

var remove = withProgress(async function () {
  if (!current || !window.confirm('Delete ' + current.path + '?')) {
    return;
  }
  var path = current.path;
  var response = await api.post('/b/dev/api/files/delete', {
    path: path,
    expected_sha256: current.sha256
  });
  // Same rule as `save` and `create`: the human clicked Delete and confirmed
  // it. A stale hash (409 — someone else changed the file since it was
  // opened, which is exactly the case `expected_sha256` exists to catch) or
  // a file already gone (404) is the answer to that click, not a log line.
  if (!response.ok) {
    var body = await refusalBody(response);
    if (isFileConflict(body)) {
      window.alert('Changed elsewhere since it was opened. Reopen ' + path + ' before deleting.');
    } else {
      window.alert(
        refusalMessage(body, 'Delete of ' + path + ' was refused (' + response.status + ').')
      );
    }
    return;
  }
  await json(response);
  current = null;
  title.textContent = 'Editor';
  text.value = '';
  setEditorEnabled(true);
  log('deleted ' + path);
});

var create = withProgress(async function () {
  var path = window.prompt('New file path (site/... or blocks/<name>/...)');
  if (!path) {
    return;
  }
  // `expected_sha256: null` is "I expect nothing here" — writing over an
  // existing file by accident is a 409, not a silent overwrite.
  var response = await api.post('/b/dev/api/files/write', {
    path: path,
    content: '',
    expected_sha256: null
  });
  // Told, not logged, on EVERY refusal. The human just typed this path; that
  // it already names a file (409), is not a path the workspace accepts (400
  // — by far the likeliest answer to something typed freehand into a
  // `prompt()`), or that the workspace hit a quota (409/413) is all an answer
  // to what they asked for, and a line in the log they have to go looking
  // for reads as "nothing happened".
  if (!response.ok) {
    var body = await refusalBody(response);
    if (isFileConflict(body)) {
      window.alert(path + ' already exists. Open it from the file list instead.');
    } else {
      window.alert(
        refusalMessage(body, path + ' was refused (' + response.status + ').')
      );
    }
    return;
  }
  await json(response);
  await openFile(path);
  log('created ' + path);
});

document.getElementById('dev-save').addEventListener('click', function () {
  save().catch(logError);
});
document.getElementById('dev-delete').addEventListener('click', function () {
  remove().catch(logError);
});
document.getElementById('dev-new-file').addEventListener('click', function () {
  create().catch(logError);
});
document.getElementById('dev-refresh-tools').addEventListener('click', function () {
  log('refreshing site tools');
  refreshSiteTools();
});

// ---- first paint ----------------------------------------------------------

loadFiles().catch(logError);
api.get('/b/dev/api/status').then(json).then(observe).catch(logError);
