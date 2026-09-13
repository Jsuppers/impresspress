//! `impresspress/signal` — WebRTC signalling rooms for blockfarming's online
//! play (spec `2026-09-11-visual-redesign-design.md` §18.3-§18.4).
//!
//! A room code, an offer, and an answer: the smallest store that can hold a
//! WebRTC handshake between two browsers that have never met. Every endpoint
//! is public — blockfarming has no account and no login, so an endpoint that
//! required one would be an endpoint the game cannot call. Rate limiting per
//! remote address (`UserRateLimiter`, category `signal`) is the abuse guard
//! in place of auth.
//!
//! Not a relay, not a lobby, not a matchmaker, not a message bus: it holds at
//! most two strings per room for at most ten minutes and hands each of them
//! over once. See [`service`] for the store and [`rest`] for the handlers.

pub(crate) mod migrations;
pub mod rest;
pub mod service;

use std::time::Duration;

use wafer_run::{BlockInfo, HttpMethod, InstanceMode};

use crate::{
    blocks::rate_limit::{
        check_rate_limit, ip_identity, RateLimit, RateLimitOutcome, UserRateLimiter,
    },
    endpoint_match::{self, EndpointRoute},
    http::err_not_found,
};

/// In-block dispatch targets, one per declared HTTP endpoint.
#[derive(Clone, Copy)]
enum Route {
    Config,
    PostOffer,
    GetOffer,
    PostAnswer,
    GetAnswer,
}

fn code_path_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["code"],
        "properties": {
            "code": {
                "type": "string",
                "description": "Six-character room code (see GET /b/signal/config for its length)"
            }
        }
    })
}

/// Request body of both `POST .../offer` and `POST .../answer`.
fn sdp_request_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["sdp"],
        "properties": {
            "sdp": {"type": "string", "description": "Session description (offer or answer)"}
        }
    })
}

/// Response body of both `GET .../offer` and `GET .../answer`.
fn sdp_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "sdp": {
                "type": ["string", "null"],
                "description": "The stored SDP, or null while the answer is still pending"
            }
        }
    })
}

/// Response body of both `POST .../offer` and `POST .../answer` on success.
fn ok_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {"ok": {"type": "boolean"}}
    })
}

/// Response body of `GET /b/signal/config`.
fn config_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "ice_servers": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"urls": {"type": "string"}}
                }
            },
            "room_seconds": {"type": "integer"},
            "code_length": {"type": "integer"}
        }
    })
}

/// The block's HTTP surface: what `handle()` dispatches on and what
/// `info().endpoints` is generated from. `/b/signal/config` first (no path
/// params, so ordering against the `{code}` rows doesn't matter, but it
/// reads as the entry point).
const ROUTES: &[EndpointRoute<Route>] = &[
    EndpointRoute::public(HttpMethod::Get, "/b/signal/config", Route::Config)
        .summary("ICE servers, room lifetime and code length")
        .output(config_response_schema)
        .tags(&["signal"]),
    EndpointRoute::public(
        HttpMethod::Post,
        "/b/signal/rooms/{code}/offer",
        Route::PostOffer,
    )
    .summary("Host puts its offer up — also the room's create")
    .description("409 if the code is already live, 400 on a malformed code or an over-long SDP")
    .path_params(code_path_schema)
    .input(sdp_request_schema)
    .output(ok_response_schema)
    .tags(&["signal"]),
    EndpointRoute::public(
        HttpMethod::Get,
        "/b/signal/rooms/{code}/offer",
        Route::GetOffer,
    )
    .summary("Guest reads the host's offer")
    .description("404 when the code is unknown or expired")
    .path_params(code_path_schema)
    .output(sdp_response_schema)
    .tags(&["signal"]),
    EndpointRoute::public(
        HttpMethod::Post,
        "/b/signal/rooms/{code}/answer",
        Route::PostAnswer,
    )
    .summary("Guest puts its answer up")
    .description("404 if the room is gone, 409 if an answer already stands")
    .path_params(code_path_schema)
    .input(sdp_request_schema)
    .output(ok_response_schema)
    .tags(&["signal"]),
    EndpointRoute::public(
        HttpMethod::Get,
        "/b/signal/rooms/{code}/answer",
        Route::GetAnswer,
    )
    .summary("Host polls for the guest's answer")
    .description(
        "null sdp while waiting; once answered, the answer is returned and the room is \
         deleted — single-use. 404 once the room is gone",
    )
    .path_params(code_path_schema)
    .output(sdp_response_schema)
    .tags(&["signal"]),
];

