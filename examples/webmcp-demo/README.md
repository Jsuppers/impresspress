# WebMCP demo Worker

**Live:** https://impresspress-webmcp-demo.jorissuppers.workers.dev

impresspress with the products block, deployed to Cloudflare Workers so a
WebMCP-capable browser can register the storefront tools from a public URL.
This is the live deployment behind the WebMCP Challenge submission; it is also
the smallest complete Cloudflare consumer in the repo.

There is no consumer code to speak of: `src/lib.rs` is the three Worker entry
points (`start`, `fetch`, `scheduled`) handing every request to
`impresspress_cloudflare`. Everything the demo shows lives in impresspress
itself:

- `crates/impresspress-core/src/blocks/products/mod.rs` — the six storefront
  endpoints annotated with `.agent_tool(...)`.
- `crates/impresspress-core/src/ui/assets/webmcp.js` — on every page, fetches
  `/b/webmcp/manifest.json` (filtered to the visitor's auth level) and calls
  `document.modelContext.registerTool` per tool.
- `crates/impresspress-core/src/pipeline.rs` — serves that manifest after auth
  resolution, so an anonymous visitor's agent never learns a tool it cannot
  use.

## Deploy

One-time, on the target account:

```sh
wrangler d1 create impresspress-webmcp-demo        # note the database_id
wrangler r2 bucket create impresspress-webmcp-demo
wrangler kv namespace create impresspress_webmcp_demo_CONFIG_CACHE   # note the id
```

The Worker also needs that KV namespace bound as `CONFIG_CACHE` (the D1 config
cache; `/_deploy/prepare` fails with `invalid kv store: CONFIG_CACHE` without
it). The deploy CLI does not emit KV bindings, so it comes from
`wrangler.overrides.toml`, deep-merged into the generated `wrangler.toml` —
put the namespace id there.

The same file sets `IMPRESSPRESS_ALLOW_WORKERS_DEV = "1"`: the adapter answers
404 on any `*.workers.dev` host unless a consumer opts in (it expects a custom
domain; `impresspress-cloudflare/src/lib.rs`, `host_is_workers_dev`). The demo
has no custom domain, so it opts in. The opt-in admits the canonical host
only — a version-preview host (`<8-hex>-<worker>.…workers.dev`) stays locked,
which is what lets `impresspress deploy`'s pre-promotion lockdown check pass.

Then, from this directory, with the wafer-run `[patch]` the workspace uses
(see the repo-level `.cargo/config.toml` note) and `wrangler` logged in:

```sh
export CLOUDFLARE_ACCOUNT_ID=<account id from `wrangler whoami`>
export IMPRESSPRESS_CLOUDFLARE_D1_DATABASE_ID=<id from `wrangler d1 create`>
export IMPRESSPRESS_DEPLOY_TOKEN=<any random string>

impresspress build --target cloudflare       # worker-build + generated wrangler.toml

# First deploy only. `impresspress deploy` uses `wrangler versions upload`,
# which cannot create a Worker (Cloudflare error 10007) — create it once with
# a plain deploy of the artifact the build just produced:
wrangler deploy --config target/impresspress-cloudflare/wrangler.toml

impresspress deploy --target cloudflare secret   # IMPRESSPRESS_DEPLOY_TOKEN + JWT secret, once
impresspress deploy --target cloudflare          # atomic versioned deploy, runs /_deploy/prepare
```

## The scheduled sweep is opt-in — two steps

The auth retention sweep (expired sessions, tokens, JWT blocklist rows, OAuth
PKCE rows) runs opportunistically on login, so a busy deployment already prunes.
The cron exists for a deployment with no logins. It is **off by default**:
`impresspress` cannot supply the `scheduled` export for you (it needs your own
registration hooks) and the CLI scaffolds no consumer source file, so a default
schedule would buy every existing consumer a daily failed invocation for code
they never wrote. Turn it on with both of:

1. **Export a `scheduled` entry point.** `src/lib.rs` here is the worked
   example — `#[worker::event(scheduled)]` handing off to
   `impresspress_cloudflare::run_scheduled`, passing **the same two
   registration hooks** the `fetch` entry point passes. Both entry points share
   one per-isolate runtime cache; a different block set on the cron would
   publish the wrong runtime for the next request to serve.
2. **Set `[cloudflare].crons` in `impresspress.toml`.** `crons = ["17 3 * * *"]`
   here: once a day at an off-peak minute that is not `:00`, since Cloudflare
   fires every account's `0 * * * *` at the same instant.

Step 2 without step 1 is a daily invocation that fails. Step 1 without step 2 is
dead code. Setting `crons = []` (or leaving it unset) removes the schedule from
the live Worker on the next deploy.

**Trap:** cron triggers are a *worker-level* setting, and `wrangler versions
upload` / `wrangler versions deploy` — which is exactly what `impresspress
deploy` runs — do not apply them. The upload says so itself: *"Changes to
triggers (routes, custom domains, cron schedules, etc) must be applied with the
command `wrangler triggers deploy`"*. `impresspress deploy` now runs that
command after promotion, so the schedule does reach the live Worker; if you
deploy by hand with `wrangler versions upload`, you have to run it yourself.

The first admin. The auth block grants the `admin` role to whoever signs up
with the email in `WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL`
(`blocks/auth/mod.rs`, `initial_role_for`) — but on Workers that shared
variable, like every `WAFER_RUN_SHARED__*` variable, currently has no read
path (impresspress#78), so setting it in D1 does nothing. Until #78 is fixed:
sign up, then promote the row directly and log in again:

```sh
curl -X POST https://<worker>.workers.dev/b/auth/api/signup -H 'Content-Type: application/json' \
  -d '{"email":"<you@example.com>","password":"<password>","name":"Admin"}'
wrangler d1 execute impresspress-webmcp-demo --remote --command \
  "UPDATE wafer_run__auth__users SET role = 'admin' WHERE email = '<you@example.com>';"
```

For the same reason `WAFER_RUN_SHARED__ENVIRONMENT`, `FRONTEND_URL` and
`APP_NAME` edits made in the admin UI do not reach the Worker yet; the demo
runs on their defaults (`development`: discovery documents carry
`Access-Control-Allow-Origin: *`, cookies are not `Secure`).

Stripe keys are entered afterwards in the admin UI (`/b/admin/variables`).
Without a Stripe key the `start_checkout` tool returns an error result rather
than a URL — which is the honest answer, and what the e2e spec
(`crates/impresspress-web/tests/e2e/webmcp.spec.ts`) pins.

## Try it

- `GET /b/webmcp/manifest.json` — the five Public tools anonymously; six with
  an authenticated session.
- Open any page in a WebMCP-capable browser and run
  `await document.modelContext.getTools()`.
