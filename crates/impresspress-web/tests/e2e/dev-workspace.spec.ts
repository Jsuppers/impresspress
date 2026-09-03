import { test, expect, type Page } from '@playwright/test';
import {
  ADMIN_EMAIL,
  ADMIN_PASSWORD,
  bootServiceWorker,
  loginToWorkspace,
  WELCOME_HEADING,
  WELCOME_PHRASE,
} from './fixtures/dev-sandbox';
import { MODEL_CONTEXT_POLYFILL } from './fixtures/model-context-polyfill';
import { SHOP_OFFER, SHOP_PRODUCT } from './fixtures/shop-fixture';
import {
  execute,
  registeredTools,
  waitForTool,
  type ToolResult,
} from './fixtures/webmcp-helpers';

/**
 * Plan 2 checkpoint for the browser development sandbox: **an agent builds
 * the site and stocks the shop; a shopper browses it.**
 *
 * `dev-foundations.spec.ts` (Plan 1) proves the machinery underneath — seed
 * on boot, hash-checked writes, compile/probe/rebuild, rollback — by calling
 * `/b/dev/api/*` directly with `fetch`. This file is the layer above it: the
 * `/b/dev` page, the tools it registers, and the two audiences that meet on
 * one origin. Nothing here calls the sandbox API directly; every mutation
 * goes through a registered WebMCP tool, because "the agent could do it" is
 * the claim under test and a `fetch` would prove only that the endpoint
 * works.
 *
 * The polyfill (`model-context-polyfill.ts`) stands in for a WebMCP-capable
 * browser — Chromium has no `document.modelContext` — and it is the ONLY
 * substitution. `/b/dev/api/tools.json`, `dev.js`'s registration, the request
 * `webmcp-core.js` builds from each tool's `invocation`, the handlers that
 * answer it, the generation the write publishes and the page the shopper is
 * served are all the real things, running inside a service worker.
 *
 * Three tests, three separate claims:
 *
 *  1. **The end-to-end scenario** (design §16 scenarios 1, 3, 4, 5): sign in
 *     from the welcome page, get the page-scoped tool set, rewrite the site,
 *     create → price → publish → activate a product, and have an anonymous
 *     shopper on the same origin see it.
 *  2. **Cross-origin isolation** (§20.3, spec amendment 14): `/b/dev` is
 *     `crossOriginIsolated` for the in-browser compiler's `SharedArrayBuffer`,
 *     and — the load-bearing half — the site carries the same COEP so a COEP
 *     document can actually frame it. A blank preview iframe is what the
 *     `require-corp` posture produced, and no header assertion alone catches
 *     it: only rendering the frame does.
 *  3. **The editor's binary guard**: the one data-loss path the editor pane
 *     has. `blocks/dev/page.rs` pins both halves of the guard as source
 *     assertions and says outright that the behaviour itself can only be
 *     driven from here.
 *
 * Every test starts from a fresh Playwright context, which is a fresh origin:
 * its own OPFS database, its own service-worker registration, its own seed
 * import. So each pays a cold boot, and each is independent of the others.
 */

/**
 * The eleven `dev_*` tools `/b/dev/api/tools.json` projects, plus the two
 * `dev.js` registers itself.
 *
 * `dev_read_reference` and `dev_create_block` are Plan 3's — the guest-API
 * reference an agent reads before writing Rust, and the scaffolder that lays
 * a block down from a template. Both are ordinary HTTP tools
 * (`blocks/dev/tools.rs`'s `SELECTIONS`), so both are in `tools.json`.
 *
 * `dev_compile_block` and `dev_export` are not: they have no HTTP endpoint
 * behind them in this build — compiling happens in a page worker (Plan 3
 * Task 5) and exporting writes a bundle from it (Plan 4). `dev.js` registers
 * them anyway so an agent that discovers them gets an honest refusal instead
 * of inventing its own way to compile, which is why they belong in this list
 * rather than in `tools.json`.
 */
