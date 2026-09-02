# Dev Sandbox Plan 0 — wafer-run producer changes

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the five producer-side changes the dev sandbox needs in wafer-run — a `cache_mode` and a `frame_ancestors` knob on two infrastructure blocks, table-scoped schema ops on the database service, JSON host calls for guests that opt in by export, and `$defs`-retaining self-contained schemas plus a selected-endpoint WebMCP projection — each independently testable and behaviour-preserving for existing callers.

**Architecture:** Every change is additive at a documented extension point: `WebConfig`/`SecurityHeadersBlock` gain one config key each; the database service gains four ops registered in all seven places the repo enumerates ops; the wasmi loader gains one guest export (`__wafer_host_codec`) and a transcoder applied at the three points where request bodies and response frames cross the guest boundary; `wafer-core::discovery` keeps cyclic definitions under `$defs` instead of refusing them and offers `generate_webmcp_selected`. Nothing here changes what an existing block, guest or manifest consumer observes unless it opts in.

**Tech Stack:** Rust (wafer-block, wafer-core, wafer-run, wafer-block-web, wafer-block-security-headers, wafer-sql-utils, wafer-schema), rmp-serde, serde_json, schemars 1.2, wasmi, a std-only `wasm32-wasip1` fixture guest.

**Spec:** `impresspress/docs/superpowers/specs/2026-09-02-dev-sandbox-design.md` — §6.3, §6.4, §9.3, §14, §20 (amendments 1–3, 5).

**Repo and base:** `wafer-run`, branch `feat/sandbox-producer` created from `origin/main` (`61e68a0`, which is also impresspress's current pin). The checkout at `../wafer-run` sits on an unrelated branch — create a fresh worktree:

```bash
cd /home/joris/Programs/suppers-ai/workspace/wafer-run
git fetch origin
git worktree add -b feat/sandbox-producer /home/joris/Programs/suppers-ai/wafer-run-worktrees/sandbox-producer origin/main
```

All paths below are relative to that worktree. Line numbers are as of `61e68a0`; re-check them before editing.

## Global Constraints

- **Additive only.** Existing blocks, guests, fixtures and manifest snapshots must behave identically unless they use the new config key, op, export or API. The only intentional snapshot change is in Task 9 (recursive schemas that were refused are now published) — read every changed line.
- **Both checks for schema ops.** A schema op authorizes on `(table, ResourceType::Db, write)` *and* on `wafer_block::wrap::DDL_RESOURCE`. Neither alone admits the call.
- **Fail closed on the wire.** A JSON body that does not parse, an unknown `kind`, or an unsupported default kind is `ErrorCode::InvalidArgument`, never a guess.
- **No raw SQL from a DTO.** The schema DTOs carry no free-form SQL fragment; `DefaultDef` has no `raw` kind.
- **Op registration is seven places** (`ServiceOp` const, `DATABASE_OPS`, `database_action_spec`, handler arm, wire DTO, native client fn, SDK wrapper + `SUPPORTED_DATABASE_OPS`) — the drift tests named in Task 2 fail until all seven are done.
- **Fixtures are built, not committed.** `scripts/build-fixtures.sh` builds every test guest; add the new one there, never check in a `.wasm`.
- Every test is verified load-bearing: revert the behaviour, watch it fail, restore.
- Run `cargo +nightly fmt` and `cargo clippy --workspace --all-targets` before each commit; the repo's pre-commit hook runs `scripts/build-fixtures.sh`.

---

### Task 1: `wafer-run/web` — `cache_mode = "no-cache"`

**Files:**
- Modify: `crates/wafer-block-web/src/lib.rs` (`WebConfig` :135-175, `cache_control` :218-231, `flow_config` :309-349, tests :389-601)

**Interfaces:**
- Consumes: `wafer_block::BlockConfig::str_or` (`crates/wafer-block/src/config.rs:39`).
- Produces: block config key `cache_mode` with values `"normal"` (default) and `"no-cache"`; when `no-cache`, every static response carries `Cache-Control: no-cache`. impresspress Plan 1 sets `{"cache_mode": "no-cache"}` on `wafer-run/web` in the sandbox bundle.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `crates/wafer-block-web/src/lib.rs`:

```rust
    #[test]
    fn cache_control_default_mode_keeps_existing_policy() {
        let cfg = WebConfig::default();
        assert_eq!(cache_control("index.html", "text/html; charset=utf-8", &cfg), "no-cache");
        assert_eq!(
            cache_control("assets/app.js", "application/javascript", &cfg),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control("style.css", "text/css", &cfg), "public, max-age=3600");
    }

    #[test]
    fn cache_control_no_cache_mode_revalidates_everything() {
        let cfg = WebConfig { cache_mode: CacheMode::NoCache, ..WebConfig::default() };
        assert_eq!(cache_control("index.html", "text/html; charset=utf-8", &cfg), "no-cache");
        assert_eq!(cache_control("assets/app.js", "application/javascript", &cfg), "no-cache");
        assert_eq!(cache_control("style.css", "text/css", &cfg), "no-cache");
    }

    #[test]
    fn cache_mode_parses_from_block_config() {
        let event = LifecycleEvent {
            event_type: LifecycleType::Init,
            data: br#"{"cache_mode":"no-cache"}"#.to_vec(),
        };
        let cfg = WebConfig::from_block_config(&BlockConfig::from_event(&event));
        assert_eq!(cfg.cache_mode, CacheMode::NoCache);

        let event = LifecycleEvent {
            event_type: LifecycleType::Init,
            data: br#"{"cache_mode":"sometimes"}"#.to_vec(),
        };
        let cfg = WebConfig::from_block_config(&BlockConfig::from_event(&event));
        assert_eq!(cfg.cache_mode, CacheMode::Normal, "unknown values fall back to the default");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p wafer-block-web cache_`
Expected: compile error — `CacheMode` and the `cache_mode` field do not exist.

- [ ] **Step 3: Implement**

In `crates/wafer-block-web/src/lib.rs`:

```rust
/// How `Cache-Control` is chosen for a served file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheMode {
    /// HTML revalidates, hashed assets are immutable, everything else
    /// gets `cache_max_age`.
    Normal,
    /// Every response is `no-cache`. For a site whose files change under
    /// the visitor's feet — a development sandbox, a local preview of an
    /// export — where an `/assets/` file cached for a year would hide the
    /// edit that was just made.
    NoCache,
}

const DEFAULT_CACHE_MODE: &str = "normal";
```

Add `cache_mode: CacheMode` to `WebConfig`; `Normal` in `Default`; parse in `from_block_config`:

```rust
            cache_mode: match config.str_or("cache_mode", DEFAULT_CACHE_MODE) {
                "no-cache" => CacheMode::NoCache,
                _ => CacheMode::Normal,
            },
```

At the top of `cache_control`:

```rust
    if config.cache_mode == CacheMode::NoCache {
        return "no-cache".to_string();
    }
```

Declare the key beside the others in `flow_config` (the drift test `declared_config_defaults_match_web_config_default` at :480 compares declared defaults with `WebConfig::default()`, so the default string must be `"normal"`):

```rust
            ConfigVar::new(
                "cache_mode",
                "`normal` (HTML revalidates, hashed assets immutable) or `no-cache` \
                 (every file revalidates — for sites edited live).",
                DEFAULT_CACHE_MODE,
            )
            .name("Cache mode"),
```

Extend that drift test's comparison if it enumerates keys explicitly.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p wafer-block-web`
Expected: PASS, including `declared_config_defaults_match_web_config_default`.

- [ ] **Step 5: Commit**

```bash
git add crates/wafer-block-web/src/lib.rs
git commit -m "feat(web): cache_mode=no-cache for live-edited sites"
```

---

### Task 2: `wafer-run/security-headers` — `frame_ancestors`

**Files:**
- Modify: `crates/wafer-block-security-headers/src/lib.rs` (`SecurityHeadersBlock` :36-63, `info` :69-83, `handle` :85-114, `lifecycle` :116-131, tests :260-352)

**Interfaces:**
- Produces: block config key `frame_ancestors` with values `"none"` (default) and `"self"`. `self` emits `frame-ancestors 'self'` in the CSP and `X-Frame-Options: SAMEORIGIN`; `none` keeps today's `'none'` / `DENY`. impresspress Plan 1 sets `self` in the sandbox bundle so `/b/dev` can frame `/`.

- [ ] **Step 1: Write the failing tests**

```rust
    fn init_event(json: &str) -> LifecycleEvent {
        LifecycleEvent { event_type: LifecycleType::Init, data: json.as_bytes().to_vec() }
    }

    #[tokio::test]
    async fn frame_ancestors_self_relaxes_both_headers() {
        let block = SecurityHeadersBlock::new();
        block
            .lifecycle(&NoopCtx, init_event(r#"{"frame_ancestors":"self"}"#))
            .await
            .unwrap();
        let out = block.handle(&NoopCtx, Message::new("retrieve:/"), InputStream::empty()).await;
        let msg = out.into_continue_message().expect("middleware continues");
        assert_eq!(msg.get_meta("resp.header.X-Frame-Options"), "SAMEORIGIN");
        let csp = msg.get_meta("resp.header.Content-Security-Policy");
        assert!(csp.contains("frame-ancestors 'self'"), "{csp}");
        assert!(!csp.contains("frame-ancestors 'none'"), "{csp}");
    }

    #[tokio::test]
    async fn frame_ancestors_defaults_to_none_and_deny() {
        let block = SecurityHeadersBlock::new();
        block.lifecycle(&NoopCtx, init_event(r#"{}"#)).await.unwrap();
        let out = block.handle(&NoopCtx, Message::new("retrieve:/"), InputStream::empty()).await;
        let msg = out.into_continue_message().expect("middleware continues");
        assert_eq!(msg.get_meta("resp.header.X-Frame-Options"), "DENY");
        assert!(msg.get_meta("resp.header.Content-Security-Policy").contains("frame-ancestors 'none'"));
    }

    #[test]
    fn merge_csp_cannot_relax_frame_ancestors_through_the_csp_key() {
        // The knob is `frame_ancestors`, never the operator CSP string.
        let merged = merge_csp(DEFAULT_CSP, "frame-ancestors 'self'");
        assert!(merged.contains("frame-ancestors 'none'"), "{merged}");
    }
```

`NoopCtx` — reuse the test context already in this file's `mod tests`; if none exists, add the smallest `impl Context` stub the crate's other tests use (see `crates/wafer-run/tests/abi_compat.rs:20-48` for the shape). If `OutputStream` has no `into_continue_message`, use the accessor the existing middleware tests in `crates/wafer-block-cors` use to read the continued message.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p wafer-block-security-headers frame_ancestors`
Expected: the `self` test fails on `X-Frame-Options == "DENY"`.

- [ ] **Step 3: Implement**

```rust
/// Who may frame this site's documents. Drives both `frame-ancestors` in
/// the CSP and the legacy `X-Frame-Options` header, so the two can never
/// disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameAncestors {
    None,
    SelfOrigin,
}

impl FrameAncestors {
    fn csp_source(self) -> &'static str {
        match self {
            Self::None => "'none'",
            Self::SelfOrigin => "'self'",
        }
    }
    fn x_frame_options(self) -> &'static str {
        match self {
            Self::None => "DENY",
            Self::SelfOrigin => "SAMEORIGIN",
        }
    }
}
```

Add `frame_ancestors: OnceLock<FrameAncestors>` to the struct (default `None` when unset, via a `fn effective_frame_ancestors(&self) -> FrameAncestors`). In `lifecycle(Init)`:

```rust
            match config.str_or("frame_ancestors", "none") {
                "self" => { let _ = self.frame_ancestors.set(FrameAncestors::SelfOrigin); }
                _ => { let _ = self.frame_ancestors.set(FrameAncestors::None); }
            }
