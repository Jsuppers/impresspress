// Registers this site's WebMCP tools with the browser agent.
//
// The manifest is generated server-side from each block's endpoint
// declarations and filtered to this session's auth level, so whatever
// arrives here is exactly what this visitor is allowed to invoke. The
// script's only job is translating that into registerTool calls.
(function () {
  'use strict';

  // Browsers without WebMCP get nothing. This ships on every page, so it
  // must never throw on an unsupported browser.
  if (!('modelContext' in document) || typeof document.modelContext.registerTool !== 'function') {
    return;
  }

  // Substitute {name} path segments, and collect the rest into query or body
  // according to the provenance the server recorded.
  function buildRequest(invocation, args) {
    var path = invocation.path;
    (invocation.path_params || []).forEach(function (name) {
      // split/join, not String.replace: replace with a string pattern
      // substitutes only the FIRST match, so a template repeating a
      // placeholder (`/x/{id}/{id}`) would keep a literal `{id}` in the URL
      // and 404 forever. The producer dedups placeholder names before
      // comparing them against the declared path params, so such a template
      // passes its eligibility check and reaches us intact.
      path = path.split('{' + name + '}').join(encodeURIComponent(args[name]));
    });

    var query = new URLSearchParams();
    (invocation.query_params || []).forEach(function (name) {
      if (args[name] !== undefined && args[name] !== null) {
        query.append(name, args[name]);
      }
    });
    var qs = query.toString();
    if (qs) {
      path += '?' + qs;
    }

    var init = { method: invocation.method.toUpperCase(), headers: {} };

    var bodyNames = invocation.body_params || [];
    if (bodyNames.length > 0) {
      var body = {};
      bodyNames.forEach(function (name) {
        if (args[name] !== undefined) {
          body[name] = args[name];
        }
      });
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
    }

    // Same-origin, so the session cookie rides along and the server applies
    // the same authorization it would to any other request. The manifest
    // filter is a UX affordance; the endpoint is still the real gate.
    init.credentials = 'same-origin';

    return { url: path, init: init };
  }

  function register(tool) {
    document.modelContext.registerTool({
      name: tool.name,
      description: tool.description,
      inputSchema: tool.inputSchema,
      execute: async function (args) {
        var req = buildRequest(tool.invocation, args || {});
        var response = await fetch(req.url, req.init);
        var text = await response.text();

        if (!response.ok) {
          // `isError` is what tells the agent this is a failure. Without it
          // the harness treats the body as a normal result, and a model can
          // relay `Request failed (403): ...` to the customer as if it were
          // product data.
          return {
            isError: true,
            content: [{
              type: 'text',
              text: 'Request failed (' + response.status + '): ' + text
            }]
          };
        }

        return { content: [{ type: 'text', text: text }] };
      }
    });
  }

  fetch('/b/webmcp/manifest.json', { credentials: 'same-origin' })
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
        } catch (e) {
          // One tool the browser rejected is not a reason to lose the rest.
        }
      });
    })
    .catch(function () {
      // A failed manifest fetch means no tools. That is a degraded page,
      // not a broken one — never surface it to the visitor.
    });
})();
