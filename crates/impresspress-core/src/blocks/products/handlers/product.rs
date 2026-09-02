//! Product CRUD: admin (`/admin/b/products/products`) and user-owned
//! (`/b/products/products`, gated on `WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS`).

use std::collections::HashMap;

use wafer_block::db::{Filter, FilterOp};
use wafer_core::clients::{config, database as db};
use wafer_run::{context::Context, ErrorCode, InputStream, Message, OutputStream};

use super::{default_template_id, seller_policy, GROUPS_TABLE, PRODUCT_TEMPLATES_TABLE};
use crate::{
    blocks::products::repo::{self, offers as offer_repo},
    http::{
        err_bad_request, err_forbidden, err_internal, err_not_found, err_unauthorized, ok_json,
    },
    util::{field_as_string, now_rfc3339, path_param, stamp_created, stamp_updated, RecordExt},
};

// Columns the products table owns internally: the row's identity, ownership,
// moderation state, the lifecycle stamps, and the Stripe/versioning linkage.
// None of them is a caller-supplied value on any tier or verb — each has a
// dedicated writer that maintains its invariants (`approve_product`/
// `reject_product`/`suspend` for `approval_status`, the delete endpoints for
// `deleted_at`, the Stripe sync for `stripe_product_id`, the
// publish flow for `submitted_at`/`published_at`, seller onboarding for
// `owner_*`/`seller_account_id`, versioning for `current_version`, and the
// database layer's own UUID synthesis for `id`). A generic create or PATCH
// that let a body through verbatim would write them behind those writers'
// backs: an admin PATCH of `{"stripe_product_id": "prod_wrong"}` desyncs the
// row from the Stripe catalog, `{"owner_kind":…,"owner_id":…}` re-parents a
// seller's product, and a create carrying `deleted_at` lands a row that
// `ensure_product_capacity`'s live-only count cannot see.
//
// `id` is on the list for both verbs, and is the one that bites hardest.
// `line_items`, `offers`, `product_versions` and `entitlements` all carry a
// `product_id` that is `TEXT NOT NULL`, so a PATCH that rewrites the key
// orphans every one of them — the exact damage soft delete exists to
// prevent — while the write still reports one row affected. On create the
// field is not a rewrite, but it is still not the caller's to choose: the
// database layer synthesizes a UUID when `id` is absent and honours it when
// present, so a caller-supplied id means a public endpoint picking a primary
// key, which can only collide (a 500) or claim the key of a row that was
// purged. The products API therefore never takes an id from a body, on
// either verb.
//
// One list, four handlers: admin/user × create/update. The admin tier is not
// exempt — this is not a privilege boundary (an admin has other, deliberate
// doors to each of these fields) but an integrity one, and the two update
// handlers previously disagreeing about it is exactly the kind of drift a
// single shared constant prevents.
const UNSETTABLE_FIELDS: &[&str] = &[
    "id",
    "created_by",
    "owner_kind",
    "owner_id",
    "seller_account_id",
    "approval_status",
    "stripe_product_id",
    "current_version",
    "submitted_at",
    "published_at",
    "deleted_at",
];

/// Refuse a caller-supplied request body that names any [`UNSETTABLE_FIELDS`]
/// entry, naming the offending fields. `Ok(())` means the whole body is
/// writable as it stands.
///
/// A refusal rather than a silent drop. Dropping answered 200 with a body in
/// which the dropped field was plainly unchanged, so a client sending
/// `{"approval_status":"approved","name":"X"}` was told its write had
/// succeeded when half of it had been discarded — and, having no signal to
/// act on, would keep re-sending it. 400 is the same shape the seller PATCH
/// already uses for an unrecognized `status`, and it costs nothing the UI
/// wants: every request the shipped admin and seller pages issue sends only
/// caller-owned fields.
///
/// Handlers that legitimately write one of these fields do so *after* this
/// call, from their own computed value — so this must run on the raw parsed
/// body, before any server-supplied default or stamp is inserted.
fn reject_unsettable_fields(data: &HashMap<String, serde_json::Value>) -> Result<(), OutputStream> {
    let named: Vec<&str> = UNSETTABLE_FIELDS
        .iter()
        .copied()
        .filter(|field| data.contains_key(*field))
        .collect();
    if named.is_empty() {
        return Ok(());
    }
    Err(err_bad_request(&format!(
        "These fields are not settable through this endpoint: {}",
        named.join(", ")
    )))
}

