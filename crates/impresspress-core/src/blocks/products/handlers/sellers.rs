//! Administrator seller governance and product moderation.

use std::collections::HashMap;

use serde_json::Value;
use wafer_block::db::{Filter, FilterOp};
use wafer_core::clients::database::{self as db, Record};
use wafer_run::{context::Context, ErrorCode, Message, OutputStream, WaferError};

use crate::{
    blocks::products::{contracts::OfferStatus, repo, stripe},
    http::{err_bad_request, err_conflict, err_internal, err_not_found, ok_json},
    util::{stamp_updated, RecordExt},
};

fn admin_error(error: WaferError, not_found: &str) -> OutputStream {
    match error.code {
        ErrorCode::NotFound => err_not_found(not_found),
        ErrorCode::InvalidArgument => err_bad_request(&error.message),
        ErrorCode::FailedPrecondition | ErrorCode::Aborted => err_conflict(&error.message),
        _ => err_internal("Seller governance operation failed", error),
    }
}

fn equal(field: &str, value: impl Into<Value>) -> Filter {
    Filter {
        field: field.to_string(),
        operator: FilterOp::Equal,
        value: value.into(),
    }
}

fn owned_by(user_id: &str) -> Vec<Filter> {
    vec![equal("owner_id", Value::String(user_id.to_string()))]
}

/// The seller's LIVE products — what the admin seller detail page lists.
/// A deleted listing is not part of a seller's catalog and does not belong in
/// a catalog view.
async fn seller_live_products(ctx: &dyn Context, user_id: &str) -> Result<Vec<Record>, WaferError> {
    repo::products::list_all(ctx, owned_by(user_id)).await
}

/// EVERY product the seller owns, soft-deleted ones included — what
/// suspension and reactivation act on.
///
/// Deliberately a different read from [`seller_live_products`] rather than a
/// shared one: the two callers want genuinely different sets, and a single
/// read would have to be wrong for one of them. Suspension is a lifecycle and
/// fraud control, so its set is "everything this seller owns": soft delete
/// changes nothing in Stripe, so a deleted product's Prices and Payment Links
/// keep taking money in the connected account until suspension archives them.
async fn seller_every_product(ctx: &dyn Context, user_id: &str) -> Result<Vec<Record>, WaferError> {
    repo::products::list_all_including_deleted(ctx, owned_by(user_id)).await
}

pub(super) async fn list(ctx: &dyn Context) -> OutputStream {
    let records = match db::list_all(ctx, repo::seller_accounts::TABLE, vec![]).await {
        Ok(records) => records,
        Err(error) => return err_internal("Could not list sellers", error),
    };
    let sellers = match records
        .iter()
        .map(repo::seller_accounts::to_contract)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(sellers) => sellers,
        Err(error) => return admin_error(error, "Seller not found"),
    };
    ok_json(&serde_json::json!({"sellers": sellers}))
}

pub(super) async fn get(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = msg.var("id");
    if id.is_empty() {
        return err_bad_request("Missing seller ID");
    }
    let record = match db::get(ctx, repo::seller_accounts::TABLE, id).await {
        Ok(record) => record,
        Err(error) => return admin_error(error, "Seller not found"),
    };
    let seller = match repo::seller_accounts::to_contract(&record) {
        Ok(seller) => seller,
        Err(error) => return admin_error(error, "Seller not found"),
    };
    let products = match seller_live_products(ctx, &seller.user_id).await {
        Ok(products) => products,
        Err(error) => return err_internal("Could not list seller products", error),
    };
    ok_json(&serde_json::json!({"seller": seller, "products": products}))
}

