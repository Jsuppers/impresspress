//! Public product catalog: `/b/products/catalog` (list of active products)
//! and `/b/products/catalog/{id}` (single active product), both unauthenticated.
//!
//! Both publish `contracts::CatalogProductView`, the public projection of a
//! product row. Its field list is what keeps the ownership, moderation and
//! provider columns off the anonymous surface; see the comment above the
//! type for what is withheld and why.

use wafer_block::db::{Filter, FilterOp, SortField};
use wafer_run::{context::Context, ErrorCode, Message, OutputStream};

use crate::{
    blocks::products::{
        contracts::{CatalogProductListResponse, CatalogProductView, PageQuery},
        repo,
    },
    http::{err_bad_request, err_internal, err_not_found, ok_json},
    util::RecordExt,
};

pub(super) async fn handle_catalog(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let query = PageQuery::from_message(msg);
    let filters = vec![Filter {
        field: "status".to_string(),
        operator: FilterOp::Equal,
        value: serde_json::Value::String("active".to_string()),
    }];
    let sort = vec![SortField {
        field: "name".to_string(),
        desc: false,
    }];
    // The door, not a generic paginated read against the table: this is the
    // anonymous catalog, so a read that skipped `deleted_at IS NULL` would
    // put soft-deleted products back on sale. The typed projection is
    // applied to what the door returns.
    match repo::products::list_page(
        ctx,
        i64::from(query.page),
        i64::from(query.page_size),
        filters,
        Some(sort),
    )
    .await
    {
        Ok(list) => ok_json(&CatalogProductListResponse::from_record_list(&list)),
        Err(e) => err_internal("Database error", e),
    }
}

pub(super) async fn handle_get_product_public(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = {
        let var = msg.var("id");
        if var.is_empty() {
            msg.path()
                .strip_prefix("/b/products/catalog/")
                .unwrap_or("")
        } else {
            var
        }
    };
    if id.is_empty() {
        return err_bad_request("Missing product ID");
    }

    match repo::products::get(ctx, id).await {
        Ok(record) => {
            let status = record.str_field("status");
            if status != "active" {
                return err_not_found("Product not found");
            }
            ok_json(&CatalogProductView::from_record(&record))
        }
        Err(e) if e.code == ErrorCode::NotFound => err_not_found("Product not found"),
        Err(e) => err_internal("Database error", e),
    }
}
