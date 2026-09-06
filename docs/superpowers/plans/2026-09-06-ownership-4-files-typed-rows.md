# Ownership and repo boundaries, PR 4: typed rows from the files repo; one decode of `public` (B13)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `blocks/files/repo/` stops returning untyped `Record`/`RecordList` and starts returning typed rows. Every files column is then spelled in exactly one Rust file, and `public` — an `INTEGER` column on SQLite, `BOOLEAN` on Postgres, written as a JSON bool by four call sites — is decoded exactly once, by `BucketRow::from_record` through `RecordExt::bool_field`. That is B13: today the user bucket page decodes it with `as_bool()` and the admin bucket page with `str_field(..) == "true"`, so the same bucket is Private on one page and Public on the other, and on SQLite it is Private on both no matter what was written.

**Architecture:** `repo::{buckets,objects,shares,quota,views}` each gain a row struct with one `from_record`. `Page<T> { rows, total }` (in `repo/mod.rs`) replaces `RecordList` on every listing function. The six page-side decoders become render-side projections built with `From<&repo::…Row>` — the projections keep the short owner / short date / object-count shaping that is genuinely presentational, and hold no field reads. `quota.rs`'s `quota_from_record` moves into `QuotaRow`. `cloud.rs` reads `share.created_by` from the row instead of from `record.data`.

**Tech Stack:** Rust, `wafer-run` at rev `7d47e5e`, `wafer_core::clients::database`, `serde_json`, `maud`, `tokio` tests, nightly `rustfmt`, `clippy -D warnings`.

**Spec:** `docs/superpowers/specs/2026-09-06-ownership-and-repo-boundaries-design.md`, section 2.3, inventory 1.3, tests 3.2 "PR 4". PRs 1 (`#19`), 2 (`#20`) and 3 (`#21`) are merged; the spec's line references predate them and are re-resolved below.

## Verified against the tree before planning (the spec's claims, re-checked)

1. **Inventory 1.3's decoder list is right, and every line still resolves.** `pages_user/buckets.rs` declares `BucketRow` at `:19-26` and decodes at `:143-170` (`public` via `v.as_bool()`); `pages_admin.rs` declares `AdminBucketRow` at `:216-222` and decodes at `:258-275` (`public` via `str_field("public") == "true"`); `pages_user/objects.rs` `ObjectRow` at `:17-23`, decoded at `:198-222`; `pages_user/cloudstorage.rs` `ShareRow` at `:23-31`, decoded at `:96-130`; `pages_admin.rs` `AdminShareRow` at `:327-336`, decoded at `:383-419`; `AdminQuotaRow` at `:447-453`, decoded at `:491-506`; `quota.rs:15-31` `quota_from_record`; `cloud.rs:189-194` reads `created_by` out of `record.data` by hand (the spec's `:137-146` predates the route-table PRs). Four `public` writers: `repo/buckets.rs:105`, `cloud.rs` (test fixture), `storage/mod.rs:67`, `pages_user/mod.rs:83`.
2. **The two pages do NOT disagree on SQLite; they are both wrong on SQLite and disagree on Postgres and D1.** `migrations/001_initial_schema.sqlite.sql:7` is `public INTEGER NOT NULL DEFAULT 0`, so the row reads back as `Number(1)`: `as_bool()` → `false`, `str_field(..) == "true"` → `false`. Both render *Private* for a bucket created public. `001_initial_schema.postgres.sql:8` is `BOOLEAN`, reading back `Bool(true)`: user → Public, admin → private. A TEXT-stored `"true"` (the shape `RecordExt::bool_field` exists to absorb) inverts that pair. So the regression test asserts the *correct* visibility on both pages, not merely that they agree — agreement alone is satisfied by today's SQLite behaviour.
3. **Spec 2.3's "`contracts.rs` views build from the rows" does not apply to today's `contracts.rs`.** `ObjectInfoResponse`/`ObjectListResponse` (`contracts.rs:27,45`) mirror `wafer_core::clients::storage::{ObjectInfo, ObjectList}` and are built in `storage/objects.rs:78-89` from `store::list` — the wafer storage service, not the objects table. No repo row reaches `contracts.rs`, and none should: the two are different sources. `contracts.rs` is untouched by this PR and the reason is recorded here rather than improvised in code.
4. **The files repo door is already closed.** No file outside `blocks/files/repo/` spells an `impresspress__files__*` literal in code except `platform_state/wrap_grants.rs:392` (a WRAP-grant fixture that must pin the wire name). The `<module>::TABLE` idents appear in `blocks/files/mod.rs`'s `collections(..)` and in four test files aiming `FailingDbOpContext`. So `tests/repo_door.rs` extended to the files tables is a standing regression guard here, not a red-then-green step, and its allowlist does not end empty — the five entries are the same two categories `blocks/admin/mod.rs` and `blocks/admin/pages/blocks.rs` are already allowlisted under.

## Decisions taken while planning (recorded, not re-litigated)

