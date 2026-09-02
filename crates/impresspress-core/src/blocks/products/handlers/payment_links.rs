//! Checkout preset and reusable Stripe Payment Link management.

use wafer_core::clients::database::Record;
use wafer_run::{context::Context, InputStream, Message, OutputStream};

use super::offers::{self, OfferAccess, ProductState};
use crate::{
    blocks::products::{
        contracts::{
            CheckoutPresetList, CheckoutPresetRequest, PaymentLinkCreateRequest, PaymentLinkList,
        },
        repo::{checkout_presets, offers as offer_repo, payment_links},
        stripe,
    },
    http::{err_bad_request, ok_json},
};

fn preset_id(msg: &Message) -> &str {
    msg.var("preset_id")
}

fn link_id(msg: &Message) -> &str {
    msg.var("link_id")
}

async fn authorized_offer(
    ctx: &dyn Context,
    msg: &Message,
    access: OfferAccess,
    state: ProductState,
) -> Result<Record, OutputStream> {
    let product = offers::verify_product(ctx, msg, access, state).await?;
    if offers::offer_id(msg).is_empty() {
        return Err(err_bad_request("Missing offer ID"));
    }
    offer_repo::get_for_product(ctx, &product.id, offers::offer_id(msg))
        .await
        .map_err(offers::domain_error)?;
    Ok(product)
}

async fn body<T: serde::de::DeserializeOwned>(input: InputStream) -> Result<T, OutputStream> {
    let raw = input.collect_to_bytes().await;
    serde_json::from_slice(&raw).map_err(|error| err_bad_request(&format!("Invalid body: {error}")))
}

pub(super) async fn list_presets(
    ctx: &dyn Context,
    msg: &Message,
    access: OfferAccess,
) -> OutputStream {
    if let Err(response) = authorized_offer(ctx, msg, access, ProductState::Live).await {
        return response;
    }
    match checkout_presets::list_for_offer(ctx, offers::offer_id(msg)).await {
        Ok(presets) => ok_json(&CheckoutPresetList { presets }),
        Err(error) => offers::domain_error(error),
    }
}

pub(super) async fn get_preset(
    ctx: &dyn Context,
    msg: &Message,
    access: OfferAccess,
) -> OutputStream {
    if let Err(response) = authorized_offer(ctx, msg, access, ProductState::Live).await {
        return response;
    }
    if preset_id(msg).is_empty() {
        return err_bad_request("Missing preset ID");
    }
    match checkout_presets::get_for_offer(ctx, offers::offer_id(msg), preset_id(msg)).await {
        Ok(preset) => ok_json(&preset),
        Err(error) => offers::domain_error(error),
    }
}

pub(super) async fn create_preset(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
    access: OfferAccess,
) -> OutputStream {
    if let Err(response) = authorized_offer(ctx, msg, access, ProductState::Live).await {
        return response;
    }
    let request: CheckoutPresetRequest = match body(input).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    match checkout_presets::create(ctx, offers::offer_id(msg), msg.user_id(), &request).await {
        Ok(preset) => ok_json(&preset),
        Err(error) => offers::domain_error(error),
    }
}

pub(super) async fn update_preset(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
    access: OfferAccess,
) -> OutputStream {
    if let Err(response) = authorized_offer(ctx, msg, access, ProductState::Live).await {
        return response;
    }
    if preset_id(msg).is_empty() {
        return err_bad_request("Missing preset ID");
    }
    let request: CheckoutPresetRequest = match body(input).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    match checkout_presets::update(ctx, offers::offer_id(msg), preset_id(msg), &request).await {
        Ok(preset) => ok_json(&preset),
        Err(error) => offers::domain_error(error),
    }
}

pub(super) async fn archive_preset(
    ctx: &dyn Context,
    msg: &Message,
    access: OfferAccess,
) -> OutputStream {
    if let Err(response) = authorized_offer(ctx, msg, access, ProductState::Live).await {
        return response;
    }
    if preset_id(msg).is_empty() {
        return err_bad_request("Missing preset ID");
    }
    match checkout_presets::archive(ctx, offers::offer_id(msg), preset_id(msg)).await {
        Ok(preset) => ok_json(&preset),
        Err(error) => offers::domain_error(error),
    }
}

pub(super) async fn list_links(
    ctx: &dyn Context,
    msg: &Message,
    access: OfferAccess,
) -> OutputStream {
    // Widened for the same reason as the offer listing: naming the links of a
    // deleted product is how the caller finds the ones still taking money.
    if let Err(response) = authorized_offer(ctx, msg, access, ProductState::LiveOrDeleted).await {
        return response;
    }
    match payment_links::list_for_offer(ctx, offers::offer_id(msg)).await {
        Ok(links) => ok_json(&PaymentLinkList {
            payment_links: links,
        }),
        Err(error) => offers::domain_error(error),
    }
}

pub(super) async fn create_link(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
    access: OfferAccess,
) -> OutputStream {
    let product = match authorized_offer(ctx, msg, access, ProductState::Live).await {
        Ok(product) => product,
        Err(response) => return response,
    };
    let request: PaymentLinkCreateRequest = match body(input).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    match stripe::create_payment_link(ctx, &product, offers::offer_id(msg), &request).await {
        Ok(link) => ok_json(&link),
        Err(error) => offers::domain_error(error),
    }
}

pub(super) async fn deactivate_link(
    ctx: &dyn Context,
    msg: &Message,
    access: OfferAccess,
) -> OutputStream {
    // Widened: deactivation only ever closes. `create_link` above deliberately
    // stays live-only — opening a new way to charge for a deleted product is
    // the opposite operation.
    if let Err(response) = authorized_offer(ctx, msg, access, ProductState::LiveOrDeleted).await {
        return response;
    }
    if link_id(msg).is_empty() {
        return err_bad_request("Missing Payment Link ID");
    }
    match stripe::deactivate_payment_link(ctx, offers::offer_id(msg), link_id(msg)).await {
        Ok(link) => ok_json(&link),
        Err(error) => offers::domain_error(error),
    }
}
