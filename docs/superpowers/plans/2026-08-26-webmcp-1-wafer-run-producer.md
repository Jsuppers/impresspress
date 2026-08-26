# WebMCP Producer (wafer-run) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `agent_tool` metadata to `BlockEndpoint` and a `generate_webmcp()` projection to `wafer-core`, so any wafer-run consumer can emit an auth-filtered WebMCP tool manifest from its existing block declarations.

**Architecture:** `generate_webmcp()` becomes the third sibling of `generate_openapi()` and `generate_agent_card()` in `wafer-core/src/discovery.rs`, walking the same `BlockInfo::endpoints`. Exposure is opt-in via a new `agent_tool(name, description)` builder on `BlockEndpoint` — absence means not exposed. Path, query, and body schemas are merged into a single flat `inputSchema` for the agent, with provenance recorded in an `invocation` block so the client can split the flat argument object back into a real HTTP request.

**Tech Stack:** Rust, `serde_json`, `schemars` v1.2.2 (already an optional dep of `wafer-block` behind the `json-schema` feature).

**Spec:** `docs/superpowers/specs/2026-08-26-webmcp-design.md` (in the `impresspress` repo)

**Repo:** All work in this plan is in `wafer-run` at `/home/joris/Programs/suppers-ai/workspace/wafer-run`. Nothing in `impresspress` changes. The active `[patch]` in `impresspress/.cargo/config.toml` means impresspress picks these changes up locally with no rev bump; the git rev pin in `impresspress/Cargo.toml:34-36` is bumped only when this lands on wafer-run's main.

## Global Constraints

- **Ref inlining is mandatory.** `schema_for!` emits `$defs`/`$ref`; many MCP-style clients resolve refs poorly. Every schema leaving `generate_webmcp()` must be self-contained with no `$ref` and no `$defs`.
- **Opt-in exposure only.** An endpoint without `agent_tool` is never a tool, regardless of whether it carries schemas. This is the SEC-073 recon-hardening posture: tool names for privileged operations must not reach clients that cannot call them.
- **Auth filtering is a caller-supplied ceiling.** `generate_webmcp()` takes the caller's `AuthLevel` and emits only tools at or below it.
- **`AuthLevel` ordering is `Public < Authenticated < Admin`** and must be expressed explicitly — the enum does not derive `Ord`.
- **Existing behavior is untouched.** `generate_openapi()` and `generate_agent_card()` must produce byte-identical output before and after this plan.
- Tests live inline in `#[cfg(test)] mod` blocks, matching the existing style in both files.

---

### Task 1: `AgentTool` metadata on `BlockEndpoint`

**Files:**
- Modify: `crates/wafer-block/src/types/endpoint.rs`
- Test: same file, in the existing `#[cfg(test)] mod block_endpoint_tests` block

**Interfaces:**
- Consumes: nothing
- Produces:
  - `pub struct AgentTool { pub name: String, pub description: String }`
  - `pub agent_tool: Option<AgentTool>` field on `BlockEndpoint`
  - `pub fn agent_tool(self, name: &str, description: &str) -> Self` builder
  - `pub fn is_agent_tool(&self) -> bool`

- [ ] **Step 1: Write the failing tests**

Add to the existing `mod block_endpoint_tests` in `crates/wafer-block/src/types/endpoint.rs`:

```rust
#[test]
fn agent_tool_defaults_to_none() {
    let ep = BlockEndpoint::get("/b/products/storefront/{id}").summary("Get product");
    assert!(ep.agent_tool.is_none());
    assert!(!ep.is_agent_tool());
}

#[test]
fn agent_tool_builder_sets_name_and_description() {
    let ep = BlockEndpoint::get("/b/products/storefront/{id}")
        .summary("Get product")
        .agent_tool("get_product", "Fetch a product and its purchasable offers by id.");
    let tool = ep.agent_tool.as_ref().expect("agent_tool must be set");
    assert_eq!(tool.name, "get_product");
    assert_eq!(
        tool.description,
        "Fetch a product and its purchasable offers by id."
    );
    assert!(ep.is_agent_tool());
}

#[test]
fn agent_tool_is_omitted_from_json_when_absent() {
    let ep = BlockEndpoint::get("/health").summary("Health check");
    let json = serde_json::to_value(&ep).expect("serialize");
    assert!(
        json.get("agent_tool").is_none(),
        "absent agent_tool must not appear in serialized output: {json}"
    );
}

#[test]
fn agent_tool_round_trips_through_serde() {
    let ep = BlockEndpoint::post("/b/products/checkout")
        .summary("Stripe checkout")
        .agent_tool("start_checkout", "Create a Stripe Checkout Session.");
    let json = serde_json::to_value(&ep).expect("serialize");
    let back: BlockEndpoint = serde_json::from_value(json).expect("deserialize");
    let tool = back.agent_tool.as_ref().expect("agent_tool survives round-trip");
    assert_eq!(tool.name, "start_checkout");
    assert_eq!(tool.description, "Create a Stripe Checkout Session.");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p wafer-block agent_tool`
Expected: FAIL — compile error, `no field 'agent_tool' on type 'BlockEndpoint'` and `no method named 'agent_tool'`.

- [ ] **Step 3: Add the `AgentTool` type**

Insert immediately above the `/// An HTTP endpoint exposed by a block.` doc comment on `pub struct BlockEndpoint` (around line 55):

