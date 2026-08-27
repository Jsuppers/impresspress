# WebMCP + derive-migration handoff

**Date:** 2026-08-28
**Repos:** `wafer-run` (producer), `impresspress` (consumer)

Goal: expose impresspress functionality as [WebMCP](https://github.com/webmachinelearning/webmcp)
tools, with schemas derived from the Rust types handlers deserialize into rather
than hand-written JSON. Governing rule throughout: **a tool that can lie about
its arguments is worse than no tool** — refuse rather than mislead.

---

## 1. State of play

| Work | Where | State |
|---|---|---|
| WebMCP producer | wafer-run **#323** | **MERGED** (`ee103db`) |
| Producer follow-ups | wafer-run **#324** | **OPEN** — `feat/webmcp-producer-followups` |
| WebMCP consumer | impresspress **#72** | **OPEN** — `worktree-webmcp-feasibility` |
| Derive migration | impresspress | **UNPUSHED** — `feat/derive-migration`, 22 commits |

### ⚠️ Merge order is load-bearing

**wafer-run #324 must merge before impresspress #72 and before the derive
migration.** Two independent reasons:

1. **#72 does not compile without it.** `builder/registration.rs:388` reads
   `WebMcpRefusalReport::scope`, which only exists in #324. Its CI is red until
   #324 lands and the rev pin is bumped a second time.
2. **The migration's snapshots are only correct with it.** Every
   `.output::<T>()` schema on `feat/derive-migration` was generated under
   schemars' **serialize** contract, which #324 introduces. `wafer-block` at the
   currently pinned `ee103db` still uses plain `schema_for!` for all four
   builders. Merge the migration first and the output schemas silently revert to
   deserialize semantics — **and the snapshot gate will not catch it**, because
   the baseline would simply be regenerated to match.

Sequence: **#324 → bump pin → #72 → migration.**

---

## 2. What is done

### wafer-run — producer (#323, merged)

`generate_webmcp*` in `crates/wafer-core/src/discovery.rs` is a third projection
of `BlockInfo::endpoints`, beside `generate_openapi` and `generate_agent_card`.

- **Opt-in.** `.agent_tool(name, description)` on `BlockEndpoint`. Carrying a
  schema is not consent. Names validated at boot.
- **Auth-filtered.** Tools above the caller's level are omitted entirely, not
  marked unavailable — a name an agent cannot use is recon surface.
- **Self-contained schemas.** The typed builders inline subschemas and drop
  document-level keys, so an embedded schema has no dangling `#/$defs/…`.
- **Nine refusal reasons**, each reported with block, method, path, tool name.

### wafer-run — follow-ups (#324, open)

- `outputSchema` projected into tools (drops the *field* if unrepresentable, not
  the tool — `inputSchema` is mandatory so omitting it claims "no arguments";
  `outputSchema` is optional so its absence claims nothing).
- Duplicate tool names now counted **per manifest**, closing an existence
  oracle; a `seal()` phase makes a cross-block collision a **boot failure**, so
  the runtime census is a net that should never fire.
- `generate_webmcp(blocks, caller, resolver)` is the safe, short name;
  `generate_webmcp_declared_auth` is the narrow one.
- Inspector gains a per-auth-level manifest view.
- **`.output::<T>()` now uses schemars' SERIALIZE contract** (`Contract::Serialize`);
  the other three builders stay on deserialize.

### impresspress — consumer (#72, open)

- `GET /b/webmcp/manifest.json`, filtered by `routing::effective_access` so the
  manifest mirrors what `route_to_block` actually admits (`max(prefix_tier,
  ep.auth)`, `router_final` short-circuiting).
- **Placement is load-bearing**: the branch runs *after* pipeline step 2. At
  step 0 `msg.user_id()` is empty, so every caller silently gets the anonymous
  manifest — invisible in a smoke test, since that is a valid document. There is
  a regression test.
- `ui/assets/webmcp.js` registers tools, rebuilds requests from `invocation`,
  passes `outputSchema` through, no-ops on unsupported browsers, isolates
  per-tool registration.
- Six storefront tools. `start_checkout` returns a Stripe-hosted URL — **no tool
  completes a payment**, structurally.
- Refusals logged once at `ImpresspressBuilder::build()` (per process natively,
  per isolate on Workers) rather than per request on an unauthenticated route.
- MIT `LICENSE` added (the repo had none).

### impresspress — derive migration (`feat/derive-migration`, 22 commits, unpushed)

Per-block `/openapi.json` snapshot gate first, then block by block:

| Block | Result |
|---|---|
| gate | built; probed per block — each fails naming itself |
| products | 54/123 sites; 69 blocked (22 recursive `Condition`, 47 handlers with no contract type) |
| auth_ui | 8/8; 9 types written; `pipeline.rs` auth assertions passed unmodified |
| files | 4 sites; local mirror of the wafer wire type, handler serializes it |
| messages | 7 sites, 3 migrated; `UpdateContextRequest` replaced a `HashMap` + runtime whitelist |
| admin | 4 endpoints typed from scratch; off the empty-allowlist |
| tickets | 13/13 JSON endpoints; off the empty-allowlist |

---

## 3. Security findings — the most valuable output

None of these were found by auditing. They surfaced because writing a true type
for a handler forces you to answer what it actually returns.

**Live leak, fixed.** `GET /b/admin/api/users` was publishing
`verification_token` — the sha256 of the email-verification token — because the
handler echoed the raw DB row. A defensive `remove("password_hash")` sat right
beside it and was a **no-op**: that column does not exist on that table. Fixed
by projection (`3fc19fb`). `PUT /b/admin/api/users/{id}` had the same leak plus
`last_verification_sent` and `auth_version` — fixed (`0ed8e20`).

**Live leak, fixed.** Tickets echoed `dedupe_hash` on GET/POST/PATCH —
`hmac(identity_secret, rotating_identity ‖ report)`, where `rotating_identity`
is an HMAC of the reporter's **IP**. The same block already redacts that
identity in its rate-limit logs; it was sensitive in one place and public in
another.

**Quarantine bypass, fixed.** `service::detail` groups the 8 reporter-controlled
ticket columns under `untrusted_report`, commented "must never be interpreted as
tool instructions". POST/PATCH returned `subject`/`description`/`evidence_url`/
`reporter_email` **ungrouped**, and the inbox listed `subject` flat. Now
structural across every ticket shape.

**No schema could have been true before.** `email_verified`/`disabled` are
INTEGER on SQLite/D1 and BOOLEAN on Postgres; `permissions` is JSON TEXT only
SQLite decodes. The admin response shape depended on the backend.

---

## 4. What still needs doing

### Blocking / sequencing
1. **Merge wafer-run #324**, then bump the rev pin in `impresspress/Cargo.toml`
   (18 entries) and `Cargo.lock` to the new merge commit.
2. **Then** #72 goes green and can merge.
3. **Then** push and PR `feat/derive-migration`.

### Decisions needed (deliberately not made)
- **Honeypot is now self-defeating.** `deny_unknown_fields` forces the public
  ticket input schema to name the honeypot field `website`, described "Must be
  left empty." A honeypot works because bots fill it. Drop
  `deny_unknown_fields` for that input, rotate the name, or accept it.
- **`/openapi.json` is unauthenticated** (`pipeline.rs:121`), so the admin
  schemas this migration created are anonymously readable. Recorded as an
  accepted trade in the spec while it was theoretical; admin now has schemas.
- **`CheckoutResponse.receipt_token` / `client_secret`** are both
  `writeOnly: true` and in `required` of a response schema — contradictory under
  OpenAPI 3.1. `pipeline.rs:635` asserts the flag, so it is load-bearing.
- **Ad hoc config variables** created with the `sensitive` box clear and no
  `_SECRET`/`_KEY` suffix are published in plain text. Masking works as
  designed; the design leans on the operator. Recommended fix in
  `admin-leak-fix-report.md`: default `sensitive: true` on create.

### Work remaining
- **Plan 2 Task 6 — measure the real wasm delta.** The spec's `+94 KB raw /
  +21 KB gzip` is from a synthetic benchmark. Build the Cloudflare consumer at
  the pre-migration commit and at HEAD with the same feature set (see
  `cli/helpers/cloudflare/build.rs`) and record it. If it disappoints,
  `worker-build` defaults to `wasm-opt -O` not `-Oz` (~−215 KB available).
- **products' 69 unmigrated sites.** 22 need the `components/schemas` hoist; 47
  need typed handlers (currently generic CRUD over DB column maps).
- **The `$defs` → `components/schemas` hoist.** Recursive contracts still
  produce dangling refs in `/openapi.json`. Not a regression — before #323 every
  named type did. The WebMCP side is fail-safe (refused, not published wrong).
- **Plan 3 Task 4 — inspector panel** in impresspress (the wafer-run half landed
  in #324).
- **Plan 3 Task 5 — end-to-end run.** Needs a human with a WebMCP-capable
  browser. **This is the only thing that tests whether the tool descriptions
  actually steer an agent**, which is the part most likely to be wrong and least
  covered by tests. Steps are in the plan.
- **`PATCH /b/auth/api/me` is dispatched but never declared** in `.endpoints`,
  so it is absent from `/openapi.json` and the access-tier table. It also
  returns a flat user object where `GET` returns `{user:{…}}`.
- **Five products contract types** (`Product`, `ProductTemplate`, `Order`,
  `OrderLineItem`, `Subscription`) are referenced by **no handler or repo**.
  They look canonical and are not. Either wire or delete them before someone
  migrates against them.
- **`impresspress-js` `IAMService` types are wrong** — declares `IAMRole[]`,
  receives an envelope.

---

## 5. Traps for whoever picks this up

- **`Cargo.lock` is rewritten by the local `[patch]`.** It is an artifact —
  `git checkout Cargo.lock` before committing, never include it. Do not pass
  `--locked` locally.
- **A worktree-local `.cargo/config.toml`** (gitignored) patches the wafer
  crates at a wafer-run worktree. The repo-level one points at `../wafer-run`,
  which sits on an unrelated branch. Delete the local override once
  `../wafer-run` tracks a main containing the merged work.
- **Never bare `git stash` / `git stash pop`.** The stash stack is shared across
  worktrees and other sessions. Use a temporary WIP commit.
- **Never regenerate a snapshot to get green.** Every changed line is a
  decision. Regenerating without reading is the one way to fail the migration
  while appearing to pass it.
- **A gate only catches what enters after it is set.** This already bit once: a
  wrong premise (`#[schemars(required)]` is inert) produced a widening that the
  gate absorbed into its baseline and then concealed. When a premise turns out
  wrong, re-verify against the **pre-change** reference, not the current
  baseline.
- **`#[schemars(required)]` is not inert.** On `Option<T>` it narrows away from
  `["T","null"]`; on non-`Option` it does nothing. It is a **response-side**
  lever — never apply it to an input.
- **`///` becomes the published `description`.** Rationale about a sensitive
  field lands in the document describing that field. Use `//`.
- **Seven tests on this project asserted less than they appeared to.** Ask of
  every test: what would still pass if the feature were broken?

---

## 6. Where the detail lives

Working notes, per-task reports and every ruling are in the gitignored SDD
workspaces under `.superpowers/sdd/` in the impresspress worktree:

- `2026-08-26-webmcp-1-wafer-run-producer/` — producer, both fix waves, audit fix
- `2026-08-26-webmcp-3-surfacing/` — consumer tasks, log-spam fix
- `2026-08-26-webmcp-2-derive-migration/` — the migration, per block, plus
  `progress.md` (the ledger) and `admin-leak-fix-report.md`

**These are gitignored and will not survive a `git clean -fdx`.** If they matter,
copy them somewhere tracked before cleaning.

Design docs are tracked on `worktree-webmcp-feasibility` (PR #72) under
`docs/superpowers/`: the spec plus three implementation plans. The spec records
where implementation overrode it and why.
