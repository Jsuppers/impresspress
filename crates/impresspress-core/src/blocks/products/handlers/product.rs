//! Product CRUD: admin (`/admin/b/products/products`) and user-owned
//! (`/b/products/products`, gated on `WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS`).
//!
//! Every response is a `contracts::ProductView` (or a list of them) built
//! from the row; every write body is a typed request whose fields are the
//! only columns a client can reach.

use std::collections::HashMap;

use wafer_block::db::{Filter, FilterOp};
use wafer_core::clients::{config, database as db};
use wafer_run::{context::Context, InputStream, Message, OutputStream};

use super::{
    default_template_id, seller_policy, GROUPS_TABLE, PRODUCTS_TABLE, PRODUCT_TEMPLATES_TABLE,
};
use crate::{
    blocks::{
        crud,
        products::{
            contracts::{
                CreateProductRequest, ProductDuplicateResponse, ProductListQuery,
                ProductListResponse, ProductStatus, ProductView, UpdateProductRequest,
            },
            repo::offers as offer_repo,
        },
    },
    http::{
        err_bad_request, err_forbidden, err_internal, err_not_found, err_unauthorized, ok_json,
    },
    util::{field_as_string, now_rfc3339, stamp_created, RecordExt},
};

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

/// The shared product list filters: `group_id` / `status` equality plus an
/// escaped `search` LIKE on `name`.
fn product_filters(query: &ProductListQuery) -> Vec<Filter> {
    let mut filters = Vec::new();
    if let Some(group_id) = &query.group_id {
        filters.push(Filter {
            field: "group_id".to_string(),
            operator: FilterOp::Equal,
            value: serde_json::Value::String(group_id.clone()),
        });
    }
    if let Some(status) = &query.status {
        filters.push(Filter {
            field: "status".to_string(),
            operator: FilterOp::Equal,
            value: serde_json::Value::String(status.clone()),
        });
    }
    if let Some(search) = query.search.as_deref().and_then(name_like_filter) {
        filters.push(search);
    }
    filters
}

/// One page of product rows, newest first, projected as a list response.
pub(super) async fn list_products(
    ctx: &dyn Context,
    query: &ProductListQuery,
    filters: Vec<Filter>,
) -> OutputStream {
    match crud::list_page(
        ctx,
        PRODUCTS_TABLE,
        i64::from(query.page),
        i64::from(query.page_size),
        filters,
        None,
    )
    .await
    {
        Ok(list) => ok_json(&ProductListResponse::from_record_list(&list)),
        Err(response) => response,
    }
}

/// User-owned product rows: `/b/products/products/{id}`, owned via `created_by`.
const USER_PRODUCT: crud::OwnedResource<'static> = crud::OwnedResource {
    collection: PRODUCTS_TABLE,
    path_prefix: "/b/products/products/",
    owner_field: "created_by",
    label: "Product",
};

const ADMIN_PRODUCT_PREFIX: &str = "/admin/b/products/products/";

// --- Product CRUD (admin) ---

pub(super) async fn handle_list_products(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let query = ProductListQuery::from_message(msg);
    let filters = product_filters(&query);
    list_products(ctx, &query, filters).await
}

pub(super) async fn handle_get_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = match crud::path_id(msg, ADMIN_PRODUCT_PREFIX, "Product") {
        Ok(id) => id,
        Err(response) => return response,
    };
    match crud::get_record(ctx, PRODUCTS_TABLE, id, "Product").await {
        Ok(record) => ok_json(&ProductView::from_record(&record)),
        Err(response) => response,
    }
}

pub(super) async fn handle_create_product(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let request: CreateProductRequest = match crud::read_json_body(input).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let mut data = request.into_columns();
    for (key, value) in [
        ("status", serde_json::json!("draft")),
        ("created_by", serde_json::json!(msg.user_id())),
        ("owner_kind", serde_json::json!("platform")),
        ("owner_id", serde_json::json!("")),
        ("approval_status", serde_json::json!("approved")),
    ] {
        data.entry(key.to_string()).or_insert(value);
    }
    match crud::create_record(ctx, PRODUCTS_TABLE, data).await {
        Ok(record) => ok_json(&ProductView::from_record(&record)),
        Err(response) => response,
    }
}

pub(super) async fn handle_update_product(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let id = match crud::path_id(msg, ADMIN_PRODUCT_PREFIX, "Product") {
        Ok(id) => id,
        Err(response) => return response,
    };
    let request: UpdateProductRequest = match crud::read_json_body(input).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    match crud::update_record(ctx, PRODUCTS_TABLE, id, request.into_columns(), "Product").await {
        Ok(record) => ok_json(&ProductView::from_record(&record)),
        Err(response) => response,
    }
}