async fn moderate_product(ctx: &dyn Context, msg: &Message, approve: bool) -> OutputStream {
    let id = msg.var("id");
    if id.is_empty() {
        return err_bad_request("Missing product ID");
    }
    let product = match repo::products::get(ctx, id).await {
        Ok(product) => product,
        Err(error) => return admin_error(error, "Product not found"),
    };
    if product.str_field("owner_kind") != "user" {
        return err_conflict("Only seller-owned products use moderation");
    }
    if approve
        && product.str_field("approval_status") == "approved"
        && product.str_field("status") == "active"
    {
        return ok_json(&product);
    }
    if !approve
        && product.str_field("approval_status") == "rejected"
        && product.str_field("status") == "draft"
    {
        return ok_json(&product);
    }
    if product.str_field("approval_status") != "pending"
        || product.str_field("status") != "pending_review"
    {
        return err_conflict("Product is not waiting for moderation");
    }
    if approve {
        if let Err(error) =
            repo::seller_accounts::ready_for_user(ctx, product.str_field("owner_id")).await
        {
            return admin_error(error, "Seller not found");
        }
    }
    let now = chrono::Utc::now().to_rfc3339();
    let mut data = if approve {
        HashMap::from([
            ("approval_status".to_string(), serde_json::json!("approved")),
            ("status".to_string(), serde_json::json!("active")),
            ("published_at".to_string(), serde_json::json!(&now)),
        ])
    } else {
        HashMap::from([
            ("approval_status".to_string(), serde_json::json!("rejected")),
            ("status".to_string(), serde_json::json!("draft")),
            ("published_at".to_string(), serde_json::json!("")),
        ])
    };
    stamp_updated(&mut data);
    match repo::products::update(ctx, id, data).await {
        Ok(product) => ok_json(&product),
        Err(error) => err_internal("Could not moderate product", error),
    }
}

pub(super) async fn approve_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    moderate_product(ctx, msg, true).await
}

pub(super) async fn reject_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    moderate_product(ctx, msg, false).await
}

async fn set_suspended(ctx: &dyn Context, msg: &Message, suspended: bool) -> OutputStream {
    let id = msg.var("id");
    if id.is_empty() {
        return err_bad_request("Missing seller ID");
    }
    let account = match db::get(ctx, repo::seller_accounts::TABLE, id).await {
        Ok(account) => account,
        Err(error) => return admin_error(error, "Seller not found"),
    };
    if (account.str_field("status") == "suspended") == suspended {
        return match repo::seller_accounts::to_contract(&account) {
            Ok(seller) => ok_json(&seller),
            Err(error) => admin_error(error, "Seller not found"),
        };
    }
    let products = match seller_every_product(ctx, account.str_field("user_id")).await {
        Ok(products) => products,
        Err(error) => return err_internal("Could not load seller products", error),
    };
    if suspended {
        for product in &products {
            let offers = match repo::offers::list_for_product(ctx, &product.id).await {
                Ok(offers) => offers,
                Err(error) => return admin_error(error, "Seller product not found"),
            };
            for offer in offers {
                if offer.status != OfferStatus::Archived {
                    if let Err(error) =
                        stripe::archive_offer_catalog(ctx, &product.id, &offer.offer.id).await
                    {
                        return admin_error(error, "Seller product not found");
                    }
                }
            }
        }
    }
    for product in products {
        let mut data = if suspended {
            HashMap::from([
                (
                    "approval_status".to_string(),
                    serde_json::json!("suspended"),
                ),
                ("status".to_string(), serde_json::json!("archived")),
            ])
        } else if product.str_field("approval_status") == "suspended" {
            HashMap::from([
                ("approval_status".to_string(), serde_json::json!("draft")),
                ("status".to_string(), serde_json::json!("draft")),
            ])
        } else {
            continue;
        };
        stamp_updated(&mut data);
        if let Err(error) = repo::products::update(ctx, &product.id, data).await {
            return err_internal("Could not update seller product state", error);
        }
    }
    match repo::seller_accounts::set_admin_suspended(ctx, id, suspended).await {
        Ok(account) => match repo::seller_accounts::to_contract(&account) {
            Ok(seller) => ok_json(&seller),
            Err(error) => admin_error(error, "Seller not found"),
        },
        Err(error) => admin_error(error, "Seller not found"),
    }
}

pub(super) async fn suspend(ctx: &dyn Context, msg: &Message) -> OutputStream {
    set_suspended(ctx, msg, true).await
}

pub(super) async fn reactivate(ctx: &dyn Context, msg: &Message) -> OutputStream {
    set_suspended(ctx, msg, false).await
}
