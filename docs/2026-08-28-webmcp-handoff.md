# WebMCP + derive-migration handoff

**Date:** 2026-08-28 (updated end of day — see §1 for what moved)
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
| Producer follow-ups | wafer-run **#324** | **MERGED** (`61e68a0`, squash). Review follow-ups filed: wafer-run **#325** (outputSchema wall lacks `required ⊆ properties`), **#326** (`RuntimeError` not `#[non_exhaustive]` + two weak tests) |
| WebMCP consumer | impresspress **#72** | **MERGED** (`8b48bcd`) after the pin bump to `61e68a0` (`e204c98`); CI 13/13 |
| Derive migration | impresspress **#74** | **OPEN** — `feat/derive-migration`; 22 migration commits + 4 follow-ups, `main` (post-#72) merged in as `e27895c` |

### Merge order — resolved

The sequence **#324 → bump pin → #72 → migration** has been executed up to the
migration PR. #324 merged as `61e68a0`; #72's pin was bumped to it (`e204c98`,
`Cargo.lock` re-resolved against the git source, not the local patch); the
migration branch merged the resulting `main` (`e27895c`; a rebase was abandoned after
the first of 27 commits conflicted — merging is what #72 itself did), so its snapshots — all
generated under schemars' **serialize** contract, which only exists from #324 —
now sit on a pin that actually provides it. The one remaining rule: **never
regenerate a snapshot to get green** (§5).

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

Follow-ups landed on the same branch on 2026-08-28, each with its snapshot
diff read line by line:

| Commit | What |
|---|---|
| `fd3c57f` | Deleted `Product`, `ProductTemplate`, `Order`, `OrderLineItem`, `Subscription` (+ `TemplateKind`, `OwnerKind`, `ProductStatus`, which only they used) — produced by nothing, they were the "schema that lies" risk in its purest form |
| `86c21b3` | `PATCH /b/auth/api/me` declared and typed; both handlers share one `me_response` projection, so the write now returns `{user: {…}}` like the read. SDK `updateUser` follows. |
| `7d601c0` | `POST/PATCH/DELETE /b/admin/api/iam/roles*` declared and projected through `AdminRoleView`; the SDK's `IAMService` (wrong envelope, wrong `IAMRole` shape, keyed by name against id routes) now matches the wire |
| `8c1e22d` | Plan 2 Task 6: real wasm cost recorded in `docs/2026-08-26-derive-migration-wasm-measurement.md` — lean +64 KB raw / +34 KB gz, full +157 KB / +67 KB; the estimate's gzip half did not hold; no action needed |

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
Done through the migration PR (#74). What is left is review and merge of
that PR; nothing else is sequenced behind it.

### Decisions — made, implemented in #75
All four were taken as recommended, each test-first, one commit apiece,
stacked on #74:

- **Honeypot** — out of the schema. `deny_unknown_fields` dropped on the
  public submission request; `website` is read from the raw JSON body and
  never declared. A gate test pins that the schema names no `website` and
  carries no `additionalProperties: false`.
- **`/openapi.json` and the agent card** — filtered by the caller's tier with
  the same `routing::effective_access` the router and the manifest use;
  rendered after step 2; `Cache-Control: no-store`. Placement pinned by a
  test that sends a real bearer through step 2. Consumers generating clients
  for Authenticated/Admin endpoints must now fetch with a bearer.
- **`writeOnly` on `CheckoutResponse`** — removed (both fields are only
  ever present in a response); sensitivity stated in the description; the
  `pipeline.rs` assertion inverted.
- **Ad hoc config variables** — absent `sensitive` means sensitive on the
  JSON and form paths; the modal is checked by default and always posts an
  explicit value.

### Work remaining
- **products' 69 unmigrated sites.** 22 need the `components/schemas` hoist; 47
  need typed handlers (currently generic CRUD over DB column maps).
- **The `$defs` → `components/schemas` hoist.** Recursive contracts still
  produce dangling refs in `/openapi.json`. Not a regression — before #323 every
  named type did. The WebMCP side is fail-safe (refused, not published wrong).
- **Plan 3 Task 4 — inspector panel.** Landed in wafer-run #324; the
  impresspress half is now **verified** by `webmcp.spec.ts` (#76):
  `/b/inspector/webmcp` through impresspress's mount answers with all three
  levels, monotone, zero refusals, tool count = `opted_in`. Its columns are
  projected from `ep.auth`, not `routing::effective_access`, and the page
  says so.
- **Plan 3 Task 2 + the mechanics of Task 5 — verified in CI** by
  `crates/impresspress-web/tests/e2e/webmcp.spec.ts` (#76): a real native
  server, `webmcp.js` registering against a polyfilled `document.modelContext`,
  every tool invoked against live endpoints (seeded product, exact price,
  `isError` on a bad receipt and on checkout without a provider).
- **Plan 3 Task 5 — the human half.** Whether an agent *chooses* the right
  tool from its description: browse → price → checkout in a WebMCP-capable
  browser with a Stripe test key, payment confirmed by a person. **The one
  thing no test covers.** Steps are in the plan.
- **Admin JSON writes still untyped:** `users` PUT is projected but
  undeclared; `permissions` and `user-roles` GET/POST/DELETE echo raw
  `RecordList`/`Record`. Same treatment as the role writes (`7d601c0`).
- **`packages/impresspress-js/src/types/generated/database.ts`** is
  hand-maintained despite its path, and its other interfaces are camelCase
  (`userId`, `keyPrefix`, `createdAt`) against a snake_case wire. `IAMRole` was
  fixed; the rest needs the same audit against the Rust views.
- **wafer-run #325** — the outputSchema wall does not check
  `required ⊆ properties` (input side does). Only reachable from hand-written
  `.output_schema(...)`; none of #72's six tools trip it. Fix upstream.
- **wafer-run #326** — `RuntimeError` should be `#[non_exhaustive]`; two
  inspector/seal tests assert less than they appear to.

---

### Submission checklist — The WebMCP Challenge (deadline 2026-09-03 13:00 PDT)
The spec's §Submission context, as of 2026-08-28 end of day:

| Requirement | State |
|---|---|
| MIT `LICENSE` on the default branch | ✅ on `main` since #72 |
| Tool-registration code visible in-repo | ✅ `crates/impresspress-core/src/ui/assets/webmcp.js` on `main` |
| Both repos public, write-up names both | ✅ public; ❌ write-up not started |
| Live URL reachable from the ChatGPT browser | ❌ not deployed. `wrangler` is logged in; the deployable is a *consumer* crate (`impresspress-cloudflare` is a library — see the wasm measurement doc), so the target is wafer-site's Cloudflare build or a minimal consumer. Outward-facing: needs a go. |
| Demo video < 3 minutes | ❌ human |
| Agent-driven browse → price → checkout | ❌ human, plan 3 task 5 |

Engineering that does **not** gate the submission but is still open: products'
69 unmigrated sites; `llm` (18 endpoints) and `vector` (11) declare no schemas
at all (spec step 8, native-only, non-gating); the `$defs` hoist (wafer-run).

## 5. Traps for whoever picks this up

- **`Cargo.lock` is rewritten by the local `[patch]`.** That rewrite is an
  artifact — `git checkout Cargo.lock` before committing — but the rule is
  *not* "never commit the lock". When a `Cargo.toml` changes (this branch
  added `schemars` and wafer-block's `json-schema` feature), CI's `--locked`
  jobs need the matching lock; #74 sat red on exactly that until the lock was
  re-resolved from outside the tree (next bullet) and committed. Do not pass
  `--locked` locally.
- **A worktree-local `.cargo/config.toml`** (gitignored) patches the wafer
  crates at a wafer-run worktree. The repo-level one points at `../wafer-run`,
  which sits on an unrelated branch. Delete the local override once
  `../wafer-run` tracks a main containing the merged work.
- **Never bare `git stash` / `git stash pop`.** The stash stack is shared across
  worktrees and other sessions. Use a temporary WIP commit.
- **`Cargo.lock` for a pin bump must be re-resolved from *outside* the
  impresspress tree** (`cargo metadata --manifest-path … ` from the scratchpad):
  cargo finds `.cargo/config.toml` by walking up from the *cwd*, so anywhere
  under `impresspress/` inherits the repo-level `[patch]` and writes path
  sources into the lock. `e204c98` on #72 was produced this way.
- **`impresspress-cloudflare` is a library.** `worker-build` inside the crate
  yields a 57 KB shell with nothing reachable. Measure through a consumer —
  the exact one is in `docs/2026-08-26-derive-migration-wasm-measurement.md`.
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