```rust
/// Opt-in metadata marking an endpoint as callable by an agent, with a
/// curated name and description written for *invocation* rather than
/// documentation.
///
/// Absence is meaningful: an endpoint without this is never exposed as a
/// tool, no matter what schemas it carries. Tool names are deliberately
/// independent of the route so renaming a path does not silently rename a
/// tool that agents have learned.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentTool {
    /// Stable tool name exposed to agents (e.g. `get_product`).
    pub name: String,
    /// Description written to help an agent decide when to call this.
    pub description: String,
}
```

- [ ] **Step 4: Add the field to `BlockEndpoint`**

Add as the final field of `pub struct BlockEndpoint`, after `pub deprecated: bool`:

```rust
    /// Opt-in agent-tool metadata. `None` means never exposed as a tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_tool: Option<AgentTool>,
```

- [ ] **Step 5: Update both constructors**

There are two places that construct every field. Add `agent_tool: None,` after `deprecated: false,` in **both**:

1. The `impl Default for BlockEndpoint` block (around line 91-105)
2. The private `fn new(method: HttpMethod, path: &str) -> Self` in `impl BlockEndpoint` (around line 108-122)

Missing either is a compile error, so the next step catches it.

- [ ] **Step 6: Add the builder and predicate**

Add to `impl BlockEndpoint`, immediately after the `pub fn deprecated(mut self) -> Self` method:

```rust
    /// Mark this endpoint as an agent-callable tool with a curated name and
    /// description. Without this call the endpoint is never exposed.
    pub fn agent_tool(mut self, name: &str, description: &str) -> Self {
        self.agent_tool = Some(AgentTool {
            name: name.into(),
            description: description.into(),
        });
        self
    }

    /// Returns true if this endpoint opted in to agent-tool exposure.
    pub fn is_agent_tool(&self) -> bool {
        self.agent_tool.is_some()
    }
```

- [ ] **Step 7: Export `AgentTool`**

`BlockEndpoint` is re-exported from three places. Add `AgentTool` alongside it in each, keeping the existing alphabetical ordering:

1. `crates/wafer-block/src/types/mod.rs:21` — currently `pub use endpoint::{AuthLevel, BlockEndpoint, HttpMethod};` → becomes `pub use endpoint::{AgentTool, AuthLevel, BlockEndpoint, HttpMethod};`
2. `crates/wafer-block/src/lib.rs:28` — add `AgentTool,` to the re-export list that begins `ActionSpec, AuthLevel, BlockCategory, BlockEndpoint, ...`
3. `crates/wafer-run/src/lib.rs:72` — add `AgentTool,` to the list that begins `AuthLevel, BlockCategory, BlockEndpoint, ...`

Consumers import from `wafer_run::AgentTool`, matching how they already import `BlockEndpoint`.

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p wafer-block`
Expected: PASS, including the four new tests and every pre-existing test in the crate.

- [ ] **Step 9: Verify no downstream breakage**

Run: `cargo check --workspace`
Expected: clean. The new field is additive with a serde default, so existing struct-literal construction outside the two constructors would be the only breakage — this confirms there is none.

- [ ] **Step 10: Commit**

```bash
git add crates/wafer-block/src/types/endpoint.rs \
        crates/wafer-block/src/types/mod.rs \
        crates/wafer-block/src/lib.rs \
        crates/wafer-run/src/lib.rs
git commit -m "feat(wafer-block): add opt-in AgentTool metadata to BlockEndpoint

Marks an endpoint as agent-callable with a curated name and description.
Absence means never exposed, so agent-tool surface is opt-in rather than
derived from route shape."
```

---

### Task 2: Inline `$defs`/`$ref` in generated schemas

**Files:**
- Modify: `crates/wafer-core/src/discovery.rs`
- Test: same file, in the existing `#[cfg(test)] mod` block

**Interfaces:**
- Consumes: nothing from Task 1
- Produces: `fn inline_refs(schema: &serde_json::Value) -> serde_json::Value` — module-private, returns a self-contained schema with every `#/$defs/*` reference replaced by its target and the `$defs` key removed.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `crates/wafer-core/src/discovery.rs`:

```rust
#[test]
fn inline_refs_leaves_flat_schema_unchanged() {
    let schema = json!({
        "type": "object",
        "properties": { "id": { "type": "string" } },
        "required": ["id"]
    });
    assert_eq!(inline_refs(&schema), schema);
}

#[test]
fn inline_refs_replaces_ref_with_target_and_drops_defs() {
    let schema = json!({
        "type": "object",
        "properties": { "status": { "$ref": "#/$defs/ProductStatus" } },
        "$defs": {
            "ProductStatus": { "type": "string", "enum": ["draft", "active"] }
        }
    });
    let out = inline_refs(&schema);
    assert_eq!(
        out["properties"]["status"],
        json!({ "type": "string", "enum": ["draft", "active"] })
    );
    assert!(out.get("$defs").is_none(), "$defs must be stripped: {out}");
}

#[test]
fn inline_refs_resolves_nested_refs() {
    let schema = json!({
        "type": "object",
        "properties": { "tier": { "$ref": "#/$defs/PricingTier" } },
        "$defs": {
            "PricingTier": {
                "type": "object",
                "properties": { "scheme": { "$ref": "#/$defs/BillingScheme" } }
            },
            "BillingScheme": { "type": "string", "enum": ["per_unit", "tiered"] }
        }
    });
    let out = inline_refs(&schema);
    assert_eq!(
        out["properties"]["tier"]["properties"]["scheme"],
        json!({ "type": "string", "enum": ["per_unit", "tiered"] })
    );
}

#[test]
fn inline_refs_resolves_refs_inside_arrays() {
    let schema = json!({
        "type": "object",
        "properties": {
            "offers": { "type": "array", "items": { "$ref": "#/$defs/Offer" } }
        },
        "$defs": { "Offer": { "type": "object" } }
    });
    let out = inline_refs(&schema);
    assert_eq!(out["properties"]["offers"]["items"], json!({ "type": "object" }));
}

#[test]
fn inline_refs_terminates_on_self_referential_schema() {
    // A `Condition` that can contain child `Condition`s is a real shape in
    // products/contracts.rs. Inlining must bottom out rather than recurse
    // forever.
    let schema = json!({
        "$ref": "#/$defs/Condition",
        "$defs": {
            "Condition": {
                "type": "object",
                "properties": { "all_of": { "type": "array", "items": { "$ref": "#/$defs/Condition" } } }
            }
        }
    });
    let out = inline_refs(&schema);
    assert_eq!(out["type"], json!("object"));
    let rendered = out.to_string();
    assert!(
        !rendered.contains("$ref"),
        "no unresolved $ref may survive: {rendered}"
    );
    // Asserting only on `$ref` above is not enough, and missing this let a
    // real bug through: with the `$ref` at the schema ROOT, the root's
    // `$defs` table is a SIBLING of that `$ref`, so a sibling-merge that
    // skips only `$ref` copies the whole reference table straight back into
    // the output.
    assert!(
        !rendered.contains("$defs"),
        "the reference table must not survive as a sibling of a root $ref: {rendered}"
    );
}

#[test]
fn inline_refs_drops_unresolvable_ref_to_empty_schema() {
    let schema = json!({ "properties": { "x": { "$ref": "#/$defs/Missing" } } });
    let out = inline_refs(&schema);
    assert_eq!(out["properties"]["x"], json!({}));
}

#[test]
fn inline_refs_preserves_keywords_sitting_beside_a_ref() {
    // JSON Schema 2020-12 allows keywords alongside `$ref`, and schemars uses
    // that for field-level docs on a named type. Returning only the target
    // would delete every such description.
    let schema = json!({
        "type": "object",
        "properties": {
            "status": {
                "description": "Current lifecycle status of the product.",
                "$ref": "#/$defs/ProductStatus"
            }
        },
        "$defs": {
            "ProductStatus": { "type": "string", "enum": ["draft", "active"] }
        }
    });
    let out = inline_refs(&schema);
    let status = &out["properties"]["status"];

    assert_eq!(
        status["description"],
        json!("Current lifecycle status of the product."),
        "a description beside $ref must survive inlining: {status}"
    );
    assert_eq!(status["type"], json!("string"));
    assert_eq!(status["enum"], json!(["draft", "active"]));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p wafer-core inline_refs`
Expected: FAIL — `cannot find function 'inline_refs' in this scope`.

- [ ] **Step 3: Implement `inline_refs`**

Add near the other private helpers at the top of `crates/wafer-core/src/discovery.rs` (beside `path_to_slug`):

```rust
/// Maximum `$ref` hops to follow before giving up. Self-referential schemas
/// (a `Condition` containing child `Condition`s) are legitimate and would
/// otherwise recurse forever; at the limit we emit `{}` — an unconstrained
/// schema — which is honest about "anything may go here" rather than wrong.
const MAX_REF_DEPTH: u8 = 8;

/// Rewrite a schemars-generated schema into a self-contained one: every
/// `#/$defs/*` reference is replaced by its target, and the `$defs` block is
/// removed.
///
/// OpenAPI clients resolve `$ref` fine, which is why `generate_openapi` does
/// not do this. Many MCP-style clients do not, so the WebMCP projection must
/// hand over schemas that stand alone.
fn inline_refs(schema: &Value) -> Value {
    let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
    resolve_refs(schema, &defs, 0)
}

fn resolve_refs(node: &Value, defs: &Value, depth: u8) -> Value {
    match node {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                if depth >= MAX_REF_DEPTH {
                    return json!({});
                }
                let target = reference
                    .strip_prefix("#/$defs/")
                    .and_then(|name| defs.get(name));
                let mut resolved = match target {
                    Some(found) => resolve_refs(found, defs, depth + 1),
                    None => json!({}),
                };

                // JSON Schema 2020-12 allows keywords ALONGSIDE `$ref`, and
                // schemars uses that: a doc-commented field of a named type
                // emits `{"description": "...", "$ref": "#/$defs/Status"}`.
                // Returning only the target would silently delete every field
                // description — the exact editorial text the migration works
                // to preserve. Siblings win over the target's own keys, since
                // they are the more specific annotation.
                if let Some(out) = resolved.as_object_mut() {
                    for (key, value) in map {
                        // `$ref` is what we just resolved. `$defs` must be
                        // skipped too: when the `$ref` sits at the schema
                        // ROOT, the root's `$defs` table is a sibling of it,
                        // and copying siblings blindly puts the whole
                        // reference table back into the output.
                        if key == "$ref" || key == "$defs" {
                            continue;
                        }
                        out.insert(key.clone(), resolve_refs(value, defs, depth));
                    }
                }
                return resolved;
            }

            let mut out = serde_json::Map::new();
            for (key, value) in map {
                // `$defs` is the reference table itself, never part of the
                // resulting schema.
                if key == "$defs" {
                    continue;
                }
                out.insert(key.clone(), resolve_refs(value, defs, depth));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| resolve_refs(item, defs, depth))
                .collect(),
        ),
        other => other.clone(),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p wafer-core inline_refs`
Expected: PASS, all six.

- [ ] **Step 5: Verify existing discovery output is unchanged**

Run: `cargo test -p wafer-core discovery`
Expected: PASS. `inline_refs` is not yet wired into anything, so every `generate_openapi` / `generate_agent_card` test must still pass untouched.

- [ ] **Step 6: Commit**

```bash
git add crates/wafer-core/src/discovery.rs
git commit -m "feat(wafer-core): add inline_refs to flatten \$defs in schemas