1. **`Page<T> { rows, total }` is generic and lives in `repo/mod.rs`.** Spec 2.3 names it. It drops `RecordList`'s `page`/`page_size`, which every caller already knows because it computed the offset it passed in. PR 3's `UserPage` is a per-table struct because `AdminUserListResponse::from_page` publishes `page`/`page_size` in a declared schema; no files endpoint declares one, so the generic is free here.
2. **`views` gets a `ViewRow` too, though spec 2.3 names only four modules.** `views::list_recent_for_user` is the sibling of `objects::search_completed` — `/b/storage/api/recent` next to `/b/storage/api/search`, both in `storage/search.rs`, both `ok_json` of the same list. Converting one and not the other would leave two adjacent endpoints on two different response shapes. Same for `shares::list_access_logs`, which gets `AccessLogRow` in the `shares` module that already owns its table.
3. **The five JSON list endpoints change response shape, and that is the one behaviour change in this PR.** They serialize what the repo hands them, which today is `RecordList` — `{"records":[{"id":..,"data":{..}}],"total_count":n,"page":p,"page_size":s}` — and after this PR is `Page<T>` — `{"rows":[{..flat typed row..}],"total":n}`. Affected: `GET /b/cloudstorage/shares`, `GET /b/cloudstorage/admin/shares`, `GET /b/cloudstorage/admin/access-logs`, `GET /b/cloudstorage/admin/quotas`, `GET /b/storage/api/search`, `GET /b/storage/api/recent`, and `PATCH /b/cloudstorage/admin/quotas/{id}` (one row, not a list). None of them declares an output schema (`blocks/files/mod.rs:228` is the only `.output(..)` in the block), so both snapshot gates stay byte-identical; the block's own JS bundle reads none of them (`ui/assets/files-browser.js` only POSTs to `/b/cloudstorage/shares` and DELETEs `/b/cloudstorage/shares/{token}`); and no other crate or block calls them. Stated in the PR body rather than hidden. Declaring the new shapes with `.output(response_schema_of::<..>)` is deliberately NOT done here: it would move `files.openapi.json`, which this PR's gate forbids. Recorded as a follow-up.
4. **The row types derive `Serialize`; there are no separate wire views.** The rows are flat column mirrors with no computed fields, so a parallel view struct per table would be six types that restate their row field-for-field. The presentational shaping (8-char owner, 10-char date, object count, `max_access_count` filtered to `> 0`) stays where it belongs — in the render-side projections, which are not serialized.
5. **The page projections keep their names and their module.** `pages_user::BucketRow`, `pages_user::ShareRow`, `pages_user::ObjectRow`, `pages_admin::{AdminBucketRow, AdminShareRow, AdminQuotaRow}` stay put; only their construction changes, from a `Record` decode to `From<&repo::…Row>`. Their existing render tests keep working unchanged, which is what proves the projections did not move any markup.
6. **`quota_from_record`'s field-by-field default fallback becomes `QuotaRow::from_record`.** `QuotaRow { id, user_id, config: QuotaConfig }` per spec 2.3; the three `quota_from_record` unit tests move onto it verbatim, so the TEXT-stored-override regression they pin is not re-argued.
7. **`repo::buckets::find_owned` returns `Option<BucketRow>` even though both callers only ask `is_some()`.** Returning `Option<Record>` from one function while every sibling returns a row would leave exactly the untyped hole this PR closes, and the ownership predicate is the one place a future caller is most likely to want the row it already fetched.

## Global Constraints

- Both snapshot gates byte-identical: `crates/impresspress-core/tests/snapshots/*.openapi.json` and `*.endpoints.json`. This PR declares no endpoint and adds no schema. `UPDATE_OPENAPI_SNAPSHOTS=1` is never run.
- No change to wafer-run (rev `7d47e5e`). No migration, no `.sql` file, no schema change.
- Core only: no crate outside `impresspress-core` is touched.
- No raw SQL outside test-fixture setup.
- TDD: write the test, run it, see it fail for the expected reason, then implement, then see it pass. Commits carry the two trailer lines:
  `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>` and
  `Claude-Session: https://claude.ai/code/session_01Vfp8g6U7PQTG1kgw2834Tp`.
