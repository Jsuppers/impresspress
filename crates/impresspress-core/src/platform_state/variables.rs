//! `impresspress__admin__variables`: the admin-managed configuration store —
//! one row per config key, the value every block resolves its declared
//! [`wafer_block::ConfigVar`]s against.
//!
//! Two callers, one codec (spec 2.1.2). The boot flavour runs before WRAP
//! over [`DatabaseService`]: native seeds pre-wafer (its immutable crypto
//! service and config snapshot need the variables before `build()`),
//! Cloudflare and the browser seed after admin's `Init` has created the
//! table. The runtime flavour runs under WRAP over [`Context`]: admin's
//! settings surface and the dev sandbox's seed diagnostics. Both decode rows
//! through [`VariableRow::from_record`] and write through
//! [`VariableRow::to_data`], so a column name is spelled in this file only.
//!
//! Before this module existed each platform carried its own copy of the
//! seeders with documented drift between them (the audit's Top-10 #9); the
//! boot functions here are that single copy, moved from the former
//! `boot.rs`.

use std::{collections::HashMap, sync::Arc};

use serde_json::{json, Value};
use wafer_block::db::{Filter, FilterOp, ListOptions};
use wafer_core::{clients::database as db, interfaces::database::service::DatabaseService};
use wafer_run::{context::Context, ErrorCode, WaferError};

use crate::util::RecordExt;

pub const TABLE: &str = "impresspress__admin__variables";

/// One row of the variables table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableRow {
    pub id: String,
    pub key: String,
    pub value: String,
    pub name: String,
    pub description: String,
    pub warning: String,
    /// Masked in every listing and never clearable through the admin UI.
    /// Stored as the integer column `sensitive` (migration 001).
    pub sensitive: bool,
    /// The `{ORG}__{BLOCK}` prefix of the key (migration 002): what
    /// `D1ConfigSource` groups rows by, so a block sees only its own. `None`
    /// for shared (`WAFER_RUN_SHARED__*`) and ad hoc keys, whose column stays
    /// NULL.
    pub block: Option<String>,
    /// The admin user who last wrote the row through the admin surface;
    /// empty for rows the seeders wrote.
    pub updated_by: String,
    pub created_at: String,
    pub updated_at: String,
}

impl VariableRow {
    /// Decode one row. `id` comes from the `Record` envelope (both the WRAP
    /// client's and the service's carry it); `data` is its column map. The
    /// only column that cannot be defaulted is `key`: a keyless row is
    /// corruption, not a variable.
    pub fn from_record(id: &str, data: &HashMap<String, Value>) -> Result<Self, String> {
        let key = data.str_field("key");
        if key.is_empty() {
            return Err(format!("{TABLE} row `{id}` has no key"));
        }
        Ok(Self {
            id: id.to_string(),
            key: key.to_string(),
            value: data.str_field("value").to_string(),
            name: data.str_field("name").to_string(),
            description: data.str_field("description").to_string(),
            warning: data.str_field("warning").to_string(),
            sensitive: data.bool_field("sensitive"),
            block: data.opt_str_field("block").filter(|b| !b.is_empty()),
            updated_by: data.str_field("updated_by").to_string(),
            created_at: data.str_field("created_at").to_string(),
            updated_at: data.str_field("updated_at").to_string(),
        })
    }

    /// The column map this row inserts as. `block` is omitted when `None` so
    /// the column stays NULL rather than becoming an empty string.
    pub fn to_data(&self) -> HashMap<String, Value> {
        let mut data = HashMap::new();
        data.insert("id".to_string(), json!(self.id));
        data.insert("key".to_string(), json!(self.key));
        data.insert("value".to_string(), json!(self.value));
        data.insert("name".to_string(), json!(self.name));
        data.insert("description".to_string(), json!(self.description));
        data.insert("warning".to_string(), json!(self.warning));
        data.insert("sensitive".to_string(), json!(i64::from(self.sensitive)));
        if let Some(block) = &self.block {
            data.insert("block".to_string(), json!(block));
        }
        data.insert("updated_by".to_string(), json!(self.updated_by));
        data.insert("created_at".to_string(), json!(self.created_at));
        data.insert("updated_at".to_string(), json!(self.updated_at));
        data
    }
}