MCP-style clients resolve \$ref poorly, so the WebMCP projection needs
self-contained schemas. Depth-capped so self-referential contracts
terminate instead of recursing forever."
```

---

### Task 3: Merge path, query, and body schemas into one agent input schema

**Files:**
- Modify: `crates/wafer-core/src/discovery.rs`
- Test: same file, `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `inline_refs` from Task 2
- Produces: `fn agent_input_schema(ep: &BlockEndpoint) -> AgentInputSchema`, where:

```rust
pub(crate) struct AgentInputSchema {
    pub schema: Value,
    pub path_params: Vec<String>,
    pub query_params: Vec<String>,
    pub body_params: Vec<String>,
    /// Property names contributed by more than one of path/query/body.
    /// Non-empty means the endpoint MUST NOT become a tool.
    pub collisions: Vec<String>,
}
```

An agent supplies one flat object; the name lists tell the client which properties belong in the URL path, the query string, and the request body.

**Why `collisions` exists.** If the same property name arrives from two sources — a path param `id` and a body field `id`, a common REST shape — the merged schema can only describe one of them. Picking a winner produces a tool that misdescribes its own arguments, and the client would place one value in both the URL and the body. The spec's first principle forbids that outright: a tool that can lie about its arguments is worse than no tool. So the collision is reported, and `generate_webmcp` refuses to emit a tool for that endpoint. An absent tool is visible; a subtly wrong one is not.

**Why flat:** WebMCP gives a tool exactly one `inputSchema`. An agent should not have to understand HTTP parameter placement. Provenance is recorded separately so the client can reassemble a correct request.

**Import note:** `discovery.rs` currently imports `BlockEndpoint` only under `#[cfg(test)]` (lines 4-5). `agent_input_schema` takes `&BlockEndpoint` in non-test code, so promote that import to module level or the crate will not build.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn agent_input_schema_is_empty_object_when_endpoint_has_no_schemas() {
    let ep = BlockEndpoint::get("/b/products/storefront/config");
    let (schema, path, query, body) = agent_input_schema(&ep);
    assert_eq!(schema, json!({ "type": "object", "properties": {} }));
    assert!(path.is_empty() && query.is_empty() && body.is_empty());
}

#[test]
fn agent_input_schema_merges_all_three_sources_and_records_provenance() {
    let ep = BlockEndpoint::post("/b/products/products/{product_id}/offers")
        .path_params_schema(json!({
            "type": "object",
            "properties": { "product_id": { "type": "string" } },
            "required": ["product_id"]
        }))
        .query_params_schema(json!({
            "type": "object",
            "properties": { "expand": { "type": "string" } }
        }))
        .input_schema(json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        }));

    let (schema, path, query, body) = agent_input_schema(&ep);

    assert_eq!(schema["properties"]["product_id"], json!({ "type": "string" }));
    assert_eq!(schema["properties"]["expand"], json!({ "type": "string" }));
    assert_eq!(schema["properties"]["name"], json!({ "type": "string" }));

    let required = schema["required"].as_array().expect("required array");
    assert!(required.contains(&json!("product_id")));
    assert!(required.contains(&json!("name")));
    assert!(
        !required.contains(&json!("expand")),
        "optional query param must not become required"
    );

    assert_eq!(path, vec!["product_id".to_string()]);
    assert_eq!(query, vec!["expand".to_string()]);
    assert_eq!(body, vec!["name".to_string()]);
}

#[test]
fn agent_input_schema_inlines_refs_from_each_source() {
    let ep = BlockEndpoint::post("/b/products/checkout").input_schema(json!({
        "type": "object",
        "properties": { "presentation": { "$ref": "#/$defs/CheckoutPresentation" } },
        "$defs": {
            "CheckoutPresentation": { "type": "string", "enum": ["hosted", "embedded"] }
        }
    }));
    let (schema, _, _, body) = agent_input_schema(&ep);
    assert_eq!(
        schema["properties"]["presentation"],
        json!({ "type": "string", "enum": ["hosted", "embedded"] })
    );
    assert!(schema.get("$defs").is_none());
    assert_eq!(body, vec!["presentation".to_string()]);
}

#[test]
fn agent_input_schema_omits_required_key_when_nothing_is_required() {
    let ep = BlockEndpoint::get("/b/products/list").query_params_schema(json!({
        "type": "object",
        "properties": { "page": { "type": "integer" } }
    }));
    let (schema, _, query, _) = agent_input_schema(&ep);
    assert!(
        schema.get("required").is_none(),
        "an all-optional schema must not carry an empty required array: {schema}"
    );
    assert_eq!(query, vec!["page".to_string()]);
}

