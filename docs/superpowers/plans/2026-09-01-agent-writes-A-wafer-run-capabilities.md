# Plan A — Capability-gated WebMCP tools (wafer-run) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a `BlockEndpoint` declare that its agent tool requires a named capability, and let the WebMCP projection omit that tool from any manifest whose caller does not hold it.

**Architecture:** `AgentTool` gains an optional `requires_capability`. `generate_webmcp_report` takes the set of capabilities the caller holds and folds "capability held" into the existing `visible_to_caller` predicate — the same predicate the auth filter already uses, so a gated tool is omitted exactly the way an out-of-tier tool is: silently, and without touching the refusal census.

**Tech Stack:** Rust, `wafer-block` (types), `wafer-core` (discovery/projection).

**Spec:** `docs/superpowers/specs/2026-09-01-agent-product-writes-design.md` (in the `impresspress` repo — this plan implements its §1)

**Repo:** This plan executes in `wafer-run`, not `impresspress`.

## Global Constraints

- **Omission is not refusal.** A capability miss must never push a `WebMcpRefusalReport`. Refusals mean "this tool is malformed"; they are counted per manifest and surfaced to operators. Counting a per-caller outcome rebuilds the existence oracle #324 closed.
- **A name the caller cannot use never reaches the page.** Gated tools are absent, never listed-as-unavailable.
- **Fail closed.** A capability string nothing can grant makes the tool invisible, never exposed.
- **`///` becomes published documentation.** Use `//` for rationale that should not ship as a doc comment.
- Every test must be verified load-bearing: revert the behaviour, watch the test fail, restore.
- Do not pass `--locked` locally.

---

### Task 1: Declare a required capability on a tool

**Files:**
- Modify: `crates/wafer-block/src/types/endpoint.rs` (the `AgentTool` struct at :64, the `agent_tool` builder at :252)
- Modify: `crates/wafer-block/src/types/block_info.rs` (`validate` at :241, and the `BlockInfoError` enum)
- Test: `crates/wafer-block/src/types/endpoint.rs` (inline `#[cfg(test)] mod tests`), `crates/wafer-block/src/types/block_info.rs` (inline tests)

**Interfaces:**
- Consumes: nothing.
- Produces: `AgentTool::requires_capability: Option<String>`; `BlockEndpoint::requires_capability(&str) -> Self`; `AgentTool::is_valid_capability(&str) -> bool`; `BlockInfoError::InvalidAgentCapability { block: String, method: HttpMethod, path: String, capability: String }`.

- [ ] **Step 1: Write the failing tests**

In `endpoint.rs` tests:

```rust
#[test]
fn requires_capability_records_the_capability_on_the_tool() {
    let ep = BlockEndpoint::post("/b/x/y")
        .agent_tool("do_thing", "Does the thing.")
        .requires_capability("products:edit");
    assert_eq!(
        ep.agent_tool.as_ref().unwrap().requires_capability.as_deref(),
        Some("products:edit")
    );
}

#[test]
fn a_tool_without_requires_capability_is_ungated() {
    let ep = BlockEndpoint::get("/b/x/y").agent_tool("read_thing", "Reads.");
    assert!(ep.agent_tool.as_ref().unwrap().requires_capability.is_none());
}

// `requires_capability` before `agent_tool` would silently discard the
// capability, because `agent_tool` replaces the whole `Option`. Gating that
// fails open is the one outcome this feature may never have, so the builder
// must panic rather than produce an ungated tool.
#[test]
#[should_panic(expected = "requires_capability")]
fn requires_capability_without_agent_tool_panics() {
    let _ = BlockEndpoint::post("/b/x/y").requires_capability("products:edit");
}

#[test]
fn valid_capability_names_are_namespaced_lowercase() {
    assert!(AgentTool::is_valid_capability("products:edit"));
    assert!(AgentTool::is_valid_capability("admin_ui:write_settings"));
    assert!(!AgentTool::is_valid_capability("products"));      // no namespace
    assert!(!AgentTool::is_valid_capability("Products:Edit")); // uppercase
    assert!(!AgentTool::is_valid_capability("products:"));     // empty verb
    assert!(!AgentTool::is_valid_capability(":edit"));         // empty namespace
    assert!(!AgentTool::is_valid_capability("a:b:c"));         // one colon only
    assert!(!AgentTool::is_valid_capability(""));
}
```

In `block_info.rs` tests:

```rust
#[test]
fn validate_rejects_a_malformed_agent_capability() {
    let info = BlockInfo::new("org/b", "0.1.0", "iface@v1", "summary").endpoints(vec![
        BlockEndpoint::post("/b/x/y")
            .agent_tool("do_thing", "Does the thing.")
            .requires_capability("NOT VALID"),
    ]);
    let err = info.validate().expect_err("malformed capability must be rejected");
    assert_eq!(
        err,
        BlockInfoError::InvalidAgentCapability {
            block: "org/b".to_string(),
            method: HttpMethod::Post,
            path: "/b/x/y".to_string(),
            capability: "NOT VALID".to_string(),
        }
    );
}

#[test]
fn validate_accepts_a_well_formed_agent_capability() {
    let info = BlockInfo::new("org/b", "0.1.0", "iface@v1", "summary").endpoints(vec![
        BlockEndpoint::post("/b/x/y")
            .agent_tool("do_thing", "Does the thing.")
            .requires_capability("products:edit"),
    ]);
    assert!(info.validate().is_ok());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p wafer-block requires_capability`
Expected: FAIL — no method named `requires_capability` found for `BlockEndpoint`.

- [ ] **Step 3: Add the field, the builder and the validator**

In `endpoint.rs`, on `AgentTool`:

```rust
    /// Capability the caller must hold for this tool to appear in their
    /// manifest, e.g. `products:edit`. `None` means the tool is gated by
    /// auth level alone.
    ///
    /// A capability nothing can grant makes the tool invisible, never
    /// exposed: the gate fails closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_capability: Option<String>,
```

Add `requires_capability: None` to every `AgentTool` construction site (the `agent_tool` builder at :253).

```rust
    /// Require `capability` for this endpoint's agent tool to be published.
    ///
    /// # Panics
    ///
    /// Panics when called before [`Self::agent_tool`]. `agent_tool` replaces
    /// the whole `Option`, so the reverse order would discard the capability
    /// and publish an ungated write tool — the one failure this gate may not
    /// have. A panic at construction is a boot failure, which is the same
    /// class of visible failure `BlockInfo::validate` already produces.
    pub fn requires_capability(mut self, capability: &str) -> Self {
        let tool = self.agent_tool.as_mut().expect(
            "requires_capability must be called after agent_tool; \
             calling it first would silently produce an ungated tool",
        );
        tool.requires_capability = Some(capability.into());
        self
    }
```

On `impl AgentTool`:

```rust
    /// Whether `capability` is a legal capability name: exactly one colon,
    /// with a non-empty `[a-z0-9_]+` on each side.
    ///
    /// The namespace half exists so a capability reads as belonging to the
    /// block that defines it, and so two blocks cannot accidentally share
    /// one grant.
    pub fn is_valid_capability(capability: &str) -> bool {
        let Some((namespace, verb)) = capability.split_once(':') else {
            return false;
        };
        let ok = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        };
        ok(namespace) && ok(verb)
    }
```

In `block_info.rs`, add the error variant beside `InvalidAgentToolName`:

```rust
    /// An endpoint declared an agent capability that is not a legal
    /// capability name.
    InvalidAgentCapability {
        block: String,
        method: HttpMethod,
        path: String,
        capability: String,
    },
```

and extend the loop in `validate` (after the `is_valid_name` check, inside the same `for ep` body):

```rust
            if let Some(capability) = tool.requires_capability.as_ref() {
                if !AgentTool::is_valid_capability(capability) {
                    return Err(BlockInfoError::InvalidAgentCapability {
                        block: self.name.clone(),
                        method: ep.method,
                        path: ep.path.clone(),
                        capability: capability.clone(),
                    });
                }
            }
```