/// Map a `repo::products` write failure onto a response.
///
/// `NotFound` is the 404 every product endpoint gives for a row that is
/// missing *or* soft-deleted — for a filtered write it is also "zero rows
/// matched", which is the same fact.
///
/// `InvalidArgument` is a CALLER error and carries a message saying what to
/// change: `repo::products::reject_id_rewrite` raises it as the backstop for
/// a caller that did not pass through [`reject_unsettable_fields`], and any
/// future repository guard will arrive the same way. Matching only `NotFound`
/// and funnelling the rest into `err_internal` answered 500 and threw the
/// message away, so the caller got an opaque server error, a correlation id,
/// and nothing to act on for a mistake that was entirely theirs.
///
/// Anything else is a genuine failure and keeps `context` for the log.
pub(in crate::blocks::products) fn write_error(
    error: wafer_run::WaferError,
    context: &str,
) -> OutputStream {
    match error.code {
        ErrorCode::NotFound => err_not_found("Product not found"),
        ErrorCode::InvalidArgument => err_bad_request(&error.message),
        _ => err_internal(context, error),
    }
}

/// Escape SQL LIKE wildcards (`%`, `_`) and the escape char (`\`) in user
/// input so a user searching for `100% off` doesn't also match arbitrary
/// characters.
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

/// Build a `name LIKE %search%` filter with LIKE wildcards escaped.
/// Returns `None` for an empty search term.
///
/// Also called from `pages::manage_products` (admin search box), hence the
/// wider-than-`handlers` visibility.
pub(in crate::blocks::products) fn name_like_filter(search: &str) -> Option<Filter> {
    if search.is_empty() {
        return None;
    }
    Some(Filter {
        field: "name".to_string(),
        operator: FilterOp::Like,
        value: serde_json::Value::String(format!("%{}%", escape_like(search))),
    })
}

/// Build the shared product list filters from query params: `group_id` /
/// `status` equality plus an escaped `search` LIKE on `name`.
fn product_filters(msg: &Message) -> Vec<Filter> {
    let mut filters = Vec::new();
    let group_id = msg.query("group_id").to_string();
    if !group_id.is_empty() {
        filters.push(Filter {
            field: "group_id".to_string(),
            operator: FilterOp::Equal,
            value: serde_json::Value::String(group_id),
        });
    }
    let status = msg.query("status").to_string();
    if !status.is_empty() {
        filters.push(Filter {
            field: "status".to_string(),
            operator: FilterOp::Equal,
            value: serde_json::Value::String(status),
        });
    }
    if let Some(search) = name_like_filter(msg.query("search")) {
        filters.push(search);
    }
    filters
}

/// Fetch a product and verify `created_by == user_id`, routing the lookup
/// through `repo::products::get` so a soft-deleted product answers 404 the
/// same as one that never existed — the generic `crud::verify_owner` reads
/// its collection raw and would let an owner keep fetching/editing a
/// soft-deleted row. Mirrors `crud::verify_owner`'s response shape: 401
/// unauthenticated, 404 for both "missing" and "not yours" (existence must
/// not leak to a non-owner).
async fn verify_product_owner(
    ctx: &dyn Context,
    id: &str,
    user_id: &str,
) -> Result<wafer_core::clients::database::Record, OutputStream> {
    if user_id.is_empty() {
        return Err(err_unauthorized("Not authenticated"));
    }
    match repo::products::get(ctx, id).await {
        Ok(record) => {
            if field_as_string(&record, "created_by") != user_id {
                return Err(err_not_found("Product not found"));
            }
            Ok(record)
        }
        Err(e) if e.code == ErrorCode::NotFound => Err(err_not_found("Product not found")),
        Err(e) => Err(err_internal("Database error", e)),
    }
}