#[test]
fn agent_input_schema_provenance_is_sorted_for_deterministic_output() {
    let ep = BlockEndpoint::get("/b/x/{b}/{a}").path_params_schema(json!({
        "type": "object",
        "properties": { "b": { "type": "string" }, "a": { "type": "string" } }
    }));
    let (_, path, _, _) = agent_input_schema(&ep);
    assert_eq!(path, vec!["a".to_string(), "b".to_string()]);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p wafer-core agent_input_schema`
Expected: FAIL — `cannot find function 'agent_input_schema' in this scope`.

- [ ] **Step 3: Implement `agent_input_schema`**

Add below `inline_refs` in `crates/wafer-core/src/discovery.rs`:

```rust
/// Collect `properties` and `required` from one schema source into the
/// merged accumulators, returning the property names it contributed.
///
/// Names come back sorted so the generated manifest is byte-stable across
/// runs — `serde_json::Map` iteration order is insertion order, but the
/// upstream schema's order is not something we control.
fn merge_schema_source(
    source: Option<&Value>,
    properties: &mut serde_json::Map<String, Value>,
    required: &mut Vec<String>,
) -> Vec<String> {
    let Some(source) = source else {
        return Vec::new();
    };
    let inlined = inline_refs(source);

    let mut contributed = Vec::new();
    if let Some(props) = inlined.get("properties").and_then(Value::as_object) {
        for (name, schema) in props {
            properties.insert(name.clone(), schema.clone());
            contributed.push(name.clone());
        }
    }
    if let Some(reqs) = inlined.get("required").and_then(Value::as_array) {
        for name in reqs.iter().filter_map(Value::as_str) {
            let owned = name.to_string();
            if !required.contains(&owned) {
                required.push(owned);
            }
        }
    }

    contributed.sort();
    contributed
}

/// Flatten an endpoint's path, query, and body schemas into the single
/// `inputSchema` a WebMCP tool exposes, plus the provenance lists the client
/// needs to rebuild a real HTTP request from the agent's flat argument
/// object.
///
/// Returns `(schema, path_params, query_params, body_params)`.
fn agent_input_schema(ep: &BlockEndpoint) -> (Value, Vec<String>, Vec<String>, Vec<String>) {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<String> = Vec::new();

    let path_params = merge_schema_source(ep.path_params.as_ref(), &mut properties, &mut required);
    let query_params =
        merge_schema_source(ep.query_params.as_ref(), &mut properties, &mut required);
    let body_params = merge_schema_source(ep.input_schema.as_ref(), &mut properties, &mut required);

    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        required.sort();
        schema.insert("required".into(), json!(required));
    }

    (
        Value::Object(schema),
        path_params,
        query_params,
        body_params,
    )
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p wafer-core agent_input_schema`
Expected: PASS, all five.

- [ ] **Step 5: Commit**

```bash
git add crates/wafer-core/src/discovery.rs
git commit -m "feat(wafer-core): flatten endpoint schemas into one agent input schema

WebMCP gives a tool a single inputSchema, so path/query/body merge into
one flat object. Provenance lists let the client rebuild a correct HTTP
request without the agent knowing about parameter placement."
```

---

### Task 4: `generate_webmcp()` — the auth-filtered manifest

**Files:**
- Modify: `crates/wafer-core/src/discovery.rs`
- Test: same file, `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `AgentTool` / `is_agent_tool()` (Task 1), `agent_input_schema` (Task 3)
- Produces: `pub fn generate_webmcp(blocks: &[BlockInfo], caller: AuthLevel) -> serde_json::Value`

Output shape:

```json
{
  "schema_version": 1,
  "tools": [
    {
      "name": "get_product",
      "description": "Fetch a product and its purchasable offers by id.",
      "inputSchema": { "type": "object", "properties": { "product_id": { "type": "string" } }, "required": ["product_id"] },
      "invocation": {
        "method": "GET",
        "path": "/b/products/storefront/{product_id}",
        "path_params": ["product_id"],
        "query_params": [],
        "body_params": []
      }
    }
  ]
}
```

- [ ] **Step 1: Write the failing tests**

```rust
/// Two blocks spanning all three auth levels, used by the tests below.
///
/// `BlockInfo::new` takes (name, version, runtime, description) — the same
/// four-argument form the existing `generate_openapi` tests in this file use
/// at line 274.
fn webmcp_fixture_blocks() -> Vec<BlockInfo> {
    let products = BlockInfo::new(
        "impresspress/products",
        "1.0.0",
        "http-handler@v1",
        "Commerce block",
    )
    .endpoints(vec![
        BlockEndpoint::get("/b/products/storefront/{product_id}")
            .summary("Storefront product")
            .auth(AuthLevel::Public)
            .path_params_schema(json!({
                "type": "object",
                "properties": { "product_id": { "type": "string" } },
                "required": ["product_id"]
            }))
            .agent_tool("get_product", "Fetch a product and its offers by id."),
        BlockEndpoint::get("/b/products/purchases")
            .summary("List own purchases")
            .auth(AuthLevel::Authenticated)
            .output_schema(json!({ "type": "object" }))
            .agent_tool("list_my_purchases", "List the signed-in user's purchases."),
        // Carries schemas but never opted in — must never appear.
        BlockEndpoint::post("/b/products/webhooks")
            .summary("Stripe webhook")
            .auth(AuthLevel::Public)
            .input_schema(json!({ "type": "object" })),
    ]);

    let admin = BlockInfo::new(
        "impresspress/admin",
        "1.0.0",
        "http-handler@v1",
        "Admin block",
    )
    .endpoints(vec![BlockEndpoint::get("/b/admin/api/users")
        .summary("List users")
        .auth(AuthLevel::Admin)
        .output_schema(json!({ "type": "object" }))
        .agent_tool("list_users", "List all user accounts.")]);

    vec![products, admin]
}

fn tool_names(doc: &Value) -> Vec<String> {
    doc["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool name").to_string())
        .collect()
}

#[test]
fn webmcp_public_caller_sees_only_public_tools() {
    let doc = generate_webmcp(&webmcp_fixture_blocks(), AuthLevel::Public);
    assert_eq!(tool_names(&doc), vec!["get_product".to_string()]);
}

#[test]
fn webmcp_authenticated_caller_sees_public_and_authenticated() {
    let doc = generate_webmcp(&webmcp_fixture_blocks(), AuthLevel::Authenticated);
    let names = tool_names(&doc);
    assert!(names.contains(&"get_product".to_string()));
    assert!(names.contains(&"list_my_purchases".to_string()));
    assert!(
        !names.contains(&"list_users".to_string()),
        "admin tool must not leak to an authenticated caller: {names:?}"
    );
}

#[test]
fn webmcp_admin_caller_sees_every_tool() {
    let doc = generate_webmcp(&webmcp_fixture_blocks(), AuthLevel::Admin);
    assert_eq!(tool_names(&doc).len(), 3);
}

#[test]
fn webmcp_excludes_endpoints_that_did_not_opt_in() {
    let doc = generate_webmcp(&webmcp_fixture_blocks(), AuthLevel::Admin);
    let rendered = doc.to_string();
    assert!(
        !rendered.contains("/b/products/webhooks"),
        "a schema-carrying endpoint without agent_tool must be absent: {rendered}"
    );
}

#[test]
fn webmcp_skips_an_endpoint_whose_parameter_names_collide() {
    // `id` arrives from BOTH the path and the body. One flat schema cannot
    // honestly describe both locations, so no tool may be emitted — an
    // absent tool is visible, a lying one is not.
    let block = BlockInfo::new("test/block", "1.0.0", "http-handler@v1", "Test").endpoints(vec![
        BlockEndpoint::post("/b/x/{id}")
            .summary("Collides")
            .auth(AuthLevel::Public)
            .path_params_schema(json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }))
            .input_schema(json!({
                "type": "object",
                "properties": { "id": { "type": "integer" } }
            }))
            .agent_tool("colliding_tool", "Should never be emitted."),
    ]);

    let doc = generate_webmcp(&[block], AuthLevel::Admin);
    assert_eq!(
        doc["tools"],
        json!([]),
        "an endpoint with a cross-location name collision must produce no tool: {doc}"
    );
}

#[test]
fn webmcp_tool_carries_invocation_metadata() {
    let doc = generate_webmcp(&webmcp_fixture_blocks(), AuthLevel::Public);
    let tool = &doc["tools"][0];
    assert_eq!(tool["name"], json!("get_product"));
    assert_eq!(
        tool["description"],
        json!("Fetch a product and its offers by id.")
    );
    assert_eq!(tool["invocation"]["method"], json!("get"));
    assert_eq!(
        tool["invocation"]["path"],
        json!("/b/products/storefront/{product_id}")
    );
    assert_eq!(tool["invocation"]["path_params"], json!(["product_id"]));
    assert_eq!(tool["invocation"]["query_params"], json!([]));
    assert_eq!(tool["invocation"]["body_params"], json!([]));
    assert_eq!(
        tool["inputSchema"]["properties"]["product_id"],
        json!({ "type": "string" })
    );
}

#[test]
fn webmcp_emits_schema_version_and_empty_tools_for_no_blocks() {
    let doc = generate_webmcp(&[], AuthLevel::Admin);
    assert_eq!(doc["schema_version"], json!(1));
    assert_eq!(doc["tools"], json!([]));
}

#[test]
fn webmcp_tool_order_is_deterministic() {
    let blocks = webmcp_fixture_blocks();
    let first = generate_webmcp(&blocks, AuthLevel::Admin);
    let second = generate_webmcp(&blocks, AuthLevel::Admin);
    assert_eq!(first, second);
}
```

**Note:** `"get"` is the expected `invocation.method` value because `method_key` (`discovery.rs:12-19`) lowercases — it exists to produce OpenAPI operation keys, and the WebMCP projection reuses it rather than introducing a second spelling of the same thing.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p wafer-core webmcp`
Expected: FAIL — `cannot find function 'generate_webmcp' in this scope`.

- [ ] **Step 3: Implement the auth ranking helper**

`AuthLevel` does not derive `Ord`, and adding a derive would be a wider change than this needs. Add above `generate_webmcp`:

```rust
/// `Public < Authenticated < Admin`, expressed explicitly because
/// `AuthLevel` deliberately does not derive `Ord`.
fn auth_rank(level: AuthLevel) -> u8 {
    match level {
        AuthLevel::Public => 0,
        AuthLevel::Authenticated => 1,
        AuthLevel::Admin => 2,
    }
}
```

- [ ] **Step 4: Implement `generate_webmcp`**

Add after `generate_agent_card` in `crates/wafer-core/src/discovery.rs`:

```rust
// ---------------------------------------------------------------------------
// generate_webmcp
// ---------------------------------------------------------------------------

/// Project the blocks' endpoint declarations into a WebMCP tool manifest,
/// filtered to what `caller` is allowed to invoke.
///
/// This is the third projection of `BlockInfo::endpoints`, alongside
/// [`generate_openapi`] and [`generate_agent_card`]. Two things make it
/// different from those:
///
/// * **Opt-in.** Only endpoints carrying [`AgentTool`] metadata appear.
///   Carrying a schema is not consent to being called by an agent.
/// * **Auth-filtered.** Tools above `caller`'s level are omitted entirely —
///   not marked unavailable. A name an agent cannot use is recon surface, so
///   it never reaches the page. This mirrors the [SEC-073] posture applied to
///   the discovery documents.
pub fn generate_webmcp(blocks: &[BlockInfo], caller: AuthLevel) -> Value {
    let ceiling = auth_rank(caller);
    let mut tools: Vec<Value> = Vec::new();

    for block in blocks {
        for ep in &block.endpoints {
            let Some(tool) = ep.agent_tool.as_ref() else {
                continue;
            };
            if auth_rank(ep.auth) > ceiling {
                continue;
            }

            let input = agent_input_schema(ep);

            // A property name arriving from two of path/query/body cannot be
            // honestly described by one flat schema, and the client would put
            // the value in both places. Emitting no tool is the safe, visible
            // failure; emitting a lying one is neither.
            if !input.collisions.is_empty() {
                continue;
            }

            tools.push(json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": input.schema,
                "invocation": {
                    "method": method_key(ep.method),
                    "path": ep.path,
                    "path_params": input.path_params,
                    "query_params": input.query_params,
                    "body_params": input.body_params,
                },
            }));
        }
    }

    json!({
        "schema_version": 1,
        "tools": tools,
    })
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p wafer-core webmcp`
Expected: PASS, all seven.

- [ ] **Step 6: Verify the other two projections are untouched**

Run: `cargo test -p wafer-core`
Expected: PASS. Every pre-existing `generate_openapi` / `generate_agent_card` test must still pass — this plan adds a projection, it does not change the existing two.

- [ ] **Step 7: Run the full workspace suite**

Run: `cargo test --workspace`
Expected: PASS.

**If failures appear in crates unrelated to this change**, check them against CI before assuming this plan caused them — the local `[patch]` wiring in this workspace is known to produce environment-specific failures that CI does not reproduce.

- [ ] **Step 8: Commit**

```bash
git add crates/wafer-core/src/discovery.rs
git commit -m "feat(wafer-core): add generate_webmcp auth-filtered tool manifest

