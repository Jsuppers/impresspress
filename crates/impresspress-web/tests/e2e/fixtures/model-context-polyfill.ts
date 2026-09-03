/**
 * Chromium has no `document.modelContext` yet, so the WebMCP surface is
 * polyfilled with the smallest thing `ui/assets/webmcp.js` (and `webmcp-core.js`)
 * need — a `registerTool`/`unregisterTool` pair that record what they were
 * given — plus test-only hooks to read the registrations back, read back
 * every name `unregisterTool` was called with (in call order, duplicates
 * included — so a test can prove a tool was dropped exactly once, not just
 * that it eventually isn't present), and invoke a tool's `execute`.
 * Everything on the other side of that boundary is real: the served
 * manifest, the registration script, the request `execute` builds, and the
 * endpoint that answers it.
 *
 * `registerTool` also honours the proposal's second argument, `{ signal }`:
 * `blocks/dev/assets/dev.js` scopes every tool the `/b/dev` page registers to
 * an `AbortController` so they go away with the page. It ALSO unregisters
 * them by name on abort, because a browser that ignores the options bag would
 * otherwise leave them live — supporting the signal here is what makes the
 * two paths distinguishable at all: a signal-driven removal is NOT recorded
 * in `__unregistered()`, so a test can tell which one a build actually took.
 *
 * Shared by `smoke.spec.ts` (SW build) and `webmcp.spec.ts` (native server) —
 * install it with `page.addInitScript(MODEL_CONTEXT_POLYFILL)` before any
 * navigation so it exists before the page's own scripts run.
 */
export const MODEL_CONTEXT_POLYFILL = `
  (function () {
    const tools = new Map();
    const unregistered = [];
    Object.defineProperty(document, 'modelContext', {
      configurable: false,
      writable: false,
      value: {
        registerTool(options, registerOptions) {
          if (!options || typeof options.name !== 'string') {
            throw new TypeError('registerTool: name is required');
          }
          if (typeof options.execute !== 'function') {
            throw new TypeError('registerTool: execute is required');
          }
          tools.set(options.name, options);
          const signal = registerOptions && registerOptions.signal;
          if (signal) {
            const drop = () => {
              // Only if this very registration is still the live one: a
              // re-registration under the same name belongs to whoever
              // registered it, not to this (now stale) signal.
              if (tools.get(options.name) === options) {
                tools.delete(options.name);
              }
            };
            if (signal.aborted) {
              drop();
            } else {
              signal.addEventListener('abort', drop);
            }
          }
        },
        unregisterTool(name) {
          unregistered.push(name);
          tools.delete(name);
        },
        // Test hooks — not part of the WebMCP surface.
        __tools() {
          return Array.from(tools.values()).map((t) => ({
            name: t.name,
            description: t.description,
            inputSchema: t.inputSchema,
            outputSchema: t.outputSchema,
          }));
        },
        __unregistered() {
          return unregistered.slice();
        },
        __execute(name, args) {
          const tool = tools.get(name);
          if (!tool) {
            throw new Error('no such tool: ' + name);
          }
          return tool.execute(args);
        },
      },
    });
  })();
`;