// --- Product CRUD (admin) ---

pub(super) async fn handle_list_products(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let (page, page_size, _) = msg.pagination_params(20);
    match repo::products::list_page(
        ctx,
        page as i64,
        page_size as i64,
        product_filters(msg),
        None,
    )
    .await
    {
        Ok(result) => ok_json(&result),
        Err(e) => err_internal("Database error", e),
    }
}

pub(super) async fn handle_get_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = path_param(msg, "id", "/admin/b/products/products/");
    if id.is_empty() {
        return err_bad_request("Missing product ID");
    }
    match repo::products::get(ctx, id).await {
        Ok(record) => ok_json(&record),
        Err(e) if e.code == ErrorCode::NotFound => err_not_found("Product not found"),
        Err(e) => err_internal("Database error", e),
    }
}

pub(super) async fn handle_create_product(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let mut defaults = HashMap::new();
    defaults.insert(
        "status".to_string(),
        serde_json::Value::String("draft".to_string()),
    );
    defaults.insert(
        "created_by".to_string(),
        serde_json::Value::String(msg.user_id().to_string()),
    );
    defaults.insert(
        "owner_kind".to_string(),
        serde_json::Value::String("platform".to_string()),
    );
    defaults.insert(
        "owner_id".to_string(),
        serde_json::Value::String(String::new()),
    );
    defaults.insert(
        "approval_status".to_string(),
        serde_json::Value::String("approved".to_string()),
    );

    let raw = input.collect_to_bytes().await;
    let mut data: HashMap<String, serde_json::Value> = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };
    // Runs before `defaults` is applied, so the `or_insert` below always
    // inserts for the four internal fields it covers — a body can no longer
    // pre-empt them, and it can no longer smuggle in the rest.
    if let Err(response) = reject_unsettable_fields(&data) {
        return response;
    }
    stamp_created(&mut data);
    for (key, val) in defaults {
        data.entry(key).or_insert(val);
    }
    match repo::products::create(ctx, data).await {
        Ok(record) => ok_json(&record),
        Err(e) => err_internal("Database error", e),
    }
}

pub(super) async fn handle_update_product(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let id = path_param(msg, "id", "/admin/b/products/products/");
    if id.is_empty() {
        return err_bad_request("Missing product ID");
    }
    let raw = input.collect_to_bytes().await;
    let mut data: HashMap<String, serde_json::Value> = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };
    if let Err(response) = reject_unsettable_fields(&data) {
        return response;
    }
    stamp_updated(&mut data);
    // A soft-deleted product is not editable: the generic PATCH refuses one
    // outright rather than silently applying unrelated field changes to a
    // dead row. Until the restore endpoint ships, clearing `deleted_at` is an
    // operator statement against the database — see the recovery note at the
    // top of `repo::products`. The liveness test is the write's own `WHERE`,
    // not a `get` before it: a separate read leaves a window in which a
    // concurrent delete commits and the PATCH then writes to the dead row and
    // answers 200 — precisely the outcome this guard exists to prevent.
    // `NotFound` matches the response every other admin product endpoint
    // gives for a soft-deleted row.
    match repo::products::update_live(ctx, id, data).await {
        Ok(record) => ok_json(&record),
        Err(e) => write_error(e, "Database error"),
    }
}

pub(super) async fn handle_delete_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = path_param(msg, "id", "/admin/b/products/products/");
    if id.is_empty() {
        return err_bad_request("Missing product ID");
    }
    match repo::products::soft_delete(ctx, id).await {
        Ok(()) => ok_json(&serde_json::json!({"deleted": true})),
        Err(e) if e.code == ErrorCode::NotFound => err_not_found("Product not found"),
        Err(e) => err_internal("Database error", e),
    }
}

fn copied_name(source: &wafer_core::clients::database::Record) -> String {
    let base = source
        .str_field("name")
        .chars()
        .take(150)
        .collect::<String>();
    format!("{base} copy")
}

