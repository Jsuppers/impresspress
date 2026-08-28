# Design note: a self-contained WebMCP demo site

**Status:** design note, not a plan. Nothing is scheduled. Written 2026-08-28
after wiring the `boutique-store` example against a real server and finding out
what actually works.

**Do not start this before the WebMCP Challenge submission (2026-09-03).**

---

## 1. The idea

`demo-webmcp.impresspress.org`: a storefront that sells things, with the whole
impresspress runtime — products, orders, admin — living in a service worker in
the visitor's own tab. An agent browses it, prices things and buys; a visitor
who wants to can open the admin panel and look around.

It would replace the current Cloudflare Worker demo
(`impresspress-webmcp-demo.jorissuppers.workers.dev`), and it is arguably the
better artifact: nothing to deploy behind it, and every visitor gets their own
world to poke at.

## 2. What already works

### The service-worker build serves the whole API, including the manifest

This is the part that looked hard and is not. `flows/site_main.rs` routes
`/b/**` to `impresspress/router`, which is the same pipeline the native server
runs. `impresspress-browser`'s `dispatch_request` dispatches into that same
`site-main` flow. So a browser build already answers:

- `/b/webmcp/manifest.json` — auth-filtered, same code path
- every tool endpoint (`storefront`, `pricing/preview`, `checkout`, `catalog`)
- `/openapi.json`, `/.well-known/agent.json`
- the admin SSR pages under `/b/admin/**`

No new server-side code is needed for any of it.

### The browser platform layer is real

`impresspress-browser` provides a sql.js database, OPFS storage, a fetch
network service, browser crypto, a console logger and the SW asset loader.
`impresspress-bundle` owns the `sw.js` / `loader.js` / `index.html` templates
and the wasm-pack output bundler.

Its `vector` module is **not** a stub: vectors are BLOBs in the shared OPFS
database and embeddings run in the page through the SW↔page bridge with
Transformers.js. That matters for §4.

### The storefront widget works against a real server

`/b/products/storefront.js` ships a framework-free
`<impresspress-product product-id="…">` custom element, and there are eleven
worked examples under `examples/products/`. Wired against a live server,
`boutique-store` renders its configurator and prices correctly — 449 + 45 wool
+ 25 monogram = **NZD 519.00**, exactly its fixture's `expected_total_label`,
conditional rows and all.

## 3. What does not work yet

### The blocker: `webmcp.js` gives up before the service worker boots

`ui/assets/webmcp.js` fetches `/b/webmcp/manifest.json` **exactly once** and,
on failure, registers nothing — deliberately, so an unsupported browser never
sees an error. In a service-worker build the first paint beats the SW, so a
cold visitor gets **zero tools and no diagnostic**; a reload fixes it.

Plan 3 called this "unproven" and deferred the whole browser runtime on it.
The mechanism is now known and the fix is small, entirely inside `webmcp.js`:
wait on `navigator.serviceWorker.ready` where a SW is expected, and re-register
after activation (the `toolchange` path Plan 3 mentions).

### No browser consumer includes the blocks

`examples/minimal-browser` deliberately excludes `impresspress-core` — it exists
to prove the framework works without it, and its README says "not intended to be
deployed". A demo needs a new consumer crate pulling `impresspress-core` with
`block-products` and `block-admin`, built for wasm32 and bundled. Same shape as
`examples/webmcp-demo` (the Cloudflare consumer), so this is known work.

### (Corrected) A shop at `/` already works — it is just not the default

This was first written here as a gap. It is not one, and the correction matters
because it changes what the future work is actually *for*.

`routing.rs` sends the root to `/b/auth/login` unless
`WAFER_RUN_SHARED__HAS_LANDING_PAGE=true`, in which case `wafer-run/web` serves
static files from the `site` storage folder (on native,
`<cwd>/data/storage/wafer-run/web/site/`). Setting that variable and dropping in
one `index.html` that fetches `/b/products/catalog` and mounts an
`<impresspress-product>` per row gives a working storefront at `/` today —
verified 2026-08-28 against the local server: four products, four live
configurators.

Serving it from impresspress rather than a separate origin also removes the CORS
problem in the next section entirely, and the session cookie rides along, so an
`/b/admin/` link in the page header works for a signed-in admin.

So impresspress deliberately does not ship a customer-facing storefront page —
the model is "you bring the site, we own pricing and checkout" — and bringing
one is genuinely cheap. What the two design notes add on top is different:

- **the pages block** (`2026-08-28-pages-block-design-note.md`) makes that
  storefront *managed* — sections in the database, editable from the admin
  panel or by an agent, instead of a hand-written file someone uploads;
- **this note** removes the server underneath it.

Neither is required to have a shop at `/`. They are required to have one that
can be edited without a deploy, and one that runs with nothing behind it.

### There is no cart

`cart` appears in the products block only as an admin-page icon. Checkout is a
single offer plus quantity plus inputs. A basket spanning several products
would touch the purchase model, line items and the Stripe session. This is the
largest gap between what exists and the demo as imagined, and it is a real
feature rather than wiring.

Worth asking whether the demo needs one. "An agent configured a jacket and
handed me a checkout link" is a complete story without a basket.

### Two rough edges the examples hide

Both were found by pointing an example at a real server for the first time —
the suite normally runs them against a mock on `:4179`, so neither had ever
been exercised.

1. **CORS blocks the widget's whole purpose by default.**
   `WAFER_RUN_SHARED__CORS_ALLOWED_ORIGINS` is empty out of the box, so the
   cross-origin embed the widget exists for fails silently until an operator
   sets it. Not a bug, but a default that makes the primary use case fail on
   first try. Deserves a line in the storefront docs, or a better failure
   message in the widget.
2. **The widget takes an id; the examples pass a slug.**
   `product-id="archive-jacket"` resolves against the mock and 404s against a
   real server, which looks up ids only. Either `/b/products/storefront/{id}`
   should accept a slug, or the examples should carry ids. Today neither side
   is wrong alone and together they do not work.

## 4. Search

`list_products` lists — paginated, sorted by name. The catalog endpoint takes
only `page` and `page_size`; there is no text search, and Plan 3 explicitly
refused to invent an endpoint for one.

Two options, and the second is the interesting one:

- **Query params on the catalog.** A `q` filter over name/description, a
  contract change and a new gate line. Small, boring, obviously correct.
- **Semantic search through the vector block, in the browser.** The browser
  vector service is real (sql.js BLOBs + Transformers.js embeddings via the
  SW↔page bridge), so a demo could embed the catalog on first boot and answer
  "something to wear in winter" without a server. That is a much better
  demonstration than `LIKE '%winter%'`, and it exercises `impresspress/vector`
  — which #79 typed but which ships in no deployed build today.

  Unverified: whether `block-vector` compiles into a browser consumer and
  whether the embedding bridge works end to end. Worth a spike before
  committing to it.

## 5. The property to design around

Every visitor gets their **own** sql.js database in their own browser. For a
public demo that is a feature: anyone can be admin, delete every product, break
whatever they like, and no one else is affected. Reset is "clear site data".

Compare the current Worker demo, where the first visitor to find the admin
panel can ruin it for everyone — and where seeding the demo data at all required
a direct `UPDATE` against D1 because of impresspress#78.

## 6. Constraints

- **Stripe cannot work client-side.** No secret key in a browser, so
  `start_checkout` stays an honest error result. The demo's story has to end at
  "here is your checkout link", or run a tiny real backend just for checkout.
- **wasm weight.** The Cloudflare build with products was 5.56 MB; a browser
  build adding admin will be comparable. That is a real first-load cost for a
  public page and needs measuring before promising it.
- **Agent-driven admin writes stay out of scope.** A human clicking the admin
  panel is fine. An agent *managing* things is the propose-then-approve question
  in `2026-08-28-pages-block-design-note.md`, unchanged.
- **Hosting is simpler than the Worker.** A SW build is a static site — Pages,
  R2, anything. The custom domain is DNS plus static hosting, with no Worker to
  deploy.

## 7. Suggested first slice

Prove the one unproven thing before building a demo on top of it:

1. **Spike the `webmcp.js` timing fix** against a browser consumer that
   includes `block-products`. If a cold visitor registers the tools without a
   reload, the whole idea is viable; if not, everything below is moot.
2. Browser consumer crate + bundle, measured for wasm size.
3. A landing page listing products from the catalog, with the widget for buy
   flows. Cheap — the native equivalent already works (§3), and the same page
   should serve from the SW build unchanged.
4. Then decide on search (§4) and cart (§3) as separate calls.

Steps 1 and 2 are the ones that retire risk. Everything after is ordinary work.