const DEV_TOOLS = [
  'dev_status',
  'dev_list_files',
  'dev_read_file',
  'dev_read_reference',
  'dev_write_file',
  'dev_delete_file',
  'dev_list_generations',
  'dev_get_generation',
  'dev_rollback',
  'dev_create_block',
  'dev_remove_block',
  'dev_compile_block',
  'dev_export',
];

/** The products admin API, projected as the shop-building half of the page. */
const SHOP_TOOLS = [
  'shop_list_products',
  'shop_create_product',
  'shop_update_product',
  'shop_delete_product',
  'shop_restore_product',
  'shop_list_groups',
  'shop_create_group',
  'shop_list_offers',
  'shop_create_offer',
  'shop_update_offer',
  'shop_publish_offer',
  'shop_archive_offer',
];

/** The two `dev.js` registers locally rather than from the manifest. */
const PAGE_LOCAL_TOOLS = ['dev_compile_block', 'dev_export'];

/** Everything the `/b/dev` page itself registers, in either half. */
const PAGE_TOOLS = [...DEV_TOOLS, ...SHOP_TOOLS].sort();

/**
 * What `/b/dev/api/tools.json` publishes — `PAGE_TOOLS` minus the two stubs.
 * `dev.js` logs this count after it registers the manifest, which is how the
 * page reports the size of the surface it was given.
 */
const MANIFEST_TOOLS = PAGE_TOOLS.filter((name) => !PAGE_LOCAL_TOOLS.includes(name));

/** The heading the agent's page carries; the proof a write reached the site. */
const SHOP_HEADING = 'The print shop';

/**
 * The page the agent writes over the welcome starter site.
 *
 * Deliberately what design §4.1's suggested prompt asks an agent for, not a
 * placeholder: it reads the *public* catalog (`/b/products/catalog`, the
 * anonymous surface — the `shop_*` tools are admin-only and a shopper has
 * none of them) and mounts the shipped storefront widget for each product.
 * `page.records` is the list field `CatalogProductListResponse` publishes
 * (`products/contracts.rs`); a page that guessed `items` would render an
 * empty shop and this whole test would still "pass" up to the last
 * assertion.
 *
 * Built through the DOM rather than `innerHTML` so the product name is text,
 * not markup — the shop is the boundary a prompt injection would cross, and a
 * fixture that pasted names into HTML would be modelling the wrong thing.
 *
 * `.shop-product-name` rather than a bare `h2`: the storefront widget renders
 * its own `h2` inside a shadow root, and Playwright's CSS engine pierces open
 * shadow roots, so `locator('h2')` would match two elements and fail strict
 * mode. Distinct class names keep the page's own markup addressable.
 *
 * # Why the page has to ask for `webmcp.js` itself
 *
 * `/b/webmcp/webmcp.js` is the deployment's WebMCP registration script at its
 * stable path, and design §16 scenario 5 requires the shopper's own agent to
 * have `list_products` on this page. Nothing puts that script here for it:
 * `ui::layout` injects the content-hashed `/b/static/webmcp-{hash}.js` into
 * every SSR-rendered ImpressPress page, but `/` on a sandbox is a file the
 * agent wrote, served verbatim by `wafer-run/web`, which injects nothing into
 * anybody's HTML. So a site that wants its visitors' agents to have tools has
 * to include the script itself, exactly as it has to include `storefront.js`
 * to get the purchase widget — and it does so at the stable path
 * (`GET /b/webmcp/webmcp.js`, `pipeline.rs`, `ui::assets::
 * WEBMCP_JS_STABLE_PATH`) rather than the content-hashed one, since a page
 * the agent writes has no SSR document to read the current hash off. The
 * guide and `SUGGESTED_PROMPT` on `/b/dev` tell the agent to write exactly
 * this tag (`blocks/dev/page.rs`).
 */