- Verification before the PR: `cargo +nightly fmt --all -- --check`; `cargo clippy -p impresspress-core --all-targets -- -D warnings`; `cargo test -p impresspress-core --no-fail-fast` (known unrelated failure `lockfile_loads_remote_block`); `cargo test -p impresspress-core --features block-dev --test endpoint_surface --test openapi_snapshot`; `bash scripts/audit-wrap-grants.sh`. `prepared_plan.rs` and the CLI are untouched, so neither the wasm suite nor `cargo test -p impresspress` is required.
- Work happens in the worktree `/home/joris/Programs/suppers-ai/impresspress-worktrees/phase0` on branch `phase2/files-typed-rows` (from `origin/main` at `9e81b9e5`, the merge of PR #21).

---

## File structure

| File | Responsibility after this PR |
|---|---|
| `crates/impresspress-core/src/blocks/files/repo/mod.rs` | `Page<T> { rows, total }` — the one paginated return shape for the block. |
| `crates/impresspress-core/src/blocks/files/repo/buckets.rs` | `BucketRow { id, name, public, created_by, created_at }` + `from_record` — **the one decode of `public`** (B13). Every function returns rows. |
| `crates/impresspress-core/src/blocks/files/repo/objects.rs` | `ObjectRow { id, bucket, key, size, status, content_type, uploaded_by, created_at, updated_at }` + `from_record`. |
| `crates/impresspress-core/src/blocks/files/repo/shares.rs` | `ShareRow { id, token, bucket, key, created_by, created_at, expires_at, access_count, max_access_count }` and `AccessLogRow`, each + `from_record`. |
| `crates/impresspress-core/src/blocks/files/repo/quota.rs` | `QuotaRow { id, user_id, config }` + `from_record`, absorbing `quota.rs`'s `quota_from_record`. |
| `crates/impresspress-core/src/blocks/files/repo/views.rs` | `ViewRow { id, bucket, key, user_id, viewed_at }` + `from_record`. |
| `crates/impresspress-core/src/blocks/files/pages_user/buckets.rs` | `BucketRow` becomes a projection: `From<(&repo::BucketRow, i64)>`. No field reads. |
| `crates/impresspress-core/src/blocks/files/pages_user/objects.rs` | `ObjectRow` becomes `From<&repo::ObjectRow>`. |
| `crates/impresspress-core/src/blocks/files/pages_user/cloudstorage.rs` | `ShareRow` becomes `From<&repo::ShareRow>`. |
| `crates/impresspress-core/src/blocks/files/pages_admin.rs` | `AdminBucketRow`/`AdminShareRow`/`AdminQuotaRow` become `From<&repo::…Row>`; the three inline decodes deleted. |
| `crates/impresspress-core/src/blocks/files/quota.rs` | `quota_from_record` deleted; `get_user_quota` reads `QuotaRow.config`. |
| `crates/impresspress-core/src/blocks/files/cloud.rs` | `share.created_by` from the row; the JSON handlers serialize `Page<T>` / rows. |
| `crates/impresspress-core/src/blocks/files/share.rs` | `expires_at`, `max_access_count`, `bucket`, `key` from `ShareRow`. |
| `crates/impresspress-core/src/blocks/files/storage/{buckets,objects,search,access,admin,mod}.rs` | Row fields instead of `record.data` lookups. |
| `crates/impresspress-core/tests/repo_door.rs` | The five files tables join the doors, with the allowlist justified entry by entry. |
| `docs/superpowers/plans/2026-09-06-ownership-4-files-typed-rows.md` | This plan. |

---

### Task 0: This plan

- [ ] Commit this file as the first commit on the branch.

### Task 1: The B13 regression, red first

- [ ] Write the DB-backed page test: seed one bucket with `public: true` through `repo::buckets::seed`, render `pages_user::bucket_list_page` and `pages_admin::buckets` against the same `TestContext::with_files()` fixture, and assert **both** report the bucket as public. Run it: it fails on both pages (SQLite hands back `Number(1)`; neither decoder accepts that shape).
- [ ] Record the failure output in the PR body.

### Task 2: The files door test

- [ ] Extend `tests/repo_door.rs`'s `TABLES` with the five files tables (qualifier `files`) and both allowlists with the entries verified above, each carrying its reason. Run all four door tests green.

### Task 3: `BucketRow`, `Page<T>`, and the two bucket projections

- [ ] Unit test `BucketRow::from_record`: `public` as `Number(1)`, `Bool(true)`, `String("true")` → `true`; `Number(0)`, `Bool(false)`, `String("false")` → `false`.
- [ ] Test both projections off one `repo::BucketRow` for each of the three true-shapes: `pages_user::BucketRow` and `pages_admin::AdminBucketRow` agree, and both are `public`.
- [ ] Implement `Page<T>`, `BucketRow`, `from_record`, the two `From` impls; move `pages_user/buckets.rs`, `pages_admin.rs`, `storage/{buckets,access,admin}.rs`. Task 1's test goes green.

### Task 4: `ObjectRow`

- [ ] Test `ObjectRow::from_record` (including a TEXT-stored `size`) and `From<&repo::ObjectRow> for pages_user::ObjectRow`.
- [ ] Implement; move `pages_user/objects.rs`, `storage/{objects,search}.rs`, `quota.rs`'s sweep path.

### Task 5: `ShareRow` and `AccessLogRow`

- [ ] Test `ShareRow::from_record` (absent `expires_at` → `None`, absent/`0`/negative `max_access_count` → `None`) and both share projections.
- [ ] Implement; move `cloud.rs` (including `created_by` off the row), `share.rs`, `pages_user/cloudstorage.rs`, `pages_admin.rs`.

### Task 6: `QuotaRow` and `ViewRow`

- [ ] Move `quota.rs`'s three `quota_from_record` tests onto `QuotaRow::from_record`; test `ViewRow::from_record`.
- [ ] Implement; delete `quota_from_record`; move `quota.rs`, `pages_admin.rs`, `cloud.rs`, `storage/search.rs`.

### Task 7: Verification

- [ ] Full verification list; both snapshots byte-identical; audit script green. PR body carries the B13 red-before output, the response-shape change with its six endpoints, and the door-test allowlist with its reasons.