Third projection of BlockInfo::endpoints alongside generate_openapi and
generate_agent_card. Opt-in via AgentTool metadata, and filtered to the
caller's AuthLevel so unusable tool names never reach the client."
```

---

### Task 5: Make the derived schemas self-contained at the source

**This task blocks Plan 2.** Do not begin the derive migration until it has landed — every schema Plan 2 produces would otherwise be structurally broken inside `/openapi.json`, and Plan 2's snapshot reviewers would be rubber-stamping dangling references.

**Files:**
- Modify: `crates/wafer-block/src/types/endpoint.rs:208-238` (the four schemars builders)
- Test: same file, `#[cfg(test)] mod block_endpoint_tests`

**Interfaces:**
- Consumes: nothing
- Produces: `.input::<T>()`, `.output::<T>()`, `.path_params::<T>()`, `.query_params::<T>()` emitting schemas with no `$ref`, no `$defs`, no `$schema`, and no root `title`

**The problem.** `schemars::schema_for!(T)` produces a *document*: a root schema plus a `$defs` table, with internal `#/$defs/X` references. `generate_openapi` embeds that value verbatim into `requestBody` and `responses` (`wafer-core/src/discovery.rs:94-133`). Inside an OpenAPI document, `#/$defs/X` resolves against the *document* root — where no `$defs` exists. Every reference dangles.