function shopPage(): string {
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>${SHOP_HEADING}</title>
  <script src="/b/products/storefront.js" defer></script>
  <script src="/b/webmcp/webmcp.js" defer></script>
</head>
<body>
  <h1>${SHOP_HEADING}</h1>
  <ul id="products"></ul>
  <script>
    fetch('/b/products/catalog')
      .then(function (response) { return response.json(); })
      .then(function (page) {
        var list = document.getElementById('products');
        page.records.forEach(function (product) {
          var item = document.createElement('li');
          var heading = document.createElement('h2');
          heading.className = 'shop-product-name';
          heading.textContent = product.name;
          var widget = document.createElement('impresspress-product');
          widget.setAttribute('product-id', product.id);
          item.appendChild(heading);
          item.appendChild(widget);
          list.appendChild(item);
        });
      });
  </script>
</body>
</html>
`;
}

/** A 1×1 transparent PNG — the smallest honestly-binary file. 67 bytes. */
const PIXEL_PNG_BASE64 =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg==';
const PIXEL_PNG_BYTES = 67;
const PIXEL_PNG_PATH = 'site/pixel.png';

/**
 * Sign in the way a first-time visitor does: from the welcome page's own
 * "Open workspace" link.
 *
 * Not `loginToWorkspace` (`fixtures/dev-sandbox.ts`), and not `goto('/b/dev')`
 * — either would skip the one navigation this deployment actually ships to get
 * a human into the workspace, which is a link the seed site carries and a
 * `?redirect=` the login page has to honour. Both are part of Plan 2's
 * surface, so both are walked here; the shared helper is for the specs whose
 * subject is what happens AFTER the workspace is open.
 */
async function openWorkspace(page: Page) {
  await expect(page.locator('body')).toContainText(WELCOME_PHRASE, { timeout: 60_000 });
  // The credentials are printed on the page for whoever lands here; that they
  // are is part of the starter site's contract, so the test reads them the
  // same way a visitor would rather than assuming them silently.
  await expect(page.locator('body')).toContainText(ADMIN_EMAIL);
  await page.getByRole('link', { name: /open workspace/i }).click();
  await page.locator('input#email').fill(ADMIN_EMAIL);
  await page.locator('input#password').fill(ADMIN_PASSWORD);
  await page.getByRole('button', { name: /sign in/i }).click();
  // `/b/auth/login?redirect=/b/dev` renders the target into the hidden
  // `#redirect` field and the login script prefers it over the role-aware
  // `default_redirect` — so landing anywhere else is the link being broken,
  // not a detail to paper over.
  await page.waitForURL((url) => url.pathname === '/b/dev', { timeout: 60_000 });
}

/**
 * The structured half of a tool result, with "and it was not an error"
 * folded in.
 *
 * Every tool on this page declares an `outputSchema`
 * (`impresspress-core/tests/snapshots/dev.tools.json`), so `webmcp-core.js`
 * parses each success body into `structuredContent` — a tool that came back
 * with only a text block either failed or lost its schema, and both are
 * defects rather than shapes to branch on. `content[0].text` is the message
 * on the failure path (`Request failed (409): …`), which is what makes a
 * broken assertion here readable.
 */
function structured<T>(result: ToolResult): T {
  expect(result.isError, result.content[0]?.text).toBeFalsy();
  expect(result.structuredContent, JSON.stringify(result)).toBeTruthy();
  return result.structuredContent as unknown as T;
}

type FileRead = { path: string; sha256: string; size: number; encoding: string; content: string };
type FileWrite = { path: string; sha256: string; size: number; generation: Generation | null };
type Generation = { id: string; cause: string; status: string };

