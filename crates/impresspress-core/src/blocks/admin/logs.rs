use wafer_block::db::{Filter, FilterOp, SortField};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, Message, OutputStream};

use super::contracts::{AdminAuditLogListQuery, AdminAuditLogListResponse};
use crate::http::{err_internal, ok_json};

/// Audit log entries (admin-initiated mutations).
pub(crate) const AUDIT_LOGS_TABLE: &str = "impresspress__admin__audit_logs";

/// Storage access log entries (one row per object read/write).
pub(crate) const STORAGE_ACCESS_LOGS_TABLE: &str = "impresspress__admin__storage_access_logs";

/// `GET /b/admin/api/logs`.
pub(super) async fn handle_list(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let query = AdminAuditLogListQuery::from_message(msg);

    let mut filters = Vec::new();
    if let Some(user_id) = query.user_id {
        filters.push(Filter {
            field: "user_id".to_string(),
            operator: FilterOp::Equal,
            value: serde_json::Value::String(user_id),
        });
    }
    if let Some(action_filter) = query.action {
        filters.push(Filter {
            field: "action".to_string(),
            operator: FilterOp::Equal,
            value: serde_json::Value::String(action_filter),
        });
    }
    if let Some(resource) = query.resource {
        filters.push(Filter {
            field: "resource".to_string(),
            operator: FilterOp::Like,
            value: serde_json::Value::String(format!("%{resource}%")),
        });
    }

    let sort = vec![SortField {
        field: "created_at".to_string(),
        desc: true,
    }];

    match db::paginated_list(
        ctx,
        AUDIT_LOGS_TABLE,
        i64::from(query.page),
        i64::from(query.page_size),
        filters,
        sort,
    )
    .await
    {
        Ok(result) => ok_json(&AdminAuditLogListResponse::from_record_list(&result)),
        Err(e) => err_internal("Database error", e),
    }
}

// ---------------------------------------------------------------------------
// Audit log helper
// ---------------------------------------------------------------------------

/// Record an admin action in the audit_logs table.
/// Fire-and-forget: errors are logged but don't block the caller.
pub async fn audit_log(
    ctx: &dyn Context,
    user_id: &str,
    action: &str,
    resource: &str,
    ip_address: &str,
) {
    let mut data = std::collections::HashMap::new();
    data.insert("user_id".to_string(), serde_json::json!(user_id));
    data.insert("action".to_string(), serde_json::json!(action));
    data.insert("resource".to_string(), serde_json::json!(resource));
    data.insert("ip_address".to_string(), serde_json::json!(ip_address));
    crate::util::stamp_created(&mut data);

    if let Err(e) = db::create(ctx, AUDIT_LOGS_TABLE, data).await {
        tracing::warn!(action, resource, "audit_log write failed: {}", e.message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{admin_msg, output_json, TestContext};

    /// The audit-log list publishes exactly `AdminAuditLogView`'s fields, and
    /// the `{records, total_count, page, page_size}` envelope the untyped
    /// `RecordList` response already had.
    #[tokio::test]
    async fn list_publishes_exactly_the_contract_fields() {
        let ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");
        audit_log(&ctx, "admin-1", "user.delete", "users/u-9", "203.0.113.7").await;

        let body =
            output_json(handle_list(&ctx, &admin_msg("retrieve", "/b/admin/api/logs")).await).await;

        let row = body["records"][0]
            .as_object()
            .expect("one audit entry on the wire");
        let mut got: Vec<&str> = row.keys().map(String::as_str).collect();
        got.sort_unstable();
        assert_eq!(
            got,
            vec![
                "action",
                "created_at",
                "id",
                "ip_address",
                "resource",
                "updated_at",
                "user_id"
            ],
            "the wire field set must equal AdminAuditLogView's"
        );

        assert_eq!(row["action"], serde_json::json!("user.delete"));
        assert_eq!(row["resource"], serde_json::json!("users/u-9"));
        assert_eq!(body["total_count"], serde_json::json!(1));
        assert_eq!(body["page"], serde_json::json!(1));
        assert_eq!(body["page_size"], serde_json::json!(50));
    }

    /// `?action=` filters, and the filter reaches the query through
    /// `AdminAuditLogListQuery` — the type the published parameter schema is
    /// derived from.
    #[tokio::test]
    async fn list_applies_the_declared_query_filters() {
        let ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");
        audit_log(&ctx, "admin-1", "user.delete", "users/u-9", "203.0.113.7").await;
        audit_log(
            &ctx,
            "admin-1",
            "role.create",
            "roles/editor",
            "203.0.113.7",
        )
        .await;

        let mut msg = admin_msg("retrieve", "/b/admin/api/logs");
        msg.set_meta("req.query.action", "role.create".to_string());

        let body = output_json(handle_list(&ctx, &msg).await).await;

        assert_eq!(body["total_count"], serde_json::json!(1));
        assert_eq!(
            body["records"][0]["action"],
            serde_json::json!("role.create")
        );
    }
}