pub(super) async fn handle_delete_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    crud::crud_delete(ctx, msg, PRODUCTS_TABLE, ADMIN_PRODUCT_PREFIX, "Product").await
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
        match crud::verify_owner(
            ctx,
            PRODUCTS_TABLE,
            source_id,
            "created_by",
            msg.user_id(),
            "Product",
        )
        .await
        {
            Ok(source) => source,
            Err(response) => return response,
        }
    } else {
        match db::get(ctx, PRODUCTS_TABLE, source_id).await {
            Ok(source) => source,
            Err(error) if error.code == wafer_run::ErrorCode::NotFound => {
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
    // Re-read after the insert so the response carries the table defaults
    // the copy did not set (see `crud::create_record`).
    let created = match db::create(ctx, PRODUCTS_TABLE, data).await {
        Ok(created) => created,
        Err(error) => return err_internal("Could not duplicate product", error),
    };
    let created = match db::get(ctx, PRODUCTS_TABLE, &created.id).await {
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
            if let Err(cleanup_error) = db::delete(ctx, PRODUCTS_TABLE, &created.id).await {
                tracing::error!(product_id = %created.id, error = %cleanup_error, "could not compensate duplicated product");
            }
            return err_internal("Could not duplicate product pricing", error);
        }
    };
    ok_json(&ProductDuplicateResponse {
        product: ProductView::from_record(&created),
        offers: duplicated_offers,
    })
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

    let query = ProductListQuery::from_message(msg);
    let mut filters = vec![Filter {
        field: "created_by".to_string(),
        operator: FilterOp::Equal,
        value: serde_json::Value::String(user_id),
    }];
    filters.extend(product_filters(&query));
    list_products(ctx, &query, filters).await
}

pub(super) async fn handle_user_get_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    match crud::get_owned(ctx, msg, &USER_PRODUCT).await {
        Ok(record) => ok_json(&ProductView::from_record(&record)),
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

    let request: CreateProductRequest = match crud::read_json_body(input).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = seller_policy::ensure_product_capacity(ctx, &user_id).await {
        return response;
    }

    // Verify user owns the group (if provided)
    if let Some(group_id) = request.group_id.as_deref().filter(|id| !id.is_empty()) {
        match db::get(ctx, GROUPS_TABLE, group_id).await {
            Ok(group) => {
                if field_as_string(&group, "user_id") != user_id {
                    return err_bad_request("You don't own this group");
                }
            }
            Err(_) => return err_bad_request("Group not found"),
        }
    }

    let mut data = request.into_columns();
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
    if data
        .get("currency")
        .is_none_or(|value| value.as_str().is_some_and(str::is_empty))
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
    if data
        .get("product_template_id")
        .is_none_or(|value| value.as_str().is_some_and(str::is_empty))
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

    match crud::create_record(ctx, PRODUCTS_TABLE, data).await {
        Ok(record) => ok_json(&ProductView::from_record(&record)),
        Err(response) => response,
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
    let current = match crud::verify_owner(
        ctx,
        PRODUCTS_TABLE,
        &id,
        "created_by",
        msg.user_id(),
        "Product",
    )
    .await
    {
        Ok(record) => record,
        Err(response) => return response,
    };

    let request: UpdateProductRequest = match crud::read_json_body(input).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    // The ownership, moderation and provider columns the untyped path had to
    // strip here are not fields of `UpdateProductRequest`, so they cannot
    // arrive.
    let requested_status = request.status;
    let mut data = request.into_columns();
    if let Err(response) = seller_policy::validate_product_fields(ctx, &data).await {
        return response;
    }

    match requested_status {
        None => {}
        Some(ProductStatus::PendingReview) => {
            return err_bad_request("Seller product status must be draft, active, or archived");
        }
        Some(ProductStatus::Draft | ProductStatus::Archived) => {}
        Some(ProductStatus::Active) => {
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

    match crud::update_record(ctx, PRODUCTS_TABLE, &id, data, "Product").await {
        Ok(record) => ok_json(&ProductView::from_record(&record)),
        Err(response) => response,
    }
}

pub(super) async fn handle_user_delete_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    crud::crud_delete_owned(ctx, msg, &USER_PRODUCT).await
}

pub(super) async fn handle_user_duplicate_product(
    ctx: &dyn Context,
    msg: &Message,
) -> OutputStream {
    duplicate_product(ctx, msg, true).await
}