test('an agent builds the shop on /b/dev and a shopper sees it at /', async ({ page }) => {
  // A cold boot compiles the wasm, creates the OPFS database, runs every
  // block's migrations and imports the seed. The default 60 s is a boot
  // budget, not a test budget.
  test.setTimeout(300_000);

  // Install the WebMCP shim before ANY navigation: `dev.js` and `webmcp.js`
  // both check `document.modelContext` as they run, and a polyfill added
  // afterwards would be a page with no agent on it.
  await page.addInitScript(MODEL_CONTEXT_POLYFILL);

  const bootStart = Date.now();
  await bootServiceWorker(page);
  console.log(`cold boot (seed import, no blocks): ${Date.now() - bootStart} ms`);

  await openWorkspace(page);

  // --- 1. The page registers its own tools, and only its own -------------
  //
  // Two registrars share `document.modelContext` here: `dev.js` adds the
  // page-scoped allowlist, `webmcp.js` adds the deployment-wide manifest for
  // the caller's tier, and they finish in whichever order their two fetches
  // complete. So both are waited for before anything is read, each by a name
  // it alone publishes: `list_products` for `webmcp.js`, and `dev_export`
  // for `dev.js` — the last tool `registerPageLocal` adds, after
  // `registerFromManifest` has finished with everything `tools.json`
  // returned. Waiting on names rather than a total (`PAGE_TOOLS.length`) is
  // what keeps this from pinning the *other* file's contract — the admin
  // manifest's size is `webmcp.spec.ts`'s subject, not this one's — and from
  // going silently racy if that manifest ever grows to `PAGE_TOOLS.length`
  // tools at the admin tier, at which point `waitForFunction`'s count could
  // be satisfied by `webmcp.js` alone, before `dev.js` had registered
  // anything.
  await waitForTool(page, 'list_products');
  await waitForTool(page, 'dev_export');
  const tools = (await registeredTools(page, 1)).map((t) => t.name);

  expect(tools.filter((n) => n.startsWith('dev_') || n.startsWith('shop_')).sort()).toEqual(
    PAGE_TOOLS,
  );
  // The site's own tools are still here: `dev.js`'s registrations are added
  // ALONGSIDE the manifest's, not instead of them. An agent on this page can
  // both build the shop and browse it.
  expect(tools).toContain('list_products');
  // …and the page said so in its own log, with the count `tools.json`
  // published. This is the only externally visible proof that
  // `registerFromManifest` consumed the whole manifest rather than losing
  // entries to its per-tool `try`.
  await expect(page.locator('#dev-log')).toContainText(
    `registered ${MANIFEST_TOOLS.length} workspace tools`,
  );

  // --- 2. `dev_status` is the first call the guide tells an agent to make --
  const status = structured<{
    active_generation: Generation | null;
    runtime_generation: number;
    blocks: unknown[];
  }>(await execute(page, 'dev_status', {}));
  // Generation 0 came from `/seed/manifest.json` on this origin's first
  // fetch, and it carries no blocks — so nothing has rebuilt the runtime.
  expect(status.active_generation?.cause).toBe('seed');
  expect(status.active_generation?.status).toBe('active');
  expect(status.blocks).toEqual([]);
  expect(status.runtime_generation).toBe(0);

  // --- 3. Read, then write the site --------------------------------------
  //
  // The seed already published `site/index.html`, so a write is an overwrite
  // and has to send the hash it expects to find. Reading it back for that
  // hash is the round trip the tool descriptions tell an agent to make, and
  // it is why `dev_read_file` reports `sha256` at all.
  const seeded = structured<FileRead>(await execute(page, 'dev_read_file', {
    path: 'site/index.html',
  }));
  expect(seeded.encoding).toBe('utf8');
  expect(seeded.content).toContain(WELCOME_PHRASE);

  const writeStart = Date.now();
  const wrote = structured<FileWrite>(await execute(page, 'dev_write_file', {
    path: 'site/index.html',
    content: shopPage(),
    expected_sha256: seeded.sha256,
  }));
  // A `site/**` write publishes immediately — there is no separate deploy.
  expect(wrote.generation, JSON.stringify(wrote)).not.toBeNull();
  expect(wrote.generation?.cause).toBe('site_write');
  expect(wrote.generation?.status).toBe('active');
  expect(wrote.sha256).not.toBe(seeded.sha256);

  // The progress channel (design §4.3): `dev.js` wraps every mutating tool in
  // `withProgress`, which polls `/b/dev/api/status` for the duration of the
  // call and logs each new live generation. Seeing the id this write returned
  // in the panel is the proof the human watching the page learns what the
  // agent did — and that the poll really ran, since nothing else writes that
  // line.
  await expect(page.locator('#dev-log')).toContainText(
    `live generation: ${wrote.generation?.id}`,
  );

  // …and the preview iframe is showing it. `withProgress`'s catch-up reloads
  // the frame after the last outstanding call, so this needs no nudge.
  await expect(page.frameLocator('#dev-preview-frame').locator('h1')).toHaveText(SHOP_HEADING, {
    timeout: 60_000,
  });
  console.log(`site write → published → preview shows it: ${Date.now() - writeStart} ms`);

  // --- 4. Stock the shop -------------------------------------------------
  const productStart = Date.now();
  const product = structured<{ id: string; name: string; status: string }>(
    await execute(page, 'shop_create_product', SHOP_PRODUCT),
  );
  // Created as a draft — invisible to shoppers until something says otherwise.
  // The shop below is empty at this point, which is the state the last step
  // has to move it out of.
  expect(product.status).toBe('draft');
  expect(product.name).toBe(SHOP_PRODUCT.name);

  const offer = structured<{ status: string; offer: { id: string } }>(
    await execute(page, 'shop_create_offer', { product_id: product.id, ...SHOP_OFFER }),
  );
  expect(offer.status).toBe('draft');

  const published = structured<{ status: string; offer: { id: string } }>(
    await execute(page, 'shop_publish_offer', {
      product_id: product.id,
      offer_id: offer.offer.id,
    }),
  );
  expect(published.status).toBe('active');
  expect(published.offer.id).toBe(offer.offer.id);

  const live = structured<{ id: string; status: string }>(
    await execute(page, 'shop_update_product', { id: product.id, status: 'active' }),
  );
  expect(live.status).toBe('active');
  console.log(`product → offer → publish → active: ${Date.now() - productStart} ms`);

  // The agent can see its own work: `shop_update_product` is a mutating tool,
  // so `withProgress` reloaded the preview, and the page the agent wrote
  // three steps ago now lists the product it just activated.
  await expect(
    page.frameLocator('#dev-preview-frame').locator('.shop-product-name'),
  ).toHaveText(SHOP_PRODUCT.name, { timeout: 60_000 });

  // --- 5. The shopper ----------------------------------------------------
  //
  // A NEW PAGE IN THE SAME CONTEXT, not `browser.newContext()`. A Playwright
  // context is an isolated storage partition: its own OPFS and its own
  // service-worker registration, so a second context would boot a second,
  // empty sandbox from the seed and never see a single thing the admin built.
  // The whole sandbox — database included — lives in this origin's storage,
  // so "another visitor" can only mean "the same storage, no session".
  // Clearing the cookies is what makes this page anonymous.
  const shopperStart = Date.now();
  await page.context().clearCookies();
  const shop = await page.context().newPage();
  await shop.addInitScript(MODEL_CONTEXT_POLYFILL);
  await bootServiceWorker(shop);

  // Anonymous for real: `/b/dev` is registered as an `Admin` extra route, and
  // a shopper who could still reach it would mean the cookie clear did
  // nothing and everything below proved only that the admin can see their own
  // work.
  expect(await shop.evaluate(async () => (await fetch('/b/dev/api/status')).status)).toBe(403);

  await expect(shop.locator('h1')).toHaveText(SHOP_HEADING, { timeout: 60_000 });
  await expect(shop.locator('.shop-product-name')).toHaveText(SHOP_PRODUCT.name, {
    timeout: 60_000,
  });
  // The storefront widget resolved the product through
  // `/b/products/storefront/{id}` — a Public route that refuses a product
  // with no ACTIVE offer. Its shadow-root title carrying the name is
  // therefore the end of the chain: create → price → publish → activate all
  // landed, and an anonymous browser can buy from it.
  await expect(shop.locator('impresspress-product').locator('.title')).toHaveText(
    SHOP_PRODUCT.name,
    { timeout: 60_000 },
  );
  console.log(`shopper: anonymous page renders the product: ${Date.now() - shopperStart} ms`);

  // The shopper's agent gets the site's Public tools and NONE of the
  // workspace's: `dev.js` is served only on `/b/dev`, and the tools it
  // registers are scoped to that document's lifetime. What this page runs is
  // the same `webmcp.js` every SSR page runs, against the same
  // `/b/webmcp/manifest.json` — filtered here to the anonymous tier, because
  // the manifest is generated per caller and this caller has no session.
  await waitForTool(shop, 'list_products');
  const shopperTools = (await registeredTools(shop, 1)).map((t) => t.name);
  expect(shopperTools).toContain('list_products');
  expect(shopperTools.filter((n) => n.startsWith('dev_') || n.startsWith('shop_'))).toEqual([]);

  await shop.close();
});

