# Ownership and repo boundaries, PR 2: block-enabled defaults from `BlockInfo`

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Delete `features::ENABLED_DEFAULTS` — a hand-maintained list of block names and enablement bools that disagrees with the blocks' own `BlockInfo` declarations — and derive the boot-time seed defaults from those declarations instead. After this PR a block declares whether it can be turned off (`can_disable`) and what it ships as (`default_enabled`) in exactly one place: its own `info()`.

**Architecture:** `plan_seed_decisions(existing, defaults)` and `block_settings::load_and_seed(db, defaults)` take `defaults: &[(String, bool)]` as a parameter, so the planner stays pure and every test drives it from a fixture slice instead of the production block set. The production slice is built once, next to the registry that owns it, by `blocks::block_enabled_defaults()`:
`all_block_infos().iter().filter(|i| i.can_disable).map(|i| (i.name.clone(), i.default_enabled)).collect()`.
The three boot callers (CLI `server.rs`, Cloudflare `lib.rs`, web `config.rs`) pass that slice.

**The ordering risk this plan is shaped around:** `plan_seed_decisions` re-seeds any row whose `seed_defaults_hash` is a *stale* `seed:<hex>` — that is the #222 hash gate. `legalpages` and `userportal` declare `.default_enabled(false)` today while `ENABLED_DEFAULTS` seeds them `true`. Switching the seed source in one step would therefore flip both blocks to disabled on every existing deployment at the next cold start (every row not admin-edited is still carrying `seed_hash_for(true)`, which becomes stale against a new default of `false`). So the declarations are aligned to `true` — the value that has been in production since the constant was written — in a **separate, earlier commit**, guarded by a test that the derived defaults equal today's `ENABLED_DEFAULTS` for every block present in both. Only then does the seed source change.

**Tech Stack:** Rust, `wafer-run` at rev `7d47e5e` (`BlockInfo::{name, can_disable, default_enabled}`, defaults `can_disable: false, default_enabled: true` at `wafer-block/src/types/block_info.rs:214-215`), `serde_json`, `tokio` tests, nightly `rustfmt`, `clippy -D warnings`, `wasm-bindgen-test` for the Cloudflare crate.

**Spec:** `docs/superpowers/specs/2026-09-06-ownership-and-repo-boundaries-design.md`, section 2.1.4, the `ENABLED_DEFAULTS` paragraph of inventory 1.1, decision 5.6, and tests 3.2 "PR 2". Section 2.1.1–2.1.3 landed in PR 1 (`#19`), which moved the loaders to `crates/impresspress-core/src/platform_state/block_settings.rs`; the spec's `features.rs:402-541` line references are pre-move and are re-resolved here.

## Decisions taken while planning (recorded, not re-litigated)

