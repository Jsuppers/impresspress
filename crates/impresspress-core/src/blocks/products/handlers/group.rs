//! Group CRUD: admin (`/admin/b/products/groups`) and user-owned
//! (`/b/products/groups`, gated on `WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS`),
//! plus the "products in a user's group" listing and the read-only
//! group-templates listing.
//!
//! Every group response is a `contracts::GroupView` (or a list of them)
//! built from the row; every write body is a typed request whose fields are
//! the only columns a client can reach.

use wafer_block::db::{Filter, FilterOp, ListOptions, SortField};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, InputStream, Message, OutputStream};

use super::{default_template_id, GROUPS_TABLE, GROUP_TEMPLATES_TABLE, PRODUCTS_TABLE};
use crate::{
    blocks::{
        crud,
        products::contracts::{
            CreateGroupRequest, CreateOwnGroupRequest, GroupListResponse,
            GroupTemplateListResponse, GroupView, PageQuery, ProductListResponse,
            UpdateGroupRequest, UpdateOwnGroupRequest,
        },
    },
    http::{err_bad_request, err_internal, err_unauthorized, ok_json},
};

/// User-owned group rows: `/b/products/groups/{id}`, owned via `user_id`.
const USER_GROUP: crud::OwnedResource<'static> = crud::OwnedResource {
    collection: GROUPS_TABLE,
    path_prefix: "/b/products/groups/",
    owner_field: "user_id",
    label: "Group",
};

const ADMIN_GROUP_PREFIX: &str = "/admin/b/products/groups/";

// --- Groups (admin) ---

pub(super) async fn handle_list_groups(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let query = PageQuery::from_message(msg);
    match crud::list_page(
        ctx,
        GROUPS_TABLE,
        i64::from(query.page),
        i64::from(query.page_size),
        vec![],
        None,
    )
    .await
    {
        Ok(list) => ok_json(&GroupListResponse::from_record_list(&list)),
        Err(response) => response,
    }
}

pub(super) async fn handle_create_group(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let request: CreateGroupRequest = match crud::read_json_body(input).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let mut data = request.into_columns();
    data.entry("user_id".to_string())
        .or_insert_with(|| serde_json::Value::String(msg.user_id().to_string()));
    match crud::create_record(ctx, GROUPS_TABLE, data).await {
        Ok(record) => ok_json(&GroupView::from_record(&record)),
        Err(response) => response,
    }
}

pub(super) async fn handle_update_group(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let id = match crud::path_id(msg, ADMIN_GROUP_PREFIX, "Group") {
        Ok(id) => id,
        Err(response) => return response,
    };
    let request: UpdateGroupRequest = match crud::read_json_body(input).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    match crud::update_record(ctx, GROUPS_TABLE, id, request.into_columns(), "Group").await {
        Ok(record) => ok_json(&GroupView::from_record(&record)),
        Err(response) => response,
    }
}

pub(super) async fn handle_delete_group(ctx: &dyn Context, msg: &Message) -> OutputStream {
    crud::crud_delete(ctx, msg, GROUPS_TABLE, ADMIN_GROUP_PREFIX, "Group").await
}

// --- User's own groups ---

pub(super) async fn handle_user_list_groups(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let user_id = msg.user_id().to_string();
    if user_id.is_empty() {
        return err_unauthorized("Not authenticated");
    }

    let opts = ListOptions {
        filters: vec![Filter {
            field: "user_id".to_string(),
            operator: FilterOp::Equal,
            value: serde_json::Value::String(user_id),
        }],
        sort: vec![SortField {
            field: "name".to_string(),
            desc: false,
        }],
        limit: 1000,
        ..Default::default()
    };
    match db::list(ctx, GROUPS_TABLE, &opts).await {
        Ok(result) => ok_json(&GroupListResponse::from_record_list(&result)),
        Err(e) => err_internal("Database error", e),
    }
}

pub(super) async fn handle_user_get_group(ctx: &dyn Context, msg: &Message) -> OutputStream {
    match crud::get_owned(ctx, msg, &USER_GROUP).await {
        Ok(record) => ok_json(&GroupView::from_record(&record)),
        Err(response) => response,
    }
}

pub(super) async fn handle_user_create_group(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let user_id = msg.user_id().to_string();
    if user_id.is_empty() {
        return err_unauthorized("Not authenticated");
    }

    let request: CreateOwnGroupRequest = match crud::read_json_body(input).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let mut body = request.into_columns();
    body.insert("user_id".to_string(), serde_json::Value::String(user_id));
    // Default group_template_id to the seeded "default" template's real
    // (UUIDv7) id — same reasoning as for product_template_id in
    // `product::handle_user_create_product`.
    if body
        .get("group_template_id")
        .is_none_or(|v| v.as_str().is_some_and(str::is_empty))
    {
        if let Some(default_id) = default_template_id(ctx, GROUP_TEMPLATES_TABLE).await {
            body.insert(
                "group_template_id".to_string(),
                serde_json::Value::String(default_id),
            );
        }
    }

    match crud::create_record(ctx, GROUPS_TABLE, body).await {
        Ok(record) => ok_json(&GroupView::from_record(&record)),
        Err(response) => response,
    }
}

pub(super) async fn handle_user_update_group(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    // `user_id` is not a field of the request, so ownership cannot change.
    let request: UpdateOwnGroupRequest = match crud::read_json_body(input).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    match crud::update_owned(ctx, msg, &USER_GROUP, request.into_columns()).await {
        Ok(record) => ok_json(&GroupView::from_record(&record)),
        Err(response) => response,
    }
}

pub(super) async fn handle_user_delete_group(ctx: &dyn Context, msg: &Message) -> OutputStream {
    crud::crud_delete_owned(ctx, msg, &USER_GROUP).await
}

// Products in a user's group
pub(super) async fn handle_user_group_products(ctx: &dyn Context, msg: &Message) -> OutputStream {
    // Path: /b/products/groups/{id}/products — prefer the matcher-bound `{id}`.
    let group_id = {
        let var = msg.var("id");
        if var.is_empty() {
            msg.path()
                .strip_prefix("/b/products/groups/")
                .unwrap_or("")
                .strip_suffix("/products")
                .unwrap_or("")
        } else {
            var
        }
    };
    if group_id.is_empty() {
        return err_bad_request("Missing group ID");
    }

    if let Err(resp) = crud::verify_owner(
        ctx,
        GROUPS_TABLE,
        group_id,
        "user_id",
        msg.user_id(),
        "Group",
    )
    .await
    {
        return resp;
    }

    let query = PageQuery::from_message(msg);
    let filters = vec![Filter {
        field: "group_id".to_string(),
        operator: FilterOp::Equal,
        value: serde_json::Value::String(group_id.to_string()),
    }];
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

// User-accessible group templates (read-only). Answers with the same
// `{records, total_count, page, page_size}` envelope as every other list in
// the block — the shape its schema always documented — over the whole
// table, sorted by name, the way the owner group list does.
pub(super) async fn handle_user_list_group_templates(
    ctx: &dyn Context,
    _msg: &Message,
) -> OutputStream {
    let opts = ListOptions {
        sort: vec![SortField {
            field: "name".to_string(),
            desc: false,
        }],
        limit: 1000,
        ..Default::default()
    };
    match db::list(ctx, GROUP_TEMPLATES_TABLE, &opts).await {
        Ok(result) => ok_json(&GroupTemplateListResponse::from_record_list(&result)),
        Err(e) => err_internal("Database error", e),
    }
}
