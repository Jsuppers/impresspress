// Registers this site's WebMCP tools with the browser agent.
//
// The manifest is generated server-side from each block's endpoint
// declarations and filtered to this session's auth level, so whatever
// arrives here is exactly what this visitor is allowed to invoke. The
// script's only job is translating that into registerTool calls.

// Browsers without WebMCP get nothing. This ships on every page, so it
// must never throw on an unsupported browser.
if (!('modelContext' in document) || typeof document.modelContext.registerTool !== 'function') {
  return;
}

function register(tool) {
  document.modelContext.registerTool(toolOptions(tool));
}

// Names this script itself registered, so `refresh()` can drop exactly what
// it added rather than guessing at the agent's whole tool set. `generation`
// counts completed `load()` calls — a caller can poll it to notice a refresh
// landed without threading a callback through `refresh()`'s promise.
var registered = [];
var generation = 0;

function unregisterAll() {
  if (typeof document.modelContext.unregisterTool === 'function') {
    registered.forEach(function (name) {
      try {
        document.modelContext.unregisterTool(name);
      } catch (e) {
        // already gone
      }
    });
  }
  registered = [];
}

function load() {
  return fetch('/b/webmcp/manifest.json', { credentials: 'same-origin' })
    .then(function (r) { return r.ok ? r.json() : null; })
    .then(function (manifest) {
      if (!manifest || !Array.isArray(manifest.tools)) {
        return;
      }
      // Per-tool try/catch: `registerTool` throwing on one malformed tool
      // must not abort registration of every tool after it (the outer
      // `.catch` would swallow it silently, leaving a half-registered page).
      manifest.tools.forEach(function (tool) {
        try {
          register(tool);
          registered.push(tool.name);
        } catch (e) {
          // One tool the browser rejected is not a reason to lose the rest.
        }
      });
      generation += 1;
    })
    .catch(function () {
      // A failed manifest fetch means no tools. That is a degraded page,
      // not a broken one — never surface it to the visitor.
    });
}

// Drops every tool this script registered and re-fetches the manifest. A
// page that changes what it can offer mid-session (e.g. the dev sandbox's
// workspace tools, which come and go with what is loaded) calls this instead
// of forcing a reload — the alternative is stale tool descriptions the agent
// can no longer act on, or worse, ones that still work but no longer mean
// what their description says.
function refresh() {
  unregisterAll();
  return load();
}

window.__impresspressWebmcp = {
  refresh: refresh,
  generation: function () { return generation; }
};

var sw = navigator.serviceWorker;
if (sw && sw.controller) {
  // Already controlled — a repeat visit, or a native server where there is
  // no worker to wait on in the first place. Nothing to wait for.
  load();
} else if (sw) {
  // In a service-worker build the first paint beats the worker: the manifest
  // route (`/b/webmcp/manifest.json`) is served by the worker, so fetching
  // it before the worker controls the page 404s through the network instead
  // of hitting the wasm router. Wait for `sw.ready`, which resolves once a
  // registration is active.
  //
  // A native server also exposes `navigator.serviceWorker` (it is a
  // standard browser API, not something the SW build adds), but there is no
  // registration to become active — `sw.ready` on a page with none never
  // resolves. `getRegistration()` tells the two apart: it resolves with
  // `undefined` when nothing is registered, so the native path falls
  // through to `load()` immediately instead of hanging.
  sw.getRegistration().then(function (r) { return r ? sw.ready : null; }).then(load, load);
} else {
  // No Service Worker support at all (or it was stripped by the embedder).
  load();
}