1. **`blocks::block_enabled_defaults()` exists; the three callers do not each re-type the iterator chain.** Spec 2.1.4 says "the caller builds them as `all_block_infos()…`". Written literally that is the same four-line chain in three crates, which is the hand-maintained-list smell this PR exists to remove, one level up. The derivation is spelled once in `blocks/mod.rs`, immediately below `all_block_infos()` (the only other place that enumerates the block set), and the callers pass its result. `plan_seed_decisions`/`load_and_seed` still take the slice as a parameter — that is what makes the planner testable against fixtures and what keeps `platform_state` from importing `blocks::`.
2. **`SeedDecision.block_name` becomes `String`.** It is `&'static str` today only because it was copied out of a `&'static [(&str, bool)]`. A `BlockInfo::name` is an owned `String`; there is no static to borrow from. The one consumer (`apply_seed_decision`) takes `&str` either way.
3. **The derived set is per-build, and that is the correct semantic.** `ENABLED_DEFAULTS` named eight blocks unconditionally, including blocks a `--no-default-features` Cloudflare bundle never compiles. `all_block_infos()` is `#[cfg]`-gated per block, so a bundle without `block-tickets` now seeds no `tickets` row. That row was previously seeded `false` for a block whose routes did not exist in that binary; without it, the absent row falls back to enabled for a block that is still not registered, so routing is identical (404 from the router either way). Recorded rather than "fixed": making the seed match the build is the point.
4. **`system` losing its row is a no-op, and the fallback is the documented contract.** `impresspress/system` is not `can_disable`, so it gets no seed row; `BlockSettings::is_block_enabled` returns `true` for an absent row (`features.rs:104-113`), which is exactly what its `("impresspress/system", true)` entry produced. Same for `wafer-run/auth`, which is not in `all_block_infos()` at all. Neither block renders a toggle in admin (`pages/blocks.rs:348` gates the toggle on `can_disable`), so no row can be created for them from the UI either.
5. **`llm` and `vector` gaining rows at `true` is a no-op too, and it deletes a false comment.** The `ENABLED_DEFAULTS` doc block excludes them "until the LlmService trait refactor lands"; `blocks/mod.rs:74-86` documents that it has landed (`llm` is registered through `register_llm` and its `info()` is in `all_block_infos()` under `block-llm`). Both declare `.default_enabled(true)`, and their absent-row fallback is already `true`, so the row makes explicit what was already in effect — and now the admin toggle has a row to update in place.
6. **`admin` stays out without a special case.** The constant excluded it in prose (its `seed_defaults_hash` column is owned by `admin::settings::seed_defaults` for the shared-vars payload hash — two writers with different formats would loop). `AdminBlock::info()` does not set `can_disable`, so the filter excludes it structurally. The reason is preserved as a doc comment on `block_enabled_defaults()`, because it is a real constraint that a future `.can_disable(true)` on admin would violate.
7. **Whether `legalpages`/`userportal` *should* default off is not decided here** (spec 5.6). This PR makes the declaration match shipped behaviour. Changing it afterwards is a one-line edit whose effect on existing rows — re-seeded unless admin-edited — is then deliberate and reviewable.
8. **The `features.rs` lane tests move to a fixture slice, not to `block_enabled_defaults()`.** Driving the planner from the production block set is what made the tests silent about the legalpages/userportal divergence in the first place. Each lane test builds its own `Vec<(String, bool)>`; the spec's "three fake `BlockInfo`s" case (`can_disable` on/off × `default_enabled` true/false) is a separate test of `block_enabled_defaults`'s *filter*, driven through a small helper that takes `&[BlockInfo]`.

## Global Constraints

- Both snapshot gates byte-identical: `crates/impresspress-core/tests/snapshots/*.openapi.json` and `*.endpoints.json`. This PR declares no endpoint and changes no schema. `UPDATE_OPENAPI_SNAPSHOTS=1` is never run.
- No change to wafer-run (rev `7d47e5e`). No SQL file touched; no migration; no schema change. The `block_settings` table shape is untouched — only which rows the seed writes.
- No raw SQL outside the existing test-fixture exception in `block_settings.rs`'s `load_and_seed_tests` (the migration-file runner).
- TDD: write the test, run it, see it fail for the expected reason, then implement, then see it pass. Commits carry the two trailer lines:
  `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>` and
  `Claude-Session: https://claude.ai/code/session_01Vfp8g6U7PQTG1kgw2834Tp`.