Add a `Display` arm for the new variant matching the style of `InvalidAgentToolName`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p wafer-block requires_capability && cargo test -p wafer-block agent_capability`
Expected: PASS.

- [ ] **Step 5: Verify the tests are load-bearing**

Change `is_valid_capability` to `true`, confirm `validate_rejects_a_malformed_agent_capability` fails, then restore. Remove the `expect` in the builder (use `if let Some`), confirm `requires_capability_without_agent_tool_panics` fails, then restore.

- [ ] **Step 6: Commit**

```bash
git add crates/wafer-block/src/types/endpoint.rs crates/wafer-block/src/types/block_info.rs
git commit -m "feat(wafer-block): let an agent tool declare a required capability"
```

---

### Task 2: Omit a gated tool the caller cannot hold

**Files:**
- Modify: `crates/wafer-core/src/discovery.rs` (`generate_webmcp` :1809, `generate_webmcp_declared_auth` :1852, `generate_webmcp_report` :1913, the `visible_to_caller` binding at :2039)
- Test: `crates/wafer-core/src/discovery.rs` (inline tests)

**Interfaces:**
- Consumes: `AgentTool::requires_capability` from Task 1.
- Produces: `pub struct CapabilitySet(BTreeSet<String>)` with `CapabilitySet::none() -> Self`, `CapabilitySet::from_iter<I: IntoIterator<Item = String>>(i: I) -> Self`, `CapabilitySet::holds(&self, capability: &str) -> bool`. All three projection functions take `held: &CapabilitySet` as their final parameter.

- [ ] **Step 1: Write the failing tests**

```rust
fn gated_block() -> BlockInfo {
    BlockInfo::new("org/b", "0.1.0", "iface@v1", "summary").endpoints(vec![
        BlockEndpoint::get("/b/b/read")
            .auth(AuthLevel::Admin)
            .output::<TestOut>()
            .agent_tool("read_thing", "Reads."),
        BlockEndpoint::post("/b/b/write")
            .auth(AuthLevel::Admin)
            .input::<TestIn>()
            .output::<TestOut>()
            .agent_tool("write_thing", "Writes.")
            .requires_capability("b:edit"),
    ])
}

