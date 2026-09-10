# Refactor handoff — 2026-09-10

Fork `main` at `7f731605`. 61 pull requests merged, none open.

Seven phases of the 2026-09-05 architecture review are complete and verified
against a running server. What remains is one architectural decision, already
taken, and the reproduction that has to fail before anyone acts on it.

A rendered version of this document:
<https://claude.ai/code/artifact/0d9a2fa5-e4dc-421e-98cb-1e0986fe1f45>

---

## Where it stands

Phases 0 through 6 are merged. Every pull request was implemented test-first by
one agent, reviewed at high effort by a second, corrected, and merged only after
the reviewer signed off and the full dev-feature suite ran locally by hand.

After the last phase merged, a live server run exercised 332 of 351 declared
endpoints plus WebMCP. **It found breakage that 60 merged pull requests and a
fully green pipeline had all missed.** A user could upload a file but could
neither download nor share it. Both are fixed and confirmed on a live server by
comparing bytes, not status codes.

That run is why this handoff exists. The recurring failure across the whole
programme was never a missing fix. It was coverage that never exercised the real
path — a fixture certifying calls production refuses, a type test that could not
fail, a contract test passing because the published shape was wrong in the same
direction as its consumer.

## What merged

| PRs | Work |
| --- | --- |
| #1–#53 | Phases 0–5: correctness bugs, route tables, platform state, error discipline, runtime lifecycle plus upstream `wafer-run` work, UI migration |
| #54 | Phase 6 spec landed, repository periphery swept |
| #55 | Unpinned third-party clone dropped, CI path filters corrected |
| #56 | A merge to `main` now runs everything a pull request runs |
| #57 | Reusable workflows — one body for both CI gates |
| #58 | Release-workflow residues; stale `packages/impresspress-web` deleted. First dry run in that workflow's history; it had never executed once |
| #59 | One SDK request client; 65 dishonest type exports removed. File transfers keep an unlimited timeout by explicit opt-out |
| #60 | SDK type-freshness gate. Endpoint coverage 56/73 → 73/73 call sites |
| #61 | Storage download and sharing restored, plus a third outage the dependency sweep found |

Upstream `wafer-run` also took #328, #330, #331, #332 during phase 4.

---

## The decision that is waiting

### Admin settings never reach a running server

Worse than it first appears. There are **three read surfaces and two write
surfaces**, and Cloudflare Workers is in a worse position than native on both
sides.

- **Read 1** — async `config::get_default`, ~101 call sites. Native reads
  `EnvConfigService`, seeded once from the variables table (`cli/server.rs`).
  Cloudflare reads `HashMapConfigService`, which by documented design holds only
  the JWT secret, CORS/CSP/strict-schema worker vars, block-settings JSON and
  consumer `request_config` (`impresspress-cloudflare/src/runtime_build.rs`).
  **No D1 variable row ever enters it.** So on Workers `SiteConfig::load`, the
  primary colour, app name, logo and the `IMPRESSPRESS__PRODUCTS__STRIPE_*`
  reads all return defaults whatever an admin saved.
- **Read 2** — sync `ctx.config_get`, 11 production sites, all branding.
  Boot-frozen on native; never holds D1 rows on Cloudflare.
- **Read 3** — `ConfigSource::load_for_block` at `Init`, block-declared keys
  only, so shared `WAFER_RUN_SHARED__*` rows never arrive this way.
- **Write 1** — `admin/ops.rs::update_variable` (the JSON API and the Variables
  page): writes the table only.
- **Write 2** — `ui/settings_form.rs::save_settings`, behind **five** admin
  forms (products, legalpages, userportal, email, auth-ui). Native: writes the
  in-memory overrides only, so it is live but **lost on restart**. Cloudflare:
  `HashMapConfigService::set` is a **documented no-op** and the handler still
  returns `200 Settings saved`.

`docs/2026-07-30-impresspress-optimization-review.md` §CFG-01 reached the same
conclusion six weeks earlier and asked for a reproduction. Nothing followed it.

### The position

**The `impresspress__admin__variables` table is authoritative on every target;
the in-memory maps become caches of it.** Only the table is durable, shared
across Worker isolates, already written by every writer, and already read by
every target's `ConfigSource`. An in-memory store cannot be authoritative on
Workers, and having a different design per target is what produced this.

### The order of work

1. **Reproduce, and stop if it passes.** The Workers case on the wasm harness
   (seed a non-default `WAFER_RUN_SHARED__APP_NAME`, render, assert), plus
   native round trips for `PATCH → get_default` and
   `save_settings → seed_and_load` across a restart. All three should be red
   today. This is the test 61 pull requests never had.
2. **Own `wafer-run/config` in impresspress** instead of registering
   `wafer_core`'s (`builder/registration.rs`). Reads take a variables snapshot;
   writes go through `platform_state::variables::upsert_by_key` with the
   sensitive/SSRF guards `update_variable` already carries. Invalidation reuses
   the generation counter in `config_generation.rs` and shares
   `D1ConfigSource::cached_snapshot`. The handler is async, so no sync bridge.
3. **Delete `EnvConfigService` and `HashMapConfigService`** from the targets,
   and `RuntimeConfig::extend_both(vars)` on native. Native converges on the
   Workers model rather than diverging from it.
4. **Move the 11 sync branding reads to the async client.** `ui/mod.rs` already
   reads the primary colour async while `auth_ui/pages/mod.rs` reads it sync;
   the sync ones are the outliers.

Leave `StaticConfigSource` / `Init`-time keys restart-bound. That is the
documented contract for Init config on every target.

### Deliberately not doing

- **A native-only refresh on write.** Adds a fourth hand-kept copy and leaves
  Workers blind.
- **Changing `ConfigService::set` upstream.** Once we own the block, the raw
  service setter is no longer the interface.