- Verification before the PR: `cargo +nightly fmt --all -- --check`; `cargo clippy -p impresspress-core --all-targets -- -D warnings`; `cargo test -p impresspress-core --no-fail-fast` (known unrelated failure `lockfile_loads_remote_block`); `cargo test -p impresspress-core --features block-dev --test endpoint_surface --test openapi_snapshot`; `cargo check -p impresspress-cloudflare --target wasm32-unknown-unknown`; `env CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner cargo test -p impresspress-cloudflare --target wasm32-unknown-unknown`; `cargo check -p impresspress-web --target wasm32-unknown-unknown`; `cargo test -p impresspress`; `cargo clippy -p impresspress --all-targets` (four pre-existing lib lints on main; add none); `bash scripts/audit-wrap-grants.sh`.
- Work happens in the worktree `/home/joris/Programs/suppers-ai/impresspress-worktrees/phase0` on branch `phase2/defaults-from-block-info` (from `origin/main` at `0d2daa19`, the merge of PR #19). The session's shell guard refuses compound commands containing `git` or shell variables; those go in a script under the scratchpad directory and run with `bash <script>`.

---

## File structure

| File | Responsibility after this PR |
|---|---|
| `crates/impresspress-core/src/blocks/mod.rs` | Adds `block_enabled_defaults()` below `all_block_infos()`: the one place the seed set is derived, carrying the `admin` / `system` / absent-row reasoning as doc comments. |
| `crates/impresspress-core/src/blocks/legalpages/mod.rs` | `.default_enabled(true)` — matches what has shipped. |
| `crates/impresspress-core/src/blocks/userportal/mod.rs` | `.default_enabled(true)` — matches what has shipped. |
| `crates/impresspress-core/src/features.rs` | `ENABLED_DEFAULTS` and its doc block deleted. `SeedDecision.block_name: String`. `plan_seed_decisions(existing, defaults)`. `seed_plan_tests` driven by fixture slices. |
| `crates/impresspress-core/src/platform_state/block_settings.rs` | `load_and_seed(db, defaults)`; module docs no longer name `ENABLED_DEFAULTS`. `load_and_seed_tests` driven by a fixture slice. |
| `crates/impresspress-core/tests/block_enabled_defaults.rs` | New: the regression guard (derived == today's constant for every block in both) plus the `can_disable`/`default_enabled` filter matrix. |
| `crates/impresspress/src/cli/server.rs` | `load_and_seed(&database, &blocks::block_enabled_defaults())`. |
| `crates/impresspress-cloudflare/src/lib.rs` | `load_and_seed(&self.db, &blocks::block_enabled_defaults())`. |
| `crates/impresspress-web/src/config.rs` | `load_and_seed(db, &blocks::block_enabled_defaults())`. |
| `docs/superpowers/plans/2026-09-06-ownership-2-defaults-from-block-info.md` | This plan. |

---

### Task 0: This plan

- [ ] Commit this file as the first commit on the branch.

---

### Task 1: Align `legalpages` and `userportal`, with the guard test

The guard has to be written against the tree that still has `ENABLED_DEFAULTS`, because the constant is the thing it compares against. It is the only test in the PR that names both sides.

**Files:**
- Create: `crates/impresspress-core/tests/block_enabled_defaults.rs`
- Modify: `crates/impresspress-core/src/blocks/legalpages/mod.rs`, `crates/impresspress-core/src/blocks/userportal/mod.rs`

**Step 1: Write the failing test.**
- [ ] `derived_defaults_match_todays_constant`: build the derived set from `all_block_infos()` (filter `can_disable`, map `(name, default_enabled)`); for every `(name, value)` in `features::ENABLED_DEFAULTS` that also appears in the derived set, assert the values are equal. Report every mismatch in one assertion message, not the first.
- [ ] In the same file, assert the two directions of the difference explicitly, so the set change is pinned rather than merely tolerated:
  - `constant_only_entries_are_not_can_disable`: every name in `ENABLED_DEFAULTS` missing from the derived set is either absent from `all_block_infos()` (`wafer-run/auth`) or declares `can_disable == false` (`impresspress/system`), **and** its constant value is `true`, so the absent-row fallback preserves it.
  - `derived_only_entries_default_true`: every name in the derived set missing from the constant (`impresspress/llm`, `impresspress/vector`) has `default_enabled == true`, so gaining a row changes nothing.
- [ ] Run `cargo test -p impresspress-core --test block_enabled_defaults`. Expect `derived_defaults_match_todays_constant` to FAIL naming `impresspress/legalpages` (derived `false`, constant `true`) and `impresspress/userportal` (derived `false`, constant `true`). The other two tests pass.

**Step 2: Align the declarations.**
- [ ] `legalpages/mod.rs:678` `.default_enabled(false)` → `.default_enabled(true)`.
- [ ] `userportal/mod.rs:140` `.default_enabled(false)` → `.default_enabled(true)`.
- [ ] Re-run; all three pass.

**Step 3: Verify nothing else read those declarations.**
- [ ] `grep -rn "default_enabled" crates/` — the only non-declaration site is `admin/pages/blocks.rs:67-68`, a test fixture for the toggle fragment. No snapshot, no rendered page and no config surface carries the field.
- [ ] `cargo test -p impresspress-core --no-fail-fast`.

**Commit:** `fix(blocks): declare legalpages and userportal enabled by default`

---

### Task 2: `plan_seed_decisions` takes the defaults

**Files:**
- Modify: `crates/impresspress-core/src/features.rs`

**Step 1: Rewrite the lane tests against fixtures (they fail to compile).**
- [ ] Add a `fixture()` helper returning `Vec<(String, bool)>` with a deliberate mix of `true` and `false` entries and at least five names, so the mixed-state lane test keeps its five lanes without depending on the production block count.
- [ ] Rewrite `plan_seed_decisions_{inserts_when_row_absent, skips_when_hash_matches_current, updates_when_hash_stale, skips_user_edited, skips_empty_hash_legacy, handles_mixed_row_states}` to build `existing` from that fixture and call `plan_seed_decisions(&existing, &fixture)`. `defaults_count()` becomes `fixture.len()`.
- [ ] Run `cargo test -p impresspress-core --lib features::` — expect a compile error: `plan_seed_decisions` takes 1 argument, 2 supplied.

**Step 2: Change the signature.**
- [ ] `pub fn plan_seed_decisions(existing: &HashMap<String, ExistingRow>, defaults: &[(String, bool)]) -> Vec<SeedDecision>`, iterating `defaults`.
- [ ] `SeedDecision.block_name: String` (doc comment: "Block name from the caller's defaults slice", no longer "from `ENABLED_DEFAULTS`").
- [ ] Update the `plan_seed_decisions` doc comment: it syncs rows with "the caller's defaults" and the caller is named.
- [ ] `block_settings::load_and_seed` will not compile yet; that is Task 3.

**Commit:** `refactor(features): plan_seed_decisions takes the defaults as a parameter`

---

### Task 3: `load_and_seed(db, defaults)`, `block_enabled_defaults()`, and the callers

One commit, because the signature change and its three cross-crate callers cannot be split without leaving a crate broken (PR 1, decision 1).

**Files:**
- Modify: `crates/impresspress-core/src/blocks/mod.rs`, `crates/impresspress-core/src/platform_state/block_settings.rs`, `crates/impresspress-core/src/features.rs`, `crates/impresspress/src/cli/server.rs`, `crates/impresspress-cloudflare/src/lib.rs`, `crates/impresspress-web/src/config.rs`, `crates/impresspress-core/tests/block_enabled_defaults.rs`

**Step 1: Point the guard test at the new function (it fails to compile).**
- [ ] In `tests/block_enabled_defaults.rs`, replace the hand-built derived set with `impresspress_core::blocks::block_enabled_defaults()`. Keep `ENABLED_DEFAULTS` on the other side of the comparison for now — this step is what proves the new function reproduces the constant.
- [ ] Rewrite `load_and_seed_tests` in `block_settings.rs` against a fixture slice (same shape as Task 2's), so `ENABLED_DEFAULTS[0]` disappears from the stale-hash / user-edited / read-only lanes and `seeds_all_defaults_on_empty_table` iterates the fixture.
- [ ] Run; expect "cannot find function `block_enabled_defaults`" and the arity error on `load_and_seed`.

**Step 2: Implement.**
- [ ] `blocks::block_enabled_defaults() -> Vec<(String, bool)>` with the filter/map from spec 2.1.4 and doc comments carrying decisions 4–6 above.
- [ ] `load_and_seed(db: &Arc<dyn DatabaseService>, defaults: &[(String, bool)])`, forwarding to `plan_seed_decisions(&existing, defaults)`. Its doc comment loses the `ENABLED_DEFAULTS` references; so do the module header and the `id_by_block` comment.
- [ ] Callers: `server.rs`, `lib.rs`, `config.rs` each pass `&impresspress_core::blocks::block_enabled_defaults()`.
- [ ] Run the core suite; all green.

**Step 3: Delete the constant.**
- [ ] Delete `ENABLED_DEFAULTS` and the whole doc block above it (`features.rs:201-232`), including the stale "until the LlmService trait refactor lands" paragraph.
- [ ] In `tests/block_enabled_defaults.rs`, replace the comparison-against-the-constant with the same table inlined in the test as `SHIPPED_DEFAULTS_BEFORE_PR2` — a frozen record of the eight rows production seeded, with a comment saying it is a historical baseline and must never be edited to match a new derivation. That is what keeps the guard meaningful after the constant is gone.
- [ ] Run everything in Global Constraints.

**Commit:** `refactor(features): seed block-enabled defaults from BlockInfo`

---

### Task 4: Verification and PR

- [ ] `cargo +nightly fmt --all -- --check`
- [ ] `cargo clippy -p impresspress-core --all-targets -- -D warnings`
- [ ] `cargo test -p impresspress-core --no-fail-fast`
- [ ] `cargo test -p impresspress-core --features block-dev --test endpoint_surface --test openapi_snapshot`
- [ ] `git status --short` on `crates/impresspress-core/tests/snapshots/` — empty.
- [ ] `cargo check -p impresspress-cloudflare --target wasm32-unknown-unknown`
- [ ] `env CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner cargo test -p impresspress-cloudflare --target wasm32-unknown-unknown`
- [ ] `cargo check -p impresspress-web --target wasm32-unknown-unknown`
- [ ] `cargo test -p impresspress`
- [ ] `cargo clippy -p impresspress --all-targets` (four pre-existing lib lints; count unchanged)
- [ ] `bash scripts/audit-wrap-grants.sh`
- [ ] `push-and-pr.sh "refactor(features): seed block-enabled defaults from BlockInfo" <body>`, with the derived-vs-constant table in the body. Do not merge.