crate::impresspress_feature_block! {
    /// WebRTC signalling rooms for blockfarming's online play
    /// (`impresspress/signal`).
    pub struct SignalBlock;
    fields: { limiter: UserRateLimiter },
    name: "impresspress/signal",
    info: |_this| {
        use wafer_run::CollectionSchema;

        BlockInfo::new(
            "impresspress/signal",
            "0.0.1",
            "http-handler@v1",
            "WebRTC signalling rooms",
        )
        .instance_mode(InstanceMode::Singleton)
        .requires(vec!["wafer-run/database".into()])
        // No `grants(..)`: the block reads and writes only its own table
        // (`impresspress__signal__rooms`), so it needs no `ResourceGrant`.
        .collections(vec![CollectionSchema::new(service::TABLE)])
        .category(wafer_run::BlockCategory::Feature)
        .description(
            "Signalling for peer-to-peer WebRTC handshakes: a six-character room code \
             holds one host offer and then one guest answer, for ten minutes. Not a \
             relay, not a lobby, not a matchmaker.",
        )
        .endpoints(endpoint_match::declare(ROUTES))
        .config_keys(service::config_vars())
        .can_disable(true)
        .default_enabled(true)
    },
    handle: |this, ctx, msg, input| {
        // Public and unauthenticated by necessity — the game has no account
        // — so the bucket is the remote address rather than a user, and it
        // is checked before dispatch so a flood cannot reach the store at
        // all. 60 a minute is a handshake plus its polling with room to
        // spare; operators tune it with WAFER_RUN_SHARED__RATE_LIMIT_SIGNAL
        // like every other category (`RateLimit::resolve` formats
        // `WAFER_RUN_SHARED__RATE_LIMIT_{NAME}` from the category name
        // passed to `check_rate_limit` below).
        const SIGNAL_LIMIT: RateLimit = RateLimit {
            max_requests: 60,
            window: Duration::from_secs(60),
        };
        if let RateLimitOutcome::Limited(out) =
            check_rate_limit(&this.limiter, ctx, &ip_identity(&msg), "signal", SIGNAL_LIMIT).await
        {
            return out;
        }
        let Some(route) = endpoint_match::dispatch(&mut msg, ROUTES) else {
            return err_not_found("not found");
        };
        match route {
            Route::Config => rest::get_config(ctx).await,
            Route::PostOffer => rest::post_offer(ctx, &msg, input).await,
            Route::GetOffer => rest::get_offer(ctx, &msg).await,
            Route::PostAnswer => rest::post_answer(ctx, &msg, input).await,
            Route::GetAnswer => rest::get_answer(ctx, &msg).await,
        }
    },
    lifecycle: |_this, ctx, event| {
        crate::migration_helper::lifecycle_init(
            ctx,
            &event,
            "impresspress/signal",
            migrations::SQLITE_MIGRATIONS,
            migrations::POSTGRES_MIGRATIONS,
        )
        .await
    },
}

#[cfg(test)]
mod tests {
    use wafer_run::Block as _;

    use super::*;

    /// The game has no account, so every endpoint here must be reachable
    /// without one. A tightened tier would not fail a unit test — it would
    /// 401 the players — so the tier itself is what is asserted.
    #[test]
    fn every_signal_endpoint_is_public() {
        let info = crate::blocks::all_block_infos()
            .into_iter()
            .find(|i| i.name == "impresspress/signal")
            .expect("signal block must be in all_block_infos()");
        assert!(!info.endpoints.is_empty());
        for e in &info.endpoints {
            assert_eq!(
                e.auth,
                wafer_run::AuthLevel::Public,
                "{} must stay public",
                e.path
            );
        }
    }

    /// The three declared vars are the three the handlers read. A fourth
    /// read with no declaration is a setting nobody can see in the admin UI.
    #[test]
    fn the_block_declares_the_config_it_reads() {
        let info = SignalBlock::new().info();
        let keys: Vec<&str> = info.config_keys.iter().map(|v| v.key.as_str()).collect();
        assert!(keys.contains(&service::TTL_KEY));
        assert!(keys.contains(&service::MAX_SDP_KEY));
        assert!(keys.contains(&service::STUN_KEY));
    }
}