/// The `block` column for a key by migration 002's rule
/// ([`crate::config_vars::key_block_prefix`]): `None` when the key carries no
/// `{ORG}__{BLOCK}__` prefix.
pub fn block_for_key(key: &str) -> Option<String> {
    let block = crate::config_vars::key_block_prefix(key);
    (!block.is_empty()).then_some(block)
}

/// A row to insert. `block` is explicit: env-seeded and admin-created rows
/// derive it from the key ([`block_for_key`]); an auto-generated secret is
/// tagged with the block that declared it
/// ([`crate::config_vars::screaming_block`]), because that is the block
/// `D1ConfigSource` must hand the row to.
#[derive(Debug, Clone)]
pub struct NewVariable {
    pub key: String,
    pub value: String,
    pub name: String,
    pub description: String,
    pub warning: String,
    pub sensitive: bool,
    pub updated_by: String,
    pub block: Option<String>,
}

impl NewVariable {
    /// The row this becomes: a synthesised `var_<uuid>` id and both
    /// timestamps set to now.
    pub fn into_row(self) -> VariableRow {
        let now = crate::util::now_rfc3339();
        VariableRow {
            id: format!("var_{}", uuid::Uuid::new_v4()),
            key: self.key,
            value: self.value,
            name: self.name,
            description: self.description,
            warning: self.warning,
            sensitive: self.sensitive,
            block: self.block,
            updated_by: self.updated_by,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

/// The columns an update may change; `None` leaves the stored value alone.
/// `key`, `id`, `block` and `created_at` are never patched.
#[derive(Debug, Clone, Default)]
pub struct VariablePatch {
    pub value: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub warning: Option<String>,
    pub sensitive: Option<bool>,
    pub updated_by: Option<String>,
}

impl VariablePatch {
    /// The column map for an `update`: the set fields plus `updated_at`.
    fn to_update_data(&self) -> HashMap<String, Value> {
        let mut data = HashMap::new();
        if let Some(value) = &self.value {
            data.insert("value".to_string(), json!(value));
        }
        if let Some(name) = &self.name {
            data.insert("name".to_string(), json!(name));
        }
        if let Some(description) = &self.description {
            data.insert("description".to_string(), json!(description));
        }
        if let Some(warning) = &self.warning {
            data.insert("warning".to_string(), json!(warning));
        }
        if let Some(sensitive) = self.sensitive {
            data.insert("sensitive".to_string(), json!(i64::from(sensitive)));
        }
        if let Some(updated_by) = &self.updated_by {
            data.insert("updated_by".to_string(), json!(updated_by));
        }
        data.insert("updated_at".to_string(), json!(crate::util::now_rfc3339()));
        data
    }

    /// The row to create when the key has none yet: unset fields take the
    /// column defaults, `block` is derived from the key.
    fn into_new(self, key: &str) -> NewVariable {
        NewVariable {
            key: key.to_string(),
            value: self.value.unwrap_or_default(),
            name: self.name.unwrap_or_default(),
            description: self.description.unwrap_or_default(),
            warning: self.warning.unwrap_or_default(),
            sensitive: self.sensitive.unwrap_or(false),
            updated_by: self.updated_by.unwrap_or_default(),
            block: block_for_key(key),
        }
    }
}

fn key_filter(key: &str) -> Filter {
    Filter {
        field: "key".to_string(),
        operator: FilterOp::Equal,
        value: Value::String(key.to_string()),
    }
}

fn decode_error(e: String) -> WaferError {
    WaferError::new(ErrorCode::Internal, e)
}

// ---------------------------------------------------------------------------
// Boot flavour: over `DatabaseService`, before WRAP.
// ---------------------------------------------------------------------------

/// The row for `key`, if any, read the way every boot function reads it.
///
/// `DatabaseService::list` tolerates a missing table (it returns empty), so
/// on a fresh database this is a clean "absent" rather than an error.
async fn find_by_key(
    db: &Arc<dyn DatabaseService>,
    key: &str,
) -> Result<Option<VariableRow>, String> {
    let opts = ListOptions {
        filters: vec![key_filter(key)],
        limit: 1,
        offset: 0,
        skip_count: true,
        ..Default::default()
    };
    let listed = db
        .list(TABLE, &opts)
        .await
        .map_err(|e| format!("list {TABLE} for key `{key}`: {e}"))?;
    listed
        .records
        .first()
        .map(|r| VariableRow::from_record(&r.id, &r.data))
        .transpose()
}

/// `INSERT OR IGNORE` semantics: insert `row` only when no row with its key
/// exists. A pre-existing row (env override, prior boot, admin-UI edit)
/// always wins — seeding never clobbers a stored value.
///
/// Returns `Ok(true)` when a row was inserted, `Ok(false)` when one already
/// existed. Errors bubble up so the caller can decide whether a failed seed
/// is fatal (a missing JWT secret) or merely logged (best-effort secrets).
async fn insert_if_absent(db: &Arc<dyn DatabaseService>, row: VariableRow) -> Result<bool, String> {
    if find_by_key(db, &row.key).await?.is_some() {
        return Ok(false);
    }
    db.create(TABLE, row.to_data())
        .await
        .map_err(|e| format!("insert variable `{}`: {e}", row.key))?;
    Ok(true)
}

/// Seed one variable when absent.
///
/// Public so platform code can seed its own non-declared defaults (the
/// browser's bootstrap-admin credentials and WebLLM script var) through the
/// same `DatabaseService` path. The `block` column is derived from the key
/// ([`block_for_key`]), matching migration 002.
///
/// Returns `Ok(true)` when a row was inserted, `Ok(false)` when one already
/// existed.
pub async fn seed_if_absent(
    db: &Arc<dyn DatabaseService>,
    key: &str,
    value: &str,
    name: &str,
    description: &str,
    sensitive: bool,
) -> Result<bool, String> {
    let row = NewVariable {
        key: key.to_string(),
        value: value.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        warning: String::new(),
        sensitive,
        updated_by: String::new(),
        block: block_for_key(key),
    }
    .into_row();
    insert_if_absent(db, row).await
}

/// Write `value` for `key`, overwriting whatever is stored.
///
/// The counterpart to [`seed_if_absent`], and the one to reach for when a
/// value is a **fact about this deployment** rather than a default an
/// operator may override. `seed_if_absent` cannot express that: by the time
/// a platform's post-admin seed hook runs, the admin block's
/// `lifecycle(Init)` has already written every declared
/// [`crate::config_vars`] default, so "insert if absent" on a declared key is
/// a guaranteed no-op. That is exactly how the browser sandbox's
/// `WAFER_RUN_SHARED__HAS_LANDING_PAGE = "true"` silently lost to the
/// declared `"false"`.
///
/// Only `value` (and `updated_at`) is written on an existing row: `name`,
/// `description` and `sensitive` describe the variable, not the deployment,
/// and an operator's edit to them survives. The metadata arguments are used
/// only when the row has to be created — the same shape [`seed_if_absent`]
/// takes, so the two read alike at a call site.
///
/// Returns `Ok(true)` when the stored value actually changed. A boot that
/// re-asserts a value it already holds performs no write at all, which is
/// what keeps this callable unconditionally on every boot.
pub async fn set(
    db: &Arc<dyn DatabaseService>,
    key: &str,
    value: &str,
    name: &str,
    description: &str,
    sensitive: bool,
) -> Result<bool, String> {
    let Some(existing) = find_by_key(db, key).await? else {
        let row = NewVariable {
            key: key.to_string(),
            value: value.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            warning: String::new(),
            sensitive,
            updated_by: String::new(),
            block: block_for_key(key),
        }
        .into_row();
        db.create(TABLE, row.to_data())
            .await
            .map_err(|e| format!("insert variable `{key}`: {e}"))?;
        return Ok(true);
    };
    if existing.value == value {
        return Ok(false);
    }
    let patch = VariablePatch {
        value: Some(value.to_string()),
        ..Default::default()
    };
    db.update(TABLE, &existing.id, patch.to_update_data())
        .await
        .map_err(|e| format!("update variable `{key}`: {e}"))?;
    Ok(true)
}

/// Auto-generate random 32-byte hex secrets for every [`wafer_block::ConfigVar`]
/// declared with `.auto_generate()` that lacks a row. Shared by all three
/// targets.
///
/// Idempotent: a key that already has a row is left untouched. Per-key
/// failures are logged and tolerated — operators retain the manual seed
/// fallback.
///
/// Ordering contract: this MUST run after the admin block's `lifecycle(Init)`
/// (so migration 002's `block` column exists) and BEFORE
/// [`wafer_run::Wafer::init_all_blocks`] on the targets that seed post-admin
/// (Cloudflare, browser). Native seeds pre-wafer, so it ensures the tables
/// itself first via [`crate::migration_helper::apply_ddl_via_service`].
pub async fn seed_auto_generated(db: &Arc<dyn DatabaseService>) {
    let block_infos = crate::blocks::all_block_infos();
    for info in &block_infos {
        let block_col = crate::config_vars::screaming_block(&info.name);
        for var in &info.config_keys {
            if !var.auto_generate {
                continue;
            }
            match seed_one_secret(db, &block_col, var).await {
                Ok(true) => tracing::warn!(
                    key = %var.key,
                    block = %info.name,
                    "auto-generated secret seeded (no row existed)"
                ),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    key = %var.key,
                    block = %info.name,
                    error = %e,
                    "seed_auto_generated failed"
                ),
            }
        }
    }
}

fn random_hex_secret() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("getrandom: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Generate one 32-byte hex secret and insert it for `var` when absent,
/// tagged with the declaring block's `block` column.
async fn seed_one_secret(
    db: &Arc<dyn DatabaseService>,
    block_col: &str,
    var: &wafer_block::ConfigVar,
) -> Result<bool, String> {
    let row = NewVariable {
        key: var.key.clone(),
        value: random_hex_secret()?,
        name: var.name.clone(),
        description: var.description.clone(),
        warning: var.warning.clone(),
        sensitive: true,
        updated_by: String::new(),
        block: (!block_col.is_empty()).then(|| block_col.to_string()),
    }
    .into_row();
    insert_if_absent(db, row).await
}

/// Seed `env_vars` into the table (`INSERT OR IGNORE`), auto-generate any
/// `auto_generate` secrets, and return the full key→value map currently
/// stored.
///
/// `env_vars` is empty for the browser and Cloudflare targets (their config
/// lives in the platform store, not process env) and carries the
/// declared-key-filtered process environment on native.
///
/// PRECONDITION: the table must already exist — either because the admin
/// block's `lifecycle(Init)` has run (browser, Cloudflare), or because the
/// caller ensured it pre-wafer (native). `db.create` does not lazily create
/// tables.
pub async fn seed_and_load(
    db: &Arc<dyn DatabaseService>,
    env_vars: &[(String, String)],
) -> Result<HashMap<String, String>, String> {
    // 1. Seed env-provided values (existing rows win).
    for (key, value) in env_vars {
        let sensitive = key.ends_with("_SECRET") || key.ends_with("_KEY");
        let row = NewVariable {
            key: key.clone(),
            value: value.clone(),
            name: String::new(),
            description: String::new(),
            warning: String::new(),
            sensitive,
            updated_by: String::new(),
            block: block_for_key(key),
        }
        .into_row();
        if let Err(e) = insert_if_absent(db, row).await {
            tracing::warn!(key = %key, error = %e, "failed to seed env variable");
        }
    }

    // 2. Auto-generate declared secrets (incl. the auth JWT secret).
    seed_auto_generated(db).await;
    seed_jwt_secret(db).await;

    // 3. Load the full set back.
    load_all(db).await
}

/// JWT_SECRET is not declared as an `auto_generate: true` `ConfigVar` by the
/// auth block (a wafer-run config-keys gap noted in the auth block module), so
/// the auto-gen loop above never seeds it. Seed it here so the strict
/// empty-secret boot check (native `server.rs`) can't trip on a fresh DB and
/// the browser/CF crypto can pick up a real key. Idempotent.
async fn seed_jwt_secret(db: &Arc<dyn DatabaseService>) {
    let key = crate::blocks::auth::JWT_SECRET_KEY;
    let secret = match random_hex_secret() {
        Ok(secret) => secret,
        Err(e) => {
            tracing::warn!(error = %e, "getrandom failed for JWT secret");
            return;
        }
    };
    let row = NewVariable {
        key: key.to_string(),
        value: secret,
        name: "JWT signing secret".to_string(),
        description: "256-bit secret used to sign access + refresh JWTs.".to_string(),
        warning: "Rotating this secret invalidates every issued session.".to_string(),
        sensitive: true,
        updated_by: String::new(),
        block: block_for_key(key),
    }
    .into_row();
    match insert_if_absent(db, row).await {
        Ok(true) => {
            tracing::warn!(key = %key, "auto-generated JWT secret (not found in variables table)")
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(key = %key, error = %e, "failed to seed JWT secret"),
    }
}

/// Read every row into a key→value map. A row that does not decode (an empty
/// `key`) is skipped and warned about as corruption rather than silently
/// dropped.
pub async fn load_all(db: &Arc<dyn DatabaseService>) -> Result<HashMap<String, String>, String> {
    let opts = ListOptions {
        offset: 0,
        limit: 100_000,
        skip_count: true,
        ..Default::default()
    };
    let listed = db
        .list(TABLE, &opts)
        .await
        .map_err(|e| format!("load variables from {TABLE}: {e}"))?;
    let mut vars = HashMap::new();
    for record in listed.records {
        match VariableRow::from_record(&record.id, &record.data) {
            Ok(row) => {
                vars.insert(row.key, row.value);
            }
            Err(e) => tracing::warn!(error = %e, "variables table contains an undecodable row"),
        }
    }
    Ok(vars)
}

// ---------------------------------------------------------------------------
// Runtime flavour: over `Context`, under WRAP.
// ---------------------------------------------------------------------------

/// Every row. A row that does not decode is skipped and warned about, the
/// same policy [`load_all`] applies at boot.
pub async fn list_all(ctx: &dyn Context) -> Result<Vec<VariableRow>, WaferError> {
    let records = db::list_all(ctx, TABLE, vec![]).await?;
    Ok(records
        .iter()
        .filter_map(|r| match VariableRow::from_record(&r.id, &r.data) {
            Ok(row) => Some(row),
            Err(e) => {
                tracing::warn!(error = %e, "variables table contains an undecodable row");
                None
            }
        })
        .collect())
}

/// The row for `key`, if any.
pub async fn get_by_key(ctx: &dyn Context, key: &str) -> Result<Option<VariableRow>, WaferError> {
    match db::get_by_field(ctx, TABLE, "key", Value::String(key.to_string())).await {
        Ok(rec) => VariableRow::from_record(&rec.id, &rec.data)
            .map(Some)
            .map_err(decode_error),
        Err(e) if e.code == ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Insert a new row and return it as stored.
pub async fn insert(ctx: &dyn Context, new: NewVariable) -> Result<VariableRow, WaferError> {
    let row = new.into_row();
    let rec = db::create(ctx, TABLE, row.to_data()).await?;
    VariableRow::from_record(&rec.id, &rec.data).map_err(decode_error)
}

/// Update the row for `key`, or create it when absent, and return the row as
/// stored.
///
/// The get-then-write shape rather than the atomic `db::upsert`: the two
/// writes it issues (`update` | `create`) are the ones the Cloudflare KV row
/// cache invalidates, and that cache *refuses* the atomic upsert on this
/// table (`KvCachedD1DatabaseService::upsert`). The create branch derives
/// `block` from the key and synthesises the id and timestamps through the
/// same codec [`insert`] uses.
pub async fn upsert_by_key(
    ctx: &dyn Context,
    key: &str,
    patch: VariablePatch,
) -> Result<VariableRow, WaferError> {
    match get_by_key(ctx, key).await? {
        Some(existing) => {
            let rec = db::update(ctx, TABLE, &existing.id, patch.to_update_data()).await?;
            VariableRow::from_record(&rec.id, &rec.data).map_err(decode_error)
        }
        None => insert(ctx, patch.into_new(key)).await,
    }
}

/// Delete the row with `id`. `NotFound` when there is none.
pub async fn delete(ctx: &dyn Context, id: &str) -> Result<(), WaferError> {
    db::delete(ctx, TABLE, id).await
}

/// Delete the row for `key`, if any. Deleting an absent key affects nothing
/// and is not an error, which is what makes this callable unconditionally.
pub async fn delete_by_key(ctx: &dyn Context, key: &str) -> Result<(), WaferError> {
    db::delete_by_filters(ctx, TABLE, vec![key_filter(key)]).await
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestContext;

    fn new_var(key: &str) -> NewVariable {
        NewVariable {
            key: key.to_string(),
            value: "noreply@example.com".to_string(),
            name: "From address".to_string(),
            description: "Sender of every outbound email".to_string(),
            warning: "Changing this breaks DKIM".to_string(),
            sensitive: true,
            updated_by: "admin_1".to_string(),
            block: block_for_key(key),
        }
    }

    /// The codec is the whole point: every column written by `to_data` comes
    /// back through `from_record` unchanged, on the real admin schema, with
    /// `sensitive` a bool and `block` derived from the key the way migration
    /// 002 backfills it.
    #[tokio::test]
    async fn insert_and_get_by_key_round_trip_every_column() {
        let ctx = TestContext::with_admin().await;
        let inserted = insert(&ctx, new_var("IMPRESSPRESS__EMAIL__FROM"))
            .await
            .expect("insert");
        assert!(inserted.id.starts_with("var_"), "{}", inserted.id);
        assert_eq!(inserted.key, "IMPRESSPRESS__EMAIL__FROM");
        assert_eq!(inserted.value, "noreply@example.com");
        assert_eq!(inserted.name, "From address");
        assert_eq!(inserted.description, "Sender of every outbound email");
        assert_eq!(inserted.warning, "Changing this breaks DKIM");
        assert!(inserted.sensitive);
        assert_eq!(inserted.block.as_deref(), Some("IMPRESSPRESS__EMAIL"));
        assert_eq!(inserted.updated_by, "admin_1");
        assert!(!inserted.created_at.is_empty());
        assert_eq!(inserted.created_at, inserted.updated_at);

        let read = get_by_key(&ctx, "IMPRESSPRESS__EMAIL__FROM")
            .await
            .expect("get")
            .expect("the row exists");
        assert_eq!(read, inserted);

        let again = VariableRow::from_record(&read.id, &read.to_data()).expect("decode");
        assert_eq!(again, read);
    }

    /// A shared or ad hoc key has no block prefix: the column stays NULL,
    /// which is what `D1ConfigSource` relies on to keep it out of every
    /// block's config.
    #[tokio::test]
    async fn a_key_without_a_block_prefix_keeps_the_column_null() {
        let ctx = TestContext::with_admin().await;
        let row = insert(&ctx, new_var("WAFER_RUN_SHARED__APP_NAME"))
            .await
            .expect("insert");
        assert_eq!(row.block, None);
        assert!(
            !row.to_data().contains_key("block"),
            "an absent block must not be written as an empty string"
        );
    }

    #[tokio::test]
    async fn get_by_key_on_an_absent_key_is_none() {
        let ctx = TestContext::with_admin().await;
        assert_eq!(get_by_key(&ctx, "NOPE").await.expect("get"), None);
    }

    #[tokio::test]
    async fn upsert_by_key_creates_then_updates_one_row() {
        let ctx = TestContext::with_admin().await;
        let created = upsert_by_key(
            &ctx,
            "SITE_TAGLINE",
            VariablePatch {
                value: Some("Hello".to_string()),
                description: Some("a fresh key".to_string()),
                updated_by: Some("admin_1".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("create branch");
        assert_eq!(created.value, "Hello");
        assert_eq!(created.description, "a fresh key");
        assert_eq!(
            created.name, "",
            "unset patch fields take the column default"
        );
        assert!(!created.sensitive);

        let updated = upsert_by_key(
            &ctx,
            "SITE_TAGLINE",
            VariablePatch {
                value: Some("Goodbye".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("update branch");
        assert_eq!(
            updated.id, created.id,
            "the update must land on the same row"
        );
        assert_eq!(updated.value, "Goodbye");
        assert_eq!(
            updated.description, "a fresh key",
            "a field the patch leaves unset is preserved"
        );
        assert_eq!(list_all(&ctx).await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn delete_removes_the_row_and_delete_by_key_tolerates_absence() {
        let ctx = TestContext::with_admin().await;
        let row = insert(&ctx, new_var("SITE_MOTTO")).await.expect("insert");
        delete(&ctx, &row.id).await.expect("delete");
        assert_eq!(get_by_key(&ctx, "SITE_MOTTO").await.expect("get"), None);
        delete_by_key(&ctx, "SITE_MOTTO")
            .await
            .expect("deleting an absent key is not an error");
    }

    #[test]
    fn a_record_without_a_key_does_not_decode() {
        let mut data = HashMap::new();
        data.insert("value".to_string(), serde_json::json!("x"));
        let err = VariableRow::from_record("var_1", &data).expect_err("no key");
        assert!(err.contains(TABLE) && err.contains("var_1"), "{err}");
    }

    /// `sensitive` arrives as an integer from SQLite, a bool from Postgres
    /// and a string from a hand-built fixture; the codec reads all three.
    #[test]
    fn sensitive_decodes_from_every_backend_shape() {
        for (shape, want) in [
            (serde_json::json!(1), true),
            (serde_json::json!(0), false),
            (serde_json::json!(true), true),
            (serde_json::json!("1"), true),
            (serde_json::json!("false"), false),
        ] {
            let mut data = HashMap::new();
            data.insert("key".to_string(), serde_json::json!("K"));
            data.insert("sensitive".to_string(), shape.clone());
            let row = VariableRow::from_record("var_1", &data).expect("decode");
            assert_eq!(row.sensitive, want, "{shape}");
        }
    }
}

/// The boot flavour, over [`DatabaseService`]: the tests `boot.rs` carried
/// for `set_variable`, against the moved names.
#[cfg(test)]
mod boot_tests {
    use super::*;

    /// A `DatabaseService` with the admin schema applied through the
    /// pre-wafer DDL runner (`migration_helper::apply_ddl_via_service` +
    /// `blocks::admin::migrations::ddl_files`), the migration-file-runner
    /// exception to the no-raw-SQL rule, so the row shape under test is the
    /// one production writes.
    async fn migrated_db() -> Arc<dyn DatabaseService> {
        let db: Arc<dyn DatabaseService> = Arc::new(
            wafer_block_sqlite::service::SQLiteDatabaseService::open_in_memory()
                .expect("open in-memory sqlite"),
        );
        crate::migration_helper::apply_ddl_via_service(
            &db,
            crate::blocks::admin::migrations::ddl_files("sqlite"),
        )
        .await
        .expect("apply admin migrations");
        db
    }

    async fn value_of(db: &Arc<dyn DatabaseService>, key: &str) -> Option<String> {
        load_all(db).await.expect("load").remove(key)
    }

    /// The bug `set` exists for, stated as a test: a declared default is
    /// already in the table when the platform hook runs, and the platform's
    /// value has to win.
    #[tokio::test]
    async fn a_forced_write_overrides_a_value_seed_if_absent_would_have_kept() {
        let db = migrated_db().await;
        let key = "WAFER_RUN_SHARED__HAS_LANDING_PAGE";

        // What the admin block's `Init` does with the declared default.
        assert!(
            seed_if_absent(&db, key, "false", "Has Landing Page", "declared", false)
                .await
                .expect("seed the declared default"),
            "the declared default is the first writer"
        );

        // What `seed_if_absent` would do from the platform hook — nothing.
        // This is the assertion that makes the fix load-bearing.
        assert!(
            !seed_if_absent(&db, key, "true", "Has Landing Page", "declared", false)
                .await
                .expect("second seed"),
            "seeding cannot beat a row that is already there"
        );
        assert_eq!(value_of(&db, key).await.as_deref(), Some("false"));

        assert!(
            set(&db, key, "true", "Has Landing Page", "declared", false)
                .await
                .expect("force-set"),
            "the value changed, so the row was written"
        );
        assert_eq!(value_of(&db, key).await.as_deref(), Some("true"));
    }

    /// Callable unconditionally on every boot: the second call writes nothing.
    #[tokio::test]
    async fn re_asserting_the_same_value_is_not_a_write() {
        let db = migrated_db().await;
        let key = "WAFER_RUN_SHARED__HAS_LANDING_PAGE";
        assert!(set(&db, key, "true", "n", "d", false)
            .await
            .expect("create"));
        assert!(
            !set(&db, key, "true", "n", "d", false)
                .await
                .expect("re-assert"),
            "an unchanged value must not be written again"
        );
        assert_eq!(value_of(&db, key).await.as_deref(), Some("true"));
    }

    /// On a key with no row at all it creates one, `block` column included —
    /// the same row shape `seed_if_absent` produces.
    #[tokio::test]
    async fn an_absent_key_is_created_with_its_block_column() {
        let db = migrated_db().await;
        let key = "WAFER_RUN__AUTH__PROBE";
        assert!(set(&db, key, "true", "Probe", "d", false)
            .await
            .expect("create"));

        let row = find_by_key(&db, key)
            .await
            .expect("list")
            .expect("exactly one row per key");
        assert_eq!(row.value, "true");
        assert_eq!(row.name, "Probe");
        assert_eq!(row.block.as_deref(), Some("WAFER_RUN__AUTH"));
    }

    /// An operator's edit to the *description* survives a forced value write:
    /// only `value` and `updated_at` are touched.
    #[tokio::test]
    async fn a_forced_write_keeps_the_metadata_the_row_already_had() {
        let db = migrated_db().await;
        let key = "WAFER_RUN_SHARED__HAS_LANDING_PAGE";
        seed_if_absent(
            &db,
            key,
            "false",
            "Has Landing Page",
            "operator wording",
            false,
        )
        .await
        .expect("seed");

        set(
            &db,
            key,
            "true",
            "A Different Name",
            "different wording",
            false,
        )
        .await
        .expect("force-set");

        let row = find_by_key(&db, key).await.expect("list").expect("row");
        assert_eq!(row.value, "true", "the value is the platform's");
        assert_eq!(
            row.description, "operator wording",
            "metadata describes the variable, not the deployment"
        );
        assert_eq!(row.name, "Has Landing Page");
    }

    /// `load_all` skips a row whose key is empty (corruption) rather than
    /// inserting an empty key into the map.
    #[tokio::test]
    async fn load_all_skips_a_keyless_row() {
        let db = migrated_db().await;
        seed_if_absent(&db, "A", "1", "", "", false)
            .await
            .expect("seed");
        let mut keyless = NewVariable {
            key: String::new(),
            value: "orphan".to_string(),
            name: String::new(),
            description: String::new(),
            warning: String::new(),
            sensitive: false,
            updated_by: String::new(),
            block: None,
        }
        .into_row();
        keyless.key = String::new();
        db.create(TABLE, keyless.to_data())
            .await
            .expect("raw create");
        let all = load_all(&db).await.expect("load");
        assert_eq!(all.get("A").map(String::as_str), Some("1"));
        assert_eq!(all.len(), 1, "{all:?}");
    }
}
