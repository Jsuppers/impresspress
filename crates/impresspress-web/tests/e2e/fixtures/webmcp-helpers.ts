import { expect, type Page } from '@playwright/test';

/**
 * Reading and driving `document.modelContext` from a test.
 *
 * These helpers were born inside `webmcp.spec.ts`; they live here because the
 * sandbox specs (`dev-workspace.spec.ts`, `dev-compile.spec.ts`,
 * `dev-scenario.spec.ts`) need exactly the same operations against a
 * completely different server (the browser-WASM sandbox bundle rather than
 * the native binary). Two copies of "wait until N tools exist, then read
 * them" would be two things to keep in step with the polyfill's test hooks —
 * and the polyfill (`model-context-polyfill.ts`) is already shared, so its
 * readers should be too.
 *
 * Everything here goes through the polyfill's `__`-prefixed hooks, which are
 * NOT part of the WebMCP surface: a real WebMCP browser exposes no way to
 * enumerate or invoke registrations from page script, which is the whole
 * reason the polyfill exists.
 */

/** One registration, as the polyfill's `__tools()` reports it. */
export type ToolRecord = {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
  outputSchema?: Record<string, unknown>;
};

/** What a tool's `execute` resolves to (the MCP `CallToolResult` shape). */
export type ToolResult = {
  isError?: boolean;
  content: Array<{ type: string; text: string }>;
  structuredContent?: Record<string, unknown>;
};

/**
 * Wait until at least `atLeast` tools are registered, then return all of
 * them.
 *
 * "At least", not "exactly": a page can have more than one registrar. On
 * `/b/dev` both `dev.js` (the page-scoped `dev_*`/`shop_*` allowlist) and
 * `webmcp.js` (the deployment-wide manifest) register into the same
 * `document.modelContext`, and they finish in whichever order their two
 * fetches happen to complete. A caller that needs a specific tool from the
 * slower registrar should wait for it by name with [`waitForTool`] rather
 * than guessing a total.
 */
export async function registeredTools(page: Page, atLeast: number): Promise<ToolRecord[]> {
  await page.waitForFunction(
    (n) => (document as unknown as { modelContext: { __tools(): unknown[] } }).modelContext.__tools().length >= n,
    atLeast,
    { timeout: 15_000 },
  );
  return page.evaluate(
    () => (document as unknown as { modelContext: { __tools(): ToolRecord[] } }).modelContext.__tools(),
  );
}

/**
 * Wait until a tool with this exact name is registered.
 *
 * The counting wait above cannot express "the other registrar has finished
 * too" without pinning a total that belongs to a different file's contract
 * (the deployment manifest's size at a given auth tier is `webmcp.spec.ts`'s
 * subject, not this one's). Waiting for one name it publishes is the same
 * fact without the coupling.
 */
export async function waitForTool(page: Page, name: string): Promise<void> {
  await page.waitForFunction(
    (n) =>
      (document as unknown as { modelContext: { __tools(): Array<{ name: string }> } }).modelContext
        .__tools()
        .some((t) => t.name === n),
    name,
    { timeout: 15_000 },
  );
}

/** Invoke a registered tool's `execute` and return its result. */
export async function execute(
  page: Page,
  name: string,
  args: Record<string, unknown>,
): Promise<ToolResult> {
  return page.evaluate(
    ([toolName, toolArgs]) =>
      (
        document as unknown as {
          modelContext: { __execute(n: string, a: unknown): Promise<ToolResult> };
        }
      ).modelContext.__execute(toolName as string, toolArgs),
    [name, args] as const,
  );
}

/**
 * The structured half of a tool result, with "and it was not an error" folded
 * in.
 *
 * Every tool the sandbox specs call declares an `outputSchema`
 * (`impresspress-core/tests/snapshots/dev.tools.json`), so `webmcp-core.js`
 * parses each success body into `structuredContent` — a tool that came back
 * with only a text block either failed or lost its schema, and both are
 * defects rather than shapes to branch on. `content[0].text` is the message on
 * the failure path (`Request failed (409): …`), which is what makes a broken
 * assertion readable.
 *
 * Shared rather than copied: `dev-compile.spec.ts`, `dev-workspace.spec.ts`
 * and `dev-scenario.spec.ts` all unwrap tool results the same way, and three
 * copies of "assert not-an-error, then cast" would be three places for the
 * failure message to drift.
 */
export function structured<T>(result: ToolResult): T {
  expect(result.isError, result.content[0]?.text).toBeFalsy();
  expect(result.structuredContent, JSON.stringify(result)).toBeTruthy();
  return result.structuredContent as unknown as T;
}
