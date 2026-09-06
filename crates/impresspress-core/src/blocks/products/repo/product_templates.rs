//! Reusable product template definitions (admin-authored, seeded by Init).

use wafer_core::clients::database as db;
use wafer_run::context::Context;

pub(crate) const TABLE: &str = "impresspress__products__product_templates";

/// The id of the `name = "default"` template the Init lifecycle seeds. Same
/// contract as [`super::group_templates::default_id`], including `None` on
/// failure: the product create it feeds proceeds without a template, as it
/// did before this module existed.
pub(crate) async fn default_id(ctx: &dyn Context) -> Option<String> {
    db::get_by_field(ctx, TABLE, "name", serde_json::json!("default"))
        .await
        .ok()
        .map(|record| record.id)
}
