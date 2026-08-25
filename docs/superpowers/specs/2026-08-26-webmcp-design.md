# WebMCP tool exposure, derived from typed contracts

**Date:** 2026-08-26
**Status:** Design, pending review
**Repos:** `wafer-run` (producer), `impresspress` (consumer)

## Goal

Expose impresspress site functionality as [WebMCP](https://github.com/webmachinelearning/webmcp) tools, so browser agents (ChatGPT browser, Chrome with WebMCP enabled) can call a site's operations directly instead of driving its UI.

The design principle that makes this worth doing properly: **an agent's tool schema must be derived from the same Rust type the server deserializes into.** A tool that can lie about its arguments is worse than no tool. Every other decision below follows from that.

## Context

WebMCP's surface is small:

```js
document.modelContext.registerTool({ name, description, inputSchema, execute })
```

Unregistration is via `AbortSignal`; `toolchange` fires when the tool set changes dynamically.

impresspress is already unusually close to this. Every block declares `BlockInfo::endpoints`, where each `BlockEndpoint` carries `method`, `path`, `summary`, `description`, `input_schema`, `output_schema`, `path_params`, `query_params`, `tags`, and `auth: AuthLevel`. Two functions in `wafer-core/src/discovery.rs` already walk that structure and project it into a wire format — `generate_openapi()` and `generate_agent_card()` — both filtered by `ep.has_schema()`, and both served at runtime from `impresspress-core/src/pipeline.rs:126`.

A WebMCP manifest is a third projection of the same data.

### The root-cause problem

The schemas that feed those projections are hand-written. There are 136 schema-builder call sites across `impresspress-core`, fed by hand-authored JSON — 51 inline `serde_json::json!` literals plus shared hand-built schema variables. Meanwhile `products/contracts.rs` is 1,115 lines of fully typed `Serialize`/`Deserialize` contracts describing the same shapes, and there are 127 `Deserialize` structs across the crate.

So the schemas are a parallel, hand-maintained duplicate of types that already exist, and nothing enforces that the two agree.

`wafer-block` already ships the fix and it has never been used: `.input::<T>()`, `.output::<T>()`, `.path_params::<T>()`, and `.query_params::<T>()` derive schemas from Rust types via `schemars` (`wafer-run/crates/wafer-block/src/types/endpoint.rs:208-238`). The `json-schema` feature that gates them is enabled nowhere in impresspress, and `schemars` does not appear in `Cargo.lock`.

This explains the coverage gap directly. Blocks with schemas: products, auth_ui, messages, files. Blocks with zero: admin, auth, vector, llm. `admin/users.rs` and `admin/settings.rs` contain no `pub struct` at all — their handlers were never typed. Nobody wanted to hand-write more JSON.

### Verified feasibility

Adding `#[derive(schemars::JsonSchema)]` to all 71 types in `products/contracts.rs` compiles clean under schemars v1.2.2. The field vocabulary is `String`, `i64`, `bool`, enums, `BTreeMap`, and `serde_json::Value` — no `chrono`, `Decimal`, or `Uuid`, which are the types that need opt-in schemars features.

## Constraints

**Production target is a Cloudflare Worker.** Limits: 3 MB compressed (Free) / 10 MB compressed (Paid), 64 MB uncompressed, and a 1,000 ms startup CPU budget enforced at deploy time (error 10021). The repo's own guard warns at 8 MB raw (`crates/impresspress/src/cli/helpers/cloudflare/profile_check.rs:44`).

Current worker size, from recorded measurements: 4,877,353 raw / 1,737,534 gzip; 4,171,741 / 1,489,572 with trimmed feature defaults (`docs/CODE_REVIEW_2026-07-16_FINDINGS.md:36-42`). A live wafer-site deploy measured 4.80 MB raw / 1.36 MB gz (`docs/2026-07-18-externalize-static-assets-benchmark.md:71`).

The bundle is already size-tuned: `opt-level = "z"`, fat LTO, `codegen-units = 1`, `strip = true`, `panic = abort`.

**Startup budget is not at risk.** Block registration — and therefore all schema construction — runs inside `runtime_cache::get_or_build` (`impresspress-cloudflare/src/lib.rs:314`, `impresspress-core/src/builder/registration.rs:309`), which executes per-isolate inside the fetch handler, not in global scope. Only schemars' *code size* is in scope for the startup budget, never its runtime cost.

**Note:** a "400 ms startup cap" appears in `docs/2026-07-18-externalize-static-assets-benchmark.md:66`. That figure is stale and overstates the pressure by 2.5×; `profile_check.rs` carries the corrected account.

## Decision

**Derive schemas at runtime from typed contracts, expose an opt-in subset as tools, and inject a session-filtered manifest at page render.**

### Measured basis

A synthetic benchmark (100 structs × 6 fields + 30 enums, matching the type vocabulary of `products/contracts.rs`) built for `wasm32-unknown-unknown` under the workspace release profile:

| Build | Raw `.wasm` | gzip-9 |
|---|---:|---:|
| serde only (baseline) | 518,897 | 65,854 |
| + 51 hand-written `json!` blobs (today) | 675,679 | 73,672 |
| + schemars derive on 130 types | 770,006 | 95,134 |

Gross schemars cost for ~130 types: **+251 KB raw / +29 KB gzip**. Net cost of the migration, after deleting the hand-written blobs it replaces: **+94 KB raw / +21 KB gzip**.

The hand-written `json!` macro expansions cost roughly 3.1 KB raw each. Schemars derive is comparable or cheaper per schema than what it replaces.

This is a synthetic benchmark, not the real contracts, which are more deeply nested and will carry doc-comment descriptions. Even at 3× the measured delta (~+300 KB raw, ~6% of current size) the result sits ~3 MB below the 8 MB warn line and well inside compressed limits on either plan.

**Size is therefore not a deciding factor.** It was treated as one earlier in this design's history; that was wrong, and the measurement corrected it.

### Rejected: build-time manifest

Generating `webmcp.json` at build time and embedding it via `include_str!` would keep schemars out of the shipped wasm. Measured, that benefit is worth ~21 KB gzip. The costs are structural:

- `/openapi.json` and the agent card are generated **at runtime** from `block_infos` (`pipeline.rs:126-129`). After the migration, schemas live behind `.input::<T>()`. Disabling `json-schema` for the wasm build would strip schemas from `/openapi.json` too — a regression. Keeping it on makes the embedded manifest pure dead weight.
- `impresspress-cloudflare` is feature-modular (`default = []` vs `full`) specifically to prevent forgot-to-opt-out bloat. A build-time manifest must match each consumer's exact feature set, making `include_str!` a drift machine — and an implicit mapping layer, which the repo's guidelines prohibit.

### Rejected: expose every schema-carrying endpoint

Mechanically registering all ~50+ schema-carrying endpoints as tools needs no new API, but:

- Tool names derived from method+path are unstable — renaming a route silently renames the tool — and 50-100 tools per page measurably degrades agent tool-selection.
- `BlockEndpoint`'s `summary`/`description` were written for OpenAPI documentation, not for invocation guidance.
- `pipeline.rs:130-142` ([SEC-073]) deliberately stops advertising the full API surface cross-origin in production as recon hardening. Auto-registering every endpoint as a named, schema-bearing tool in every page's HTML would re-create precisely that surface, for anonymous visitors.

## Architecture

### wafer-run (producer — lands first)

**1. `wafer-block`: `agent_tool` metadata on `BlockEndpoint`.**
A builder method `agent_tool(name, description)` marking an endpoint as agent-exposed and giving it a stable, curated name independent of its route. Absence means not exposed. This is the opt-in gate and the naming control.

**2. `wafer-core/src/discovery.rs`: `generate_webmcp()`.**
A third sibling to `generate_openapi()` and `generate_agent_card()`, walking the same `BlockInfo::endpoints`. It takes the caller's `AuthLevel` and emits only tools at or below it.

It must **inline `$defs`/`$ref`**. `schema_for!` emits ref-heavy schemas by default; that is correct for OpenAPI but many MCP-style clients resolve refs poorly. Either use schemars' `inline_subschemas` setting or run a deref pass inside `generate_webmcp()`. Products' nested contracts make this load-bearing, not cosmetic.

**3. Rev pin.** impresspress pins wafer-run by git rev (`Cargo.toml:34-36`). Development uses the active `[patch]` in `impresspress/.cargo/config.toml`, which resolves to the sibling checkout; the pin is bumped at merge.

### impresspress (consumer)

**4. Enable `json-schema`; migrate the schemas.**
Derive `JsonSchema` on existing contract types; replace every hand-written schema call site with `.input::<T>()` / `.output::<T>()` / `.path_params::<T>()` / `.query_params::<T>()`. This deletes duplication rather than adding any.

**5. Type the untyped blocks.** admin and auth ship in the Worker and have no typed contracts. Writing them is a genuine improvement to those blocks independent of this feature.

**6. Annotate the tool surface.** Mark tool-worthy endpoints with `agent_tool(...)`, with names and descriptions written for invocation rather than documentation.

**7. Inject at the render chokepoints.** `ui/layout.rs:47` and `ui/templates.rs:449` both already iterate `config.embedded_scripts`. The session-filtered manifest is inlined into the page; the registration script itself stays a static asset. Inlining is preferred over fetching `/.well-known/webmcp.json` because per-session filtering makes such an endpoint uncacheable anyway, and 10-15 tools is roughly 3-6 KB gzipped.

**8. Inspector panel.** `wafer-block-inspector` already serves a static `inspector.html` at `/b/inspector`, and `routing.rs:304 routes_config()` already projects endpoint granularity from `BlockInfo::endpoints`. Add a view showing the live tool manifest as each auth level sees it.

**9. Storefront tool surface.** See below.

### Auth scoping

`BlockInfo::endpoints` is already the single source of truth for per-endpoint access, enforced centrally through `declared_access` (`routing.rs:275-310`), with identity from `msg.user_id()`. Tool filtering becomes the third consumer of that same data, after the router and the inspector — an established pattern, not a new mechanism.

Filtering happens **server-side, at render**. An anonymous page emits only `AuthLevel::Public` tools. Tool names for privileged operations never reach a client that could not call them, which is consistent with the SEC-073 posture.

## The storefront surface

The commerce path is already fully schema'd and `AuthLevel::Public`, and is the most legible demonstration:

- `GET /b/products/storefront/config`
- `GET /b/products/storefront/{product_id}`
- `POST /b/products/pricing/preview`
- `POST /b/products/checkout` — guest checkout supported
- `GET /b/products/orders/{id}/status?receipt_token=...`

**Payment stays human-in-the-loop by construction.** `checkout` returns a Stripe-hosted session URL; no tool completes a payment. The agent assembles the order, the human confirms it at Stripe. This is a property of the existing architecture, not a guard bolted on for the occasion.

## Migration fidelity gate

This is the highest risk in the plan, and it is not size.

Replacing curated hand-written schemas with derived ones can silently change the public contract in two ways:

1. **Widening.** Derive exposes every field not marked `serde(skip)`. Any field a hand-written schema deliberately omitted becomes public in `/openapi.json`.
2. **Description loss.** Editorial `description` text in the hand-written blobs disappears unless it is reintroduced as doc comments on the contract types.

**Required gate:** snapshot `/openapi.json` per block before and after migration, and diff. Every widening is a review item requiring an explicit decision — `serde(skip)`, a dedicated view type, or acceptance. Diffs are not noise to be waved through.

## Scope

**In, on the critical path** (Worker-shipping blocks): products, files, messages, auth_ui, auth, admin.

**In, but not gating**: llm and vector. `impresspress-cloudflare/Cargo.toml:74` excludes `block-llm` and `block-vector` from the Worker build pending the LlmService refactor, so typing them is native-only work. It must not block the WebMCP-in-Worker milestone.

**Out**: new admin *write* endpoints for agent-driven site configuration. Admin gets typed contracts and read tools here; agent-driven writes are a separate decision with its own risk profile.

## Testing

- `pipeline.rs:477-507` already asserts OpenAPI security/schema properties per auth level. Extend that harness to `generate_webmcp()`.
- Snapshot tests for the generated manifest at each of `Public`, `Authenticated`, `Admin` — these encode the security property that privileged tool names never reach unprivileged pages.
- Per-block `/openapi.json` snapshot diffs as the migration fidelity gate above.
- End-to-end: a real agent invoking the storefront chain against a deployed Worker.

## Sequencing

1. wafer-run: `agent_tool` metadata + `generate_webmcp()` with ref inlining, with tests
2. impresspress: enable `json-schema`, migrate products (largest, best-typed), snapshot-diff the result
3. Migrate files, messages, auth_ui the same way
4. Type admin + auth contracts, then migrate them
5. Annotate the tool surface; implement render injection with auth filtering
6. Inspector manifest view
7. Deploy to Cloudflare; measure real wasm delta against the synthetic estimate
8. llm + vector typing (native-only, non-gating)

## Submission context

This work was scoped with [The WebMCP Challenge](https://webmcp.devpost.com/) in view (deadline 2026-09-03 13:00 PDT). The engineering decisions above stand on their own merits and are not contingent on entering; the following are submission-only requirements:

- **`impresspress` has no `LICENSE` file.** The rules require an OSS license visible in the repository's About section. The repo is public and `wafer-run` already carries a license; impresspress does not. This is disqualifying if missed and takes minutes to fix.
- A submission spans both repos, since `generate_webmcp()` lands in wafer-core. Both are public, so this is permitted, but the write-up should name both.
- Also required: a live URL reachable from the ChatGPT browser, a demo video under 3 minutes, and the tool-registration code visible in-repo.

## Open questions

- **Real wasm delta.** The +94 KB net figure is synthetic. Step 7 measures the true number. Mitigation if it disappoints: `worker-build` 0.7 defaults to `wasm-opt -O` rather than `-Oz`, worth about −215 KB raw per `CODE_REVIEW_2026-07-16` — enough to absorb several times the projected delta.
- **Tool granularity.** Whether some multi-step flows (search → price → checkout) are better exposed as one composite tool than three primitives is an agent-UX question best settled by testing against a real agent, not decided up front.