fn copied_slug(source: &wafer_core::clients::database::Record) -> String {
    let base = source.str_field("slug");
    let base = if base.is_empty() { "product" } else { base };
    let base = base.chars().take(140).collect::<String>();
    let suffix = uuid::Uuid::now_v7().to_string();
    format!("{base}-copy-{}", &suffix[..8])
}

async fn duplicate_product(ctx: &dyn Context, msg: &Message, owner_only: bool) -> OutputStream {
    let source_id = msg.var("id");
    if source_id.is_empty() {
        return err_bad_request("Missing product ID");
    }
    let source = if owner_only {
        match verify_product_owner(ctx, source_id, msg.user_id()).await {
            Ok(source) => source,
            Err(response) => return response,
        }
    } else {
        match repo::products::get(ctx, source_id).await {
            Ok(source) => source,
            Err(error) if error.code == ErrorCode::NotFound => {
                return err_not_found("Product not found");
            }
            Err(error) => return err_internal("Could not load product", error),
        }
    };
    if owner_only {
        if let Err(response) = seller_policy::ensure_product_capacity(ctx, msg.user_id()).await {
            return response;
        }
        if let Err(response) = seller_policy::validate_product_record(ctx, &source).await {
            return response;
        }
    }

    let mut data = HashMap::new();
    for field in [
        "description",
        "currency",
        "group_id",
        "image_url",
        "product_template_id",
        "fulfillment_kind",
        "tags",
        "metadata",
    ] {
        if let Some(value) = source.data.get(field) {
            data.insert(field.to_string(), value.clone());
        }
    }
    data.insert(
        "name".to_string(),
        serde_json::Value::String(copied_name(&source)),
    );
    data.insert(
        "slug".to_string(),
        serde_json::Value::String(copied_slug(&source)),
    );
    data.insert(
        "status".to_string(),
        serde_json::Value::String("draft".to_string()),
    );
    data.insert(
        "created_by".to_string(),
        serde_json::Value::String(msg.user_id().to_string()),
    );
    if owner_only {
        let moderation_required = seller_moderation_required(ctx).await;
        data.insert(
            "owner_kind".to_string(),
            serde_json::Value::String("user".to_string()),
        );
        data.insert(
            "owner_id".to_string(),
            serde_json::Value::String(msg.user_id().to_string()),
        );
        data.insert(
            "approval_status".to_string(),
            serde_json::Value::String(
                if moderation_required {
                    "draft"
                } else {
                    "approved"
                }
                .to_string(),
            ),
        );
        if let Some(account_id) = source.data.get("seller_account_id") {
            data.insert("seller_account_id".to_string(), account_id.clone());
        }
    } else {
        data.insert(
            "owner_kind".to_string(),
            serde_json::Value::String("platform".to_string()),
        );
        data.insert(
            "owner_id".to_string(),
            serde_json::Value::String(String::new()),
        );
        data.insert(
            "approval_status".to_string(),
            serde_json::Value::String("approved".to_string()),
        );
    }
    stamp_created(&mut data);
    let created = match repo::products::create(ctx, data).await {
        Ok(created) => created,
        Err(error) => return err_internal("Could not duplicate product", error),
    };
    let duplicated_offers = match offer_repo::duplicate_for_product(
        ctx,
        source_id,
        &created.id,
        msg.user_id(),
    )
    .await
    {
        Ok(offers) => offers,
        Err(error) => {
            if let Err(cleanup_error) = offer_repo::delete_for_product(ctx, &created.id).await {
                tracing::error!(product_id = %created.id, error = %cleanup_error, "could not compensate duplicated offers");
            }
            // Hard-delete, not the door: this product was never visible to
            // anyone (creation failed before returning), so a soft-deleted
            // husk would needlessly consume its slug against the partial
            // unique index instead of freeing it for retry.
            if let Err(cleanup_error) = repo::products::purge(ctx, &created.id).await {
                tracing::error!(product_id = %created.id, error = %cleanup_error, "could not compensate duplicated product");
            }
            return err_internal("Could not duplicate product pricing", error);
        }
    };
    ok_json(&serde_json::json!({
        "product": created,
        "offers": duplicated_offers,
    }))
}

