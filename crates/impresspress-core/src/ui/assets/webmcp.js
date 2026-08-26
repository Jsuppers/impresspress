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
      path = path.replace('{' + name + '}', encodeURIComponent(args[name]));
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
          return {
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
      manifest.tools.forEach(register);
    })
    .catch(function () {
      // A failed manifest fetch means no tools. That is a degraded page,
      // not a broken one — never surface it to the visitor.
    });
})();