test('the workspace is cross-origin isolated and its preview frames the live site', async ({
  page,
}) => {
  test.setTimeout(300_000);
  await page.addInitScript(MODEL_CONTEXT_POLYFILL);
  await bootServiceWorker(page);
  await openWorkspace(page);

  // `/b/dev` sets COOP + COEP on its own response (`blocks/dev/page.rs`) so
  // the in-browser Rust compiler can use `SharedArrayBuffer`. This is the
  // only place that pair can be observed as the browser sees it — a Rust test
  // can read the header, but only a browser computes the capability from it.
  expect(await page.evaluate(() => window.crossOriginIsolated)).toBe(true);

  // Both documents must carry the pair, and for different reasons. `/b/dev`
  // needs it to BE isolated; `/` needs it to be EMBEDDABLE by an isolated
  // document — per the HTML spec a COEP document may only frame nested
  // documents with a compatible COEP, and that check is origin-independent,
  // so same-origin does not exempt the site. `/` gets it deployment-wide from
  // `wafer-run/security-headers` (`runtime_factory.rs` sets
  // `cross_origin_isolation = "credentialless"` when the sandbox is on).
  const headers = await page.evaluate(async (paths: string[]) => {
    const out: Record<string, { coep: string | null; coop: string | null }> = {};
    for (const path of paths) {
      const response = await fetch(path);
      out[path] = {
        coep: response.headers.get('cross-origin-embedder-policy'),
        coop: response.headers.get('cross-origin-opener-policy'),
      };
    }
    return out;
  }, ['/b/dev', '/']);
  expect(headers['/b/dev']).toEqual({ coep: 'credentialless', coop: 'same-origin' });
  expect(headers['/']).toEqual({ coep: 'credentialless', coop: 'same-origin' });

  // …and the assertion the headers alone cannot make. `credentialless` rather
  // than `require-corp` is the whole point of spec amendment 14: under
  // `require-corp` this frame rendered blank and every header above still
  // read exactly as an operator intended. Only the frame having content
  // proves a COEP page can embed this site.
  await expect(page.frameLocator('#dev-preview-frame').locator('h1')).toHaveText(WELCOME_HEADING, {
    timeout: 60_000,
  });
  await expect(page.frameLocator('#dev-preview-frame').locator('body')).toContainText(
    WELCOME_PHRASE,
  );
});