pub(super) async fn handle_duplicate_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    duplicate_product(ctx, msg, false).await
}

// --- User's own products ---

async fn seller_moderation_required(ctx: &dyn Context) -> bool {
    config::get_default(
        ctx,
        "IMPRESSPRESS__PRODUCTS__SELLER_MODERATION_REQUIRED",
        "true",
    )
    .await
        == "true"
}

pub(super) async fn handle_user_list_products(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let user_id = msg.user_id().to_string();
    if user_id.is_empty() {
        return err_unauthorized("Not authenticated");
    }

    let mut filters = vec![Filter {
        field: "created_by".to_string(),
        operator: FilterOp::Equal,
        value: serde_json::Value::String(user_id),
    }];
    filters.extend(product_filters(msg));

    let (page, page_size, _) = msg.pagination_params(20);
    match repo::products::list_page(ctx, page as i64, page_size as i64, filters, None).await {
        Ok(result) => ok_json(&result),
        Err(e) => err_internal("Database error", e),
    }
}

pub(super) async fn handle_user_get_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = path_param(msg, "id", "/b/products/products/");
    if id.is_empty() {
        return err_bad_request("Missing product ID");
    }
    match verify_product_owner(ctx, id, msg.user_id()).await {
        Ok(record) => ok_json(&record),
        Err(response) => response,
    }
}

pub(super) async fn handle_user_create_product(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let user_id = msg.user_id().to_string();
    if user_id.is_empty() {
        return err_unauthorized("Not authenticated");
    }

    let raw = input.collect_to_bytes().await;
    let mut data: HashMap<String, serde_json::Value> = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };
    // Before the capacity check, not after: a body carrying `deleted_at`
    // would otherwise create a row the check's live-only count cannot see,
    // leaving the seller's slot free for the next create and the one after
    // that.
    if let Err(response) = reject_unsettable_fields(&data) {
        return response;
    }
    if let Err(response) = seller_policy::ensure_product_capacity(ctx, &user_id).await {
        return response;
    }

    // Verify user owns the group (if provided)
    let group_id_str = data
        .get("group_id")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .or_else(|| {
            data.get("group_id")
                .and_then(|v| v.as_i64().map(|n| n.to_string()))
        })
        .unwrap_or_default();
    if !group_id_str.is_empty() {
        match db::get(ctx, GROUPS_TABLE, &group_id_str).await {
            Ok(group) => {
                if field_as_string(&group, "user_id") != user_id {
                    return err_bad_request("You don't own this group");
                }
            }
            Err(_) => return err_bad_request("Group not found"),
        }
    }

    let moderation_required = seller_moderation_required(ctx).await;
    data.insert(
        "status".to_string(),
        serde_json::Value::String("draft".to_string()),
    );
    data.insert(
        "approval_status".to_string(),
        serde_json::Value::String(
            if moderation_required {
                "draft"
            } else {
                "approved"
            }
            .to_string(),
        ),
    );
    data.insert(
        "owner_kind".to_string(),
        serde_json::Value::String("user".to_string()),
    );
    data.insert(
        "owner_id".to_string(),
        serde_json::Value::String(user_id.clone()),
    );
    data.insert("created_by".to_string(), serde_json::Value::String(user_id));
    if !data.contains_key("currency")
        || data
            .get("currency")
            .is_some_and(|value| value.as_str().is_some_and(str::is_empty))
    {
        data.insert(
            "currency".to_string(),
            serde_json::json!(
                config::get_default(ctx, "IMPRESSPRESS__PRODUCTS__DEFAULT_CURRENCY", "USD").await
            ),
        );
    }
    stamp_created(&mut data);
    // Default product_template_id to the seeded "default" template's real
    // (UUIDv7) id if the caller didn't specify one. The previous fallback
    // to the literal integer `1` would never match a seeded record (ids
    // are UUIDs, not integers).
    if !data.contains_key("product_template_id")
        || data
            .get("product_template_id")
            .is_some_and(|v| v.is_null() || v.as_str().is_some_and(|s| s.is_empty()))
    {
        if let Some(default_id) = default_template_id(ctx, PRODUCT_TEMPLATES_TABLE).await {
            data.insert(
                "product_template_id".to_string(),
                serde_json::Value::String(default_id),
            );
        }
    }
    if let Err(response) = seller_policy::validate_product_fields(ctx, &data).await {
        return response;
    }

    match repo::products::create(ctx, data).await {
        Ok(record) => ok_json(&record),
        Err(e) => err_internal("Database error", e),
    }
}

