//! Constant-statement-count retention and operational status.

use serde::Serialize;
use wafer_core::clients::database as db;
use wafer_run::{context::Context, WaferError};

use super::{config::SecurityReadiness, repo};
use crate::util::json_map;

#[derive(Debug, Serialize)]
pub struct MaintenanceResult {
    pub complete: bool,
    pub analyses_deleted: i64,
    pub events_deleted: i64,
    pub tickets_deleted: i64,
    pub rate_counters_deleted: i64,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct OperationalStatus {
    pub security: SecurityReadiness,
    pub new_tickets: i64,
    pub urgent_tickets: i64,
    pub open_tickets: i64,
    pub last_maintenance: Option<db::Record>,
    pub audit_degraded: bool,
}

/// Database operations performed by a normal maintenance pass: three ticket-owned
/// expiry deletes, one auth rate-counter delete, and one singleton status write.
pub const STATEMENT_COUNT: usize = 5;

pub async fn prune(ctx: &dyn Context) -> MaintenanceResult {
    let now = chrono::Utc::now();
    let now_text = now.to_rfc3339();
    // `updated_at` on the auth rate-counter table is stamped by the windowed
    // upsert with SQL `CURRENT_TIMESTAMP`, which SQLite stores as
    // `YYYY-MM-DD HH:MM:SS` text (UTC, space separator). The cutoff must use
    // the same shape: RFC3339's `T`/offset compares wrong lexicographically
    // on SQLite and does not bind against Postgres's TIMESTAMPTZ column.
    let rate_cutoff = (now - chrono::Duration::hours(72))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let mut result = MaintenanceResult {
        complete: true,
        analyses_deleted: 0,
        events_deleted: 0,
        tickets_deleted: 0,
        rate_counters_deleted: 0,
        errors: Vec::new(),
    };

    prune_table(
        ctx,
        repo::ANALYSES,
        &now_text,
        "analyses",
        &mut result.analyses_deleted,
        &mut result.errors,
    )
    .await;
    prune_table(
        ctx,
        repo::EVENTS,
        &now_text,
        "events",
        &mut result.events_deleted,
        &mut result.errors,
    )
    .await;
    prune_table(
        ctx,
        repo::TICKETS,
        &now_text,
        "tickets",
        &mut result.tickets_deleted,
        &mut result.errors,
    )
    .await;
    match db::delete_by_filters_count(
        ctx,
        crate::blocks::auth::RATE_LIMITS_TABLE,
        vec![repo::before("updated_at", &rate_cutoff)],
    )
    .await
    {
        Ok(count) => result.rate_counters_deleted = count,
        Err(error) => {
            tracing::warn!(error = %error, "ticket maintenance rate-counter prune failed");
            result.errors.push("rate-counters".into());
        }
    }
    result.complete = result.errors.is_empty();
    store_result(ctx, &result).await;
    result
}

pub async fn status(ctx: &dyn Context) -> Result<OperationalStatus, WaferError> {
    let security = SecurityReadiness::load(ctx).await;
    let new_tickets = repo::count_tickets(ctx, vec![repo::eq("status", "new")]).await?;
    let urgent_tickets = repo::count_tickets(ctx, vec![repo::eq("priority", "urgent")]).await?;
    let open_tickets = repo::count_tickets(
        ctx,
        vec![wafer_block::db::Filter {
            field: "status".into(),
            operator: wafer_block::db::FilterOp::In,
            value: serde_json::json!(["new", "triaged", "investigating"]),
        }],
    )
    .await?;
    let last_maintenance = db::get(ctx, repo::MAINTENANCE, "singleton").await.ok();
    let audit_degraded = last_maintenance
        .as_ref()
        .is_some_and(|record| super::service::bool_field(record, "audit_degraded"));
    Ok(OperationalStatus {
        security,
        new_tickets,
        urgent_tickets,
        open_tickets,
        last_maintenance,
        audit_degraded,
    })
}

async fn prune_table(
    ctx: &dyn Context,
    table: &str,
    cutoff: &str,
    label: &str,
    count: &mut i64,
    errors: &mut Vec<String>,
) {
    match db::delete_by_filters_count(ctx, table, vec![repo::before("expires_at", cutoff)]).await {
        Ok(deleted) => *count = deleted,
        Err(error) => {
            tracing::warn!(table = label, error = %error, "ticket maintenance prune failed");
            errors.push(label.to_string());
        }
    }
}

async fn store_result(ctx: &dyn Context, result: &MaintenanceResult) {
    let now = chrono::Utc::now();
    let data = json_map(serde_json::json!({
        "last_pruned_day": now.format("%Y-%m-%d").to_string(),
        "last_pruned_at": now.to_rfc3339(),
        "last_prune_error": if result.complete { String::new() } else { result.errors.join(",") },
    }));
    if db::update(ctx, repo::MAINTENANCE, "singleton", data.clone())
        .await
        .is_err()
    {
        let mut create = data;
        create.insert("id".into(), serde_json::json!("singleton"));
        if let Err(error) = db::create(ctx, repo::MAINTENANCE, create).await {
            tracing::warn!(error = %error, "ticket maintenance status write failed");
        }
    }
}