/**
 * The workspace finds the toolchain this build shipped, and the isolation the
 * toolchain needs reaches the whole deployment.
 *
 * Two facts, tested together because each is the other's precondition. A
 * cross-origin-isolated document is the only place `SharedArrayBuffer` exists,
 * and Rubrc's threads need it; a compiler manifest is the only way the page
 * learns which worker to start. Either one alone would leave `#dev-compile` a
 * button that cannot work.
 *
 * # Why `/` is isolated too, when the design once said it must not be
 *
 * Spec amendment 14. A document with any non-`unsafe-none` COEP may only
 * embed nested documents that carry a compatible COEP, and that check is
 * origin-independent — so as long as `/b/dev` frames the live site, `/` has to
 * carry the pair as well, and the browser then reports it as isolated. The
 * earlier reading ("the published site is NOT isolated") described a
 * deployment whose preview pane rendered blank. `credentialless` rather than
 * `require-corp` is what keeps the cost of that bounded: a site an agent built
 * here can still show a cross-origin image with no CORP header.
 *
 * On this bundle those headers reach `/` from `wafer-run/security-headers`
 * (`runtime_factory.rs`), and reach the compiler's own static files from the
 * service worker (`impresspress-bundle`'s `sw.js.tmpl`) — the static host is
 * `python3 -m http.server` and sends neither.
 */