```

In `handle`, replace the hard-coded `DENY` with `self.effective_frame_ancestors().x_frame_options()`, and rewrite the CSP's `frame-ancestors` directive after `effective_csp()`:

```rust
fn with_frame_ancestors(csp: &str, fa: FrameAncestors) -> String {
    csp.split(';')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| {
            if d.starts_with("frame-ancestors") {
                format!("frame-ancestors {}", fa.csp_source())
            } else {
                d.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}
```

Keep `merge_csp` untouched — the operator `csp` string still cannot relax `frame-ancestors`; only the dedicated key can. Declare the key in `info().flow_config` next to `csp`:

```rust
        ConfigVar::new(
            "frame_ancestors",
            "`none` (default: frame-ancestors 'none' + X-Frame-Options DENY) or `self` \
             (same-origin framing allowed: frame-ancestors 'self' + SAMEORIGIN).",
            "none",
        )
        .name("Frame ancestors"),
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p wafer-block-security-headers`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/wafer-block-security-headers/src/lib.rs
git commit -m "feat(security-headers): frame_ancestors=self for same-origin framing"
```

---

### Task 3: Schema-op wire DTOs, op constants, action catalog, client and SDK wrappers

**Files:**
- Modify: `crates/wafer-block/src/wire/database.rs` (append after `ExecRawRequest` :176-184)
- Modify: `crates/wafer-block/src/common/service_names.rs` (consts :35-75, `DATABASE_OPS` :199-218)
- Modify: `crates/wafer-block/src/interfaces.rs` (`database_action_spec` :161-461)
- Modify: `crates/wafer-core/src/clients/database.rs` (after `ddl` :276-297)
- Modify: `sdks/rust/src/clients/database.rs` (after `ddl` :141-143, `SUPPORTED_DATABASE_OPS` :149-168)
- Modify: `crates/wafer-core/tests/handler_wrap_completeness.rs` (`database_op_body` :594-696)

**Interfaces:**
- Produces (wire, `wafer_block::wire::database`):

```rust
pub struct ColumnDef { pub name: String, pub kind: String, pub nullable: bool, pub primary_key: bool,
                       pub auto_increment: bool, pub unique: bool, pub default: Option<DefaultDef> }
pub struct DefaultDef { pub kind: String, pub value: serde_json::Value }
pub struct IndexDef  { pub name: String, pub columns: Vec<String>, pub unique: bool }
pub struct TableDef  { pub name: String, pub columns: Vec<ColumnDef>, pub indexes: Vec<IndexDef>,
                       pub primary_key: Vec<String>, pub unique_keys: Vec<Vec<String>> }
pub struct EnsureTableRequest { pub table: TableDef }
pub struct AddColumnRequest   { pub table: String, pub column: ColumnDef }
pub struct DropTableRequest   { pub table: String }
pub struct TableExistsRequest { pub table: String }
pub struct SchemaOpResponse   { pub table: String }
pub struct TableExistsResponse { pub table: String, pub exists: bool }
```
  `kind` ∈ `string|text|int|int64|float|bool|datetime|json|blob`; `DefaultDef.kind` ∈ `null|now|value`.
- Produces (ops): `ServiceOp::DATABASE_ENSURE_TABLE = "database.ensure_table"`, `DATABASE_ADD_COLUMN = "database.add_column"`, `DATABASE_DROP_TABLE = "database.drop_table"`, `DATABASE_TABLE_EXISTS = "database.table_exists"`.
- Produces (clients): `wafer_core::clients::database::{ensure_table, add_column, drop_table, table_exists}` and the same four in `wafer_sdk::clients::database`.

- [ ] **Step 1: Write the failing drift tests' expectations**

No new test file: the existing drift tests are the RED. Add the four constants to `DATABASE_OPS` first and run:

Run: `cargo test -p wafer-block interfaces::` and `cargo test -p wafer-core --test handler_wrap_completeness`
Expected: `database_action_spec` panics `BUG: no ActionSpec for database op 'database.ensure_table'`; `database_op_body` panics `completeness test has no minimal-body case`.

- [ ] **Step 2: Wire DTOs**

Append to `crates/wafer-block/src/wire/database.rs`, following the module's rules (`wire/mod.rs:1-38`: `#[derive(Debug, Clone, Serialize, Deserialize)]`, `#[serde(default)]` on every optional or collection field, no `deny_unknown_fields`):

```rust
/// One column of a [`TableDef`] / [`AddColumnRequest`].
///
/// `kind` is one of `string`, `text`, `int`, `int64`, `float`, `bool`,
/// `datetime`, `json`, `blob` — the names of `wafer_schema::DataType`,
/// lower-cased. The host maps them; an unknown kind is `InvalidArgument`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default)]
    pub primary_key: bool,
    #[serde(default)]
    pub auto_increment: bool,
    #[serde(default)]
    pub unique: bool,
    #[serde(default)]
    pub default: Option<DefaultDef>,
}

/// A column default. `kind` is `null`, `now`, or `value` (with `value` a
/// JSON string, integer, float or boolean). There is deliberately no raw
/// SQL kind: a schema op never carries a SQL fragment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultDef {
    pub kind: String,
    #[serde(default)]
    pub value: serde_json::Value,
}

/// A secondary index of a [`TableDef`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexDef {
    #[serde(default)]
    pub name: String,
    pub columns: Vec<String>,
    #[serde(default)]
    pub unique: bool,
}

/// A table definition for `database.ensure_table`. Mirrors
/// `wafer_schema::Table` field for field; the host converts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableDef {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    #[serde(default)]
    pub indexes: Vec<IndexDef>,
    #[serde(default)]
    pub primary_key: Vec<String>,
    #[serde(default)]
    pub unique_keys: Vec<Vec<String>>,
}

/// Request for `database.ensure_table` — create the table and its indexes
/// if they do not exist. Authorized on `table.name` and on `__ddl__`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnsureTableRequest {
    pub table: TableDef,
}

/// Request for `database.add_column`. Authorized on `table` and `__ddl__`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddColumnRequest {
    pub table: String,
    pub column: ColumnDef,
}

/// Request for `database.drop_table`. Authorized on `table` and `__ddl__`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DropTableRequest {
    pub table: String,
}

/// Request for `database.table_exists` — a read, authorized on `table` only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableExistsRequest {
    pub table: String,
}

/// Response for `ensure_table`, `add_column` and `drop_table`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaOpResponse {
    pub table: String,
}

/// Response for `database.table_exists`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableExistsResponse {
    pub table: String,
    pub exists: bool,
}
```

- [ ] **Step 3: Constants and family slice**

In `crates/wafer-block/src/common/service_names.rs`, after `DATABASE_DDL` (:59):

```rust
    /// Create a table (and its indexes) if absent — structured, table-scoped DDL.
    pub const DATABASE_ENSURE_TABLE: &str = "database.ensure_table";
    /// Add one column to an existing table — structured, table-scoped DDL.
    pub const DATABASE_ADD_COLUMN: &str = "database.add_column";
    /// Drop a table if present — structured, table-scoped DDL.
    pub const DATABASE_DROP_TABLE: &str = "database.drop_table";
    /// Whether a table exists — a read.
    pub const DATABASE_TABLE_EXISTS: &str = "database.table_exists";
```

and append all four to `DATABASE_OPS` after `Self::DATABASE_DDL`.

- [ ] **Step 4: Action catalog**

In `crates/wafer-block/src/interfaces.rs` `database_action_spec`, before the `other => panic!` arm:

```rust
        ServiceOp::DATABASE_ENSURE_TABLE => ActionSpec {
            description: "Create a table and its indexes if absent (structured DDL; authorized on the table name and __ddl__).".into(),
            message_schema: Some(json!({
                "type": "object",
                "properties": { "table": { "type": "object" } },
                "required": ["table"]
            })),
            response_schema: Some(json!({
                "type": "object",
                "properties": { "table": { "type": "string" } }
            })),
        },
        ServiceOp::DATABASE_ADD_COLUMN => ActionSpec {
            description: "Add a column to an existing table (structured DDL; authorized on the table name and __ddl__).".into(),
            message_schema: Some(json!({
                "type": "object",
                "properties": { "table": { "type": "string" }, "column": { "type": "object" } },
                "required": ["table", "column"]
            })),
            response_schema: Some(json!({
                "type": "object",
                "properties": { "table": { "type": "string" } }
            })),
        },
        ServiceOp::DATABASE_DROP_TABLE => ActionSpec {
            description: "Drop a table if present (structured DDL; authorized on the table name and __ddl__).".into(),
            message_schema: Some(json!({
                "type": "object",
                "properties": { "table": { "type": "string" } },
                "required": ["table"]
            })),
            response_schema: Some(json!({
                "type": "object",
                "properties": { "table": { "type": "string" } }
            })),
        },
        ServiceOp::DATABASE_TABLE_EXISTS => ActionSpec {
            description: "Whether a table exists (read; authorized on the table name).".into(),
            message_schema: Some(json!({
                "type": "object",
                "properties": { "table": { "type": "string" } },
                "required": ["table"]
            })),
            response_schema: Some(json!({
                "type": "object",
                "properties": { "table": { "type": "string" }, "exists": { "type": "boolean" } }
            })),
        },
```

If `ActionSpec` in this file has fields beyond `description`, `message_schema`, `response_schema`, copy the `DATABASE_DDL` arm (:441-457) and fill them the same way.

- [ ] **Step 5: Native client**

In `crates/wafer-core/src/clients/database.rs` after `ddl` (:297), inside the same `impl`/macro block that defines `ddl`:

```rust
    /// Create `table` (and its indexes) if absent. Authorized on the table
    /// name and on `__ddl__`; a block may only ensure its own
    /// `{org}__{block}__*` tables.
    pub fn ensure_table(ctx, table: &TableDef) -> Result<SchemaOpResponse, WaferError> {
        let req = EnsureTableRequest { table: table.clone() };
        let data = svc!(ctx, BLOCK, ServiceOp::DATABASE_ENSURE_TABLE, &req, Some(&table.name), true, Some("db"))?;
        decode(&data)
    }

    /// Add `column` to `table`. Authorized on the table name and `__ddl__`.
    pub fn add_column(ctx, table: &str, column: &ColumnDef) -> Result<SchemaOpResponse, WaferError> {
        let req = AddColumnRequest { table: table.to_string(), column: column.clone() };
        let data = svc!(ctx, BLOCK, ServiceOp::DATABASE_ADD_COLUMN, &req, Some(table), true, Some("db"))?;
        decode(&data)
    }

    /// Drop `table` if present. Authorized on the table name and `__ddl__`.
    pub fn drop_table(ctx, table: &str) -> Result<SchemaOpResponse, WaferError> {
        let req = DropTableRequest { table: table.to_string() };
        let data = svc!(ctx, BLOCK, ServiceOp::DATABASE_DROP_TABLE, &req, Some(table), true, Some("db"))?;
        decode(&data)
    }

    /// Whether `table` exists. A read authorized on the table name.
    pub fn table_exists(ctx, table: &str) -> Result<bool, WaferError> {
        let req = TableExistsRequest { table: table.to_string() };
        let data = svc!(ctx, BLOCK, ServiceOp::DATABASE_TABLE_EXISTS, &req, Some(table), false, Some("db"))?;
        let resp: TableExistsResponse = decode(&data)?;
        Ok(resp.exists)
    }
```

Import the new wire types at the top of the file with the existing `wire::database` imports. The `svc!` resource argument is the *table name* (not `DDL_RESOURCE`) — the host adds the `__ddl__` check itself (Task 4).

- [ ] **Step 6: SDK wrapper**

In `sdks/rust/src/clients/database.rs` after `ddl`:

```rust
/// Buffered: create a table and its indexes if absent (structured DDL,
/// authorized on the table name and `__ddl__`).
pub fn ensure_table(request: &EnsureTableRequest) -> Result<SchemaOpResponse, WaferError> {
    call(BLOCK, ServiceOp::DATABASE_ENSURE_TABLE, request)
}
/// Buffered: add one column to a table.
pub fn add_column(request: &AddColumnRequest) -> Result<SchemaOpResponse, WaferError> {
    call(BLOCK, ServiceOp::DATABASE_ADD_COLUMN, request)
}
/// Buffered: drop a table if present.
pub fn drop_table(request: &DropTableRequest) -> Result<SchemaOpResponse, WaferError> {
    call(BLOCK, ServiceOp::DATABASE_DROP_TABLE, request)
}
/// Buffered: whether a table exists.
pub fn table_exists(request: &TableExistsRequest) -> Result<TableExistsResponse, WaferError> {
    call(BLOCK, ServiceOp::DATABASE_TABLE_EXISTS, request)
}
```

and add the four constants to `SUPPORTED_DATABASE_OPS`.

- [ ] **Step 7: Completeness bodies**

In `crates/wafer-core/tests/handler_wrap_completeness.rs` `database_op_body`, before `other => panic!`:

```rust
        ServiceOp::DATABASE_ENSURE_TABLE => codec::encode(&wire::EnsureTableRequest {
            table: wire::TableDef {
                name: "my_org__auth__widgets".into(),
                columns: vec![wire::ColumnDef {
                    name: "id".into(), kind: "string".into(), nullable: false,
                    primary_key: true, auto_increment: false, unique: false, default: None,
                }],
                indexes: vec![], primary_key: vec![], unique_keys: vec![],
            },
        }),
        ServiceOp::DATABASE_ADD_COLUMN => codec::encode(&wire::AddColumnRequest {
            table: "my_org__auth__widgets".into(),
            column: wire::ColumnDef {
                name: "label".into(), kind: "text".into(), nullable: true,
                primary_key: false, auto_increment: false, unique: false, default: None,
            },
        }),
        ServiceOp::DATABASE_DROP_TABLE => codec::encode(&wire::DropTableRequest {
            table: "my_org__auth__widgets".into(),
        }),
        ServiceOp::DATABASE_TABLE_EXISTS => codec::encode(&wire::TableExistsRequest {
            table: "my_org__auth__widgets".into(),
        }),
```

- [ ] **Step 8: Run the drift tests**

Run: `cargo test -p wafer-block` and `cargo test -p wafer-core --test handler_wrap_completeness` and `cargo test -p wafer-run --test validation_test` and `cargo test --manifest-path sdks/rust/Cargo.toml sdk_covers_every_database_op`
Expected: `wafer-block`, `validation_test`, SDK PASS. `handler_wrap_completeness` now fails on the *handler* (`Unimplemented` for the new ops instead of `PermissionDenied` under `DenyCtx`) — that is Task 4's RED.

- [ ] **Step 9: Commit**

```bash
git add crates/wafer-block/src/wire/database.rs crates/wafer-block/src/common/service_names.rs \
        crates/wafer-block/src/interfaces.rs crates/wafer-core/src/clients/database.rs \
        sdks/rust/src/clients/database.rs crates/wafer-core/tests/handler_wrap_completeness.rs
git commit -m "feat(database): structured schema ops — ensure_table, add_column, drop_table, table_exists (wire + catalog + clients)"
```

---

### Task 4: Schema-op handler arms, authorized on the table and on `__ddl__`

**Files:**
- Create: `crates/wafer-core/src/interfaces/database/schema_wire.rs`
- Modify: `crates/wafer-core/src/interfaces/database/mod.rs` (add `pub(crate) mod schema_wire;`)
- Modify: `crates/wafer-core/src/interfaces/database/handler.rs` (match at :416, before the `Unimplemented` fallback :809)
- Create: `crates/wafer-core/tests/handler_database_schema_ops.rs`

**Interfaces:**
- Consumes: `decode_and_authorize` (`handler_util.rs:166`), `DatabaseService::{ensure_schema_table, schema_add_column, schema_drop_table, schema_table_exists}` (`service.rs:463-480`), `wafer_schema::{Table, Column, Index, DataType, DefaultValue, DefaultVal, default_now, default_null}`, `wafer_block::wrap::DDL_RESOURCE`.
- Produces: `schema_wire::table_from_def(&TableDef) -> Result<Table, WaferError>`, `column_from_def(&ColumnDef) -> Result<Column, WaferError>`; the four handler arms.

- [ ] **Step 1: Write the failing tests**

`crates/wafer-core/tests/handler_database_schema_ops.rs`. Copy the `db_fakes::RecordingDb`, `Calls`, `new_calls`, `msg_without_wrap_meta` and `expect_permission_denied` helpers from `handler_database_wrap_authorization.rs:40-330` into a shared `tests/common/db_fakes.rs` if they are not already shared (then `mod common;` from both files). Add a table-scoped context:

```rust
/// Admits `(my_org__auth__*, Db)` and the `__ddl__` sentinel; denies every
/// other table. The shape a sandboxed guest sees: own tables plus ddl.
struct OwnTablesCtx;

#[wafer_block::wafer_async_trait]
impl Context for OwnTablesCtx {
    // ...the same unimplemented!/default bodies DenyCtx uses for the other
    // methods (copy them verbatim from handler_database_wrap_authorization.rs)...
    fn check_resource_access(
        &self,
        resource: &str,
        resource_type: ResourceType,
        _is_write: bool,
    ) -> Result<(), WaferError> {
        if resource_type == ResourceType::Db
            && (resource.starts_with("my_org__auth__") || resource == wafer_block::wrap::DDL_RESOURCE)
        {
            Ok(())
        } else {
            Err(WaferError::new(ErrorCode::PermissionDenied, format!("denied: {resource}")))
        }
    }
}

/// Admits own tables but NOT `__ddl__` — a guest with `ddl: false`.
struct OwnTablesNoDdlCtx;
// identical, minus the DDL_RESOURCE clause

fn widgets_table() -> wire::TableDef {
    wire::TableDef {
        name: "my_org__auth__widgets".into(),
        columns: vec![
            wire::ColumnDef { name: "id".into(), kind: "string".into(), nullable: false, primary_key: true, auto_increment: false, unique: false, default: None },
            wire::ColumnDef { name: "created_at".into(), kind: "datetime".into(), nullable: false, primary_key: false, auto_increment: false, unique: false,
                default: Some(wire::DefaultDef { kind: "now".into(), value: serde_json::Value::Null }) },
        ],
        indexes: vec![wire::IndexDef { name: String::new(), columns: vec!["created_at".into()], unique: false }],
        primary_key: vec![],
        unique_keys: vec![],
    }
}

#[tokio::test]
async fn ensure_table_on_own_table_reaches_service() {
    let calls = new_calls();
    let svc = db_fakes::RecordingDb::new(calls.clone());
    let body = codec::encode(&wire::EnsureTableRequest { table: widgets_table() }).unwrap();
    let msg = msg_without_wrap_meta(ServiceOp::DATABASE_ENSURE_TABLE);
    let out = handler::handle_message(&svc, &OwnTablesCtx, &msg, &body).await;
    let buf = out.collect_buffered().await.expect("ok");
    let resp: wire::SchemaOpResponse = codec::decode(&buf.body).unwrap();
    assert_eq!(resp.table, "my_org__auth__widgets");
    assert_eq!(calls.lock().unwrap().as_slice(), &["ensure_schema_table"]);
}

#[tokio::test]
async fn ensure_table_on_foreign_table_is_denied_before_service() {
    let calls = new_calls();
    let svc = db_fakes::RecordingDb::new(calls.clone());
    let mut table = widgets_table();
    table.name = "my_org__other__secrets".into();
    let body = codec::encode(&wire::EnsureTableRequest { table }).unwrap();
    let msg = msg_without_wrap_meta(ServiceOp::DATABASE_ENSURE_TABLE);
    let out = handler::handle_message(&svc, &OwnTablesCtx, &msg, &body).await;
    expect_permission_denied(out).await;
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ensure_table_without_ddl_capability_is_denied() {
    let calls = new_calls();
    let svc = db_fakes::RecordingDb::new(calls.clone());
    let body = codec::encode(&wire::EnsureTableRequest { table: widgets_table() }).unwrap();
    let msg = msg_without_wrap_meta(ServiceOp::DATABASE_ENSURE_TABLE);
    let out = handler::handle_message(&svc, &OwnTablesNoDdlCtx, &msg, &body).await;
    expect_permission_denied(out).await;
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn table_exists_is_a_read_that_needs_no_ddl() {
    let calls = new_calls();
    let svc = db_fakes::RecordingDb::new(calls.clone());
    let body = codec::encode(&wire::TableExistsRequest { table: "my_org__auth__widgets".into() }).unwrap();
    let msg = msg_without_wrap_meta(ServiceOp::DATABASE_TABLE_EXISTS);
    let out = handler::handle_message(&svc, &OwnTablesNoDdlCtx, &msg, &body).await;
    let buf = out.collect_buffered().await.expect("ok");
    let resp: wire::TableExistsResponse = codec::decode(&buf.body).unwrap();
    assert_eq!(resp.table, "my_org__auth__widgets");
    assert_eq!(calls.lock().unwrap().as_slice(), &["schema_table_exists"]);
}

#[tokio::test]
async fn unknown_column_kind_is_invalid_argument() {
    let calls = new_calls();
    let svc = db_fakes::RecordingDb::new(calls.clone());
    let mut table = widgets_table();
    table.columns[0].kind = "money".into();
    let body = codec::encode(&wire::EnsureTableRequest { table }).unwrap();
    let msg = msg_without_wrap_meta(ServiceOp::DATABASE_ENSURE_TABLE);
    let out = handler::handle_message(&svc, &OwnTablesCtx, &msg, &body).await;
    let err = out.collect_buffered().await.expect_err("must fail");
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn table_from_def_maps_every_kind_and_default() {
    use wafer_core::interfaces::database::schema_wire::table_from_def;
    let table = table_from_def(&widgets_table()).unwrap();
    assert_eq!(table.name, "my_org__auth__widgets");
    assert_eq!(table.columns[0].data_type, wafer_schema::DataType::String);
    assert!(table.columns[0].primary_key);
    assert_eq!(table.columns[1].data_type, wafer_schema::DataType::DateTime);
    assert!(table.columns[1].default.as_ref().is_some_and(|d| d.raw.contains("CURRENT_TIMESTAMP") || d.is_raw));
    assert_eq!(table.indexes.len(), 1);
    assert_eq!(table.indexes[0].columns, vec!["created_at"]);
}
```

`RecordingDb` must record `"ensure_schema_table"`, `"schema_add_column"`, `"schema_drop_table"`, `"schema_table_exists"` in those four trait methods (add `self.record(...)` lines at `handler_database_wrap_authorization.rs:200-212` if they only return `Ok`). Adjust the `default` assertion to whatever `wafer_schema::default_now()` actually produces (read `wafer-schema/src/types.rs:70-90`) — the test pins the *mapping*, not a SQL string.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p wafer-core --test handler_database_schema_ops`
Expected: `schema_wire` module missing / handler returns `Unimplemented`.

- [ ] **Step 3: Implement `schema_wire.rs`**

```rust
//! Wire DTO → `wafer_schema` conversion for the structured schema ops.
//!
//! `wafer-block` (where the wire types live) does not depend on
//! `wafer-schema`, so the mapping lives host-side. Every unknown name is
//! `InvalidArgument`: a schema op never guesses.

use wafer_block::{wire::database as wire, ErrorCode, WaferError};
use wafer_schema::{default_now, default_null, Column, DataType, DefaultVal, DefaultValue, Index, Table};

fn invalid(msg: String) -> WaferError {
    WaferError::new(ErrorCode::InvalidArgument, msg)
}

pub(crate) fn data_type_from_kind(kind: &str) -> Result<DataType, WaferError> {
    Ok(match kind {
        "string" => DataType::String,
        "text" => DataType::Text,
        "int" => DataType::Int,
        "int64" => DataType::Int64,
        "float" => DataType::Float,
        "bool" => DataType::Bool,
        "datetime" => DataType::DateTime,
        "json" => DataType::Json,
        "blob" => DataType::Blob,
        other => return Err(invalid(format!("unknown column kind `{other}`"))),
    })
}

fn default_from_def(def: &wire::DefaultDef) -> Result<DefaultValue, WaferError> {
    match def.kind.as_str() {
        "null" => Ok(default_null()),
        "now" => Ok(default_now()),
        "value" => {
            let value = match &def.value {
                serde_json::Value::String(s) => DefaultVal::String(s.clone()),
                serde_json::Value::Bool(b) => DefaultVal::Bool(*b),
                serde_json::Value::Number(n) if n.is_i64() => DefaultVal::Int(n.as_i64().unwrap_or_default()),
                serde_json::Value::Number(n) => DefaultVal::Float(n.as_f64().unwrap_or_default()),
                other => return Err(invalid(format!("unsupported default value {other}"))),
            };
            let raw = match &value {
                DefaultVal::String(s) => s.clone(),
                DefaultVal::Int(i) => i.to_string(),
                DefaultVal::Float(f) => f.to_string(),
                DefaultVal::Bool(b) => b.to_string(),
            };
            Ok(DefaultValue { raw, value: Some(value), is_raw: false, is_null: false })
        }
        other => Err(invalid(format!("unknown default kind `{other}`"))),
    }
}

pub(crate) fn column_from_def(def: &wire::ColumnDef) -> Result<Column, WaferError> {
    let mut column = Column::new(def.name.clone(), data_type_from_kind(&def.kind)?);
    column.nullable = def.nullable;
    column.primary_key = def.primary_key;
    column.auto_increment = def.auto_increment;
    column.unique = def.unique;
    column.default = def.default.as_ref().map(default_from_def).transpose()?;
    Ok(column)
}

pub(crate) fn table_from_def(def: &wire::TableDef) -> Result<Table, WaferError> {
    if def.columns.is_empty() {
        return Err(invalid(format!("table `{}` declares no columns", def.name)));
    }
    let mut table = Table::new(def.name.clone());
    table.columns = def.columns.iter().map(column_from_def).collect::<Result<_, _>>()?;
    table.indexes = def
        .indexes
        .iter()
        .map(|i| Index { name: i.name.clone(), columns: i.columns.clone(), unique: i.unique })
        .collect();
    table.primary_key = def.primary_key.clone();
    table.unique_keys = def.unique_keys.clone();
    Ok(table)
}
```

Check `DefaultValue`'s field visibility and `Table::new`'s field defaults against `wafer-schema/src/types.rs:43-53, 329-345`; if `DefaultValue` fields are private, use the crate's constructors (`default_string`, `default_int`, …) instead of struct literals.

- [ ] **Step 4: Handler arms**

In `handler.rs`, add a helper next to `decode_and_authorize` usage and four arms before `_ => Unimplemented`:

```rust
        ServiceOp::DATABASE_ENSURE_TABLE => {
            let req = match decode_and_authorize::<wire::EnsureTableRequest>(
                ctx, body, "database.ensure_table",
                |r| (r.table.name.clone(), ResourceType::Db, true),
            ) { Ok(r) => r, Err(out) => return out };
            if let Err(e) = ctx.check_resource_access(DDL_RESOURCE, ResourceType::Db, true) {
                return OutputStream::error(e);
            }
            let table = match schema_wire::table_from_def(&req.table) {
                Ok(t) => t, Err(e) => return OutputStream::error(e),
            };
            match service.ensure_schema_table(&table).await {
                Ok(()) => to_output(&wire::SchemaOpResponse { table: req.table.name }),
                Err(e) => OutputStream::error(db_error_to_wafer(e)),
            }
        }
        ServiceOp::DATABASE_ADD_COLUMN => {
            let req = match decode_and_authorize::<wire::AddColumnRequest>(
                ctx, body, "database.add_column",
                |r| (r.table.clone(), ResourceType::Db, true),
            ) { Ok(r) => r, Err(out) => return out };
            if let Err(e) = ctx.check_resource_access(DDL_RESOURCE, ResourceType::Db, true) {
                return OutputStream::error(e);
            }
            let column = match schema_wire::column_from_def(&req.column) {
                Ok(c) => c, Err(e) => return OutputStream::error(e),
            };
            match service.schema_add_column(&req.table, &column).await {
                Ok(()) => to_output(&wire::SchemaOpResponse { table: req.table }),
                Err(e) => OutputStream::error(db_error_to_wafer(e)),
            }
        }
        ServiceOp::DATABASE_DROP_TABLE => {
            let req = match decode_and_authorize::<wire::DropTableRequest>(
                ctx, body, "database.drop_table",
                |r| (r.table.clone(), ResourceType::Db, true),
            ) { Ok(r) => r, Err(out) => return out };
            if let Err(e) = ctx.check_resource_access(DDL_RESOURCE, ResourceType::Db, true) {
                return OutputStream::error(e);
            }
            match service.schema_drop_table(&req.table).await {
                Ok(()) => to_output(&wire::SchemaOpResponse { table: req.table }),
                Err(e) => OutputStream::error(db_error_to_wafer(e)),
            }
        }
        ServiceOp::DATABASE_TABLE_EXISTS => {
            let req = match decode_and_authorize::<wire::TableExistsRequest>(
                ctx, body, "database.table_exists",
                |r| (r.table.clone(), ResourceType::Db, false),
            ) { Ok(r) => r, Err(out) => return out };
            match service.schema_table_exists(&req.table).await {
                Ok(exists) => to_output(&wire::TableExistsResponse { table: req.table, exists }),
                Err(e) => OutputStream::error(db_error_to_wafer(e)),
            }
        }
```

`check_resource_access`'s exact signature is at `crates/wafer-run/src/context.rs:489`; match it (it may take `&str, ResourceType, bool` or `&str, &ResourceType, bool`).

- [ ] **Step 5: Run the tests**

Run: `cargo test -p wafer-core --test handler_database_schema_ops --test handler_wrap_completeness` and the sqlite conformance suite `cargo test -p wafer-block-sqlite --test conformance` (unchanged, proves the trait methods still behave).
Expected: PASS.

- [ ] **Step 6: Verify load-bearing**

Comment out the `DDL_RESOURCE` check in `DATABASE_ENSURE_TABLE`; `ensure_table_without_ddl_capability_is_denied` must fail. Restore.

- [ ] **Step 7: Commit**

```bash
git add crates/wafer-core/src/interfaces/database/ crates/wafer-core/tests/
git commit -m "feat(database): schema ops authorized on the table and on __ddl__"
```

---

### Task 5: `__wafer_host_codec` negotiation stored on the host state

**Files:**
- Modify: `crates/wafer-run/src/wasm/wasmi_loader/codec.rs` (after `verify_abi_version` :42)
- Modify: `crates/wafer-run/src/wasm/wasmi_loader/abi.rs` (`WasmiHostState` :47-105, test ctor :355-369)
- Modify: `crates/wafer-run/src/wasm/wasmi_loader/instance.rs` (`instantiate` :52-113)
- Modify: `crates/wafer-block/src/abi.rs` (constants beside `ABI_VERSION_EXPORT` :35)

**Interfaces:**
- Produces: `wafer_block::abi::HOST_CODEC_EXPORT = "__wafer_host_codec"`, `HOST_CODEC_JSON: i32 = 1`, `HOST_CODEC_RMP: i32 = 2`; `pub(super) enum HostCodec { Rmp, Json }` on `WasmiHostState::host_codec`, seeded once per instance in `instantiate`.

- [ ] **Step 1: Write the failing tests**

In `crates/wafer-run/src/wasm/wasmi_loader/mod.rs` `mod tests` (beside the inline-WAT tests at :1084-1160):

```rust
    #[test]
    fn host_codec_defaults_to_rmp_without_the_export() {
        let wat = r#"(module
            (memory (export "memory") 1)
            (func (export "__wafer_alloc") (param i32) (result i32) i32.const 1024)
            (func (export "__wafer_info") (result i64) i64.const 0)
            (func (export "__wafer_handle") (param i32 i32) (result i64) i64.const 0)
        )"#;
        let wasm = wat::parse_str(wat).unwrap();
        let block = WasmiBlock::load_from_bytes(&wasm).unwrap();
        let (store, _inst) = block.instantiate_for_test().unwrap();
        assert_eq!(store.data().host_codec, HostCodec::Rmp);
    }

    #[test]
    fn host_codec_json_is_negotiated_by_export() {
        let wat = r#"(module
            (memory (export "memory") 1)
            (func (export "__wafer_alloc") (param i32) (result i32) i32.const 1024)
            (func (export "__wafer_info") (result i64) i64.const 0)
            (func (export "__wafer_handle") (param i32 i32) (result i64) i64.const 0)
            (func (export "__wafer_host_codec") (result i32) i32.const 1)
        )"#;
        let wasm = wat::parse_str(wat).unwrap();
        let block = WasmiBlock::load_from_bytes(&wasm).unwrap();
        let (store, _inst) = block.instantiate_for_test().unwrap();
        assert_eq!(store.data().host_codec, HostCodec::Json);
    }

    #[test]
    fn host_codec_unknown_value_fails_instantiation() {
        let wat = r#"(module
            (memory (export "memory") 1)
            (func (export "__wafer_alloc") (param i32) (result i32) i32.const 1024)
            (func (export "__wafer_info") (result i64) i64.const 0)
            (func (export "__wafer_handle") (param i32 i32) (result i64) i64.const 0)
            (func (export "__wafer_host_codec") (result i32) i32.const 7)
        )"#;
        let wasm = wat::parse_str(wat).unwrap();
        let block = WasmiBlock::load_from_bytes(&wasm).unwrap();
        let err = block.instantiate_for_test().err().expect("must refuse");
        assert!(err.to_string().contains("__wafer_host_codec"), "{err}");
    }
