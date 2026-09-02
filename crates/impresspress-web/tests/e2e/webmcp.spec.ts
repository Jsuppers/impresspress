import { expect, test, type APIRequestContext, type Page } from '@playwright/test';
import { ADMIN_STATE_PATH } from './fixtures/auth';
import { ADMIN_EMAIL, ADMIN_PASSWORD } from './fixtures/global-setup';

/**
 * WebMCP end-to-end against the real native server (visual-baseline config:
 * port 8093 in CI, admin session via globalSetup).
 *
 * Chromium has no `document.modelContext` yet, so the WebMCP surface is
 * polyfilled with the smallest thing `ui/assets/webmcp.js` needs — a
 * `registerTool` that records what it was given — plus two test-only hooks
 * to read the registrations back and to invoke a tool's `execute`. Everything
 * on the other side of that boundary is real: the served manifest, the
 * registration script, the request `execute` builds, and the endpoint that
 * answers it. What this cannot test is whether an agent *chooses* the right
 * tool from its description; that needs a WebMCP-capable browser and a human
 * (plan 3, task 5).
 */

const MODEL_CONTEXT_POLYFILL = `
  (function () {
    const tools = new Map();
    Object.defineProperty(document, 'modelContext', {
      configurable: false,
      writable: false,
      value: {
        registerTool(options) {
          if (!options || typeof options.name !== 'string') {
            throw new TypeError('registerTool: name is required');
          }
          if (typeof options.execute !== 'function') {
            throw new TypeError('registerTool: execute is required');
          }
          tools.set(options.name, options);
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

type ToolRecord = {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
  outputSchema?: Record<string, unknown>;
};

type ToolResult = {
  isError?: boolean;
  content: Array<{ type: string; text: string }>;
  structuredContent?: Record<string, unknown>;
};

const PUBLIC_TOOLS = [
  'get_storefront_config',
  'get_product',
  'preview_price',
  'start_checkout',
  'get_order_status',
];

async function registeredTools(page: Page, atLeast: number): Promise<ToolRecord[]> {
  await page.waitForFunction(
    (n) => (document as unknown as { modelContext: { __tools(): unknown[] } }).modelContext.__tools().length >= n,
    atLeast,
    { timeout: 15_000 },
  );
  return page.evaluate(
    () => (document as unknown as { modelContext: { __tools(): ToolRecord[] } }).modelContext.__tools(),
  );
}

async function execute(page: Page, name: string, args: Record<string, unknown>): Promise<ToolResult> {
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

/** Bearer for the bootstrap admin — bearer auth is exempt from the CSRF origin policy, cookies are not. */
async function adminBearer(request: APIRequestContext): Promise<string> {
  const res = await request.post('/b/auth/api/login', {
    data: { email: ADMIN_EMAIL, password: ADMIN_PASSWORD },
    headers: { 'Content-Type': 'application/json' },
  });
  expect(res.status(), await res.text()).toBe(200);
  const { access_token } = (await res.json()) as { access_token: string };
  return `Bearer ${access_token}`;
}

/**
 * One product with one published, component-priced offer: `pages` (integer)
 * at 1500 minor units per page. Unique slug per run so re-running against the
 * same database never collides.
 */
async function seedProductWithOffer(
  request: APIRequestContext,
): Promise<{ productId: string; offerId: string }> {
  const auth = { Authorization: await adminBearer(request), 'Content-Type': 'application/json' };
  const stamp = Date.now().toString(36);

  const productRes = await request.post('/b/products/api/admin/products', {
    headers: auth,
    data: {
      name: `WebMCP e2e print ${stamp}`,
      slug: `webmcp-e2e-print-${stamp}`,
      description: 'Seeded by webmcp.spec.ts',
      currency: 'nzd',
      status: 'active',
      fulfillment_kind: 'manual',
    },
  });
  expect(productRes.status(), await productRes.text()).toBe(200);
  const productBody = (await productRes.json()) as { id?: string; data?: { id?: string } };
  const productId = productBody.id ?? productBody.data?.id;
  expect(productId, JSON.stringify(productBody)).toBeTruthy();

  const offerRes = await request.post(`/b/products/api/admin/products/${productId}/offers`, {
    headers: auth,
    data: {
      name: 'Custom print',
      mode: 'payment',
      currency: 'nzd',
      pricing_model: 'components',
      usage_type: 'licensed',
      billing_scheme: 'per_unit',
      tax_behavior: 'exclusive',
      variables: [
        {
          key: 'pages',
          kind: 'integer',
          label: 'Pages',
          required: true,
          minimum: '1',
          maximum: '20',
          step: '1',
          sort_order: 0,
        },
      ],
      components: [
        {
          key: 'pages',
          label: 'Printed pages',
          sort_order: 0,
          required: true,
          amount: { type: 'per_unit', input: 'pages', unit_amount_minor: 1500 },
        },
      ],
      checkout: {},
    },
  });
  expect(offerRes.status(), await offerRes.text()).toBe(200);
  const offerBody = (await offerRes.json()) as { offer?: { id?: string }; id?: string };
  const offerId = offerBody.offer?.id ?? offerBody.id;
  expect(offerId, JSON.stringify(offerBody)).toBeTruthy();

  const publishRes = await request.post(
    `/b/products/api/admin/products/${productId}/offers/${offerId}/publish`,
    { headers: auth, data: {} },
  );
  expect(publishRes.status(), await publishRes.text()).toBe(200);
  const published = (await publishRes.json()) as { status?: string };
  expect(published.status).toBe('active');

  return { productId: productId as string, offerId: offerId as string };
}

test.describe('WebMCP registration on an anonymous page', () => {
  test.beforeEach(async ({ page }) => {
    await page.addInitScript(MODEL_CONTEXT_POLYFILL);
  });

  test('registers exactly the five Public storefront tools, each with both schemas', async ({ page }) => {
    await page.goto('/b/auth/login');
    const tools = await registeredTools(page, PUBLIC_TOOLS.length);

    expect(tools.map((t) => t.name).sort()).toEqual([...PUBLIC_TOOLS].sort());
    for (const tool of tools) {
      expect(tool.description.length, tool.name).toBeGreaterThan(20);
      expect(tool.inputSchema?.type, `${tool.name} inputSchema`).toBe('object');
      expect(tool.outputSchema?.type, `${tool.name} outputSchema`).toBe('object');
    }
  });

  test('get_storefront_config returns structured content from the real endpoint', async ({ page }) => {
    await page.goto('/b/auth/login');
    await registeredTools(page, PUBLIC_TOOLS.length);

    const result = await execute(page, 'get_storefront_config', {});
    expect(result.isError).toBeFalsy();
    expect(result.content[0]?.type).toBe('text');
    expect(result.structuredContent?.schema_version).toBe(1);
    expect(typeof result.structuredContent?.embedded_checkout_available).toBe('boolean');
  });

  test('get_order_status with a bad receipt is an error result, not data', async ({ page }) => {
    await page.goto('/b/auth/login');
    await registeredTools(page, PUBLIC_TOOLS.length);

    const result = await execute(page, 'get_order_status', {
      id: 'order_does_not_exist',
      receipt_token: 'not-a-receipt',
    });
    expect(result.isError).toBe(true);
    expect(result.content[0]?.text).toMatch(/^Request failed \(4\d\d\)/);
    expect(result.structuredContent).toBeUndefined();
  });
});

test.describe('WebMCP tools against a seeded product', () => {
  let productId: string;
  let offerId: string;

  test.beforeAll(async ({ request }) => {
    ({ productId, offerId } = await seedProductWithOffer(request));
  });

  test.beforeEach(async ({ page }) => {
    await page.addInitScript(MODEL_CONTEXT_POLYFILL);
    await page.goto('/b/auth/login');
    await registeredTools(page, PUBLIC_TOOLS.length);
  });

  test('get_product returns the seeded product and its published offer', async ({ page }) => {
    const result = await execute(page, 'get_product', { product_id: productId });
    expect(result.isError, result.content[0]?.text).toBeFalsy();

    const product = result.structuredContent as {
      id: string;
      offers: Array<{ id: string; variables: Array<{ key: string; kind: string }> }>;
    };
    expect(product.id).toBe(productId);
    expect(product.offers.map((o) => o.id)).toEqual([offerId]);
    expect(product.offers[0].variables.map((v) => v.key)).toEqual(['pages']);
  });

  test('preview_price prices the offer from the customer inputs', async ({ page }) => {
    const result = await execute(page, 'preview_price', {
      offer_id: offerId,
      quantity: 1,
      inputs: { pages: 3 },
    });
    expect(result.isError, result.content[0]?.text).toBeFalsy();

    const quote = result.structuredContent as {
      offer_id: string;
      amounts: { currency: string; total_minor: number; subtotal_minor: number };
      components: Array<{ key: string; total_amount_minor: number }>;
    };
    expect(quote.offer_id).toBe(offerId);
    // 3 pages × 1500 minor units per page.
    expect(quote.components.map((c) => [c.key, c.total_amount_minor])).toEqual([['pages', 4500]]);
    expect(quote.amounts.subtotal_minor).toBe(4500);
    expect(quote.amounts.total_minor).toBe(4500);
    expect(quote.amounts.currency.toLowerCase()).toBe('nzd');
  });

  test('start_checkout cannot complete a payment here: no provider, an error result', async ({ page }) => {
    // No Stripe key is configured on the test server. The tool must surface
    // that as `isError`, never as a success the agent could relay.
    const result = await execute(page, 'start_checkout', {
      offer_id: offerId,
      quantity: 1,
      inputs: { pages: 1 },
      presentation: 'hosted',
    });
    expect(result.isError).toBe(true);
    expect(result.content[0]?.text).toMatch(/^Request failed \(\d{3}\)/);
    expect(result.structuredContent).toBeUndefined();
  });
});

test.describe('WebMCP registration for a signed-in admin', () => {
  test.use({ storageState: ADMIN_STATE_PATH });

  test.beforeEach(async ({ page }) => {
    await page.addInitScript(MODEL_CONTEXT_POLYFILL);
  });

  test('adds the Authenticated tool on top of the Public set', async ({ page }) => {
    await page.goto('/b/admin/');
    const tools = await registeredTools(page, PUBLIC_TOOLS.length + 1);
    expect(tools.map((t) => t.name).sort()).toEqual([...PUBLIC_TOOLS, 'list_my_purchases'].sort());
  });

  test('the inspector shows the manifest at every auth level', async ({ page }) => {
    const res = await page.request.get('/b/inspector/webmcp');
    expect(res.status(), await res.text()).toBe(200);
    const view = (await res.json()) as {
      levels: Array<{
        level: string;
        manifest: { tools: Array<{ name: string }> };
        refusals: unknown[];
        opted_in: number;
      }>;
    };

    const entry = (level: string) => {
      const found = view.levels.find((l) => l.level === level);
      expect(found, `level ${level} in ${JSON.stringify(view.levels.map((l) => l.level))}`).toBeTruthy();
      return found as NonNullable<typeof found>;
    };
    const names = (level: string) => entry(level).manifest.tools.map((t) => t.name).sort();
    const pub = names('public');
    const authed = names('authenticated');
    const admin = names('admin');

    expect(pub).toEqual([...PUBLIC_TOOLS].sort());
    for (const name of pub) expect(authed, 'monotone: public ⊆ authenticated').toContain(name);
    for (const name of authed) expect(admin, 'monotone: authenticated ⊆ admin').toContain(name);
    expect(authed).toContain('list_my_purchases');

    // Every opted-in endpoint produced a tool at every level: nothing refused,
    // and the count the page reports is the count it publishes.
    for (const level of view.levels) {
      expect(level.refusals, `${level.level} refusals`).toEqual([]);
      expect(level.manifest.tools.length, `${level.level} opted_in`).toBe(level.opted_in);
    }
  });
});