test('the workspace discovers the packaged compiler on a cross-origin-isolated deployment', async ({
  page,
}) => {
  test.setTimeout(300_000);
  await loginToWorkspace(page);

  // The header is `blocks/dev/page.rs`'s; this is the capability the browser
  // computed from it, which no Rust test can observe.
  expect(await page.evaluate(() => crossOriginIsolated)).toBe(true);
  // …and the one thing that capability is FOR. Rubrc's rustc runs threaded.
  expect(await page.evaluate(() => typeof SharedArrayBuffer)).toBe('function');

  // `dev.js` fetches this on load. A build with no compiler overlay 404s here
  // and leaves the button disabled with a reason on it — the branch
  // `dev_compiler_discovery.test.mjs` covers, since `build.sh` refuses to
  // produce a bundle this spec could drive it with.
  const manifest = await page.evaluate(
    async () => (await fetch('/__impresspress_dev/compiler/manifest.json')).json(),
  );
  expect(manifest.schema_version).toBe(1);
  expect(manifest.entry).toBe(`/__impresspress_dev/compiler/${manifest.version}/worker.js`);

  // What the page did with it. The version is the pinned rubrc sha every
  // compiler URL carries, so a page showing the wrong one is a page that would
  // start the wrong worker.
  await expect(page.locator('#dev-compiler-version')).toHaveText(
    new RegExp(`^Compiler v${manifest.version} · \\d+\\.\\d MiB$`),
  );
  // Compile needs a toolchain AND a block, and this workspace is the seed —
  // `site/**` and nothing under `blocks/`. So the button is still disabled,
  // and the reason on it is what proves the manifest half succeeded: a build
  // that had not found a compiler would say so instead
  // (`dev.js`'s `updateCompileButton`). `dev-compile-tool.spec.ts` is where
  // it becomes enabled, one `dev_create_block` later.
  await expect(page.locator('#dev-compile')).toBeDisabled();
  await expect(page.locator('#dev-compile')).toHaveAttribute(
    'title',
    /No block to compile yet/,
  );
  await expect(page.locator('#dev-compile-block option')).toHaveCount(0);

  // The published site, which the preview pane frames — isolated for the
  // reason in this test's header, not as an accident.
  await page.goto('/', { waitUntil: 'commit' });
  await expect(page.locator('body')).toContainText(WELCOME_PHRASE, { timeout: 60_000 });
  expect(await page.evaluate(() => crossOriginIsolated)).toBe(true);
});