```

`instantiate_for_test` — a `#[cfg(test)] pub(super) fn` on `WasmiBlock` that calls `instantiate(&self.engine, &self.linker, &self.module, &self.capabilities, self.limits)` with whatever field names the struct at `mod.rs:46-91` uses. Mirror how the existing test at `mod.rs:1164` reaches `instantiate`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p wafer-run --lib wasmi_loader::tests::host_codec`
Expected: compile error, `HostCodec` unknown.

- [ ] **Step 3: Implement**

`crates/wafer-block/src/abi.rs`:

```rust
/// Optional guest export negotiating the *host-call* payload codec:
/// `fn __wafer_host_codec() -> i32`. Absent = MessagePack (every SDK guest);
/// `HOST_CODEC_JSON` = the guest writes JSON request bodies and reads JSON
/// response frames and errors, and the host transcodes. Independent of
/// `__wafer_abi_version`, which negotiates only the handle/lifecycle frames.
pub const HOST_CODEC_EXPORT: &str = "__wafer_host_codec";
pub const HOST_CODEC_JSON: i32 = 1;
pub const HOST_CODEC_RMP: i32 = 2;
```

`codec.rs`:

```rust
/// Codec of host-call payloads, per instance. See `wafer_block::abi::HOST_CODEC_EXPORT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostCodec {
    Rmp,
    Json,
}