pub(super) async fn handle_user_update_product(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let id = msg.var("id").to_string();
    if id.is_empty() {
        return err_bad_request("Missing product ID");
    }
    let current = match verify_product_owner(ctx, &id, msg.user_id()).await {
        Ok(record) => record,
        Err(response) => return response,
    };

    let raw = input.collect_to_bytes().await;
    let mut data: HashMap<String, serde_json::Value> = match serde_json::from_slice(&raw) {
        Ok(data) => data,
        Err(error) => return err_bad_request(&format!("Invalid body: {error}")),
    };
    let requested_status = data
        .get("status")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    if let Err(response) = reject_unsettable_fields(&data) {
        return response;
    }
    if let Err(response) = seller_policy::validate_product_fields(ctx, &data).await {
        return response;
    }

    if let Some(status) = requested_status.as_deref() {
        if !matches!(status, "draft" | "active" | "archived") {
            return err_bad_request("Seller product status must be draft, active, or archived");
        }
        if status == "active" {
            if let Err(response) =
                seller_policy::validate_product_record_with_patch(ctx, &current, &data).await
            {
                return response;
            }
            let approval = field_as_string(&current, "approval_status");
            if approval == "suspended" {
                return err_forbidden("Suspended products cannot be published");
            }
            if seller_moderation_required(ctx).await && approval != "approved" {
                data.insert(
                    "status".to_string(),
                    serde_json::Value::String("pending_review".to_string()),
                );
                data.insert(
                    "approval_status".to_string(),
                    serde_json::Value::String("pending".to_string()),
                );
                data.insert(
                    "submitted_at".to_string(),
                    serde_json::Value::String(now_rfc3339()),
                );
            } else {
                data.insert(
                    "status".to_string(),
                    serde_json::Value::String("active".to_string()),
                );
                data.insert(
                    "approval_status".to_string(),
                    serde_json::Value::String("approved".to_string()),
                );
                data.insert(
                    "published_at".to_string(),
                    serde_json::Value::String(now_rfc3339()),
                );
            }
        }
    }

    stamp_updated(&mut data);
    // `update_live`, for the same reason the admin PATCH uses it: the
    // ownership check above is a separate read, and the validation between it
    // and this write only widens the window a concurrent delete can land in.
    // The write itself has to be the thing that tests liveness.
    match repo::products::update_live(ctx, &id, data).await {
        Ok(record) => ok_json(&record),
        Err(error) => write_error(error, "Database error"),
    }
}

pub(super) async fn handle_user_delete_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = path_param(msg, "id", "/b/products/products/").to_string();
    if id.is_empty() {
        return err_bad_request("Missing product ID");
    }
    if let Err(response) = verify_product_owner(ctx, &id, msg.user_id()).await {
        return response;
    }
    match repo::products::soft_delete(ctx, &id).await {
        Ok(()) => ok_json(&serde_json::json!({"deleted": true})),
        Err(e) if e.code == ErrorCode::NotFound => err_not_found("Product not found"),
        Err(e) => err_internal("Database error", e),
    }
}

pub(super) async fn handle_user_duplicate_product(
    ctx: &dyn Context,
    msg: &Message,
) -> OutputStream {
    duplicate_product(ctx, msg, true).await
}