It is worse for parameters: `extract_params` (`discovery.rs:23-48`) lifts each property subschema into a standalone parameter object and drops the root `$defs` entirely, so any enum-typed path or query field becomes a `$ref` with no referent anywhere in the document.

This affects `/openapi.json` — a surface that works today — so it is a regression the migration would introduce, not a WebMCP concern. Fixing it in the builders fixes both consumers at once and makes `inline_refs` (Task 2) a defensive net rather than the only line of defence.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn derived_input_schema_is_self_contained() {
    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    enum Status { Draft, Active }

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct CreateProduct {
        /// Human-readable product name.
        name: String,
        /// Current lifecycle status.
        status: Status,
    }

    let ep = BlockEndpoint::post("/b/products").input::<CreateProduct>();
    let schema = ep.input_schema.expect("input schema set");
    let rendered = schema.to_string();

    assert!(
        !rendered.contains("$ref"),
        "derived schemas are embedded into OpenAPI documents where #/$defs \
         does not resolve — no $ref may survive: {rendered}"
    );
    assert!(
        !rendered.contains("$defs"),
        "the $defs table must not travel with the schema: {rendered}"
    );
    assert!(
        schema.get("$schema").is_none(),
        "root $schema is meaningless inside an OpenAPI requestBody: {rendered}"
    );
}

#[test]
fn derived_schema_keeps_field_descriptions() {
    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct WithDocs {
        /// Human-readable product name.
        name: String,
    }

    let ep = BlockEndpoint::post("/b/x").input::<WithDocs>();
    let schema = ep.input_schema.expect("input schema set");
    assert_eq!(
        schema["properties"]["name"]["description"],
        serde_json::json!("Human-readable product name."),
        "doc comments must reach the schema — Plan 2 relies on this to \
         preserve editorial text: {schema}"
    );
}

