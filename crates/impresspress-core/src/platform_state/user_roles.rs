//! `impresspress__admin__user_roles`: role grants beyond a user's initial
//! role — one row per `(user_id, role)`, read by the framework auth block on
//! every login (`get_user_roles` merges them with the inline `users.role`)
//! and managed by admin's IAM surface.
//!
//! Runtime flavour only (spec 2.1.2): nothing reads these rows before WRAP.
//! [`assign`] is the single writer — the login-time admin grant
//! (`ensure_admin_role`) and admin's assign endpoint both go through it, so
//! every row has the same shape. Signup writes no row: the initial role is
//! the inline `users.role`, and a row here means "granted beyond it" (spec
//! 2.2.3).

use std::collections::HashMap;

use serde_json::{json, Value};
use wafer_block::db::{Filter, FilterOp};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, ErrorCode, WaferError};

use crate::util::RecordExt;

pub const TABLE: &str = "impresspress__admin__user_roles";

/// One row of the user_roles table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRoleRow {
    pub id: String,
    pub user_id: String,
    /// The role NAME (`admin/iam.rs` renames cascade here), not a role id.
    pub role: String,
    /// When the grant was made; nullable in the schema.
    pub assigned_at: Option<String>,
    /// The admin who granted it; empty for a grant the system made.
    pub assigned_by: String,
    pub created_at: String,
    pub updated_at: String,
}

impl UserRoleRow {
    /// Decode one row. `user_id` and `role` are required (both `NOT NULL`);
    /// a row without them grants nothing and is refused rather than
    /// defaulted.
    pub fn from_record(id: &str, data: &HashMap<String, Value>) -> Result<Self, String> {
        let user_id = data.str_field("user_id");
        if user_id.is_empty() {
            return Err(format!("{TABLE} row `{id}` has no user_id"));
        }
        let role = data.str_field("role");
        if role.is_empty() {
            return Err(format!("{TABLE} row `{id}` has no role"));
        }
        Ok(Self {
            id: id.to_string(),
            user_id: user_id.to_string(),
            role: role.to_string(),
            assigned_at: data.opt_str_field("assigned_at"),
            assigned_by: data.str_field("assigned_by").to_string(),
            created_at: data.str_field("created_at").to_string(),
            updated_at: data.str_field("updated_at").to_string(),
        })
    }

    /// The column map this row inserts as. `assigned_at` is omitted when
    /// `None` so the nullable column stays NULL.
    pub fn to_data(&self) -> HashMap<String, Value> {
        let mut data = HashMap::new();
        data.insert("id".to_string(), json!(self.id));
        data.insert("user_id".to_string(), json!(self.user_id));
        data.insert("role".to_string(), json!(self.role));
        if let Some(assigned_at) = &self.assigned_at {
            data.insert("assigned_at".to_string(), json!(assigned_at));
        }
        data.insert("assigned_by".to_string(), json!(self.assigned_by));
        data.insert("created_at".to_string(), json!(self.created_at));
        data.insert("updated_at".to_string(), json!(self.updated_at));
        data
    }
}

/// What [`assign`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assigned {
    /// The grant did not exist and was written.
    Created(UserRoleRow),
    /// The user already held the role; nothing was written.
    AlreadyAssigned,
}

fn decode_error(e: String) -> WaferError {
    WaferError::new(ErrorCode::Internal, e)
}

fn eq(field: &str, value: &str) -> Filter {
    Filter {
        field: field.to_string(),
        operator: FilterOp::Equal,
        value: Value::String(value.to_string()),
    }
}

/// List with `filters`, warning about and skipping a row that does not
/// decode — the policy the auth block's role merge and admin's bulk role
/// fetch have always applied to a malformed row.
async fn list_where(
    ctx: &dyn Context,
    filters: Vec<Filter>,
) -> Result<Vec<UserRoleRow>, WaferError> {
    let records = db::list_all(ctx, TABLE, filters).await?;
    Ok(records
        .iter()
        .filter_map(|r| match UserRoleRow::from_record(&r.id, &r.data) {
            Ok(row) => Some(row),
            Err(e) => {
                tracing::warn!(error = %e, "user_roles table contains an undecodable row");
                None
            }
        })
        .collect())
}