/// Call the guest's `__wafer_host_codec` export if present.
pub(super) fn negotiate_host_codec(
    store: &mut Store<WasmiHostState>,
    instance: wasmi::Instance,
) -> Result<HostCodec, RuntimeError> {
    let Ok(f) = instance.get_typed_func::<(), i32>(&*store, wafer_block::abi::HOST_CODEC_EXPORT) else {
        return Ok(HostCodec::Rmp);
    };
    match f.call(&mut *store, ()) {
        Ok(v) if v == wafer_block::abi::HOST_CODEC_JSON => Ok(HostCodec::Json),
        Ok(v) if v == wafer_block::abi::HOST_CODEC_RMP => Ok(HostCodec::Rmp),
        Ok(v) => Err(RuntimeError::Wasm(format!("__wafer_host_codec returned unsupported value {v}"))),
        Err(e) => Err(RuntimeError::Wasm(format!("calling __wafer_host_codec: {e}"))),
    }
}
```

`abi.rs`: add `pub(super) host_codec: HostCodec,` to `WasmiHostState` with a doc comment; `HostCodec::Rmp` in the test constructor at :355. `instance.rs`: initialise `host_codec: HostCodec::Rmp` in the literal, then after the `_start` block and before `Ok((store, instance))`:

```rust
    let codec = negotiate_host_codec(&mut store, instance)?;
    store.data_mut().host_codec = codec;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p wafer-run --lib wasmi_loader`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/wafer-block/src/abi.rs crates/wafer-run/src/wasm/wasmi_loader/
git commit -m "feat(wasmi): negotiate the host-call codec from __wafer_host_codec"
```

---

### Task 6: The JSON⇄MessagePack transcoder

**Files:**
- Create: `crates/wafer-run/src/wasm/wasmi_loader/transcode.rs`
- Modify: `crates/wafer-run/src/wasm/wasmi_loader/mod.rs` (`mod transcode;`)

**Interfaces:**
- Produces: `pub(super) fn json_to_rmp(json: &[u8]) -> Result<Vec<u8>, WaferError>` and `pub(super) fn rmp_to_json(rmp: &[u8]) -> Result<Vec<u8>, WaferError>`; both `InvalidArgument` on malformed input.

- [ ] **Step 1: Write the failing tests** (inside `transcode.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use wafer_block::wire::database as wire;

    #[test]
    fn json_request_decodes_as_the_named_map_dto() {
        let json = br#"{"collection":"site__notes__items","data":{"id":"1","body":[104,105]}}"#;
        let rmp = json_to_rmp(json).unwrap();
        let req: wire::CreateRequest = wafer_block::codec::decode(&rmp).unwrap();
        assert_eq!(req.collection, "site__notes__items");
        assert_eq!(req.data["body"], serde_json::json!([104, 105]));
    }

    #[test]
    fn rmp_response_round_trips_bytes_as_integer_arrays() {
        let resp = wafer_block::wire::storage::GetResponse {
            data: vec![1, 2, 3],
            info: wafer_block::wire::storage::ObjectInfo::default(),
        };
        let rmp = wafer_block::codec::encode(&resp).unwrap();
        let json = rmp_to_json(&rmp).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["data"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn malformed_json_is_invalid_argument() {
        let err = json_to_rmp(b"{not json").unwrap_err();
        assert_eq!(err.code, wafer_block::ErrorCode::InvalidArgument);
    }

    #[test]
    fn malformed_rmp_is_invalid_argument() {
        let err = rmp_to_json(&[0xc1]).unwrap_err();
        assert_eq!(err.code, wafer_block::ErrorCode::InvalidArgument);
    }
}
```

If `ObjectInfo` has no `Default`, construct it field by field from `wire/storage.rs:106-117`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p wafer-run --lib transcode`
Expected: module missing.

- [ ] **Step 3: Implement**

```rust
//! JSON ⇄ MessagePack transcoding for guests that negotiated
//! `HostCodec::Json` (see `wafer_block::abi::HOST_CODEC_EXPORT`).
//!
//! Wire DTOs are MessagePack *named maps* with plain `Vec<u8>` byte fields
//! (no `serde_bytes` in `wafer_block::wire`), so a lossless round trip
//! through `serde_json::Value` exists: bytes are integer arrays on both
//! sides and map keys are strings. Depth is bounded on both decoders.

use wafer_block::{ErrorCode, WaferError};

fn invalid(what: &str, e: impl std::fmt::Display) -> WaferError {
    WaferError::new(ErrorCode::InvalidArgument, format!("{what}: {e}"))
}

pub(super) fn json_to_rmp(json: &[u8]) -> Result<Vec<u8>, WaferError> {
    let value: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| invalid("host-call body is not JSON", e))?;
    rmp_serde::to_vec_named(&value).map_err(|e| invalid("encoding host-call body", e))
}

