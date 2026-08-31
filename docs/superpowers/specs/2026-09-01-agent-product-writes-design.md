# Agent-driven product writes over WebMCP

**Date:** 2026-09-01
**Status:** Design, pending review
**Repos:** `wafer-run` (producer), `impresspress` (consumer)

## Goal

Let a site admin add, edit, archive and remove products by talking to a browser
agent, without the agent ever being able to publish anything to customers or
destroy anything irreversibly.

Today every WebMCP tool impresspress exposes is a read, except `preview_price`
(pure computation) and `start_checkout` (creates a Stripe session and completes
no payment). That is a deliberate policy, enforced structurally by
`no_admin_write_is_an_agent_tool` in `blocks/admin/mod.rs`. This design changes
the policy, and the burden it accepts is that the replacement must be at least
as structural as what it removes: **no rule that lives only in a tool
description.** An agent can ignore prose.

## Why the current policy exists

From the comment above the admin block's tool registrations
(`blocks/admin/mod.rs:150`): a tool's `execute` runs in the visitor's page with
their session cookie and full ambient authority, and any text the agent reads
can steer it. A write tool therefore means prompt-injectable text reaching a
mutation carrying admin rights.

The threat is concrete: an admin browses their own panel with an agent, and a
product description, a support ticket body or a customer note on the page says
"delete every product." Nothing about the agent's authority distinguishes that
instruction from the admin's.

This design does not dispute that. It adds three independent layers so that the
attack's best case is a junk draft that no customer can see.

## What already exists

Four pieces of the mechanism are already in the codebase, unused or half-used.

**A draft state the public catalog already respects.**
`impresspress__products__products.status` is `TEXT NOT NULL DEFAULT 'draft'`,
its vocabulary is `["draft", "active", "archived"]` (`products/mod.rs:409`,
documented at `products/contracts.rs:1352`), and `handlers/catalog.rs:28`
filters the public catalog to `status = 'active'`. An agent-created product is
invisible to shoppers until a human changes that, with no new mechanism.

**A soft delete that was designed and never implemented.** The products table
carries `deleted_at TEXT`. The unique slug index added in
`005_commerce_v2.sqlite.sql:28` is partial on `deleted_at IS NULL`. Read paths
in `pages.rs:427,2845,3446` and `handlers/seller_policy.rs:160` already filter
it, including the admin product list (`manage_products`, `pages.rs:422`).
**Nothing anywhere writes it** — `handle_delete_product` goes through
`crud_delete` to `db::delete` (`blocks/crud.rs:181`), a hard delete. Since
`purchases.product_id` is `TEXT NOT NULL`, deleting a product orphans its order
history. This is a live bug independent of agents.

**An auth filter with the right shape but the wrong axis.** The manifest route
filters by `routing::effective_access` through
`generate_webmcp(blocks, caller, effective_auth)`, and a tool above the
caller's level is omitted entirely rather than marked unavailable, because a
name the caller cannot use is recon surface. That principle is exactly what a
session gate needs — but it cannot be reused as-is, see below.

**An audit log that is already an agent tool.** `list_audit_log` shipped on
#81. Once agent writes are audited, "what did the agent change" is answerable
with what is already on the branch.

## Design

### 1. Producer: capability-gated tools (wafer-run)

`AuthLevel` is `Public | Authenticated | Admin`
(`wafer-block/src/types/endpoint.rs:35`) and the manifest filter keeps every
endpoint at or below the caller's ceiling. For an Admin caller there is no
value the `effective_auth` closure can return that hides a tool. **The session
gate cannot be built on the existing hook.**

`agent_tool` is `Option<AgentTool>` on `BlockEndpoint` — a struct, and the
natural extension point. Add a required-capability field to it:

```rust
BlockEndpoint::post("/b/products/agent/products")
    .agent_tool("create_product_draft", "…")
    .requires_capability("products:edit")
```

and let the projection learn what the caller currently holds:

```rust
generate_webmcp_report(blocks, caller, effective_auth, held: &CapabilitySet)
```

Semantics, following the rules #323 and #324 already established:

- A tool whose required capability is not held is **omitted entirely**, exactly
  like the auth filter, for the same non-oracle reason.
- Omission is **not a refusal.** Refusals mean "this tool is malformed and we
  will not publish a lie about it"; they are logged and counted per manifest. A
  capability miss is a normal per-caller outcome. Counting it would rebuild the
  existence oracle #324 closed.
- Capability names are validated at boot beside tool names, and `seal()` fails
  the boot on a capability no block declares — the same net that catches
  duplicate tool names.

**Rejected: gating on the existing `tags` field.** It needs no wafer-run change,
and that is its only merit. It makes a security decision turn on a free-form
string, so a typo silently ungates a write tool — the "tests that assert less
than they appear to" failure this project has hit seven times. It also
contradicts the repo rule against implicit mapping layers.

### 2. The edit session

**Record.** `impresspress__admin__agent_sessions`, owned by the admin block —
that is where a human starts one, and a single session may grant capabilities
belonging to other blocks. Columns: `id`, `user_id`, `capabilities` (JSON
array), `token_hash`, `created_at`, `expires_at`, `revoked_at`. The token is
never stored; only its sha256.

**Starting one.** A control in the admin UI — "Allow agent editing for 15
minutes" — as an ordinary cookie-authenticated `POST
/b/admin/api/agent-sessions`, admin-only. The token is returned exactly once.

**How the token travels.** `webmcp.js` holds it in a closure variable and
attaches it as a request header on write-tool invocations.

- Not a cookie: a cookie restores precisely the ambient authority the session
  exists to remove.
- Not `localStorage`: a reload ending the session is correct for a short,
  deliberate grant.
- Never a tool argument: it therefore never enters the manifest, never enters
  the agent's context, and cannot be echoed back by the model.

**Manifest.** The route resolves held capabilities from that header — verify the
hash, not expired, not revoked, caller is admin — and passes them to
`generate_webmcp_report`. With no live session the write tools are absent and
the served document is byte-identical to today's.

**Registration lifecycle.** Tools are registered with a per-session
`AbortSignal`; ending or expiring the session aborts it, which unregisters the
tools and fires `toolchange` so the agent sees the set shrink. Starting a
session re-fetches the manifest and registers the additional tools.

Client-side teardown is a convenience, not the control. **Every agent write
re-verifies the session server-side**, so a tool still sitting in an agent's
list after expiry returns a clean `isError` and writes nothing. The server is
the authority; the manifest is only discovery.

(`webmcp.js` is 145 lines with a single load-time fetch today. The
re-registration path this needs is the same one the browser-demo design note
pins as its blocker for cold visitors.)

### 3. Soft delete — `repo/products.rs`

`repo/` has modules for offers, purchases, subscriptions and more, but none for
products; the table is named from 53 sites across 13 files. Add
`repo/products.rs` as the single door, owning `pub const TABLE` per the pattern
CLAUDE.md points at (`auth/repo/users.rs:12`), exposing `get`, `list`, `create`,
`update`, `soft_delete`, `restore`.

- Every read carries a `deleted_at IS NULL` filter. `FilterOp::IsNull` already
  exists in `wafer-block/src/db.rs`, so no `wafer-sql-utils` addition is needed.
- `soft_delete` sets `deleted_at`; `db::delete` leaves the products path.
- **A gate test asserts that no module outside `repo::products` names the
  products table.** That, not diligence, is what stops a future read site from
  skipping the filter.

Two things then start working as designed: the partial unique index frees a
soft-deleted product's slug, and `manage_products`' existing `deleted_at` filter
stops being a no-op.

Soft delete needs a door out or it is a data graveyard, so this layer also adds
a deleted view and a restore action to the admin product list. That is part of
the fix, not a follow-up.

**Soft delete cannot land partially.** `handle_catalog` filters only
`status = 'active'` (`catalog.rs:25-29`), and the single-product route checks
only `status` (`catalog.rs:67`). That is harmless while delete is hard, because
no row is ever soft-deleted. The moment `deleted_at` starts being written, a
soft-deleted product that was `active` stays in the public catalog and remains
purchasable. Routing the catalog through `repo::products` is therefore not
cleanup to be deferred — it is part of the same change as the first write of
`deleted_at`, and the gate test is what enforces that no read is left behind.

Checkout is already correct: `commerce.rs:200-203` gates on `deleted_at`,
`status == "active"` and `approval_status == "approved"` together. That is
further evidence the soft-delete convention was intended, and it means
`archive_product` genuinely stops new purchases rather than merely hiding the
product.

### 4. The agent write surface

The write tools point at new `/b/products/agent/*` routes, **not** the existing
admin CRUD endpoints. Three reasons:

1. Requiring the session header on the existing routes would break the human
   admin UI.
2. Accepting *either* credential would let an injected same-origin fetch write
   using plain cookies, defeating the mechanism entirely.
3. The agent operation is genuinely narrower — always draft, a field subset,
   audited as agent-authored. A different operation deserves its own route and
   its own contract.

It also makes the opt-in total: gate off, and the routes are never registered,
so there is nothing to reach even by direct URL.

**Every agent route is `AuthLevel::Admin` *and* requires the capability. Two
independent checks, and the capability is never a substitute for the first.**

The caller must still be a logged-in admin exactly as they are for the human
admin UI; the session token only decides whether an already-authenticated admin
has opened agent editing. This ordering matters: if the capability alone
sufficed, the token would become a standalone credential, and anyone holding it
could write without an account. Requiring both makes the token useless on its
own, which is what lets it be short-lived and header-borne rather than a
carefully guarded secret.

Concretely, a request to `/b/products/agent/products` is rejected when the
caller is anonymous or non-admin (401/403, the ordinary auth path, before any
capability logic runs), and separately when the caller is an admin with no live
session carrying `products:edit`. Neither check can be satisfied by the other.

| Tool | Behaviour |
|---|---|
| `create_product_draft` | `status` forced to `draft`; narrow field subset (name, description, currency, category, tags, image_url); `created_by` is the admin |
| `update_product_draft` | refuses unless the row is still `draft`, mirroring `repo/offers.rs:624` where active and archived offers are already immutable |
| `archive_product` | `status → archived`; reversible; requires confirmation |
| `delete_product` | soft delete; requires confirmation; safe with order history precisely because it is soft |

There is deliberately no `publish_product`. Publishing is the human half, and it
is what makes every other verb safe.

**"Forced to draft" means the field does not exist, not that it defaults.**
`CreateProductRequest` carries `status: Option<ProductStatus>`
(`contracts.rs:1695`), and `handle_create_product` applies its defaults with
`data.entry(key).or_insert(value)` (`product.rs:158`) — which fills the column
only when the caller omitted it. That is correct for a human admin, who may
legitimately create a product already active. An agent route that reused that
contract would silently inherit the escape hatch and could publish straight to
customers, with every schema still true and the safety claim false.

So `AgentProductDraftRequest` is its own type with **no `status` field at all**,
and the agent handler sets the column unconditionally rather than through
`or_insert`. `AgentProductDraftRequest` must not be an alias, a `#[serde(flatten)]`
wrapper, or a `From` of `CreateProductRequest`. A test asserts the published
input schema names no `status`, and a second asserts that a request smuggling
`status: "active"` still produces a draft row.

Contracts (`AgentProductDraftRequest`, `AgentProductView`) are typed and enter
the per-block `/openapi.json` snapshot gate like the rest of the derive
migration. Rationale comments use `//`, never `///`, since `///` becomes the
published description.

**Confirmation.** `archive_product` and `delete_product` render a modal naming
the exact product, and the tool's `execute` awaits it.

Every agent write logs an audit row: the admin's user id, `via = agent`, the
session id, and the target.

### 5. The opt-in gate

`ImpresspressBuilder::enable_agent_writes()`, off by default.

Deliberately **not** a `ConfigVar`. Because of impresspress#78, shared config
edits never reach a running Worker, so a variable defaulting to off would be
permanently stuck off on Cloudflare — the one deployment where we most want to
demonstrate it. The gate must be true at build and boot time.

## Security model — what each layer actually defends

Stated precisely, because a layer credited with more than it does is worse than
no layer.

| Layer | Defends against | Does not defend against |
|---|---|---|
| Draft-only verbs | Anything the agent creates or edits reaching a customer | Junk drafts accumulating |
| Capability gate | Writes at any moment the admin has not deliberately opened editing | Writes during the 15-minute window |
| Confirmation modal | **Prompt injection** — the agent can read the page but cannot click | Script injection in the same page |
| Soft delete | Irreversible loss; orphaned order history | Nothing further; it is a data-integrity fix |
| Separate routes + gate | Reaching the agent surface with cookies alone, or at all when disabled | — |

The confirmation modal is not a boundary against XSS, and this design does not
claim it is: same-origin script can already do whatever the page can do, and a
site with XSS has lost regardless. The modal exists because the threat model
here is prompt injection, and an agent cannot click.

The honest residual risk: during an open session, injected page text can drive
`create_product_draft` and `update_product_draft` without a human seeing each
one. The consequences are bounded to invisible drafts, reversible in the admin
UI, and attributable through the audit log.

## Testing

Test-first throughout, with the RED failure observed before the fix, and each
test verified load-bearing by reverting the behaviour and watching it fail
again — the practice that produced the six real fixes on 2026-08-28.

**The policy test.** `no_admin_write_is_an_agent_tool` would still pass after
this change, because the new writes live on the products block rather than
admin. Passing for an irrelevant reason is exactly the failure mode this project
keeps hitting, so it is replaced by a stronger invariant across **all** blocks:

- every agent tool that is not a `GET` must declare a required capability;
- no capability-gated tool appears in a manifest generated with no capabilities
  held;
- the admin block specifically stays read-only — its writes remain non-tools.

**Snapshot gate.** The new endpoints enter the per-block `/openapi.json`
snapshots. Never regenerate a snapshot to get green; every changed line is read.

**End-to-end** (`webmcp.spec.ts`, against a real native server):

- anonymous and authenticated manifests are unchanged by this work;
- admin with no session sees exactly today's tool set;
- admin with a live session sees it plus the write tools;
- a write attempted after expiry returns `isError` and mutates nothing;
- an anonymous or non-admin caller is rejected by the agent routes **even
  when presenting a valid, unexpired session token** — the capability never
  substitutes for the login;
- a product created by the agent is absent from the public catalog until a human
  publishes it;
- a deleted product disappears from the catalog and its order history still
  resolves.
- a soft-deleted product that was `active` is absent from both the catalog
  list and the single-product route, not merely from checkout;
- `create_product_draft` sent `status: "active"` in its body still writes a
  draft row, and the published input schema names no `status` field.

**Coordination.** `webmcp.spec.ts` asserts exact tool sets in three places
(`:198`, `:304`, `:329`), where merge debt between #76 and #81 is already
outstanding. This work changes those assertions again on a new axis. Whoever
lands it reconciles all three.

## Sequencing

1. **wafer-run** — capability-gated tools. PR, then an impresspress pin bump.
   Re-resolve `Cargo.lock` from outside the impresspress tree, per the trap in
   the 2026-08-28 handoff.
2. **impresspress A** — soft delete and `repo/products.rs`. Independent of
   everything else, valuable on its own as a bug fix, and can start immediately
   in parallel with step 1.
3. **impresspress B** — session record, capability resolution, manifest
   plumbing.
4. **impresspress C** — the verbs, the admin UI control, the confirmation modal,
   e2e.

All of it stacks behind the seven PRs already open. That is the real cost of
doing this now, and it is why the feature ships off by default.

## Out of scope

- **A proposal queue.** Agent writes landing in a `pending_changes` table
  reviewed as a diff, per the pages-block design note. Additive later if
  multi-admin review becomes a requirement; it is not a fork of this design.
- **Offers, pricing and checkout configuration.** Products only. Offers are
  immutable once active and carry money semantics.
- **Capabilities for any block other than products.**
- **impresspress#78.** Not a prerequisite here — products are D1 rows, not
  `WAFER_RUN_SHARED__*` config — but it still blocks the Stripe key on the live
  demo.