/// Every grant `user_id` holds.
pub async fn list_for_user(
    ctx: &dyn Context,
    user_id: &str,
) -> Result<Vec<UserRoleRow>, WaferError> {
    list_where(ctx, vec![eq("user_id", user_id)]).await
}

/// Every grant any of `user_ids` holds, in one `In` query. The bulk lookup
/// behind admin's user list; no users, no query.
pub async fn list_for_users(
    ctx: &dyn Context,
    user_ids: &[&str],
) -> Result<Vec<UserRoleRow>, WaferError> {
    if user_ids.is_empty() {
        return Ok(Vec::new());
    }
    let values: Vec<Value> = user_ids
        .iter()
        .map(|id| Value::String((*id).to_string()))
        .collect();
    list_where(
        ctx,
        vec![Filter {
            field: "user_id".to_string(),
            operator: FilterOp::In,
            value: Value::Array(values),
        }],
    )
    .await
}

/// Every grant.
pub async fn list_all(ctx: &dyn Context) -> Result<Vec<UserRoleRow>, WaferError> {
    list_where(ctx, vec![]).await
}

/// Every grant of `role`, for a rename to carry along.
pub async fn list_by_role(ctx: &dyn Context, role: &str) -> Result<Vec<UserRoleRow>, WaferError> {
    list_where(ctx, vec![eq("role", role)]).await
}