pub(super) fn rmp_to_json(rmp: &[u8]) -> Result<Vec<u8>, WaferError> {
    let mut de = rmp_serde::Deserializer::from_read_ref(rmp);
    de.set_max_depth(wafer_block::codec::WIRE_MAX_DEPTH);
    let value: serde_json::Value = serde::Deserialize::deserialize(&mut de)
        .map_err(|e| invalid("response frame is not MessagePack", e))?;
    serde_json::to_vec(&value).map_err(|e| invalid("encoding response frame as JSON", e))
}
```

Make `WIRE_MAX_DEPTH` `pub` in `wafer_block::codec` if it is private (`codec.rs:21`). `rmp_serde` is already a dependency of `wafer-run` (check `Cargo.toml`; add it if only `wafer-block` has it).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p wafer-run --lib transcode`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/wafer-run/src/wasm/wasmi_loader/transcode.rs crates/wafer-run/src/wasm/wasmi_loader/mod.rs crates/wafer-block/src/codec.rs
git commit -m "feat(wasmi): JSON<->MessagePack transcoder for host-call payloads"
```

---

### Task 7: Apply the transcoder at the guest boundary; refuse `stream_attach` for JSON guests

**Files:**
- Modify: `crates/wafer-run/src/wasm/wasmi_loader/mod.rs` (resume loop: finish :739-753, read :766-796, take_error :808-830)
- Modify: `crates/wafer-run/src/wasm/wasmi_loader/imports.rs` (`__wafer_host_stream_attach` :212-242)

**Interfaces:**
- Consumes: `HostCodec` (Task 5), `transcode::{json_to_rmp, rmp_to_json}` (Task 6).
- Produces: for a `HostCodec::Json` instance — request bodies are transcoded JSON→rmp before `call_block`; each response frame and `take_error` payload is rmp→JSON; `stream_attach` returns `InvalidArgument`. `HostCodec::Rmp` instances are byte-for-byte unchanged.

- [ ] **Step 1: Write the failing test** — the end-to-end proof is Task 8's fixture; this task's unit test pins the attach refusal with WAT:

```rust
    #[tokio::test]
    async fn json_guest_attach_is_refused() {
        // A JSON-codec guest that calls stream_init then stream_attach and
        // returns the attach status code as its response body.
        let wat = r#"(module
            (import "wafer" "__wafer_host_stream_init" (func $init (param i32 i32 i32 i32) (result i64)))
            (import "wafer" "__wafer_host_stream_attach" (func $attach (param i64 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "wafer-run/config")
            (data (i32.const 32) "{\"kind\":\"config.get\",\"meta\":[]}")
            (func (export "__wafer_alloc") (param i32) (result i32) i32.const 4096)
            (func (export "__wafer_info") (result i64) i64.const 0)
            (func (export "__wafer_host_codec") (result i32) i32.const 1)
            (func (export "__wafer_handle") (param i32 i32) (result i64)
                (local $h i64)
                (local.set $h (call $init (i32.const 0) (i32.const 16) (i32.const 32) (i32.const 31)))
                (drop (call $attach (local.get $h) (i32.const 32) (i32.const 4)))
                i64.const 0)
        )"#;
        // Assert through the loader's stream registry: after the handle call,
        // the stream's last_error is InvalidArgument. Use the same harness the
        // existing inline-WAT tests use (mod.rs:1084-1160) to run __wafer_handle
        // with a MockContext whose call_block is never reached.
        ...
    }
```

Write the assertion with the harness those tests use (`MockContext` from `tests/abi_compat.rs:20-48` or the in-module one); the load-bearing check is that `attach` returned `error_code_to_neg_i32(InvalidArgument)` for a JSON guest and `0` for the same module without the `__wafer_host_codec` export. If asserting the import's return value directly is awkward, have the guest write the i32 into memory at a known offset and read it back through `store` after the call.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p wafer-run --lib json_guest_attach_is_refused`
Expected: attach returns `0` for the JSON guest.

- [ ] **Step 3: Implement — attach**

In `imports.rs` `__wafer_host_stream_attach`, immediately after the handle validation and before decoding the payload:

```rust
                // A JSON-codec guest has no MessagePack encoder; attachments
                // stay a v2/rmp feature. Refuse rather than mis-decode.
                if caller.data().host_codec == HostCodec::Json {
                    return Ok(error_code_to_neg_i32(ErrorCode::InvalidArgument));
                }
```

- [ ] **Step 4: Implement — finish (request body)**

In `mod.rs`, restructure the `Some((Ok((target, msg, body, forged)), attachments))` arm so the transcode result is matched before dispatch. The arm's shape becomes:

```rust
                    Some((Ok((target, msg, body, forged)), attachments)) => {
                        if !forged.is_empty() {
                            self.warn_once_forged_identity(&forged);
                        }
                        let json = scope.store().data().host_codec == HostCodec::Json;
                        let body: Result<Vec<u8>, WaferError> = if json && !body.is_empty() {
                            transcode::json_to_rmp(&body)
                        } else {
                            Ok(body)
                        };
                        match body {
                            Err(e) => {
                                // The guest sent a body its declared codec cannot
                                // carry. Record it so take_error explains, and
                                // resume with the negative code like any failure.
                                let code = e.code;
                                if let Some(state) = scope.store_mut().data_mut().streams.get_mut(handle) {
                                    state.record_error_and_close(e);
                                }
                                error_code_to_neg_i32(code)
                            }
                            Ok(body) => {
                                debug!(block = target, body_len = body.len(), attachments = attachments.len(),
                                       "resolving stream_finish from WASM guest");
                                let input = if body.is_empty() { InputStream::empty() } else { InputStream::from_bytes(body) };
                                let out = if attachments.is_empty() {
                                    ctx.call_block(&target, msg, input).await
                                } else {
                                    ctx.call_block_with_attachments(&target, msg, input, attachments).await
                                };
                                if let Some(state) = scope.store_mut().data_mut().streams.get_mut(handle) {
                                    state.finish_with_stream(out);
                                }
                                0
                            }
                        }
                    }
```

- [ ] **Step 5: Implement — read (response frames)**

In the `pending_stream_read` arm, transcode between `next_chunk` and the allocation:

```rust
                let json = scope.store().data().host_codec == HostCodec::Json;
                let next = match scope.store_mut().data_mut().streams.get_mut(handle) {
                    Some(s) => s.next_chunk().await,
                    None => Err(WaferError::new(ErrorCode::NotFound, "unknown stream handle")),
                };
                let next = match next {
                    Ok(Some(bytes)) if json => match transcode::rmp_to_json(&bytes) {
                        Ok(b) => Ok(Some(b)),
                        Err(e) => {
                            // A frame the callee wrote is not a wire DTO. Fail the
                            // stream rather than hand the guest bytes it cannot read.
                            if let Some(s) = scope.store_mut().data_mut().streams.get_mut(handle) {
                                s.record_error_and_close(e.clone());
                            }
                            Err(e)
                        }
                    },
                    other => other,
                };
                // ...the existing `let resume_packed: i64 = match next { ... }` follows unchanged.
```

- [ ] **Step 6: Implement — take_error**

Replace the unconditional `wafer_block::codec::encode(&err)` at `mod.rs:815` with:

```rust
                        let bytes = match scope.store().data().host_codec {
                            HostCodec::Rmp => wafer_block::codec::encode(&err).map_err(|e| {
                                RuntimeError::Wasm(format!("encoding WaferError for stream_take_error: {e}"))
                            })?,
                            HostCodec::Json => serde_json::to_vec(&err).map_err(|e| {
                                RuntimeError::Wasm(format!("encoding WaferError as JSON for stream_take_error: {e}"))
                            })?,
                        };
```

- [ ] **Step 7: Run every wasmi test**

Run: `scripts/build-fixtures.sh && cargo test -p wafer-run`
Expected: PASS — in particular `attachment_e2e_wasmi`, `service_client_e2e`, `dispatch_streaming`, `wrap_hostile_guest_e2e`, `wasm_instance_pooling`, `abi_negotiation` are unchanged (they are all `HostCodec::Rmp`).

- [ ] **Step 8: Commit**

```bash
git add crates/wafer-run/src/wasm/wasmi_loader/
git commit -m "feat(wasmi): transcode host calls for JSON-codec guests; attach stays rmp-only"
```

---

### Task 8: The std-only JSON-codec fixture guest and its end-to-end test

**Files:**
- Create: `crates/wafer-run/tests/json_host_guest/Cargo.toml`
- Create: `crates/wafer-run/tests/json_host_guest/src/lib.rs`
- Create: `crates/wafer-run/tests/json_host_codec_e2e.rs`
- Modify: `scripts/build-fixtures.sh` (add the fixture)
- Modify: `.gitignore` entries if the other fixtures' `target/` dirs are listed individually

**Interfaces:**
- Consumes: the host imports (`imports.rs:35-430`), `HostCodec::Json` negotiation (Task 5), the schema ops (Task 4).
- Produces: `crates/wafer-run/tests/json_host_guest/target/wasm32-wasip1/release/json_host_guest.wasm` — a guest with **no dependencies** that exercises `database.ensure_table`, `database.create`, `database.get`, `storage.put`/`get` and `config.get` over JSON. This guest is the compatibility fixture impresspress Plan 3's `wafer_guest.rs` is tested against.

- [ ] **Step 1: The guest crate**

`Cargo.toml`:

```toml
[workspace]

[package]
name = "json-host-guest"
version = "0.0.0"
edition = "2021"
publish = false

[lib]
crate-type = ["cdylib"]
path = "src/lib.rs"

[dependencies]

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
```

`src/lib.rs` — no JSON parser: every request body is a static string, and the guest returns the *raw* frames it received so the host-side test parses them:

```rust
//! JSON-codec fixture guest. No crates: this is the smallest guest a
//! dependency-free toolchain (Rubrc) can build, and it proves the host
//! transcodes for `__wafer_host_codec() == 1`.
//!
//! Operations, selected by the request `kind` (read from the JSON frame by
//! substring match — the frame is `[{"kind":"…","meta":[…]}, [bytes]]`):
//!   test.roundtrip — ensure_table, create, get; body = the `get` frame (JSON)
//!   test.storage   — storage.put then storage.get; body = the get frame
//!   test.config    — config.get; body = the frame
//!   test.error     — database.get of a missing id; body = the take_error JSON
//!   test.attach    — stream_attach; body = "attach=<code>"

#[link(wasm_import_module = "wafer")]
extern "C" {
    fn __wafer_host_stream_init(name_ptr: i32, name_len: i32, msg_ptr: i32, msg_len: i32) -> i64;
    fn __wafer_host_stream_write_chunk(handle: i64, ptr: i32, len: i32) -> i32;
    fn __wafer_host_stream_attach(handle: i64, ptr: i32, len: i32) -> i32;
    fn __wafer_host_stream_finish(handle: i64) -> i32;
    fn __wafer_host_stream_read_chunk(handle: i64) -> i64;
    fn __wafer_host_stream_take_error(handle: i64) -> i64;
    fn __wafer_host_stream_close(handle: i64);
}

const INFO: &str = r#"{
  "name":"test/json-host-guest","version":"0.0.0","interface":"handler@v1",
  "summary":"JSON host-codec fixture",
  "requires":["wafer-run/database","wafer-run/storage","wafer-run/config"],
  "capabilities":{"collections":{"Only":["site__jhg__notes"]},"ddl":true,
    "storage_folders":{"Only":["site/jhg"]},"config":{"Only":["SITE__JHG__GREETING"]},
    "callable_blocks":{"Only":["wafer-run/database","wafer-run/storage","wafer-run/config"]}}
}"#;

fn pack(bytes: &[u8]) -> i64 {
    ((bytes.as_ptr() as u32 as i64) << 32) | bytes.len() as i64
}
fn unpack(packed: i64) -> &'static [u8] {
    let ptr = (packed >> 32) as u32 as *const u8;
    let len = (packed & 0xffff_ffff) as usize;
    unsafe { std::slice::from_raw_parts(ptr, len) }
}
fn leak(s: String) -> &'static [u8] { Box::leak(s.into_boxed_str()).as_bytes() }

#[no_mangle]
pub extern "C" fn __wafer_alloc(size: i32) -> i32 {
    let v = vec![0u8; size.max(0) as usize].into_boxed_slice();
    Box::leak(v).as_mut_ptr() as i32
}
#[no_mangle]
pub extern "C" fn __wafer_host_codec() -> i32 { 1 }
#[no_mangle]
pub extern "C" fn __wafer_info() -> i64 { pack(INFO.as_bytes()) }
#[no_mangle]
pub extern "C" fn __wafer_lifecycle(_p: i32, _l: i32) -> i64 { pack(br#"{"Ok":null}"#) }

/// One buffered host call. Returns (status, concatenated frames, error json).
fn call(target: &str, kind: &str, body: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let msg = format!(r#"{{"kind":"{kind}","meta":[]}}"#);
    unsafe {
        let h = __wafer_host_stream_init(
            target.as_ptr() as i32, target.len() as i32, msg.as_ptr() as i32, msg.len() as i32);
        if h < 0 { return (h as i32, Vec::new(), Vec::new()); }
        if !body.is_empty() {
            __wafer_host_stream_write_chunk(h, body.as_ptr() as i32, body.len() as i32);
        }
        let status = __wafer_host_stream_finish(h);
        let mut frames = Vec::new();
        if status == 0 {
            loop {
                let packed = __wafer_host_stream_read_chunk(h);
                if packed == 0 { break; }
                if packed < 0 { break; }
                frames.extend_from_slice(unpack(packed));
            }
        }
        let err_packed = __wafer_host_stream_take_error(h);
        let err = if err_packed > 0 { unpack(err_packed).to_vec() } else { Vec::new() };
        __wafer_host_stream_close(h);
        (status, frames, err)
    }
}

fn respond(body: &[u8], content_type: &str) -> i64 {
    let data: Vec<String> = body.iter().map(|b| b.to_string()).collect();
    let out = format!(
        r#"{{"action":"Respond","response":{{"data":[{}],"meta":[{{"key":"resp.content_type","value":"{content_type}"}}]}},"error":null,"message":null}}"#,
        data.join(",")
    );
    pack(leak(out))
}

#[no_mangle]
pub extern "C" fn __wafer_handle(ptr: i32, len: i32) -> i64 {
    let frame = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let text = String::from_utf8_lossy(frame);
    if text.contains("test.roundtrip") {
        let table = r#"{"table":{"name":"site__jhg__notes","columns":[{"name":"id","kind":"string","primary_key":true},{"name":"body","kind":"text","nullable":true}]}}"#;
        let (s1, _, e1) = call("wafer-run/database", "database.ensure_table", table);
        if s1 != 0 { return respond(&e1, "application/json"); }
        let (s2, _, e2) = call("wafer-run/database", "database.create",
            r#"{"collection":"site__jhg__notes","data":{"id":"n1","body":"hello"}}"#);
        if s2 != 0 { return respond(&e2, "application/json"); }
        let (_, frames, _) = call("wafer-run/database", "database.get",
            r#"{"collection":"site__jhg__notes","id":"n1"}"#);
        return respond(&frames, "application/json");
    }
    if text.contains("test.storage") {
        let _ = call("wafer-run/storage", "storage.put",
            r#"{"folder":"site/jhg","key":"a.txt","data":[104,105],"content_type":"text/plain"}"#);
        let (_, frames, _) = call("wafer-run/storage", "storage.get", r#"{"folder":"site/jhg","key":"a.txt"}"#);
        return respond(&frames, "application/json");
    }
    if text.contains("test.config") {
        let (_, frames, _) = call("wafer-run/config", "config.get", r#"{"key":"SITE__JHG__GREETING"}"#);
        return respond(&frames, "application/json");
    }
    if text.contains("test.error") {
        let (_, _, err) = call("wafer-run/database", "database.get",
            r#"{"collection":"site__jhg__notes","id":"missing"}"#);
        return respond(&err, "application/json");
    }
    if text.contains("test.attach") {
        let target = "wafer-run/config";
        let msg = r#"{"kind":"config.get","meta":[]}"#;
        let code = unsafe {
            let h = __wafer_host_stream_init(target.as_ptr() as i32, target.len() as i32, msg.as_ptr() as i32, msg.len() as i32);
            let c = __wafer_host_stream_attach(h, msg.as_ptr() as i32, 4);
            __wafer_host_stream_close(h);
            c
        };
        return respond(format!("attach={code}").as_bytes(), "text/plain");
    }
    respond(b"unknown", "text/plain")
}
```

Check the exact field names of `storage.put`/`storage.get`/`config.get` requests against `crates/wafer-block/src/wire/storage.rs:10-55` and `wire/config.rs`, and whether the `capabilities` JSON shape above matches `BlockCapabilities`' serde form (`capabilities.rs:117-161` and the `Allowlist` serde at :32) — write the JSON the way `serde_json::to_string(&BlockCapabilities{…})` prints it.

- [ ] **Step 2: Register the fixture**

In `scripts/build-fixtures.sh`, after the existing `build_fixture` calls:

```bash
build_fixture \
    crates/wafer-run/tests/json_host_guest/target/wasm32-wasip1/release/json_host_guest.wasm \
    crates/wafer-run/tests/json_host_guest/Cargo.toml \
    crates/wafer-run/tests/json_host_guest/target/wasm32-wasip1/release/json_host_guest.wasm
```

Run it: `scripts/build-fixtures.sh` — the guest must build with **no** `[dependencies]`.

- [ ] **Step 3: Write the failing e2e test**

`crates/wafer-run/tests/json_host_codec_e2e.rs`, using `build_wafer_with_real_db` from `wrap_hostile_guest_e2e.rs:73-96` (copy it; register `wafer-run/storage` with an in-memory storage block and `wafer-run/config` with a static config the same way that file or `service_client_e2e.rs:64-108` does):

```rust
fn guest_wasm() -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"),
        "/tests/json_host_guest/target/wasm32-wasip1/release/json_host_guest.wasm");
    std::fs::read(path).unwrap_or_else(|e| panic!("run scripts/build-fixtures.sh first: {e}"))
}

async fn run(kind: &str) -> wafer_block::BufferedResponse {
    let mut wafer = build_wafer_with_real_db_storage_and_config(&[("SITE__JHG__GREETING", "hi")]).await;
    let block = WasmiBlock::load_from_bytes(&guest_wasm()).expect("load");
    wafer.register_block("test/json-host-guest", Arc::new(block)).unwrap();
    let wafer = wafer.start().await.unwrap();
    wafer.run_block("test/json-host-guest", Message::new(kind), InputStream::empty())
        .await.collect_buffered().await.expect("buffered")
}

#[tokio::test]
async fn json_guest_creates_its_table_and_reads_back_a_record() {
    let out = run("test.roundtrip").await;
    let v: serde_json::Value = serde_json::from_slice(&out.body).expect("get frame is JSON");
    assert_eq!(v["id"], "n1");
    assert_eq!(v["data"]["body"], "hello");
}

#[tokio::test]
async fn json_guest_storage_round_trip() {
    let out = run("test.storage").await;
    let v: serde_json::Value = serde_json::from_slice(&out.body).unwrap();
    assert_eq!(v["data"], serde_json::json!([104, 105]));
}

#[tokio::test]
async fn json_guest_reads_config() {
    let out = run("test.config").await;
    let v: serde_json::Value = serde_json::from_slice(&out.body).unwrap();
    assert_eq!(v["value"], "hi");
}

#[tokio::test]
async fn json_guest_receives_errors_as_json() {
    let out = run("test.error").await;
    let v: serde_json::Value = serde_json::from_slice(&out.body).unwrap();
    assert_eq!(v["code"], "NotFound");
}

#[tokio::test]
async fn json_guest_cannot_attach() {
    let out = run("test.attach").await;
    let code = wafer_block::ErrorCode::InvalidArgument;
    assert_eq!(String::from_utf8_lossy(&out.body), format!("attach={}", -(code as i32)));
}

#[tokio::test]
async fn json_guest_cannot_touch_another_table() {
    // Same guest, but the host loads it with capabilities that exclude its table.
    let mut wafer = build_wafer_with_real_db_storage_and_config(&[]).await;
    let caps = BlockCapabilities { collections: Allowlist::Only(vec!["site__other__t".into()]), ddl: true, ..BlockCapabilities::none() };
    let block = WasmiBlock::load_with_capabilities(&guest_wasm(), caps).unwrap();
    wafer.register_block("test/json-host-guest", Arc::new(block)).unwrap();
    let wafer = wafer.start().await.unwrap();
    let out = wafer.run_block("test/json-host-guest", Message::new("test.roundtrip"), InputStream::empty())
        .await.collect_buffered().await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.body).unwrap();
    assert_eq!(v["code"], "PermissionDenied");
}
```

Match the `ErrorCode` serde spelling (`"NotFound"` vs `"not_found"`) to `core_types.rs`. The `record.get` JSON shape (`id` + `data` map) is `wire::Record` at `wire/database.rs:402`.

- [ ] **Step 4: Run to verify failure, then pass**

Run: `cargo test -p wafer-run --test json_host_codec_e2e`
Expected before Task 7: the `get` frame is MessagePack bytes → `serde_json::from_slice` fails. After Tasks 5–7: PASS.

- [ ] **Step 5: Wire CI**

`.github/workflows/ci.yml` and `ci-main.yml` already trigger on `scripts/build-fixtures.sh`; confirm the job that runs the script (`ci-jobs.yml:81` area) picks up the new fixture with no change. If fixtures are cached by path, add the new `target/` path to the cache list.

- [ ] **Step 6: Commit**

```bash
git add crates/wafer-run/tests/json_host_guest crates/wafer-run/tests/json_host_codec_e2e.rs scripts/build-fixtures.sh
git commit -m "test(wasmi): std-only JSON-codec guest proves DB/storage/config over JSON"
```

---

### Task 8b: Raw body frames reach JSON guests untranscoded; storage folder capabilities are prefixes

*(Added during execution from Task 8's findings C1/C2 — see the Plan 0 ledger.)*

**Files:**
- Modify: `crates/wafer-block/src/streams/output.rs` (or wherever `OutputStream`/`StreamEvent` are defined) — an explicit raw-body marker
- Modify: `crates/wafer-core/src/interfaces/storage/handler.rs` (`storage.get`, `storage.get_streaming`) and the network handler's streamed body if it uses the same header+raw-body shape
- Modify: `crates/wafer-run/src/wasm/stream.rs` (`StreamState` remembers raw mode) and `crates/wafer-run/src/wasm/wasmi_loader/mod.rs` (read arm honours it)
- Modify: `crates/wafer-block/src/capabilities.rs` (`allows_storage_folder`)
- Modify: `crates/wafer-run/tests/json_host_guest/src/lib.rs`, `crates/wafer-run/tests/json_host_codec_e2e.rs`

**Interfaces:**
- Produces: a stream-level, explicit signal — `StreamEvent::Meta` carrying the key `frame.encoding` with value `raw` (constant `wafer_block::stream::FRAME_ENCODING_META` / `FRAME_ENCODING_RAW`) emitted by a handler immediately before it starts writing raw body chunks. `StreamState` records `raw_frames = true` when it sees it; the wasmi read arm skips transcoding for a JSON guest while `raw_frames` is set. Frames before the marker (the `ObjectInfo` header) are still transcoded. No content sniffing anywhere.
- Produces: `BlockCapabilities::allows_storage_folder(resource)` admits `resource` when an `Only` entry equals it OR is a proper prefix followed by `/` (`"site/jhg"` admits `"site/jhg/a.txt"` and `"site/jhg/sub/b"`, never `"site/jhgx/..."`); `Any`/`None` unchanged. Document the field as folders-or-object-paths.
- Produces: the fixture's `test.storage` returns `header_json + "\n" + body_bytes` so the test asserts both the transcoded `ObjectInfo` and the raw `[104,105]` body; the guest's `capabilities.storage_folders` is `Only(["<its folder>"])` (a folder, not an object path).

- [ ] **Step 1: Failing tests.** (a) `capabilities.rs` unit tests: folder prefix admits nested keys, rejects sibling-prefix collisions, exact object path still admits itself. (b) `json_host_codec_e2e::json_guest_storage_round_trip` now asserts the header decodes as JSON (`content_type == "text/plain"`, `size == 2`) AND the body bytes equal `[104, 105]`; the guest's declared `storage_folders` is the folder. (c) a `stream.rs` unit test: a `Meta(frame.encoding=raw)` event flips `raw_frames` and later chunks are yielded unchanged by `next_chunk`. Run: they fail (InvalidArgument on the body frame; folder capability denied).
- [ ] **Step 2: Implement** the marker constant + emission in the storage handler(s) (and the network streamed-body handler if it shares the shape — check `crates/wafer-core/src/interfaces/network/handler.rs`), the `StreamState` flag (`next_chunk` observes `Meta` events instead of skipping them silently, sets the flag, and keeps skipping them as frames), the read-arm condition (`if json && !bytes.is_empty() && !raw`), and the prefix semantics. Native clients that already concatenate raw body frames are unaffected because they never saw `Meta` frames as data.
- [ ] **Step 3: Run** `scripts/build-fixtures.sh && cargo test -p wafer-run && cargo test -p wafer-core --test handler_storage_wrap_authorization` plus `cargo test -p wafer-block`. Expected: PASS; existing storage/network e2e suites unchanged.
- [ ] **Step 4: Commit** `fix(wasmi): raw body frames pass through to JSON guests via an explicit stream marker; storage folder capabilities are prefixes`.

---

### Task 9: Keep cyclic definitions under `$defs` instead of refusing them

**Files:**
- Modify: `crates/wafer-core/src/discovery.rs` — `inline_refs` :181, `RefIssues` :203, `RefWalk` :224-330, `resolve` :331-380, `FLATTENABLE_KEYWORDS` :443, `merge_schema_source` :609, `AgentInputSchema` :679, `agent_input_schema` :846, `agent_output_schema` :1067, `WebMcpRefusal` :1470 (+ `Display` :1616), `generate_webmcp_report` :1913, tests :2312+ (the recursion tests at :2602, :2709, :3427, :3965, :4449, :5055, :5091, and the exhaustive table at :5722; snapshots :4076/:4109/:4154)

**Interfaces:**
- Consumes: schemars 1.2 output where the producer keeps `$defs` (`wafer-block/src/types/endpoint.rs:440`).
- Produces: `inline_refs(schema) -> (Value, RefIssues)` where the `Value` may carry a root `$defs` table holding exactly the definitions that are referenced cyclically (every other definition is still inlined); `RefIssues { unresolved, oversized }` (no `recursive`); `WebMcpRefusal::{RecursiveSchema, OutputSchemaRecursive}` removed, `WebMcpRefusal::CollidingDefinitions { names }` added; input schemas hoist per-source `$defs` into one root table; output schemas keep theirs.

- [ ] **Step 1: Invert the recursion tests** (RED)

Rewrite these tests to assert retention. Two representative rewrites; apply the same shape to :3427, :3965, :4449, :5055, :5091 and delete the `RecursiveSchema` / `OutputSchemaRecursive` rows from :5722.

```rust
    #[test]
    fn inline_refs_keeps_a_self_referential_definition_under_defs() {
        let schema = json!({
            "$defs": { "Node": { "type": "object", "properties": {
                "children": { "type": "array", "items": { "$ref": "#/$defs/Node" } } } } },
            "type": "object",
            "properties": { "root": { "$ref": "#/$defs/Node" } }
        });
        let (out, issues) = inline_refs(&schema);
        assert_eq!(issues, RefIssues::default());
        // The first reference is inlined; the cycle inside it is a `$ref`
        // back to the kept definition.
        assert_eq!(out["properties"]["root"]["type"], "object");
        assert_eq!(
            out["properties"]["root"]["properties"]["children"]["items"],
            json!({ "$ref": "#/$defs/Node" })
        );
        assert_eq!(
            out["$defs"]["Node"]["properties"]["children"]["items"],
            json!({ "$ref": "#/$defs/Node" })
        );
        assert!(out["$defs"].as_object().unwrap().len() == 1, "only the cyclic def is kept: {out}");
    }

    #[test]
    fn inline_refs_rebases_root_recursion_to_a_named_definition() {
        let schema = json!({
            "title": "Condition",
            "type": "object",
            "properties": { "all": { "type": "array", "items": { "$ref": "#" } } }
        });
        let (out, issues) = inline_refs(&schema);
        assert_eq!(issues, RefIssues::default());
        assert_eq!(out["properties"]["all"]["items"], json!({ "$ref": "#/$defs/Condition" }));
        assert_eq!(out["$defs"]["Condition"]["properties"]["all"]["items"], json!({ "$ref": "#/$defs/Condition" }));
        assert!(out["$defs"]["Condition"].get("$defs").is_none(), "kept defs carry no nested table");
    }

    #[test]
    fn webmcp_publishes_an_endpoint_with_a_recursive_body_and_keeps_defs() {
        let blocks = vec![BlockInfo::new("test/block", "1.0.0", "http-handler@v1", "Test").endpoints(vec![
            BlockEndpoint::post("/b/test/offers")
                .auth(AuthLevel::Public)
                .input_schema(json!({
                    "type": "object",
                    "properties": { "name": { "type": "string" }, "condition": { "$ref": "#/$defs/Condition" } },
                    "$defs": { "Condition": { "type": "object", "properties": {
                        "all": { "type": "array", "items": { "$ref": "#/$defs/Condition" } } } } }
                }))
                .agent_tool("create_offer", "Create an offer."),
        ])];
        let (doc, refused) = generate_webmcp_report(&blocks, AuthLevel::Public, |_b, ep| ep.auth);
        assert!(refused.is_empty(), "{refused:?}");
        let tool = &doc["tools"][0];
        assert_eq!(tool["name"], "create_offer");
        assert_eq!(tool["inputSchema"]["properties"]["condition"]["type"], "object");
        assert_eq!(tool["inputSchema"]["$defs"]["Condition"]["properties"]["all"]["items"], json!({ "$ref": "#/$defs/Condition" }));
        assert_eq!(tool["invocation"]["body_params"], json!(["condition", "name"]));
    }

    #[test]
    fn webmcp_refuses_two_sources_defining_the_same_name_differently() {
        let blocks = vec![BlockInfo::new("test/block", "1.0.0", "http-handler@v1", "Test").endpoints(vec![
            BlockEndpoint::post("/b/test/x")
                .auth(AuthLevel::Public)
                .query_params_schema(json!({ "type": "object", "properties": { "q": { "$ref": "#/$defs/T" } },
                    "$defs": { "T": { "type": "object", "properties": { "n": { "$ref": "#/$defs/T" } } } } }))
                .input_schema(json!({ "type": "object", "properties": { "b": { "$ref": "#/$defs/T" } },
                    "$defs": { "T": { "type": "object", "properties": { "m": { "$ref": "#/$defs/T" } } } } }))
                .agent_tool("x", "x"),
        ])];
        let (doc, refused) = generate_webmcp_report(&blocks, AuthLevel::Public, |_b, ep| ep.auth);
        assert!(doc["tools"].as_array().unwrap().is_empty());
        assert!(matches!(refused[0].reason, WebMcpRefusal::CollidingDefinitions { ref names } if names == &["T".to_string()]));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p wafer-core --lib discovery::tests`
Expected: the new tests fail (cycle is cut to `{}`, `RecursiveSchema` refused); the deleted variants still compile.

- [ ] **Step 3: Implement `RefWalk` retention**

Replace `RefIssues.recursive` with nothing (delete the field and every `recursive` read), and extend `RefWalk`:

```rust
struct RefWalk<'a> {
    defs: &'a Value,
    active: Vec<String>,
    issues: RefIssues,
    emitted: usize,
    /// Definitions that closed a cycle and are therefore kept under the
    /// output's `$defs` instead of inlined. Filled when a frame that is
    /// `active` is referenced again; the body is stored when that frame
    /// finishes resolving.
    kept: std::collections::BTreeMap<String, Value>,
    /// Names whose bodies must be captured when their frame completes.
    cyclic: std::collections::BTreeSet<String>,
    /// The name the document root is known by when it references itself.
    root_name: String,
    root_recursive: bool,
}
```

`resolve_ref_target` becomes:

```rust
    fn resolve_ref_target(&mut self, reference: &str) -> Value {
        if reference == "#" {
            self.root_recursive = true;
            return json!({ "$ref": format!("#/$defs/{}", self.root_name) });
        }
        let Some(name) = reference.strip_prefix("#/$defs/").and_then(decode_ref_name) else {
            self.issues.unresolved = true;
            return json!({});
        };
        let Some(target) = self.defs.get(name.as_str()) else {
            self.issues.unresolved = true;
            return json!({});
        };
        if self.active.contains(&name) {
            // Cycle: refer back to the definition and keep it.
            self.cyclic.insert(name.clone());
            return json!({ "$ref": format!("#/$defs/{}", encode_ref_name(&name)) });
        }
        let target = target.clone();
        self.active.push(name.clone());
        let resolved = self.resolve(&target);
        self.active.pop();
        if self.cyclic.contains(&name) {
            self.kept.entry(name).or_insert_with(|| resolved.clone());
        }
        resolved
    }
```

`encode_ref_name` is the inverse of `decode_ref_name` (:143): `~` → `~0`, `/` → `~1`, then percent-encode anything outside unreserved characters. `inline_refs`:

```rust
fn inline_refs(schema: &Value) -> (Value, RefIssues) {
    let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
    let root_name = schema.get("title").and_then(Value::as_str)
        .filter(|t| !t.is_empty()).unwrap_or("Root").to_string();
    let mut walk = RefWalk { defs: &defs, active: Vec::new(), issues: RefIssues::default(), emitted: 0,
        kept: Default::default(), cyclic: Default::default(), root_name: root_name.clone(), root_recursive: false };
    let mut resolved = walk.resolve(schema);
    let mut kept = walk.kept;
    if walk.root_recursive {
        let mut body = resolved.clone();
        if let Some(map) = body.as_object_mut() { map.remove("$defs"); }
        kept.insert(root_name, body);
    }
    if !kept.is_empty() {
        if let Some(map) = resolved.as_object_mut() {
            map.insert("$defs".into(), Value::Object(kept.into_iter().collect()));
        }
    }
    (resolved, walk.issues)
}
```

The root-recursion `kept` body must be computed **after** `resolved` is final and with its own `$defs` stripped (a kept definition never nests a table). `resolve` still drops a literal `$defs` key from every object it walks (:356-358, :372-374) — that stays: the *output* table is added once at the root by `inline_refs`.

- [ ] **Step 4: Hoist per-source tables in the input schema**

`merge_schema_source` gains `defs: &mut serde_json::Map<String, Value>` and `MergedSource` gains `colliding_defs: Vec<String>`; after inlining:

```rust
    if let Some(table) = inlined.get("$defs").and_then(Value::as_object) {
        for (name, body) in table {
            match defs.get(name) {
                Some(existing) if existing != body => colliding_defs.push(name.clone()),
                Some(_) => {}
                None => { defs.insert(name.clone(), body.clone()); }
            }
        }
    }
```

Add `"$defs"` to `FLATTENABLE_KEYWORDS` so `source_is_flattenable` accepts it. In `agent_input_schema`, pass one shared `defs` map through all three `merge_schema_source` calls, collect `colliding_defs` into `AgentInputSchema.colliding_defs: Vec<String>`, and when building `schema` insert `"$defs": defs` if non-empty. Delete `recursive_refs` from `AgentInputSchema` and the `RecursiveSchema` refusal branch in `generate_webmcp_report`; add, next to the `CollidingParameterNames` branch:

```rust
            if !input.colliding_defs.is_empty() {
                refuse(Scope::Tool, WebMcpRefusal::CollidingDefinitions { names: input.colliding_defs.clone() });
                continue;
            }
```

with the variant:

```rust
    /// Two of path/query/body carry a `$defs` entry of the same name with
    /// different bodies. One flat schema has one `$defs` table, so the tool
    /// could describe only one of them.
    CollidingDefinitions { names: Vec<String> },
```

and its `Display` arm. `agent_output_schema`: drop the `recursive → OutputSchemaRecursive` branch; the output schema keeps its `$defs` (it is one source, no hoisting needed).

- [ ] **Step 5: Run the discovery tests and read the snapshot diffs**

Run: `cargo test -p wafer-core --lib discovery`
Expected: the three snapshot tests (:4076/:4109/:4154) fail only if the fixture blocks contain a recursive type — if they do, every changed line must be a formerly-refused tool now published with a `$defs` table. Update the snapshot deliberately. Every other test PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/wafer-core/src/discovery.rs
git commit -m "feat(discovery)!: keep cyclic definitions under \$defs instead of refusing recursive schemas"
```

---

### Task 10: OpenAPI — hoist `$defs` into `components/schemas`

**Files:**
- Modify: `crates/wafer-core/src/discovery.rs` (`generate_openapi` :1218-1335)

**Interfaces:**
- Produces: `/openapi.json` where every schema's `$defs` entries live under `components.schemas` and refs read `#/components/schemas/<Name>`; two different definitions sharing a name are disambiguated as `<Name>` and `<Name>_<8-hex sha256 of the body>`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn openapi_hoists_defs_into_components_and_rewrites_refs() {
        let blocks = vec![BlockInfo::new("test/block", "1.0.0", "http-handler@v1", "Test").endpoints(vec![
            BlockEndpoint::post("/b/test/offers").auth(AuthLevel::Public).input_schema(json!({
                "type": "object",
                "properties": { "condition": { "$ref": "#/$defs/Condition" } },
                "$defs": { "Condition": { "type": "object", "properties": {
                    "all": { "type": "array", "items": { "$ref": "#/$defs/Condition" } } } } }
            })),
        ])];
        let doc = generate_openapi(&blocks, "t", "t", "https://x.test");
        let schema = &doc["paths"]["/b/test/offers"]["post"]["requestBody"]["content"]["application/json"]["schema"];
        assert!(schema.get("$defs").is_none(), "{schema}");
        assert_eq!(schema["properties"]["condition"], json!({ "$ref": "#/components/schemas/Condition" }));
        assert_eq!(
            doc["components"]["schemas"]["Condition"]["properties"]["all"]["items"],
            json!({ "$ref": "#/components/schemas/Condition" })
        );
        let text = doc.to_string();
        assert!(!text.contains("#/$defs/"), "no dangling local refs: {text}");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p wafer-core --lib openapi_hoists_defs`
Expected: `$defs` still present under the request body schema.

- [ ] **Step 3: Implement**

```rust
/// Move a schema's `$defs` into `components` and rewrite `#/$defs/X` to
/// `#/components/schemas/X`. Same-named definitions with identical bodies
/// share one entry; different bodies get a content-hash suffix.
///
/// Two passes: decide every name first (bodies are compared *unrewritten*,
/// so the decision does not depend on rewrite order), then rewrite the root
/// and every hoisted body with the final rename map.
fn hoist_defs_into_components(
    schema: &Value,
    raw: &mut std::collections::BTreeMap<String, Value>,   // unrewritten bodies, for comparison
    components: &mut serde_json::Map<String, Value>,        // rewritten bodies, published
) -> Value {
    let table: Vec<(String, Value)> = schema
        .get("$defs")
        .and_then(Value::as_object)
        .map(|t| t.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();

    // Pass 1: names. `pending` remembers the unrewritten body we compared
    // against so a later duplicate in the same schema compares equal.
    let mut renames: std::collections::BTreeMap<String, String> = Default::default();
    let mut pending: Vec<(String, Value)> = Vec::new();
    for (name, body) in &table {
        let target = match raw.get(name).or_else(|| pending.iter().find(|(n, _)| n == name).map(|(_, b)| b)) {
            None => name.clone(),
            Some(existing) if existing == body => name.clone(),
            Some(_) => format!("{name}_{}", short_sha256(&body.to_string())),
        };
        renames.insert(name.clone(), target.clone());
        pending.push((target, body.clone()));
    }

    // Pass 2: rewrite with the complete map, then publish.
    for (target, body) in pending {
        if !raw.contains_key(&target) {
            components.insert(target.clone(), rewrite_local_refs(&body, &renames));
            raw.insert(target, body);
        }
    }
    let mut out = rewrite_local_refs(schema, &renames);
    if let Some(map) = out.as_object_mut() {
        map.remove("$defs");
    }
    out
}

fn short_sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

fn rewrite_local_refs(node: &Value, renames: &std::collections::BTreeMap<String, String>) -> Value {
    match node {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    if k == "$ref" {
                        if let Some(name) = v
                            .as_str()
                            .and_then(|r| r.strip_prefix("#/$defs/"))
                            .and_then(decode_ref_name)
                        {
                            let target = renames.get(&name).cloned().unwrap_or(name);
                            return (k.clone(), json!(format!("#/components/schemas/{target}")));
                        }
                    }
                    (k.clone(), rewrite_local_refs(v, renames))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| rewrite_local_refs(v, renames)).collect()),
        other => other.clone(),
    }
}
```

`generate_openapi` owns both maps for the duration of one document build and passes them to every call. Apply `hoist_defs_into_components` to `input`, `pp`, `qp` and `output` where `generate_openapi` embeds them (:1252-1290), and merge the map into the existing `"components"` object (:1325) as `"schemas"`. If `wafer-core` does not already depend on `sha2`, add it (it is already in the workspace lock).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p wafer-core --lib discovery`
Expected: PASS; any openapi snapshot in this crate's tests changes only by moving `$defs` — read the diff.

- [ ] **Step 5: Commit**

```bash
git add crates/wafer-core/src/discovery.rs
git commit -m "feat(discovery): hoist \$defs into components/schemas in the OpenAPI document"
```

---

### Task 11: `generate_webmcp_selected` — page-scoped projection of an endpoint allowlist

**Files:**
- Modify: `crates/wafer-core/src/discovery.rs` (after `generate_webmcp_report` :1913-2305)

**Interfaces:**
- Produces:

```rust
/// One endpoint a consumer wants projected as a tool even though the
/// block did not annotate it with `agent_tool`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSelection {
    pub block: String,
    pub method: HttpMethod,
    pub path: String,
    pub name: String,
    pub description: String,
}

pub fn generate_webmcp_selected(
    blocks: &[BlockInfo],
    caller: AuthLevel,
    effective_auth: impl Fn(&BlockInfo, &BlockEndpoint) -> AuthLevel,
    selections: &[ToolSelection],
) -> (Value, Vec<WebMcpRefusalReport>)
```
  and `WebMcpRefusal::SelectionNotFound` (Scope::Tool) for a selection naming no declared endpoint. impresspress Plan 2 calls this for `/b/dev/api/tools.json`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn selected_projection_names_and_describes_unannotated_endpoints() {
        let blocks = webmcp_fixture_blocks();
        // Pick an endpoint from the fixture that is NOT an agent tool.
        let (block, ep) = blocks.iter().flat_map(|b| b.endpoints.iter().map(move |e| (b, e)))
            .find(|(_, e)| !e.is_agent_tool() && e.input_schema.is_some())
            .expect("fixture has an unannotated typed endpoint");
        let selections = vec![ToolSelection {
            block: block.name.clone(), method: ep.method, path: ep.path.clone(),
            name: "shop_do_thing".into(), description: "Do the thing.".into(),
        }];
        let (doc, refused) = generate_webmcp_selected(&blocks, AuthLevel::Admin, |_b, e| e.auth, &selections);
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(tool_names(&doc), vec!["shop_do_thing".to_string()]);
        assert_eq!(doc["tools"][0]["description"], "Do the thing.");
        assert_eq!(doc["tools"][0]["invocation"]["path"], json!(ep.path));
    }

    #[test]
    fn selected_projection_ignores_the_blocks_own_annotations() {
        let blocks = webmcp_fixture_blocks();
        let (doc, _) = generate_webmcp_selected(&blocks, AuthLevel::Admin, |_b, e| e.auth, &[]);
        assert!(doc["tools"].as_array().unwrap().is_empty(), "nothing selected, nothing published: {doc}");
    }

    #[test]
    fn selected_projection_reports_a_missing_endpoint() {
        let blocks = webmcp_fixture_blocks();
        let selections = vec![ToolSelection {
            block: "impresspress/products".into(), method: HttpMethod::Delete,
            path: "/b/products/no/such/path".into(), name: "x".into(), description: "x".into(),
        }];
        let (doc, refused) = generate_webmcp_selected(&blocks, AuthLevel::Admin, |_b, e| e.auth, &selections);
        assert!(doc["tools"].as_array().unwrap().is_empty());
        assert_eq!(refused.len(), 1);
        assert!(matches!(refused[0].reason, WebMcpRefusal::SelectionNotFound));
        assert_eq!(refused[0].tool_name, "x");
    }

    #[test]
    fn selected_projection_still_filters_by_caller_level() {
        let blocks = webmcp_fixture_blocks();
        let (block, ep) = blocks.iter().flat_map(|b| b.endpoints.iter().map(move |e| (b, e)))
            .find(|(_, e)| e.auth == AuthLevel::Admin).expect("fixture has an admin endpoint");
        let selections = vec![ToolSelection { block: block.name.clone(), method: ep.method, path: ep.path.clone(),
            name: "admin_thing".into(), description: "x".into() }];
        let (doc, _) = generate_webmcp_selected(&blocks, AuthLevel::Public, |_b, e| e.auth, &selections);
        assert!(doc["tools"].as_array().unwrap().is_empty(), "a Public caller sees no Admin tool");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p wafer-core --lib selected_projection`
Expected: `ToolSelection` unknown.

- [ ] **Step 3: Implement**

```rust
pub fn generate_webmcp_selected(
    blocks: &[BlockInfo],
    caller: AuthLevel,
    effective_auth: impl Fn(&BlockInfo, &BlockEndpoint) -> AuthLevel,
    selections: &[ToolSelection],
) -> (Value, Vec<WebMcpRefusalReport>) {
    // Build a shadow block set: every endpoint loses its own annotation and
    // only selected endpoints gain one. `generate_webmcp_report` then does
    // exactly what it does for the global manifest — name validation,
    // per-manifest duplicate detection, auth filtering, schema walls.
    let mut shadow: Vec<BlockInfo> = blocks
        .iter()
        .map(|b| {
            let mut b = b.clone();
            for ep in &mut b.endpoints {
                ep.agent_tool = None;
            }
            b
        })
        .collect();
    let mut not_found = Vec::new();
    for sel in selections {
        let hit = shadow
            .iter_mut()
            .find(|b| b.name == sel.block)
            .and_then(|b| b.endpoints.iter_mut().find(|ep| ep.method == sel.method && ep.path == sel.path));
        match hit {
            Some(ep) => {
                ep.agent_tool = Some(AgentTool { name: sel.name.clone(), description: sel.description.clone() });
            }
            None => not_found.push(WebMcpRefusalReport {
                block: sel.block.clone(),
                method: sel.method,
                path: sel.path.clone(),
                tool_name: sel.name.clone(),
                scope: WebMcpRefusalScope::Tool,
                reason: WebMcpRefusal::SelectionNotFound,
                visible_to_caller: true,
            }),
        }
    }
    let (doc, mut refused) = generate_webmcp_report(&shadow, caller, effective_auth);
    refused.extend(not_found);
    (doc, refused)
}
```

`BlockEndpoint.agent_tool` must be assignable from `wafer-core` — it is `pub` at `endpoint.rs:136`; `AgentTool`'s fields are `pub` at :64. If `WebMcpRefusalReport` is constructed only inside `discovery.rs` its fields are reachable here. Add `SelectionNotFound` to the enum and `Display`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p wafer-core --lib discovery`
Expected: PASS, including the exhaustive-reason test at :5722 once `SelectionNotFound` is added to its table.

- [ ] **Step 5: Commit**

```bash
git add crates/wafer-core/src/discovery.rs
git commit -m "feat(discovery): generate_webmcp_selected projects an explicit endpoint allowlist"
```

---

### Task 12: Whole-repo verification and PR

- [ ] **Step 1: Full suite**

Run, from the worktree root:

```bash
scripts/build-fixtures.sh
cargo +nightly fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
cargo test --manifest-path sdks/rust/Cargo.toml
```

Expected: all PASS. The only snapshot changes are the ones read in Tasks 9 and 10.

- [ ] **Step 2: Inspector parity**

Run: `cargo test -p wafer-block-inspector`
Expected: PASS — `webmcp_view` calls `generate_webmcp_report` unchanged; recursive tools now count as published, not refused.

- [ ] **Step 3: Open the PR**

```bash
git push -u origin feat/sandbox-producer
gh pr create --title "Producer changes for the ImpressPress dev sandbox" --body-file - <<'EOF'
Five additive changes consumed by impresspress's dev-sandbox plans:

1. `wafer-run/web` `cache_mode = "no-cache"` for live-edited sites.
2. `wafer-run/security-headers` `frame_ancestors = "self"` (CSP + X-Frame-Options together).
3. `database.ensure_table` / `add_column` / `drop_table` / `table_exists` — structured DDL authorized on the table name and `__ddl__`.
4. `__wafer_host_codec` guest export: JSON host-call payloads for dependency-free guests (std-only fixture included).
5. `$defs`-retaining self-contained schemas (WebMCP) + `components/schemas` hoist (OpenAPI) + `generate_webmcp_selected`.

Spec: impresspress `docs/superpowers/specs/2026-09-02-dev-sandbox-design.md` §14, §20.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
```

Record the merge SHA — impresspress Plan 1 Task 1 pins it.

## Self-review notes

- Spec §14 items 1–4 map to Tasks 5–8 (JSON), 3–4 (DDL), 1 (cache), 9–11 (schemas/projection); amendment 3 (framing) is Task 2.
- Names used downstream: `cache_mode`, `frame_ancestors`, `database.ensure_table|add_column|drop_table|table_exists`, `wire::database::{TableDef, ColumnDef, IndexDef, DefaultDef, EnsureTableRequest, AddColumnRequest, DropTableRequest, TableExistsRequest, SchemaOpResponse, TableExistsResponse}`, `__wafer_host_codec` (1 = JSON), `generate_webmcp_selected` + `ToolSelection`, `WebMcpRefusal::{CollidingDefinitions, SelectionNotFound}`. Plans 1–4 use exactly these.