- **The duplicate-key 409.** `DatabaseError` has only `NotFound`/`Internal`; a
  correct 409 needs a producer-side `Conflict` variant and a pre-check races.
  Admin-only endpoint.
- **An SDK version bump.** Nothing is published; the clock starts at first
  publish.

---

## Open for the maintainer

Neither is an engineering call.

**Recent files returns the wrong kind of row.** `GET /b/storage/api/recent`
returns object-*view audit* rows while both SDK consumers read it as object
metadata, and it has behaved that way since `views.rs` was written. #60 made the
published schema match today's behaviour, so either direction is now caught by
the gate. Recommendation: make it return object rows — that is what both
consumers expect, and nothing is published, so the change is free right now.

**SDK versioning after 65 removed exports.** Nothing exists on the public npm
registry under `@impresspress/sdk`, `@impresspress/types`, `impresspress-js`,
`impresspress-web` or `solobase-web` — all 404. Version sits at `1.0.0`.
Recommendation: note it in `RELEASE.md` and move on.

---

## Known and unfixed

Found by the live run, recorded, deliberately deferred.

**Registered is not the same as enabled.** Three symptoms, one cause, one pull
request. `handle_extensions` (`admin/mod.rs`) hardcodes `enabled: true` per
block and never reads `block_settings`. `retain_registered`
(`ui/nav_groups.rs`) gates the sidebar on registration, while
`register_feature_blocks` registers everything compiled in regardless of
enablement — so a disabled Tickets is registered, shown, and then 404s. The
toggle (`admin/pages/blocks.rs`) validates nothing, so a typo writes a permanent
phantom row.

**Four handlers answer 500 for a missing id** — llm model status, llm model
unload, llm threads page, userportal button delete. They bypass the classifier
that already exists for this (`crud.rs`, `DbFailure::Refused` → 404). Two
**public** payment endpoints (`products/stripe.rs`) also answer 500 on a default
install because Stripe is unconfigured, where 503 is right; being public, they
skew error metrics.

**Environment variables are silently overridden.** Not a behaviour bug:
`WAFER_RUN_SHARED__*` values seed on first boot and existing rows then win,
which is intended. The defect is the silence. One `tracing::warn!` when an env
value differs from the stored row.

**Nothing enforces declared dependencies.** A block's `requires` list versus the
`wafer_core::clients::*` its module tree calls is checked by nobody. That gap
caused two of the three outages and was found by a hand sweep. #61 added an
enumeration guard over the storage shim's op dispatch; `access_type_for_op` is
still unguarded, so a new upstream *read* op would silently demand a write
grant. Fail-closed, but unguarded.

---

## What is not proven

Stated plainly so nobody inherits false confidence.

- **Every Cloudflare claim above.** The 332-endpoint run was native. The Workers
  analysis is static tracing corroborated by the July review, not observation.
  This is exactly why step 1 is a reproduction and not a fix.
- **Business logic behind the endpoint surface.** The probe drove parameterised
  paths with non-existent ids and `{}` bodies. 132 of the admin-authenticated
  404s and 49 of the 400s mean the handler was reached and its real work never
  ran. Reachability and auth are proven; behaviour is not.
- **That streaming actually streams.** The new download tests assert bytes, not
  memory behaviour, and the in-memory backend uses the buffering default. They
  would pass equally against a fully buffered path.
- **The Cloudflare/R2 storage path.** `impresspress-cloudflare` is excluded from
  the host suite; its `get_streaming` is a real implementation, and no test in
  the #61 run exercised R2, D1, or the wasm32 build of that path.
- **19 of 351 endpoints were never exercised** — the whole `dev` block, because
  `block-dev` is not in the CLI's default features, so those routes 404 in a
  release binary.

Also worth knowing: WebMCP's two routes (`/b/webmcp/manifest.json`,
`/b/webmcp/webmcp.js`) are pipeline-level and appear in **no**
`*.endpoints.json`, so the endpoint-surface contract does not cover them.

---

## Resuming the work

The working checkout is a git worktree, detached at the last merge:

```
/home/joris/Programs/suppers-ai/impresspress-worktrees/phase0
```

`origin` is the fork `Jsuppers/impresspress`. The organisation repo
`impresspress/impresspress` is frozen as a hackathon submission and its push URL
is deliberately `DISABLED`. Do not re-enable it.

### The process that worked

One agent implements a single pull request, test-first. A second reviews at high
effort, read-only. Findings are fixed. The full dev-feature suite runs locally,
by hand, before merge. Then merge, and branch the next from the result.

The briefs are in [`docs/process/`](process/):

- [`implementation-brief.md`](process/implementation-brief.md) — the standing
  brief every implementer reads first.
- [`review-brief.md`](process/review-brief.md) — the reviewer's shape.
- [`carry-forward.md`](process/carry-forward.md) — accumulated notes from
  earlier phases that later work was meant to pick up.

The review gate sent back every pull request in phases 4, 5 and 6 at least once,
and the finds were not cosmetic: a request path converted from write-free to
writing; a codec adoption that emptied a payments audit trail; a
request-forgery gate that did not survive a redirect; a gate that certified a
response shape the server never sends.

### Verification before any merge

```sh
cargo +nightly fmt --all -- --check
cargo clippy -p impresspress-core --features block-dev,test-support \
      --all-targets -- -D warnings
cargo test -p impresspress-core --features block-dev --no-fail-fast
```

One failure is expected and unrelated: `lockfile_loads_remote_block` fails
locally because the patched `wafer-run` checkout is built without the `wasmi`
feature. Everything else must pass.

**This step is not optional.** A pull request once merged green and broke `main`
because the dev-feature suite was skipped — the dev feature compiles tests the
default run does not.
