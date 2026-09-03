//! `GET /b/dev/api/tools.json` — the page-scoped WebMCP manifest.
//!
//! `/b/webmcp/manifest.json` (`pipeline.rs`) publishes every endpoint any
//! block opted into with `.agent_tool(..)`, deployment-wide. The `/b/dev`
//! page wants something narrower and page-specific instead: a curated
//! allowlist of the dev block's own control-plane endpoints (`dev_*`) plus
//! the products admin endpoints an agent needs to build a storefront
//! (`shop_*`) — so the in-page agent sees exactly the tools this page's
//! workflow is about, not the deployment's whole surface.
//!
//! [`SELECTIONS`] is that allowlist, and [`handle`] is what projects it: both
//! are [`wafer_core::discovery::generate_webmcp_selected`], which builds this
//! exact manifest shape from a caller-chosen `(block, method, path)` list
//! rather than from `agent_tool` annotations on the selected endpoints (the
//! selected products endpoints carry none — the manifest below is the only
//! place they become tools at all).

use wafer_core::discovery::{generate_webmcp_selected, ToolSelection, WebMcpRefusal};
use wafer_run::{context::Context, AuthLevel, HttpMethod, OutputStream};

/// The curated tool list this manifest publishes: `(block, method, path,
/// tool name, description)`.
///
/// `block`/`method`/`path` name an endpoint exactly as its owning block
/// declared it in `BlockInfo::endpoints` — a typo here is a
/// [`wafer_core::discovery::WebMcpRefusal::SelectionNotFound`] refusal, not a
/// compile error, which is why `tools_json_publishes_every_selection_with_zero_refusals`
/// (`tests/dev_tools_manifest.rs`) asserts the published tool names equal
/// this list's names exactly: a silently dropped row would otherwise only
/// show up as one fewer tool on the page.
///
/// Every mutating tool's description names its side effect — the agent
/// deciding whether to call it has to be able to tell from the description
/// alone, the same way a human would from a button's label.
pub const SELECTIONS: &[(&str, HttpMethod, &str, &str, &str)] = &[
    // -- dev_* : the sandbox control plane -----------------------------
    (
        "impresspress/dev",
        HttpMethod::Get,
        "/b/dev/api/status",
        "dev_status",
        "Read the sandbox state: active generation, runtime generation, active blocks, any \
         activation in progress. Call this first.",
    ),
    (
        "impresspress/dev",
        HttpMethod::Get,
        "/b/dev/api/files",
        "dev_list_files",
        "List files in the sandbox workspace, optionally filtered to those whose path starts \
         with `prefix` (e.g. `site/` or `blocks/hello/`).",
    ),
    (
        "impresspress/dev",
        HttpMethod::Post,
        "/b/dev/api/files/read",
        "dev_read_file",
        "Read one workspace file's content and its SHA-256. Pass the hash back as \
         `expected_sha256` on `dev_write_file` or `dev_delete_file` to avoid overwriting an \
         edit you have not seen.",
    ),
    (
        "impresspress/dev",
        HttpMethod::Post,
        "/b/dev/api/files/write",
        "dev_write_file",
        "Write a workspace file under `site/` or `blocks/<name>/`. Writing under `site/` \
         publishes a new generation immediately, live on the sandbox; writing under `blocks/` \
         only stages source — nothing serves it until the block is compiled and staged.",
    ),
    (
        "impresspress/dev",
        HttpMethod::Post,
        "/b/dev/api/files/delete",
        "dev_delete_file",
        "Delete a workspace file. Requires the file's current SHA-256 as `expected_sha256`; a \
         mismatch is refused so you never delete over an edit you have not seen. Deleting under \
         `site/` publishes a new generation; deleting under `blocks/` does not.",
    ),
    (
        "impresspress/dev",
        HttpMethod::Get,
        "/b/dev/api/generations",
        "dev_list_generations",
        "List the publication ledger, newest first: every generation the sandbox has created.",
    ),
    (
        "impresspress/dev",
        HttpMethod::Get,
        "/b/dev/api/generations/{id}",
        "dev_get_generation",
        "Read one generation's full manifest — its site files and its blocks — and what it \
         changed relative to the generation it was derived from.",
    ),
    (
        "impresspress/dev",
        HttpMethod::Post,
        "/b/dev/api/generations/{id}/rollback",
        "dev_rollback",
        "Republish an earlier generation as the active one, reverting every site and block \
         change made since. Call `dev_list_generations` first to find the `id` to roll back to.",
    ),
    (
        "impresspress/dev",
        HttpMethod::Post,
        "/b/dev/api/blocks/{name}/remove",
        "dev_remove_block",
        "Remove a compiled block from the running site and activate the resulting generation. \
         Its source under `blocks/<name>/` is untouched — this only takes the compiled block out \
         of what serves.",
    ),
    // -- shop_* : the products admin API ------------------------------
    (
        "impresspress/products",
        HttpMethod::Get,
        "/b/products/api/admin/products",
        "shop_list_products",
        "List products with their status (`draft`, `active`, `archived`) and moderation state.",
    ),
    (
        "impresspress/products",
        HttpMethod::Post,
        "/b/products/api/admin/products",
        "shop_create_product",
        "Create a product. It starts in `draft` status and is invisible to shoppers until \
         `shop_update_product` sets `status: \"active\"`.",
    ),
    (
        "impresspress/products",
        HttpMethod::Patch,
        "/b/products/api/admin/products/{id}",
        "shop_update_product",
        "Update a product's fields, including `status`. Set `status: \"active\"` to make a \
         draft product visible to shoppers, or `\"archived\"` to retire it without deleting it.",
    ),
    (
        "impresspress/products",
        HttpMethod::Delete,
        "/b/products/api/admin/products/{id}",
        "shop_delete_product",
        "Soft-delete a product, hiding it from shoppers immediately. Call \
         `shop_restore_product` to undo.",
    ),
    (
        "impresspress/products",
        HttpMethod::Post,
        "/b/products/api/admin/products/{id}/restore",
        "shop_restore_product",
        "Restore a soft-deleted product, undoing `shop_delete_product` and clearing \
         `deleted_at`. The product is not editable through `shop_update_product` until this \
         call succeeds.",
    ),
    (
        "impresspress/products",
        HttpMethod::Get,
        "/b/products/api/admin/groups",
        "shop_list_groups",
        "List the product groups used to organize the catalog.",
    ),
    (
        "impresspress/products",
        HttpMethod::Post,
        "/b/products/api/admin/groups",
        "shop_create_group",
        "Create a product group. Assign a product to it by setting that product's `group_id` \
         with `shop_update_product`.",
    ),
    (
        "impresspress/products",
        HttpMethod::Get,
        "/b/products/api/admin/products/{product_id}/offers",
        "shop_list_offers",
        "List a product's pricing offers, draft and published.",
    ),
    (
        "impresspress/products",
        HttpMethod::Post,
        "/b/products/api/admin/products/{product_id}/offers",
        "shop_create_offer",
        "Create a pricing offer for a product. It starts in `draft` status, unpurchasable until \
         `shop_publish_offer` publishes it.",
    ),
    (
        "impresspress/products",
        HttpMethod::Patch,
        "/b/products/api/admin/products/{product_id}/offers/{offer_id}",
        "shop_update_offer",
        "Update a draft offer's pricing definition. Only a `draft` offer is editable this way — \
         a published offer must be archived with `shop_archive_offer` and recreated with \
         `shop_create_offer` to change its pricing.",
    ),
    (
        "impresspress/products",
        HttpMethod::Post,
        "/b/products/api/admin/products/{product_id}/offers/{offer_id}/publish",
        "shop_publish_offer",
        "Publish a draft offer, making it purchasable by shoppers.",
    ),
    (
        "impresspress/products",
        HttpMethod::Delete,
        "/b/products/api/admin/products/{product_id}/offers/{offer_id}",
        "shop_archive_offer",
        "Archive an offer, removing it from sale immediately without deleting its history.",
    ),
];

