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
// Every tool registered here is PAGE-scoped: the list comes from
// `/b/dev/api/tools.json` (the curated `dev_*` / `shop_*` allowlist), never
// from the deployment-wide manifest `webmcp.js` owns, and it is torn down
// when this page goes away. An agent on any other page of the site sees the
// site's tools and none of these.

// ---- the API client -------------------------------------------------------

// The lifetime of everything this page registers. Aborting it is the single
// teardown path: `pagehide` fires it, and so does the first sign that the
// session behind the page is gone (below).
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
    startPolling();
    try {
      return await execute(args);
    } finally {
      stopPolling();
      await refreshAfterChange();
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
// and the panes refreshed. `shop_` covers the whole products family; the
// four `dev_` names are the mutating half of the control plane (its reads —
// `dev_status`, `dev_list_files`, `dev_read_file`, `dev_list_generations`,
// `dev_get_generation` — change nothing and must not restart the poll).
var MUTATING = /^(dev_write_file|dev_delete_file|dev_rollback|dev_remove_block|shop_)/;

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
  if (typeof document.modelContext.unregisterTool !== 'function') {
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
window.addEventListener('pagehide', function () {
  abort.abort();
});

abort.signal.addEventListener('abort', function () {
  stopPolling();
  unregisterPageTools();
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
  text.disabled = file.encoding !== 'utf8';
  await loadFiles();
}

var save = withProgress(async function () {
  if (!current) {
    return;
  }
  var response = await api.post('/b/dev/api/files/write', {
    path: current.path,
    content: text.value,
    expected_sha256: current.sha256
  });
  // A 409 is not a failure of the request — it is the answer to it, and it
  // carries the hash the file actually has. Read before `json()`, which
  // would turn it into a thrown error and lose the conflict body.
  if (response.status === 409) {
    var conflict = await response.json();
    window.alert('Changed elsewhere (now ' + conflict.current_sha256 + '). Reopen the file.');
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
  await json(
    await api.post('/b/dev/api/files/delete', { path: path, expected_sha256: current.sha256 })
  );
  current = null;
  title.textContent = 'Editor';
  text.value = '';
  text.disabled = false;
  log('deleted ' + path);
});

var create = withProgress(async function () {
  var path = window.prompt('New file path (site/... or blocks/<name>/...)');
  if (!path) {
    return;
  }
  // `expected_sha256: null` is "I expect nothing here" — writing over an
  // existing file by accident is a 409, not a silent overwrite.
  await json(await api.post('/b/dev/api/files/write', { path: path, content: '', expected_sha256: null }));
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
