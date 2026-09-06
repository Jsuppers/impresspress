//! Reusable group template definitions (admin-authored, seeded by Init).

use wafer_block::db::{ListOptions, SortField};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, WaferError};

pub(crate) const TABLE: &str = "impresspress__products__group_templates";

/// Every group template, name-ascending. Read-only listing for users.
pub(crate) async fn list_by_name(
    ctx: &dyn Context,
    limit: i64,
) -> Result<db::RecordList, WaferError> {
    let opts = ListOptions {
        sort: vec![SortField {
            field: "name".to_string(),
            desc: false,
        }],
        limit,
        ..Default::default()
    };
    db::list(ctx, TABLE, &opts).await
}

/// The id of the `name = "default"` template the Init lifecycle seeds, so a
/// client-omitted `group_template_id` defaults to a real (UUIDv7) row rather
/// than the literal integer `1`, which never matches a seeded record.
///
/// `None` on any failure, including a missing row: the create it feeds is
/// allowed to proceed without a template, and it did so before this module
/// existed. The caller sees the same behaviour; what changed is that the
/// table name is no longer a parameter.
pub(crate) async fn default_id(ctx: &dyn Context) -> Option<String> {
    db::get_by_field(ctx, TABLE, "name", serde_json::json!("default"))
        .await
        .ok()
        .map(|record| record.id)
}