/// Serve the manifest: project [`SELECTIONS`] against the runtime's actually
/// registered blocks, using each selected endpoint's own declared
/// [`AuthLevel`] as the enforced one.
///
/// `|_b, ep| ep.auth` (rather than a router-aware resolver like
/// `routing::effective_access`) is correct here specifically because every
/// row in [`SELECTIONS`] names an `Admin`-declared endpoint mounted under an
/// `Admin`-only prefix (`/b/dev`'s own routes, and `/b/products/api/admin/*`)
/// — there is no tier this route's mount could raise the ceiling to that the
/// declaration does not already claim. See `pipeline.rs`'s
/// `/b/webmcp/manifest.json` handler for the case where that is not true.
pub async fn handle(ctx: &dyn Context) -> OutputStream {
    let blocks = ctx.registered_blocks();
    let selections: Vec<ToolSelection> = SELECTIONS
        .iter()
        .map(|(block, method, path, name, description)| ToolSelection {
            block: (*block).into(),
            method: *method,
            path: (*path).into(),
            name: (*name).into(),
            description: (*description).into(),
        })
        .collect();
    let (doc, refused) =
        generate_webmcp_selected(blocks, AuthLevel::Admin, |_b, ep| ep.auth, &selections);
    for r in &refused {
        match r.reason {
            // Expected whenever this build was compiled without
            // `block-products`: the products rows in `SELECTIONS` name a
            // block that simply is not registered, which is a build
            // configuration, not a defect in this manifest's own
            // declarations.
            WebMcpRefusal::SelectionNotFound => tracing::warn!(
                block = %r.block,
                path = %r.path,
                tool = %r.tool_name,
                reason = %r.reason,
                "dev tools.json: selection not found — its block is likely absent from this build",
            ),
            // Every other reason is a defect in this file's own
            // declarations (a schema wall the selected endpoint's contract
            // hits) and should never fire against the endpoints
            // `SELECTIONS` actually names.
            _ => tracing::error!(
                block = %r.block,
                path = %r.path,
                tool = %r.tool_name,
                reason = %r.reason,
                "dev tools.json refusal",
            ),
        }
    }
    super::no_store().json(&doc)
}