test('the editor refuses to save a binary file over itself', async ({ page }) => {
  test.setTimeout(300_000);

  // `dev.js` uses `alert`/`confirm`/`prompt`; the only one reachable here is
  // `save()`'s refusal alert, which fires for ANY status the server refuses
  // with, not just a 409. Playwright auto-dismisses an unhandled dialog, so
  // without this the one thing a broken guard might surface would vanish
  // silently — the recorder turns it into an assertion instead.
  const dialogs: string[] = [];
  page.on('dialog', (dialog) => {
    dialogs.push(dialog.message());
    return dialog.dismiss();
  });

  await page.addInitScript(MODEL_CONTEXT_POLYFILL);
  await bootServiceWorker(page);
  await openWorkspace(page);
  // `dev_export` is the last tool `dev.js` registers (see the wait above) —
  // this is a readiness barrier, not a read, so waiting by name rather than
  // `PAGE_TOOLS.length` is what it needs, not what it happens to satisfy.
  await waitForTool(page, 'dev_export');

  // A `.png` is binary whatever its bytes: `paths::content_type_for` maps the
  // extension to `image/png`, `may_be_text` says no, and so `dev_read_file`
  // answers `base64` for it — which is exactly the case the editor cannot
  // hold as text.
  const written = structured<FileWrite>(await execute(page, 'dev_write_file', {
    path: PIXEL_PNG_PATH,
    content: PIXEL_PNG_BASE64,
    encoding: 'base64',
  }));
  expect(written.size).toBe(PIXEL_PNG_BYTES);
  // Under `site/`, so it published — which is what makes "no new generation"
  // below a real assertion rather than a property of a path that never
  // publishes at all.
  expect(written.generation?.cause).toBe('site_write');

  const before = structured<{ generations: Generation[] }>(
    await execute(page, 'dev_list_generations', {}),
  );

  // Open it from the file pane, the way the human would. `dev_write_file` is
  // a mutating tool, so `withProgress`'s catch-up has already reloaded the
  // list; the click auto-waits for the entry regardless.
  await page.locator(`#dev-file-list a[data-path="${PIXEL_PNG_PATH}"]`).click();
  await expect(page.locator('#dev-editor-title')).toHaveText(PIXEL_PNG_PATH);

  // The box holds a PLACEHOLDER, not the file. This is the whole hazard: the
  // stored `expected_sha256` still describes the real file, so a save would
  // be accepted, would replace 67 bytes of PNG with this sentence, and would
  // publish a generation for the loss.
  await expect(page.locator('#dev-editor-text')).toHaveValue(
    `(binary file, ${PIXEL_PNG_BYTES} bytes)`,
  );
  await expect(page.locator('#dev-editor-text')).toBeDisabled();
  await expect(page.locator('#dev-save')).toBeDisabled();

  // Half one: the button. A disabled `<button>` swallows the click in the
  // browser, so `force` (which skips Playwright's own actionability checks)
  // still reaches a no-op — nothing runs, nothing to wait for.
  await page.locator('#dev-save').click({ force: true });

  // Half two: `save()`'s own early return, which `page.rs` pins as a source
  // assertion precisely because "a caller could reach it another way". Here
  // that caller is the DOM: re-enable ONLY the button, leaving the textarea
  // disabled, and click it for real. Emptying the file list first is the
  // tripwire — `withProgress` reloads it in its `finally` whichever branch
  // `save()` took, so a repopulated list means the handler really ran and
  // this is not a click that quietly went nowhere.
  await page.evaluate(() => {
    document.getElementById('dev-file-list')!.replaceChildren();
    (document.getElementById('dev-save') as HTMLButtonElement).disabled = false;
  });
  await page.locator('#dev-save').click();
  await expect(page.locator('#dev-file-list a')).not.toHaveCount(0);

  // Nothing moved: same bytes, same hash, same ledger.
  const after = structured<FileRead>(await execute(page, 'dev_read_file', {
    path: PIXEL_PNG_PATH,
  }));
  expect(after.encoding).toBe('base64');
  expect(after.content).toBe(PIXEL_PNG_BASE64);
  expect(after.sha256).toBe(written.sha256);
  expect(after.size).toBe(PIXEL_PNG_BYTES);

  const afterGenerations = structured<{ generations: Generation[] }>(
    await execute(page, 'dev_list_generations', {}),
  );
  expect(afterGenerations.generations.map((g) => g.id)).toEqual(
    before.generations.map((g) => g.id),
  );

  // Any refusal the server sent back would have alerted — `save()` reports
  // every one of them, not just the 409. It refused before it asked.
  expect(dialogs).toEqual([]);
});