/// The grant with `id`, if any.
pub async fn get(ctx: &dyn Context, id: &str) -> Result<Option<UserRoleRow>, WaferError> {
    match db::get(ctx, TABLE, id).await {
        Ok(rec) => UserRoleRow::from_record(&rec.id, &rec.data)
            .map(Some)
            .map_err(decode_error),
        Err(e) if e.code == ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Grant `role` to `user_id` unless they already hold it. `assigned_by` is
/// the granting admin's id, or empty for a grant the system makes. The
/// single writer for this table.
pub async fn assign(
    ctx: &dyn Context,
    user_id: &str,
    role: &str,
    assigned_by: &str,
) -> Result<Assigned, WaferError> {
    let existing = list_where(ctx, vec![eq("user_id", user_id), eq("role", role)]).await?;
    if !existing.is_empty() {
        return Ok(Assigned::AlreadyAssigned);
    }
    let now = crate::util::now_rfc3339();
    let row = UserRoleRow {
        id: format!("ur_{}", uuid::Uuid::new_v4()),
        user_id: user_id.to_string(),
        role: role.to_string(),
        assigned_at: Some(now.clone()),
        assigned_by: assigned_by.to_string(),
        created_at: now.clone(),
        updated_at: now,
    };
    let rec = db::create(ctx, TABLE, row.to_data()).await?;
    UserRoleRow::from_record(&rec.id, &rec.data)
        .map(Assigned::Created)
        .map_err(decode_error)
}

/// Point the grant with `id` at `new_role` (a role definition was renamed).
pub async fn rename_role(ctx: &dyn Context, id: &str, new_role: &str) -> Result<(), WaferError> {
    let mut data = HashMap::new();
    data.insert("role".to_string(), json!(new_role));
    data.insert("updated_at".to_string(), json!(crate::util::now_rfc3339()));
    db::update(ctx, TABLE, id, data).await.map(|_| ())
}

/// Revoke the grant with `id`. `NotFound` when there is none.
pub async fn remove(ctx: &dyn Context, id: &str) -> Result<(), WaferError> {
    db::delete(ctx, TABLE, id).await
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestContext;

    /// The codec: every column `assign` writes comes back through
    /// `list_for_user`, and a second grant of the same role is reported
    /// rather than duplicated.
    #[tokio::test]
    async fn assign_and_list_for_user_round_trip_and_are_idempotent() {
        let ctx = TestContext::with_admin().await;
        let created = match assign(&ctx, "u-1", "editor", "admin_1")
            .await
            .expect("assign")
        {
            Assigned::Created(row) => row,
            Assigned::AlreadyAssigned => panic!("first grant must create the row"),
        };
        assert!(created.id.starts_with("ur_"), "{}", created.id);
        assert_eq!(created.user_id, "u-1");
        assert_eq!(created.role, "editor");
        assert!(created
            .assigned_at
            .as_deref()
            .is_some_and(|at| !at.is_empty()));
        assert_eq!(created.assigned_by, "admin_1");
        assert!(!created.created_at.is_empty());
        assert_eq!(created.created_at, created.updated_at);

        let rows = list_for_user(&ctx, "u-1").await.expect("list");
        assert_eq!(rows, vec![created.clone()]);

        let again = UserRoleRow::from_record(&created.id, &created.to_data()).expect("decode");
        assert_eq!(again, created);

        assert!(matches!(
            assign(&ctx, "u-1", "editor", "admin_2")
                .await
                .expect("second grant"),
            Assigned::AlreadyAssigned
        ));
        assert_eq!(
            list_for_user(&ctx, "u-1").await.expect("list").len(),
            1,
            "a repeated grant must not add a row"
        );
    }

    /// `ensure_admin_role` grants with no admin behind it; the column keeps
    /// its empty default.
    #[tokio::test]
    async fn assign_by_the_system_leaves_assigned_by_empty() {
        let ctx = TestContext::with_admin().await;
        let Assigned::Created(row) = assign(&ctx, "u-1", "admin", "").await.expect("assign") else {
            panic!("first grant must create the row");
        };
        assert_eq!(row.assigned_by, "");
    }

    /// The bulk lookup behind the admin user list buckets every requested
    /// user in one query, and asks nothing for no users.
    #[tokio::test]
    async fn list_for_users_covers_every_requested_user() {
        let ctx = TestContext::with_admin().await;
        assign(&ctx, "u-1", "editor", "").await.expect("assign");
        assign(&ctx, "u-1", "auditor", "").await.expect("assign");
        assign(&ctx, "u-2", "editor", "").await.expect("assign");
        assign(&ctx, "u-3", "editor", "").await.expect("assign");

        let mut rows = list_for_users(&ctx, &["u-1", "u-2"]).await.expect("list");
        rows.sort_by(|a, b| (&a.user_id, &a.role).cmp(&(&b.user_id, &b.role)));
        let pairs: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.user_id.as_str(), r.role.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![("u-1", "auditor"), ("u-1", "editor"), ("u-2", "editor")]
        );
        assert!(list_for_users(&ctx, &[]).await.expect("empty").is_empty());
    }

    /// A role rename carries every grant naming the old value with it.
    #[tokio::test]
    async fn rename_role_moves_a_grant_and_list_by_role_finds_it() {
        let ctx = TestContext::with_admin().await;
        let Assigned::Created(row) = assign(&ctx, "u-1", "editor", "").await.expect("assign")
        else {
            panic!("first grant must create the row");
        };
        assign(&ctx, "u-2", "viewer", "").await.expect("assign");

        let editors = list_by_role(&ctx, "editor").await.expect("list");
        assert_eq!(editors.len(), 1);
        rename_role(&ctx, &row.id, "editor-v2")
            .await
            .expect("rename");
        assert!(list_by_role(&ctx, "editor").await.expect("list").is_empty());
        let renamed = list_by_role(&ctx, "editor-v2").await.expect("list");
        assert_eq!(renamed.len(), 1);
        assert_eq!(renamed[0].id, row.id);
        assert_eq!(list_all(&ctx).await.expect("all").len(), 2);
    }

    #[tokio::test]
    async fn get_and_remove() {
        let ctx = TestContext::with_admin().await;
        let Assigned::Created(row) = assign(&ctx, "u-1", "editor", "").await.expect("assign")
        else {
            panic!("first grant must create the row");
        };
        assert_eq!(get(&ctx, &row.id).await.expect("get"), Some(row.clone()));
        remove(&ctx, &row.id).await.expect("remove");
        assert_eq!(get(&ctx, &row.id).await.expect("get"), None);
        let err = remove(&ctx, &row.id)
            .await
            .expect_err("removing a gone grant is NotFound");
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[test]
    fn a_record_without_a_user_or_role_does_not_decode() {
        for missing in ["user_id", "role"] {
            let mut data = HashMap::new();
            data.insert("user_id".to_string(), serde_json::json!("u-1"));
            data.insert("role".to_string(), serde_json::json!("editor"));
            data.remove(missing);
            let err = UserRoleRow::from_record("ur_1", &data).expect_err(missing);
            assert!(err.contains(missing) && err.contains("ur_1"), "{err}");
        }
    }
}