fn tool_names(manifest: &Value) -> Vec<String> {
    manifest["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_gated_tool_is_absent_when_the_capability_is_not_held() {
    let manifest = generate_webmcp_declared_auth(
        &[gated_block()],
        AuthLevel::Admin,
        &CapabilitySet::none(),
    );
    assert_eq!(tool_names(&manifest), vec!["read_thing"]);
}

#[test]
fn a_gated_tool_is_present_when_the_capability_is_held() {
    let held = CapabilitySet::from_iter(["b:edit".to_string()]);
    let manifest = generate_webmcp_declared_auth(&[gated_block()], AuthLevel::Admin, &held);
    let mut names = tool_names(&manifest);
    names.sort();
    assert_eq!(names, vec!["read_thing", "write_thing"]);
}

/// An unrelated capability must not open an unrelated gate.
#[test]
fn holding_a_different_capability_does_not_publish_the_gated_tool() {
    let held = CapabilitySet::from_iter(["other:edit".to_string()]);
    let manifest = generate_webmcp_declared_auth(&[gated_block()], AuthLevel::Admin, &held);
    assert_eq!(tool_names(&manifest), vec!["read_thing"]);
}

/// The capability gate is a per-caller outcome, not a defect. Recording it
/// as a refusal would put a tool the caller may not see into the operator's
/// per-manifest census — the existence oracle #324 closed.
#[test]
fn omitting_a_gated_tool_records_no_refusal() {
    let (_manifest, refused) = generate_webmcp_report(
        &[gated_block()],
        AuthLevel::Admin,
        |_b, ep| ep.auth,
        &CapabilitySet::none(),
    );
    assert!(
        refused.is_empty(),
        "capability omission must not be a refusal, got {refused:?}"
    );
}

/// The gate is independent of auth: holding the capability does not lift the
/// caller's tier.
#[test]
fn the_capability_does_not_substitute_for_auth_level() {
    let held = CapabilitySet::from_iter(["b:edit".to_string()]);
    let manifest = generate_webmcp_declared_auth(&[gated_block()], AuthLevel::Public, &held);
    assert_eq!(tool_names(&manifest), Vec::<String>::new());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p wafer-core gated_tool`
Expected: FAIL — `CapabilitySet` not found; the projection functions take three arguments.

- [ ] **Step 3: Add `CapabilitySet` and thread it through**

Near the other discovery types in `discovery.rs`:

```rust
/// The capabilities a manifest's caller currently holds.
///
/// Held capabilities are resolved by the host from whatever it uses to grant
/// them — a session, a role, a signed grant — and this type carries only the
/// result. `discovery` never learns how a capability was obtained.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySet(std::collections::BTreeSet<String>);

impl CapabilitySet {
    /// A caller holding nothing. Every gated tool is omitted.
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether this caller holds `capability`.
    pub fn holds(&self, capability: &str) -> bool {
        self.0.contains(capability)
    }
}

impl FromIterator<String> for CapabilitySet {
    fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}
```

Add `held: &CapabilitySet` as the final parameter of `generate_webmcp`, `generate_webmcp_declared_auth` and `generate_webmcp_report`, forwarding it through. Then define the predicate once, immediately above the `visible_to_caller` binding at :2039:

```rust
            // A gate the caller has not opened is indistinguishable, from the
            // page's point of view, from a tool that does not exist. Folding
            // it into `visible_to_caller` rather than adding a second filter
            // is deliberate: the refusal reports, the name census and the
            // manifest then all agree on one definition of "this caller may
            // invoke this", and there is no second place for them to drift.
            let capability_held = tool
                .requires_capability
                .as_ref()
                .is_none_or(|c| held.holds(c));

            let visible_to_caller =
                auth_rank(effective_auth(block, ep)) <= ceiling && capability_held;
```

Update the two `generate_webmcp*` doc comments to state that a gated tool the caller cannot hold is omitted and is not a refusal.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p wafer-core webmcp`
Expected: PASS, including the pre-existing WebMCP tests (which now pass `&CapabilitySet::none()`).

- [ ] **Step 5: Verify the tests are load-bearing**

Replace `capability_held` with `true` in the `visible_to_caller` expression. Confirm `a_gated_tool_is_absent_when_the_capability_is_not_held` and `holding_a_different_capability_does_not_publish_the_gated_tool` both fail. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/wafer-core/src/discovery.rs
git commit -m "feat(wafer-core)!: omit an agent tool whose capability the caller does not hold"
```

---

### Task 3: Scope the duplicate-name census by capability

**Files:**
- Modify: `crates/wafer-core/src/discovery.rs` (the `name_counts` census loop, ~:2014-2024)
- Test: `crates/wafer-core/src/discovery.rs` (inline tests)

**Interfaces:**
- Consumes: `CapabilitySet` and `capability_held` semantics from Task 2.
- Produces: no new API.

**Why this is its own task:** the census currently counts by auth only. Leaving it that way makes a gated tool suppress an identically-named ungated tool for a caller who can never hold the gate — the tool vanishes with no refusal the operator can act on, and its *absence* leaks that a gated endpoint claims that name. That is precisely the leak the per-manifest census was introduced to close, on a new axis.

- [ ] **Step 1: Write the failing test**

```rust
/// A name claimed by a gated tool must not suppress an ungated tool of the
/// same name for a caller who does not hold the gate. For that caller there
/// is exactly one claimant, so the name is unambiguous and must publish.
#[test]
fn a_gated_claimant_does_not_suppress_a_visible_tool_of_the_same_name() {
    let block = BlockInfo::new("org/b", "0.1.0", "iface@v1", "summary").endpoints(vec![
        BlockEndpoint::get("/b/b/public-thing")
            .auth(AuthLevel::Public)
            .output::<TestOut>()
            .agent_tool("thing", "The ungated one."),
        BlockEndpoint::post("/b/b/gated-thing")
            .auth(AuthLevel::Public)
            .input::<TestIn>()
            .output::<TestOut>()
            .agent_tool("thing", "The gated one.")
            .requires_capability("b:edit"),
    ]);

    let (manifest, refused) = generate_webmcp_report(
        &[block],
        AuthLevel::Public,
        |_b, ep| ep.auth,
        &CapabilitySet::none(),
    );

    assert_eq!(tool_names(&manifest), vec!["thing"]);
    assert!(
        refused.is_empty(),
        "the surviving claimant is unambiguous for this caller, got {refused:?}"
    );
}

/// And with the gate open both claim it, so the name really is ambiguous and
/// neither may publish.
#[test]
fn both_claimants_visible_makes_the_name_ambiguous() {
    let block = BlockInfo::new("org/b", "0.1.0", "iface@v1", "summary").endpoints(vec![
        BlockEndpoint::get("/b/b/public-thing")
            .auth(AuthLevel::Public)
            .output::<TestOut>()
            .agent_tool("thing", "The ungated one."),
        BlockEndpoint::post("/b/b/gated-thing")
            .auth(AuthLevel::Public)
            .input::<TestIn>()
            .output::<TestOut>()
            .agent_tool("thing", "The gated one.")
            .requires_capability("b:edit"),
    ]);

    let held = CapabilitySet::from_iter(["b:edit".to_string()]);
    let (manifest, refused) =
        generate_webmcp_report(&[block], AuthLevel::Public, |_b, ep| ep.auth, &held);

    assert_eq!(tool_names(&manifest), Vec::<String>::new());
    assert_eq!(refused.len(), 2);
}
```

- [ ] **Step 2: Run the tests to verify the first fails**

Run: `cargo test -p wafer-core gated_claimant`
Expected: FAIL — `thing` is suppressed and two `DuplicateToolName` refusals are recorded, because the census still counts the gated claimant.

- [ ] **Step 3: Scope the census by capability**

In the census loop, replace the condition:

```rust
            if let Some(tool) = ep.agent_tool.as_ref() {
                // The census counts endpoints this caller may invoke, and a
                // gate the caller has not opened makes an endpoint exactly
                // as un-invokable as a tier they do not hold. Counting a
                // gated claimant would let it suppress a visible tool of the
                // same name, so the tool would vanish for a caller who can
                // never open the gate — and its absence would leak that some
                // endpoint they cannot reach claims that name.
                let capability_held = tool
                    .requires_capability
                    .as_ref()
                    .is_none_or(|c| held.holds(c));
                if AgentTool::is_valid_name(&tool.name)
                    && auth_rank(effective_auth(block, ep)) <= ceiling
                    && capability_held
                {
                    *name_counts.entry(tool.name.as_str()).or_insert(0) += 1;
                }
            }
```

Extend the census doc comment above it to record that the count is scoped by auth **and** capability, with the leak argument.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p wafer-core webmcp`
Expected: PASS.

- [ ] **Step 5: Verify the test is load-bearing**

Drop `&& capability_held` from the census condition. Confirm `a_gated_claimant_does_not_suppress_a_visible_tool_of_the_same_name` fails. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/wafer-core/src/discovery.rs
git commit -m "fix(wafer-core): scope the duplicate-tool-name census by capability too"
```

---

### Task 4: Update every in-repo caller and document the change

**Files:**
- Modify: every `generate_webmcp*` call site in `wafer-run` (inspector views, examples, docs)
- Modify: `CHANGELOG.md`
- Test: existing suites

**Interfaces:**
- Consumes: the signatures from Task 2.
- Produces: nothing new.

- [ ] **Step 1: Find every call site**

Run: `rg -n 'generate_webmcp' --type rust -g '!target'`
Every hit outside `discovery.rs` needs a `&CapabilitySet` argument. The inspector's manifest view passes `&CapabilitySet::none()` — it renders what a caller with no open gate receives, which is the honest default for a diagnostic page.

- [ ] **Step 2: Build to enumerate the failures**

Run: `cargo check --workspace --all-targets`
Expected: FAIL, one error per call site, each naming the missing argument.

- [ ] **Step 3: Update each call site**

Pass `&CapabilitySet::none()` at every site except a deliberate capability-aware one. Where the inspector renders a per-auth-level table, add a short note in the page copy that gated tools are not shown, so an operator does not read a missing write tool as a bug.

- [ ] **Step 4: Run the full suite**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Write the CHANGELOG entry**

```markdown
### Changed

- **Breaking:** `generate_webmcp`, `generate_webmcp_declared_auth` and
  `generate_webmcp_report` take a final `held: &CapabilitySet` argument. Pass
  `&CapabilitySet::none()` to preserve existing behaviour.

### Added

- `BlockEndpoint::requires_capability(..)` gates an agent tool on a named
  capability the host resolves per request. A caller who does not hold it
  receives a manifest with the tool absent — omitted like an out-of-tier tool,
  never recorded as a refusal. A capability nothing can grant fails closed.
```

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "chore: thread CapabilitySet through every webmcp call site; changelog"
```

---

### Task 5: Open the PR

- [ ] **Step 1: Push and open**

```bash
git push -u origin feat/webmcp-capability-gating
gh pr create --title "feat(webmcp)!: capability-gated agent tools" --body "$(cat <<'BODY'
Adds `BlockEndpoint::requires_capability(..)`. A tool whose capability the
caller does not hold is omitted from their manifest, exactly the way an
out-of-tier tool is — and, like the auth filter, it is an omission rather
than a refusal, so it never enters the per-manifest census.

The duplicate-name census is scoped by capability for the same reason it is
scoped per manifest: otherwise a gated claimant suppresses a visible tool of
the same name, and its absence leaks that an unreachable endpoint claims it.

Breaking: the three `generate_webmcp*` functions take a final
`&CapabilitySet`.

Consumer: impresspress agent-driven product writes
(`docs/superpowers/specs/2026-09-01-agent-product-writes-design.md`).

🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_01WMJ8nQz9HTrc6CsSesAXUk
BODY
)"
```

- [ ] **Step 2: Wait for CI, then hand off**

Once merged, note the merge SHA — Plan C's first task bumps the impresspress pin to it. Re-resolve `Cargo.lock` from **outside** the impresspress tree (`cargo metadata --manifest-path …` run from the scratchpad), or the repo-level `[patch]` writes path sources into the lock.
