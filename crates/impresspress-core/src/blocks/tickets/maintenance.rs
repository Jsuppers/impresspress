//! Constant-statement-count retention and operational status.

use serde::Serialize;
use wafer_core::clients::database as db;
use wafer_run::{context::Context, WaferError};

use super::{config::SecurityReadiness, repo};
use crate::util::json_map;

/// Outcome of one retention pass, as returned by
/// `POST /b/tickets/api/admin/retention/prune`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MaintenanceResult {
    /// Whether every delete in the pass succeeded. A `false` here is answered
    /// with HTTP 503 so a scheduler retries.
    pub complete: bool,
    /// Expired analyses removed.
    pub analyses_deleted: i64,
    /// Expired audit events removed.
    pub events_deleted: i64,
    /// Expired tickets removed. Tickets under legal hold never expire.
    pub tickets_deleted: i64,
    /// Stale submission rate-limit counters removed.
    pub rate_counters_deleted: i64,
    /// Names of the deletes that failed (`"analyses"`, `"events"`,
    /// `"tickets"`, `"rate-counters"`).
    pub errors: Vec<String>,
}

/// The stored record of the last retention pass.
// `id` is not published: the row is a singleton keyed on the literal
// `"singleton"`, so the column carries no information.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MaintenanceState {
    /// `YYYY-MM-DD` of the last pass, or `""` before the first one.
    pub last_pruned_day: String,
    /// RFC 3339 timestamp of the last pass, or `null` before the first one.
    pub last_pruned_at: Option<String>,
    /// Comma-joined names of the deletes that failed on the last pass, or `""`
    /// when it completed.
    pub last_prune_error: String,
}

impl MaintenanceState {
    /// Project the `impresspress__tickets__maintenance` singleton row.
    fn from_record(record: &db::Record) -> Self {
        use crate::util::RecordExt;

        Self {
            last_pruned_day: record.str_field("last_pruned_day").to_string(),
            last_pruned_at: match record.data.get("last_pruned_at") {
                Some(serde_json::Value::String(value)) => Some(value.clone()),
                _ => None,
            },
            last_prune_error: record.str_field("last_prune_error").to_string(),
        }
    }
}

/// Response body of `GET /b/tickets/api/admin/status`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct OperationalStatus {
    /// Whether protected public reporting is currently able to accept a
    /// submission, and what is missing when it is not.
    pub security: SecurityReadiness,
    /// Tickets still in the `"new"` state.
    pub new_tickets: i64,
    /// Tickets at `"urgent"` priority, in any state.
    pub urgent_tickets: i64,
    /// Tickets in `"new"`, `"triaged"` or `"investigating"`.
    pub open_tickets: i64,
    /// The last retention pass, or `null` when none has run.
    pub last_maintenance: Option<MaintenanceState>,
    /// Whether an audit-timeline write has failed since the flag was last
    /// cleared. While true, the timeline may be incomplete.
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
    let stored = db::get(ctx, repo::MAINTENANCE, "singleton").await.ok();
    let audit_degraded = stored
        .as_ref()
        .is_some_and(|record| super::service::bool_field(record, "audit_degraded"));
    Ok(OperationalStatus {
        security,
        new_tickets,
        urgent_tickets,
        open_tickets,
        last_maintenance: stored.as_ref().map(MaintenanceState::from_record),
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