#[test]
fn derived_query_params_schema_inlines_enums() {
    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    enum SortOrder { Asc, Desc }

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct ListQuery { sort: Option<SortOrder> }

    let ep = BlockEndpoint::get("/b/x").query_params::<ListQuery>();
    let schema = ep.query_params.expect("query params schema set");
    assert!(
        !schema.to_string().contains("$ref"),
        "extract_params lifts each property out standalone and drops $defs, \
         so an enum-typed query param must already be inlined: {schema}"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p wafer-block --features json-schema derived_`
Expected: FAIL — the current builders use `schema_for!` and emit `$defs`/`$ref`.

- [ ] **Step 3: Switch the builders to an inlining generator**

Replace `schemars::schema_for!(T)` in all four builders with a shared helper that configures the generator to inline subschemas and strips the document-level keys.

In schemars v1 this is `SchemaSettings` with `inline_subschemas: true`, driven through a `SchemaGenerator`. **Verify the exact API against the vendored schemars 1.x source before writing it** — the module path moved between 0.8 and 1.0, and guessing it will cost more than reading it. Look for `SchemaSettings::default().with(|s| s.inline_subschemas = true)` and the generator's `into_root_schema_for::<T>()`.

```rust
/// Derive a self-contained JSON Schema for `T`.
///
/// `schema_for!` produces a *document* — a root schema plus a `$defs` table
/// with internal `#/$defs/X` references. These schemas get embedded into
/// OpenAPI documents (and lifted apart into individual parameter objects),
/// where those references have no referent. So we inline everything and drop
/// the document-level keys.
///
/// Recursive types are the reason `inline_subschemas` is not universally
/// safe: a type that contains itself cannot be fully inlined. schemars
/// breaks such cycles by keeping a `$ref`; if a contract ever does that, the
/// self-contained tests above fail loudly rather than shipping a dangling
/// reference.
#[cfg(feature = "json-schema")]
fn self_contained_schema<T: schemars::JsonSchema>() -> serde_json::Value {
    // Construct the inlining generator here — exact API per the schemars 1.x
    // source, see Step 3 note.
    let schema = /* generator with inline_subschemas = true */;
    let mut value = serde_json::to_value(schema).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = value.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
        obj.remove("$defs");
    }
    value
}
```

Then each builder becomes, e.g.:

```rust
    #[cfg(feature = "json-schema")]
    pub fn input<T: schemars::JsonSchema>(mut self) -> Self {
        self.input_schema = Some(self_contained_schema::<T>());
        self
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p wafer-block --features json-schema`
Expected: PASS, all three new tests plus every existing one.

- [ ] **Step 5: Confirm the recursion escape hatch behaves**

Add a self-referential type and confirm the failure is loud rather than silent:

```rust
#[test]
fn recursive_type_does_not_silently_emit_a_dangling_ref() {
    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct Condition {
        all_of: Vec<Condition>,
    }

    let ep = BlockEndpoint::post("/b/x").input::<Condition>();
    let schema = ep.input_schema.expect("input schema set");
    let rendered = schema.to_string();

    // schemars must break the cycle somehow. Whatever it does, a `$ref`
    // pointing at a `$defs` table we just deleted is the one unacceptable
    // outcome — `inline_refs` in generate_webmcp would resolve it to `{}`,
    // and generate_openapi would emit it dangling.
    assert!(
        !rendered.contains("\"$ref\""),
        "recursive contract produced a dangling $ref after $defs removal — \
         keep $defs for this case, or reject recursive contracts explicitly: \
         {rendered}"
    );
}
```

If this test fails, **do not delete it and move on.** The correct response is to keep `$defs` when inlining could not fully resolve, and teach `generate_openapi` to hoist it into `components/schemas` — a larger change that must be made deliberately. `products/contracts.rs` has a `Condition` type with exactly this shape, so this is not hypothetical.

- [ ] **Step 6: Verify existing discovery output**

Run: `cargo test --workspace`
Expected: PASS. No impresspress endpoint uses the typed builders yet, so `/openapi.json` output is unchanged.

- [ ] **Step 7: Commit**

```bash
git add crates/wafer-block/src/types/endpoint.rs
git commit -m "fix(wafer-block): emit self-contained schemas from typed builders

schema_for! produces a root schema plus a \$defs table with internal
refs. These schemas are embedded into OpenAPI documents and lifted apart
into parameter objects, where #/\$defs has no referent — every reference
would dangle. Inline subschemas and drop the document-level keys."
```

---

## Done criteria

- [ ] `cargo test --workspace` passes in `wafer-run`
- [ ] `generate_openapi()` and `generate_agent_card()` output is unchanged
- [ ] `cargo check --workspace` passes in `impresspress` against the patched wafer-run, proving the additive `BlockEndpoint` field breaks no consumer
- [ ] Branch pushed and PR opened against wafer-run's default branch

## What this plan deliberately does not do

- **No consumer wiring.** Nothing serves the manifest or injects it into a page; that is Plan 3.
- **No schema migration.** No `#[derive(JsonSchema)]`, no `json-schema` feature, no deletion of hand-written blobs; that is Plan 2.
- **No `agent_tool` annotations on real endpoints.** Only the mechanism ships here. Choosing which of impresspress's endpoints become tools is a curation decision that belongs with the block code, in Plan 3.

## Next plans

- **Plan 2 — impresspress derive migration:** enable `json-schema`, derive `JsonSchema` across contracts, replace all 136 hand-written schema call sites, type the untyped admin and auth blocks, gated per block by `/openapi.json` snapshot diffs.
- **Plan 3 — WebMCP surfacing:** `agent_tool` annotations, render injection at `ui/layout.rs:47` and `ui/templates.rs:449` with session auth filtering, the inspector manifest view, and the storefront tool surface.
