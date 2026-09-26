//! `wafer-run/auth` — service module.
//!
//! Plan A2 PR 5 split the old monolithic `AuthBlock` in two:
//!
//! - The framework `wafer_core::service_blocks::auth::AuthBlock` wraps
//!   `service::AuthServiceImpl` and owns the `wafer-run/auth` block id
//!   (registered via `crate::blocks::register_auth`). It has no HTTP routes.
//! - `crate::blocks::auth_ui::AuthUiBlock` owns every `/b/auth/*` HTTP
//!   route (login, signup, OAuth, API keys, settings, dashboard, orgs, …).
//!
//! What lives in this module after the split:
//!
//! - Module decls for the supporting layers (`bootstrap`, `config`,
//!   `maintenance`, `migrations`, `repo`, `service`).
//! - Constants other blocks still reference (`AUTH_BLOCK_ID`,
//!   `JWT_SECRET_KEY`) and the login timing-equalization hash
//!   (`timing_equalization_hash`). Every auth table is reached through
//!   its own `repo::<table>` module; there are no table-name re-exports
//!   here for a caller to build a query around.
//! - `helpers` — token/cookie/role utilities consumed by `auth_ui::api::*`.
//! - `authenticate_api_key` — called by `crate::pipeline` to populate auth
//!   meta from an `Authorization: Bearer <api-key>` header.

pub mod bootstrap;
pub mod config;
pub mod maintenance;
pub mod migrations;
pub mod repo;
pub mod service;

use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use wafer_core::clients::{config as config_client, crypto};
use wafer_run::WaferError;

use crate::util::hex_encode;

pub const AUTH_BLOCK_ID: &str = "wafer-run/auth";

/// Config key for the JWT signing secret used by the auth block.
/// Owner: the `wafer-run/auth` block. Read by the ImpresspressRouter
/// for token validation and by the Cloudflare adapter to seed the
/// crypto service.
pub const JWT_SECRET_KEY: &str = "WAFER_RUN__AUTH__JWT_SECRET";

use crate::platform_state::user_roles;

// ---------------------------------------------------------------------------
// Timing equalization for password login
// ---------------------------------------------------------------------------
//
// `auth_ui::api::login` must take the same time whether the email is unknown,
// the account carries no local-credentials row, or the password is simply
// wrong; otherwise the response time is a user-enumeration oracle. It gets
// that by verifying a password against a throwaway hash whenever there is no
// real credential it can verify against — none stored, or one the crypto
// service cannot check, which it rejects without doing the work.
//
// That only equalizes anything if the throwaway hash is in the scheme THIS
// deployment's crypto service actually writes, which is why it cannot be a
// constant in this file. Native and Cloudflare hash with argon2id; the browser
// hashes with PBKDF2-SHA256, which holds no working memory, where argon2id's
// stays allocated in the Service Worker's linear memory (shared with sql.js)
// for the worker's life (see `impresspress-browser`'s `crypto` module).
//
// The constant this replaced was an argon2id string, so it was wrong in both
// directions:
//
// - on the browser it did not equalize anything. `compare_hash` used to reject
//   any non-PBKDF2 string as an unsupported format in microseconds, while a
//   real verification ran a full PBKDF2 — so the timings were wildly
//   asymmetric and the defence this exists to provide had never worked there.
// - once the browser's verifier learned to dispatch on the stored scheme (so
//   an argon2id credential written by another target against the same
//   workspace verifies), that same constant started RUNNING argon2id in the
//   Service Worker on every failed login: ~19.9 MiB of linear memory per
//   mistyped email, permanently, because wasm memory never shrinks.
//
// Sourcing it from `crypto::hash` closes both. One hash is computed per
// process/isolate and cached, so a failed login costs exactly one
// verification, the same as a successful one.

/// The password [`timing_equalization_hash`] hashes. Its value is irrelevant
/// and deliberately public: no comparison against the resulting hash can ever
/// authenticate anybody, because `login` only reaches that comparison
/// ([`burn_timing_equalization`]) when it has no credential it can
/// authenticate against, and discards the result (pinned by
/// `login::tests::the_timing_equalization_password_cannot_log_anyone_in`).
pub(crate) const TIMING_EQUALIZATION_PASSWORD: &str =
    "impresspress timing-equalization placeholder; never a credential";

/// Cached for the life of the process (native) or isolate (Cloudflare /
/// Service Worker): the scheme and cost the crypto service writes cannot
/// change under a running runtime, and re-hashing per failed login would make
/// a failed login cost twice what a successful one does — the asymmetry this
/// whole mechanism exists to remove.
static TIMING_EQUALIZATION_HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// A password hash in whatever scheme this deployment's crypto service writes,
/// for `login` to burn a verification against when it has no real credential
/// (or has one the crypto service cannot check).
///
/// `Err` means the crypto service could not produce one at all — an
/// unreachable, refused or broken `wafer-run/crypto` block. The caller does
/// not substitute something in the wrong scheme, which would reintroduce
/// exactly the asymmetry described above; [`burn_timing_equalization`] answers
/// the login with the classified error instead, the same answer a real
/// credential's comparison gets when the crypto service is down.
pub(crate) async fn timing_equalization_hash(
    ctx: &dyn wafer_run::context::Context,
) -> Result<&'static str, WaferError> {
    timing_equalization_hash_in(&TIMING_EQUALIZATION_HASH, ctx).await
}

/// [`timing_equalization_hash`] against a caller-supplied cache. The cache is
/// a parameter only so a test can hold its own: the production cell is filled
/// once per process, which would otherwise make "what does this crypto service
/// write?" answerable only by whichever test ran first.
async fn timing_equalization_hash_in<'a>(
    cache: &'a std::sync::OnceLock<String>,
    ctx: &dyn wafer_run::context::Context,
) -> Result<&'a str, WaferError> {
    if let Some(hash) = cache.get() {
        return Ok(hash.as_str());
    }
    let hash = crypto::hash(ctx, TIMING_EQUALIZATION_PASSWORD).await?;
    Ok(cache.get_or_init(|| hash).as_str())
}

/// Spend one password verification against [`timing_equalization_hash`], so a
/// login with nothing it can verify costs what a wrong password costs.
///
/// The comparison's outcome is discarded: the caller reaches this only when
/// there is nothing to authenticate against, so it can never be a successful
/// login however it returns. (The placeholder password is a public constant;
/// treating a match as a login would sign the caller in as any account whose
/// local-credentials row is missing.) A comparison the crypto service could
/// not run is not discarded, though: [`check_password`] answers a real
/// credential's comparison with the same classified error in that case, and
/// answering this one "invalid credentials" instead would make the response
/// to an unknown email differ from a known one's for as long as the crypto
/// service is down.
pub(crate) async fn burn_timing_equalization(
    ctx: &dyn wafer_run::context::Context,
    password: &str,
) -> Result<(), WaferError> {
    const CONTEXT: &str = "auth: login timing equalization";
    let equalizer = timing_equalization_hash(ctx)
        .await
        .map_err(|e| credential_check_failed(e, CONTEXT))?;
    match classify_comparison(crypto::compare_hash(ctx, password, equalizer).await) {
        Comparison::Matches | Comparison::DoesNotMatch => Ok(()),
        Comparison::MalformedHash(e) => {
            tracing::error!(
                error = %e,
                "the crypto service rejected its own timing-equalization hash as malformed; \
                 failed logins may answer faster for unknown accounts"
            );
            Ok(())
        }
        Comparison::Failed(e) => Err(credential_check_failed(e, CONTEXT)),
    }
}

/// What checking a password against a stored credential found.
pub(crate) enum PasswordCheck {
    Matches,
    /// The password is wrong.
    DoesNotMatch,
    /// The stored hash is one the crypto service cannot check
    /// (`CryptoError::MalformedHash`), so the password is neither right nor
    /// wrong. [`check_password`] has already logged it, with the user id, at
    /// error level; the error is the crypto service's.
    Unverifiable(WaferError),
}

/// Check `password` against `user_id`'s stored `password_hash`.
///
/// `crypto::compare_hash` answers `Unauthenticated` only for a wrong password.
/// A stored hash that is malformed, names an unsupported scheme, or carries
/// cost parameters outside the accepted range is `CryptoError::MalformedHash`
/// (see [`MALFORMED_HASH_PREFIX`]): the account cannot sign in with a password
/// until the hash is replaced, so it is logged at error level with the user id
/// (never the hash) for an operator to find and reset, and the caller decides
/// what the requester is told. Every other failure says nothing about the
/// password or the stored hash — the call refused by WRAP, the service
/// unreachable, or the crypto service's own fault while checking (an `Internal`
/// such as a failed offload to its blocking pool) — and is `Err`, logged and
/// classified by [`credential_check_failed`] (a refusal keeps its 403 or 429,
/// anything else is a 503).
pub(crate) async fn check_password(
    ctx: &dyn wafer_run::context::Context,
    user_id: &str,
    password: &str,
    stored_hash: &str,
) -> Result<PasswordCheck, WaferError> {
    match classify_comparison(crypto::compare_hash(ctx, password, stored_hash).await) {
        Comparison::Matches => Ok(PasswordCheck::Matches),
        Comparison::DoesNotMatch => Ok(PasswordCheck::DoesNotMatch),
        Comparison::MalformedHash(e) => {
            tracing::error!(
                user_id = %user_id,
                error = %e,
                "stored password hash could not be checked; the account needs a password reset"
            );
            Ok(PasswordCheck::Unverifiable(e))
        }
        Comparison::Failed(e) => Err(credential_check_failed(e, "auth: password check")),
    }
}

/// How a `CryptoError::MalformedHash` reads once it has crossed the wire.
///
/// The crypto block sends it as `ErrorCode::Internal` carrying the error's
/// `Display` (`wafer_core::interfaces::crypto::handler::crypto_error_to_wafer`),
/// the same code its own faults travel under, so the message is the only thing
/// that tells a stored hash needing a reset from a transient fault. Pinned
/// against the variant's `Display` by `malformed_hash_prefix_tests`.
const MALFORMED_HASH_PREFIX: &str = "malformed password hash: ";

/// A `crypto::compare_hash` result, by what it says about the credential.
enum Comparison {
    Matches,
    DoesNotMatch,
    MalformedHash(WaferError),
    Failed(WaferError),
}

fn classify_comparison(result: Result<(), WaferError>) -> Comparison {
    match result {
        Ok(()) => Comparison::Matches,
        Err(e) if e.code == wafer_run::ErrorCode::Unauthenticated => Comparison::DoesNotMatch,
        Err(e)
            if e.code == wafer_run::ErrorCode::Internal
                && e.message.starts_with(MALFORMED_HASH_PREFIX) =>
        {
            Comparison::MalformedHash(e)
        }
        Err(e) => Comparison::Failed(e),
    }
}

#[cfg(test)]
mod malformed_hash_prefix_tests {
    use wafer_core::interfaces::crypto::service::CryptoError;

    use super::{classify_comparison, Comparison, MALFORMED_HASH_PREFIX};

    /// The prefix is the variant's own `Display`; a rewording upstream would
    /// otherwise turn every malformed hash into a 503 without a sound.
    #[test]
    fn the_prefix_is_how_malformed_hash_displays() {
        let shown = CryptoError::MalformedHash("argon2: bad params".to_string()).to_string();
        assert!(
            shown.starts_with(MALFORMED_HASH_PREFIX),
            "{shown:?} no longer starts with {MALFORMED_HASH_PREFIX:?}"
        );
    }

    /// Only a malformed hash is one; the crypto service's own `Internal`
    /// faults are failures of the check.
    #[test]
    fn only_a_malformed_hash_is_classified_as_one() {
        let internal = |m: &str| {
            Err(wafer_run::WaferError::new(
                wafer_run::ErrorCode::Internal,
                m,
            ))
        };
        assert!(matches!(
            classify_comparison(internal("malformed password hash: argon2: bad params")),
            Comparison::MalformedHash(_)
        ));
        assert!(matches!(
            classify_comparison(internal("crypto blocking task failed: panicked")),
            Comparison::Failed(_)
        ));
    }
}

#[cfg(test)]
mod timing_equalization_tests {
    use std::sync::{Arc, OnceLock};

    use wafer_block_crypto::service::{Argon2JwtCryptoService, PasswordScheme};

    use super::{timing_equalization_hash, timing_equalization_hash_in};
    use crate::test_support::TestContext;

    /// The `$name$` segment of a PHC-style hash string: `argon2id` natively
    /// and on Cloudflare, `pbkdf2-sha256` in the browser.
    fn scheme_of(hash: &str) -> &str {
        hash.split('$')
            .nth(1)
            .unwrap_or_else(|| panic!("not a PHC-style hash: {hash}"))
    }

    /// A context whose `wafer-run/crypto` block WRITES `scheme`, so the
    /// browser's choice is exercisable on the native test lane.
    async fn ctx_writing(scheme: PasswordScheme) -> TestContext {
        let mut ctx = TestContext::with_auth()
            .await
            .running_as(crate::blocks::auth_ui::AUTH_UI_BLOCK_ID);
        let svc = Arc::new(
            Argon2JwtCryptoService::new("test-jwt-secret-padded-to-min-32-bytes-aaaa".to_string())
                .expect("test secret is long enough")
                .with_password_scheme(scheme),
        );
        let block: Arc<dyn wafer_run::Block> =
            Arc::new(wafer_core::service_blocks::crypto::CryptoBlock::new(svc));
        ctx.register_block("wafer-run/crypto", block);
        ctx
    }

    /// The equalization hash has to be in the scheme the crypto service
    /// actually WRITES, or it equalizes nothing: verifying a hash the platform
    /// never produces takes a different amount of work than verifying one it
    /// does — instantly, if the verifier rejects the scheme outright.
    ///
    /// The PBKDF2 case is the browser's, and the one the constant this
    /// replaced got wrong in both directions: an argon2id string that the
    /// browser's old PBKDF2-only verifier rejected in microseconds (so failed
    /// logins there were never equalized at all), and that its scheme-
    /// dispatching successor now *runs*, allocating ~19.9 MiB of the Service
    /// Worker's linear memory per mistyped email, permanently.
    #[tokio::test]
    async fn the_equalization_hash_names_the_scheme_the_service_writes() {
        for scheme in [
            PasswordScheme::default(),
            PasswordScheme::Pbkdf2Sha256 { iterations: 10_000 },
        ] {
            let ctx = ctx_writing(scheme).await;
            let cache = OnceLock::new();

            let written = wafer_core::clients::crypto::hash(&ctx, "some real password")
                .await
                .expect("crypto block hashes");
            let equalizer = timing_equalization_hash_in(&cache, &ctx)
                .await
                .expect("crypto block is registered, so the equalizer is derivable");

            assert_eq!(
                scheme_of(equalizer),
                scheme_of(&written),
                "the timing-equalization hash ({equalizer}) must name the same scheme \
                 this platform writes ({written}), or a failed login costs a different \
                 amount of work than a successful one"
            );
        }
    }

    /// Derived once per process, not once per failed login: hashing on every
    /// miss would make a failed login cost a hash PLUS a verification, which is
    /// the asymmetry the equalizer exists to remove — and on the browser it
    /// would double the per-miss cost of the most expensive thing that runs
    /// there. A second `crypto::hash` carries a fresh random salt, so an
    /// identical string is proof the cache was used.
    #[tokio::test]
    async fn the_equalization_hash_is_derived_once_and_reused() {
        let ctx = ctx_writing(PasswordScheme::default()).await;
        let cache = OnceLock::new();

        let first = timing_equalization_hash_in(&cache, &ctx)
            .await
            .expect("first")
            .to_string();
        let second = timing_equalization_hash_in(&cache, &ctx)
            .await
            .expect("second");

        assert_eq!(
            first, second,
            "each call re-hashed; a fresh hash would carry a different salt"
        );
    }

    /// A crypto service that cannot hash yields no equalizer rather than a
    /// wrong-scheme stand-in.
    #[tokio::test]
    async fn no_crypto_block_yields_no_equalizer() {
        let ctx = TestContext::with_auth().await;
        let cache = OnceLock::new();

        assert!(timing_equalization_hash_in(&cache, &ctx).await.is_err());
        assert!(
            cache.get().is_none(),
            "a failure must not be cached as an answer"
        );
    }

    /// The production entry point uses the process-wide cell. Kept separate
    /// from the tests above precisely because that cell is shared across every
    /// test in this binary.
    #[tokio::test]
    async fn the_process_wide_entry_point_answers() {
        let ctx = ctx_writing(PasswordScheme::default()).await;
        assert!(timing_equalization_hash(&ctx).await.is_ok());
    }
}

// ---------------------------------------------------------------------------
// auth_version — invalidates already-issued access JWTs on account/role
// changes (P2c: CODE_REVIEW_2026-07-16, "Access JWTs outlive account and role
// changes").
// ---------------------------------------------------------------------------
//
// Every access JWT embeds the minting user's `auth_version`
// (`repo::users::AUTH_VERSION_FIELD`) at issuance — see
// [`helpers::generate_tokens`]. `crate::crypto::extract_auth_meta` (the
// request-auth verify path) rejects a token whose embedded version is behind
// the user's current stored value, so a password change, disable,
// soft-delete, or role change takes effect on the very next request instead
// of waiting out the token's natural expiry.
//
// Reading `auth_version` on every authenticated request would cost a DB read
// per request, so verification goes through the short-lived
// process/isolate-local cache below ([`current_auth_version`]) instead of
// `repo::users::auth_version` directly. [`bump_auth_version`] is the single
// call site every security-relevant mutation uses — it increments the
// column AND drops the cache entry in the same call, so a bump can never
// forget to invalidate. The cache TTL (not the invalidate call) is what
// bounds worst-case staleness: invalidation is same-isolate/process only and
// therefore best-effort across a fleet of warm Cloudflare isolates, but the
// TTL below is short enough that this doesn't matter in practice.

/// How long a cached `auth_version` read is trusted before the next verify
/// re-reads `repo::users`. An internal cache-freshness knob — not
/// config-driven, unlike the access-token lifetime cap in
/// [`config::ACCESS_TOKEN_LIFETIME_SECS_MAX`] — bounding how long a bumped
/// version can still look "current" to a warm isolate that hasn't seen the
/// bump's (best-effort) invalidation.
const AUTH_VERSION_CACHE_TTL_MS: i64 = 5_000;

/// Cap on the cache's entry count before a full clear, mirroring
/// `rate_limit::UserRateLimiter`'s eviction policy — bounds memory in a
/// long-lived native process / warm CF isolate.
const AUTH_VERSION_CACHE_MAX_ENTRIES: usize = 50_000;

struct AuthVersionCacheEntry {
    version: i64,
    cached_at_ms: i64,
}

static AUTH_VERSION_CACHE: std::sync::OnceLock<
    std::sync::Mutex<HashMap<String, AuthVersionCacheEntry>>,
> = std::sync::OnceLock::new();

fn auth_version_cache() -> &'static std::sync::Mutex<HashMap<String, AuthVersionCacheEntry>> {
    AUTH_VERSION_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Resolve `user_id`'s current `auth_version`, serving a cache hit fresher
/// than [`AUTH_VERSION_CACHE_TTL_MS`] or reading through to
/// `repo::users::auth_version` and repopulating the cache. `now_ms` is
/// injectable so tests can simulate TTL expiry deterministically (no real
/// sleep); [`current_auth_version`] is the one production caller and always
/// passes the real clock.
async fn current_auth_version_at(
    ctx: &dyn wafer_run::context::Context,
    user_id: &str,
    now_ms: i64,
) -> Result<i64, WaferError> {
    if let Some(v) = {
        let guard = auth_version_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard
            .get(user_id)
            .filter(|e| now_ms - e.cached_at_ms < AUTH_VERSION_CACHE_TTL_MS)
            .map(|e| e.version)
    } {
        return Ok(v);
    }

    let version = repo::users::auth_version(ctx, user_id).await?;

    let mut guard = auth_version_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if guard.len() >= AUTH_VERSION_CACHE_MAX_ENTRIES {
        guard.clear();
    }
    guard.insert(
        user_id.to_string(),
        AuthVersionCacheEntry {
            version,
            cached_at_ms: now_ms,
        },
    );
    Ok(version)
}

/// Resolve `user_id`'s current `auth_version` through the short-lived cache,
/// using the real wall clock. Called by `crate::crypto::verify_access_token`
/// for every access JWT it checks — the pipeline's and `AuthServiceImpl`'s.
/// See the module docs above.
pub(crate) async fn current_auth_version(
    ctx: &dyn wafer_run::context::Context,
    user_id: &str,
) -> Result<i64, WaferError> {
    current_auth_version_at(ctx, user_id, crate::util::now_millis() as i64).await
}

/// A credential check that could not be completed, as the error the request
/// is answered with.
///
/// Both request-time credential checks read the database —
/// `crate::crypto::verify_access_token` (the JWT blocklist and
/// `auth_version`) and [`authenticate_api_key`] (the key, its user and their
/// roles) — and a failed read decides neither way: the request is not
/// authenticated, and it is not anonymous either, because answering it as
/// anonymous tells a signed-in client it has been signed out. So the request
/// is refused with this. A refusal the database classifier keeps
/// ([`crate::blocks::crud::classify_db_error`]: a WRAP denial's 403 "Access
/// denied", a quota's 429, a duplicate key's 409) is answered as it stands; any other fault is
/// logged under `context` and answered `Unavailable` (503), the status a
/// client retries rather than one that means "sign in again".
pub(crate) fn credential_check_failed(error: WaferError, context: &str) -> WaferError {
    use crate::blocks::crud::{classify_db_error, DbFailure};

    match classify_db_error(error, None, context) {
        DbFailure::Refused(refusal) => refusal.into_error(),
        DbFailure::Internal(fault) => {
            tracing::error!(context = %context, error = %fault, "credential check failed");
            WaferError::new(
                wafer_run::ErrorCode::Unavailable,
                "Authentication is temporarily unavailable",
            )
        }
    }
}

/// Drop `user_id`'s cached `auth_version` entry, if any.
fn invalidate_auth_version_cache(user_id: &str) {
    auth_version_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(user_id);
}

/// Bump `user_id`'s `auth_version` and invalidate its cache entry in one
/// call, so a mutation can never bump the column and forget to invalidate
/// the cache (or vice versa).
///
/// The single call site for every security-relevant mutation: password
/// change (`auth_ui::api::change_password`), disable/soft-delete
/// (`admin::ops::{set_user_disabled,delete_user,update_user_fields}`), and
/// role change (`admin::iam::{handle_assign_role,handle_remove_role,
/// cascade_role_rename}` and `admin::ops::delete_role`).
pub(crate) async fn bump_auth_version(
    ctx: &dyn wafer_run::context::Context,
    user_id: &str,
) -> Result<(), WaferError> {
    repo::users::bump_auth_version(ctx, user_id).await?;
    invalidate_auth_version_cache(user_id);
    Ok(())
}

#[cfg(test)]
mod auth_version_cache_tests {
    use super::*;
    use crate::test_support::TestContext;

    async fn seed(ctx: &TestContext) -> String {
        repo::users::insert(
            ctx,
            repo::users::NewUser {
                email: "cache@example.com".into(),
                display_name: "Cache".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: false,
                verification_token_hash: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn fresh_read_matches_the_stored_column() {
        let ctx = TestContext::with_auth().await;
        let uid = seed(&ctx).await;
        assert_eq!(current_auth_version(&ctx, &uid).await.unwrap(), 0);

        repo::users::bump_auth_version(&ctx, &uid).await.unwrap();
        // Cache was never populated with a stale value for this uid before
        // the bump, so an uncached read must see the new column value.
        invalidate_auth_version_cache(&uid);
        assert_eq!(current_auth_version(&ctx, &uid).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn cache_hit_serves_stale_value_within_ttl() {
        let ctx = TestContext::with_auth().await;
        let uid = seed(&ctx).await;

        // Populate the cache at version 0.
        assert_eq!(current_auth_version_at(&ctx, &uid, 1_000).await.unwrap(), 0);

        // Bump the column directly (bypassing the wrapper, so the cache is
        // NOT invalidated) — simulates a bump landing on another
        // isolate/process that this cache hasn't heard about yet.
        repo::users::bump_auth_version(&ctx, &uid).await.unwrap();

        // Still within the TTL window from the first read: the cache must
        // serve the stale (pre-bump) value, not re-read the DB.
        assert_eq!(
            current_auth_version_at(&ctx, &uid, 1_000 + AUTH_VERSION_CACHE_TTL_MS - 1)
                .await
                .unwrap(),
            0,
            "a fresh cache entry must be served as-is within its TTL"
        );
    }

    #[tokio::test]
    async fn expired_cache_entry_is_not_served_past_ttl() {
        let ctx = TestContext::with_auth().await;
        let uid = seed(&ctx).await;

        // Populate the cache at version 0, timestamped at now=1_000.
        assert_eq!(current_auth_version_at(&ctx, &uid, 1_000).await.unwrap(), 0);

        // Bump without invalidating (see previous test).
        repo::users::bump_auth_version(&ctx, &uid).await.unwrap();

        // Once the TTL has elapsed, the stale cache entry must NOT be served
        // — the read must fall through to the DB and see the bumped value.
        let past_ttl = 1_000 + AUTH_VERSION_CACHE_TTL_MS + 1;
        assert_eq!(
            current_auth_version_at(&ctx, &uid, past_ttl).await.unwrap(),
            1,
            "a cache entry older than its TTL must not be served — must re-read the DB"
        );
    }

    #[tokio::test]
    async fn bump_auth_version_wrapper_invalidates_immediately() {
        let ctx = TestContext::with_auth().await;
        let uid = seed(&ctx).await;

        // Populate the cache at version 0.
        assert_eq!(current_auth_version(&ctx, &uid).await.unwrap(), 0);

        // The wrapper bumps AND invalidates in one call.
        bump_auth_version(&ctx, &uid).await.unwrap();

        // A read immediately after (well within what would otherwise be the
        // TTL window) must see the new value — proving invalidation, not
        // TTL expiry, made this visible.
        assert_eq!(
            current_auth_version(&ctx, &uid).await.unwrap(),
            1,
            "bump_auth_version must invalidate the cache so the bump is visible immediately"
        );
    }
}

// --- Shared helpers used by auth_ui::api::* and auth_ui::oauth::* ---

/// Token / cookie / role / role-mint helpers shared by the auth_ui HTTP
/// handlers.
///
/// **`auth_method` values** stamped onto access + refresh JWTs (see
/// [`generate_tokens`]) — handlers that care about authentication strength
/// match on these strings:
/// - `"password"` — email + password login or signup.
/// - `"oauth.<provider>"` — OAuth callback. `<provider>` is one of
///   `google`, `github`, `microsoft`.
/// - `"bootstrap"` — bootstrap-token redemption (see [`bootstrap`]).
pub(crate) mod helpers {
    use super::*;
    use crate::{
        blocks::auth::config::{ALLOWED_EMAIL_DOMAINS_KEY, BOOTSTRAP_ADMIN_EMAIL_KEY},
        config_vars::{ALLOW_SIGNUP_KEY, ENVIRONMENT_KEY, FRONTEND_URL_KEY},
    };

    /// Resolve `user_id`'s merged role set: the inline `users.role` (the
    /// bootstrap path) plus any rows in the legacy `user_roles::TABLE`
    /// (multi-role history / admin-IAM grants), deduped since both can
    /// produce `"admin"` for the bootstrapped admin.
    ///
    /// Both reads propagate `Err` instead of swallowing it (SB-3): a WRAP
    /// denial or transient DB error on `user_roles::TABLE` must not look
    /// identical to "user has no roles" — that would silently 403 every
    /// admin (`AuthServiceImpl::require_role`), re-attempt the admin grant
    /// on every login (`ensure_admin_role`), and stamp empty roles on
    /// API keys (`authenticate_api_key`). `NotFound` on the inline-role read
    /// is the one case that is genuinely "no role from this source", not a
    /// failure, and stays non-fatal.
    pub(crate) async fn get_user_roles(
        ctx: &dyn wafer_run::context::Context,
        user_id: &str,
    ) -> Result<Vec<String>, WaferError> {
        let mut roles: Vec<String> = Vec::new();
        if let Some(user) = repo::users::find_by_id(ctx, user_id).await? {
            if !user.role.is_empty() {
                roles.push(user.role);
            }
        }

        let grants = user_roles::list_for_user(ctx, user_id)
            .await
            .map_err(|e| repo::db_failed("get_user_roles: roles table lookup", e))?;
        for grant in grants {
            if !roles.contains(&grant.role) {
                roles.push(grant.role);
            }
        }
        Ok(roles)
    }

    /// Resolve user roles, idempotently granting `admin` if the user's email
    /// matches the configured `WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL`
    /// and they don't already have it.
    ///
    /// This closes a real footgun: roles are normally only assigned at signup,
    /// so changing the configured admin email after a user already exists
    /// never elevates them. With this helper, every login re-checks the rule
    /// and grants admin once when appropriate.
    ///
    /// Intentionally **upgrade-only**: never removes a role, never demotes.
    /// Unsetting the admin email does not revoke admin from anyone — that has
    /// to be done explicitly via the admin UI / DB. Removing roles silently
    /// on login would be an availability foot-gun (one typo in env locks
    /// everyone out).
    ///
    /// Propagates the read's own [`wafer_run::WaferError`] (SB-3) when the
    /// underlying roles read fails — a WRAP denial or DB error must not be
    /// mistaken for "user has no admin row yet" and drive a grant attempt
    /// into `user_roles::TABLE`.
    pub(crate) async fn ensure_admin_role(
        ctx: &dyn wafer_run::context::Context,
        user_id: &str,
        email: &str,
    ) -> Result<Vec<String>, WaferError> {
        // Read the bootstrap-admin email *before* the role lookup. The
        // common case in production is "unset" — early-return then,
        // skipping the second `db::create` path entirely. Authenticated
        // routes mint tokens often enough that the saved DB reads accumulate.
        let admin_email = config_client::get_default(ctx, BOOTSTRAP_ADMIN_EMAIL_KEY, "").await?;

        let mut roles = get_user_roles(ctx, user_id).await?;

        if admin_email.is_empty()
            || !email.eq_ignore_ascii_case(&admin_email)
            || roles.iter().any(|r| r == "admin")
        {
            return Ok(roles);
        }

        // Email matches and admin role is missing — grant it, through the
        // table's single writer, with no admin behind the grant. A concurrent
        // login of the same account can win the insert between the read above
        // and this write; `assign` then answers `AlreadyAssigned`, and the
        // user holds admin all the same.
        match user_roles::assign(ctx, user_id, "admin", "").await {
            Ok(user_roles::Assigned::Created(_)) => {
                tracing::info!(
                    user_id = %user_id,
                    email = %email,
                    "granted admin role on login (email matches ADMIN_EMAIL)"
                );
                roles.push("admin".to_string());
            }
            Ok(user_roles::Assigned::AlreadyAssigned) => roles.push("admin".to_string()),
            Err(e) => {
                tracing::warn!(
                    user_id = %user_id,
                    "failed to grant admin role on login: {e}"
                );
            }
        }
        Ok(roles)
    }

    /// Whether new-account registration is allowed
    /// (`WAFER_RUN_SHARED__ALLOW_SIGNUP`, default on). The single signup toggle
    /// across the JSON signup endpoint and the OAuth callback's
    /// brand-new-user branch — `WAFER_RUN_SHARED__AUTH__SIGNUP_ENABLED` was a
    /// dead duplicate with the opposite default and has been removed.
    pub(crate) async fn signup_allowed(
        ctx: &dyn wafer_run::context::Context,
    ) -> Result<bool, WaferError> {
        crate::config_vars::get_bool(ctx, ALLOW_SIGNUP_KEY, true).await
    }

    /// Whether `email`'s domain is permitted to register.
    ///
    /// When `WAFER_RUN__AUTH__ALLOWED_EMAIL_DOMAINS` is unset (the default)
    /// every domain is allowed. When set to a comma-separated allow-list, only
    /// matching domains pass. `email` is expected pre-lowercased; the domain is
    /// the substring after the last `@` (empty for a malformed address, which
    /// then fails a non-empty allow-list).
    pub(crate) async fn email_domain_allowed(
        ctx: &dyn wafer_run::context::Context,
        email: &str,
    ) -> Result<bool, WaferError> {
        let allowed = config_client::get_default(ctx, ALLOWED_EMAIL_DOMAINS_KEY, "").await?;
        if allowed.is_empty() {
            return Ok(true);
        }
        let domain = email.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
        Ok(allowed.split(',').any(|d| d.trim() == domain))
    }

    /// The role a newly registered user should receive: `"admin"` when `email`
    /// matches the configured `WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL`,
    /// otherwise `"user"`. Shared by the JSON signup and OAuth-callback create
    /// paths so the bootstrap-admin rule can't drift between them.
    pub(crate) async fn initial_role_for(
        ctx: &dyn wafer_run::context::Context,
        email: &str,
    ) -> Result<&'static str, WaferError> {
        use super::config::BOOTSTRAP_ADMIN_EMAIL_KEY;
        let admin_email = config_client::get_default(ctx, BOOTSTRAP_ADMIN_EMAIL_KEY, "").await?;
        Ok(
            if !admin_email.is_empty() && email.eq_ignore_ascii_case(&admin_email) {
                "admin"
            } else {
                "user"
            },
        )
    }

    /// Resolve the configured access-token lifetime (SEC-042). Reads
    /// `WAFER_RUN__AUTH__ACCESS_TOKEN_LIFETIME_SECS`; falls back to the
    /// declared default (30 min) if unset or unparseable, and is always
    /// clamped to [`config::ACCESS_TOKEN_LIFETIME_SECS_MAX`] (P2c) — an admin
    /// cannot configure this past the hard cap. A failed read is returned.
    pub(crate) async fn access_token_lifetime_secs(
        ctx: &dyn wafer_run::context::Context,
    ) -> Result<u64, WaferError> {
        use super::config::{
            ACCESS_TOKEN_LIFETIME_SECS_DEFAULT, ACCESS_TOKEN_LIFETIME_SECS_KEY,
            ACCESS_TOKEN_LIFETIME_SECS_MAX,
        };
        let raw = config_client::get_default(ctx, ACCESS_TOKEN_LIFETIME_SECS_KEY, "").await?;
        Ok(raw
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(ACCESS_TOKEN_LIFETIME_SECS_DEFAULT)
            .min(ACCESS_TOKEN_LIFETIME_SECS_MAX))
    }

    /// Returns (access_token, refresh_token, family).
    ///
    /// `auth_method` records *how* the user authenticated for this token —
    /// `"password"` for email/password login or signup, `"oauth.<provider>"`
    /// for OAuth (e.g. `"oauth.github"`). The claim rides on both access and
    /// refresh tokens so it survives refresh, letting downstream gates (like
    /// the wafer registry's publish endpoint) require a stronger method.
    ///
    /// `family` selects the refresh-token rotation family for the SEC-039
    /// reuse-detection ladder: pass `None` to mint a brand-new family (initial
    /// login / signup / OAuth / bootstrap), or `Some(existing)` to re-issue
    /// within an established family on refresh rotation so the new refresh
    /// JWT's `family` claim agrees with the DB row that anchors reuse
    /// detection.
    ///
    /// Access tokens carry a random `jti` so logout can blocklist the
    /// in-flight JWT (SEC-042) without affecting other live sessions for
    /// the same user.
    ///
    /// Refresh tokens carry one too, for a different reason: everything else
    /// on a refresh JWT is the same on both sides of a rotation — same user,
    /// same family, same auth method, same issuer — and `iat`/`exp` are whole
    /// seconds. The signer encodes claims canonically, so without a
    /// per-token nonce a rotation inside the second its predecessor was
    /// minted in signs to exactly the predecessor's bytes, and its
    /// `token_hash` collides with the row the rotation has just revoked.
    ///
    /// Access tokens also carry the user's current `auth_version` (P2c) —
    /// see the module-level docs above `current_auth_version` — so a later
    /// password-change/disable/role-change bump invalidates this token on
    /// verify instead of only at its natural expiry.
    pub(crate) async fn generate_tokens(
        ctx: &dyn wafer_run::context::Context,
        lifetime: &SessionLifetime,
        user_id: &str,
        email: &str,
        roles: &[String],
        auth_method: &str,
        family: Option<&str>,
    ) -> std::result::Result<(String, String, String), wafer_run::OutputStream> {
        // Two per-token random ids: the access token's (SEC-042: logout
        // revokes a single JWT without touching the user's other live
        // sessions) and the refresh token's (what makes one rotation's
        // output differ from the mint before it — see the doc comment).
        // Both, plus the family id when minting a brand-new family
        // (`family` is `None`), come out of ONE `crypto::random_bytes` call
        // split into 16-byte pieces, because each call is a host round-trip
        // on Cloudflare.
        let want = if family.is_some() { 32 } else { 48 };
        let bytes = match crypto::random_bytes(ctx, want).await {
            Ok(bytes) => bytes,
            Err(e) => return Err(wafer_run::OutputStream::error(e)),
        };
        let (family, jti, refresh_jti) = match family {
            Some(f) => (
                f.to_string(),
                hex_encode(&bytes[..16]),
                hex_encode(&bytes[16..]),
            ),
            None => (
                hex_encode(&bytes[..16]),
                hex_encode(&bytes[16..32]),
                hex_encode(&bytes[32..]),
            ),
        };

        let access_lifetime_secs = access_token_lifetime_secs(ctx)
            .await
            .map_err(wafer_run::OutputStream::error)?;

        // [SEC-038] Stamp `iss` on every token we mint so the read side can
        // reject tokens minted by a different deployment (e.g. a sibling
        // env's leaked secret) instead of trusting any signature with the
        // same HMAC key.
        let issuer = expected_issuer(ctx)
            .await
            .map_err(wafer_run::OutputStream::error)?;

        // [P2c] Embed the user's *current* auth_version so a subsequent
        // password-change/disable/role-change bump (`bump_auth_version`)
        // invalidates this token on verify (`crate::crypto::extract_auth_meta`)
        // instead of only at its natural expiry. Always read fresh here
        // (never from `current_auth_version`'s verify-side cache) so a
        // freshly minted token reflects the true value, not a stale cache
        // hit — a lookup failure fails the mint closed rather than risk
        // embedding a version the caller can't vouch for.
        let auth_version = repo::users::auth_version(ctx, user_id)
            .await
            .map_err(|e| crate::blocks::crud::db_error_internal(e, "auth_version lookup failed"))?;

        let mut access_claims = BTreeMap::new();
        access_claims.insert(
            "user_id".to_string(),
            serde_json::Value::String(user_id.to_string()),
        );
        access_claims.insert(
            "sub".to_string(),
            serde_json::Value::String(user_id.to_string()),
        );
        access_claims.insert(
            "email".to_string(),
            serde_json::Value::String(email.to_string()),
        );
        access_claims.insert("roles".to_string(), serde_json::json!(roles));
        access_claims.insert(
            "type".to_string(),
            serde_json::Value::String("access".to_string()),
        );
        access_claims.insert(
            "auth_method".to_string(),
            serde_json::Value::String(auth_method.to_string()),
        );
        access_claims.insert("jti".to_string(), serde_json::Value::String(jti));
        access_claims.insert("iss".to_string(), serde_json::Value::String(issuer.clone()));
        // [B12] The rotation family, carried on the access token as well as
        // the refresh token. It is what tells the userportal sessions page
        // which listed device is the one making the request; reading it off
        // the request's own cookie instead would let any caller paint the
        // "current session" badge on another user's row.
        access_claims.insert(
            "family".to_string(),
            serde_json::Value::String(family.clone()),
        );
        access_claims.insert(
            repo::users::AUTH_VERSION_FIELD.to_string(),
            serde_json::json!(auth_version),
        );

        let access_token = crypto::sign(
            ctx,
            &access_claims,
            Duration::from_secs(access_lifetime_secs),
        )
        .await
        .map_err(wafer_run::OutputStream::error)?;

        let mut refresh_claims = BTreeMap::new();
        refresh_claims.insert(
            "user_id".to_string(),
            serde_json::Value::String(user_id.to_string()),
        );
        refresh_claims.insert(
            "sub".to_string(),
            serde_json::Value::String(user_id.to_string()),
        );
        refresh_claims.insert(
            "type".to_string(),
            serde_json::Value::String("refresh".to_string()),
        );
        refresh_claims.insert(
            "family".to_string(),
            serde_json::Value::String(family.clone()),
        );
        refresh_claims.insert(
            "auth_method".to_string(),
            serde_json::Value::String(auth_method.to_string()),
        );
        refresh_claims.insert("iss".to_string(), serde_json::Value::String(issuer.clone()));
        // The nonce that makes this token its own. Never read back: a refresh
        // token is identified by the `token_hash` of the whole JWT
        // (`repo::tokens`), and `verify_access_token` refuses anything whose
        // `type` is not `access` before it looks at a `jti` at all.
        refresh_claims.insert("jti".to_string(), serde_json::Value::String(refresh_jti));

        let refresh_token =
            crypto::sign(ctx, &refresh_claims, Duration::from_secs(lifetime.ttl_secs))
                .await
                .map_err(wafer_run::OutputStream::error)?;

        Ok((access_token, refresh_token, family))
    }

    /// [SEC-038] Resolve the canonical JWT `iss` value for this deployment.
    ///
    /// `WAFER_RUN_SHARED__FRONTEND_URL` doubles as the issuer: it's the only
    /// per-deployment URL admins reliably set, and treating it as the issuer
    /// means a token minted in dev (`http://localhost:5173`) won't validate
    /// against a production secret if one leaks between environments.
    pub(crate) async fn expected_issuer(
        ctx: &dyn wafer_run::context::Context,
    ) -> Result<String, WaferError> {
        config_client::get_default(ctx, FRONTEND_URL_KEY, "http://localhost:5173").await
    }

    /// Persist a freshly minted refresh token.
    ///
    /// Stores only the SHA-256 hash of the raw JWT (SEC-032); the JWT itself
    /// never lands in the database. New families start at `generation = 0`;
    /// rotation from `auth_ui::api::refresh::handle` calls this with the same
    /// `family` and `generation = prev + 1` (SEC-039).
    ///
    /// The row is the token: `refresh::handle` looks the presented JWT up by
    /// hash and refuses it when no row matches. A failure here therefore has
    /// to abort issuance rather than be logged — handing the caller a JWT with
    /// no row gives them a credential that is already dead, and on rotation it
    /// is worse, because the predecessor row was revoked first and the family
    /// has no live generation left to refresh from.
    pub(crate) async fn store_refresh_token(
        ctx: &dyn wafer_run::context::Context,
        lifetime: &SessionLifetime,
        user_id: &str,
        token: &str,
        family: &str,
        generation: i64,
    ) -> Result<(), WaferError> {
        super::repo::tokens::insert(
            ctx,
            user_id,
            token,
            family,
            generation,
            &lifetime.expires_at,
        )
        .await
    }

    /// The `; Secure` attribute, or nothing on a development deployment that
    /// serves plain HTTP (where a `Secure` cookie would be dropped by the
    /// browser and nothing would work).
    ///
    /// Every cookie this app sets shares the rule, so it is resolved here
    /// rather than re-derived from `WAFER_RUN_SHARED__ENVIRONMENT` per cookie.
    pub(crate) async fn cookie_secure_attribute(
        ctx: &dyn wafer_run::context::Context,
    ) -> Result<&'static str, WaferError> {
        let env = config_client::get_default(ctx, ENVIRONMENT_KEY, "development").await?;
        Ok(if env.to_lowercase() == "development" {
            ""
        } else {
            "; Secure"
        })
    }

    pub(crate) async fn build_auth_cookie(
        token: &str,
        max_age: u64,
        ctx: &dyn wafer_run::context::Context,
    ) -> Result<String, WaferError> {
        Ok(format!(
            "auth_token={}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}{}",
            token,
            max_age,
            cookie_secure_attribute(ctx).await?
        ))
    }

    /// Resolve the configured minimum signup password length
    /// (`WAFER_RUN_SHARED__AUTH__PASSWORD_MIN_LENGTH`). Falls back to the
    /// declared default (8) if unset or unparseable; a failed read is
    /// returned. Read by the signup
    /// handler so the admin-visible config var is actually enforced instead of
    /// a hardcoded literal.
    pub(crate) async fn password_min_length(
        ctx: &dyn wafer_run::context::Context,
    ) -> Result<usize, WaferError> {
        use super::config::{PASSWORD_MIN_LENGTH_DEFAULT, PASSWORD_MIN_LENGTH_KEY};
        let raw = config_client::get_default(ctx, PASSWORD_MIN_LENGTH_KEY, "").await?;
        Ok(raw
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(PASSWORD_MIN_LENGTH_DEFAULT as usize))
    }

    /// Resolve the configured login lifetime in days
    /// (`WAFER_RUN_SHARED__AUTH__SESSION_LIFETIME_DAYS`) through
    /// [`config::parse_session_lifetime_days`]: the declared default when
    /// unset, and an `Internal` error when the stored value is not one a login
    /// can use.
    ///
    /// An error, not the default and not a clamp. The write surfaces refuse
    /// such a value, so one that is stored arrived around them (the process
    /// environment, a data import, a row older than the bound), and quietly
    /// issuing a lifetime other than the one configured would hide that.
    /// Every login fails with a 500 naming the key until it is corrected.
    pub(crate) async fn session_lifetime_days(
        ctx: &dyn wafer_run::context::Context,
    ) -> Result<u32, WaferError> {
        use super::config::{parse_session_lifetime_days, SESSION_LIFETIME_DAYS_KEY};
        let raw = config_client::get_default(ctx, SESSION_LIFETIME_DAYS_KEY, "").await?;
        parse_session_lifetime_days(&raw).map_err(|e| {
            WaferError::new(
                wafer_run::ErrorCode::Internal,
                format!("{SESSION_LIFETIME_DAYS_KEY} is misconfigured: {e}"),
            )
        })
    }

    /// [B12] How long a login lasts, resolved once per issuance — the one
    /// value the refresh JWT's `exp`, the refresh row's `expires_at` and the
    /// session row's `expires_at` are all taken from.
    ///
    /// [`generate_tokens`] signs the refresh JWT with `ttl_secs`,
    /// [`store_refresh_token`] writes the row's `expires_at`, and
    /// [`issue_tokens_and_cookie`] gives the session row the same expiry, so
    /// the device list cannot claim a session outlives its token. Derived
    /// from [`session_lifetime_days`] rather than a constant of its own: two
    /// constants for one lifetime is what let them disagree (30 days on the
    /// row, 7 on the token).
    ///
    /// Resolving it is the only step of issuance that reads configuration, and
    /// it can fail. So every handler resolves it BEFORE it consumes anything
    /// single-use — the refresh row's compare-and-set claim, a bootstrap token,
    /// an OAuth state — and before signup creates the account: a
    /// misconfigured lifetime then refuses the request and leaves the
    /// credential it presented intact, instead of spending it on an issuance
    /// that was always going to fail.
    pub(crate) struct SessionLifetime {
        ttl_secs: u64,
        /// Now plus `ttl_secs`, in the `…Z` form every auth table uses.
        expires_at: String,
    }

    impl SessionLifetime {
        /// Read and check the configured lifetime.
        ///
        /// Checked addition: `+` on a `DateTime` panics past the last date
        /// chrono can represent, and a panic aborts the native server. The
        /// bound on the lifetime keeps this far from that edge; the check
        /// makes the edge an error rather than a crash should the bound ever be
        /// raised past it.
        pub(crate) async fn resolve(
            ctx: &dyn wafer_run::context::Context,
        ) -> Result<Self, WaferError> {
            let ttl_secs = u64::from(session_lifetime_days(ctx).await?) * 86_400;
            let expires_at = i64::try_from(ttl_secs)
                .ok()
                .and_then(chrono::TimeDelta::try_seconds)
                .and_then(|ttl| chrono::Utc::now().checked_add_signed(ttl))
                .map(|at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .ok_or_else(|| {
                    WaferError::new(
                        wafer_run::ErrorCode::Internal,
                        format!(
                            "refresh-token lifetime of {ttl_secs}s is past the representable \
                             date range"
                        ),
                    )
                })?;
            Ok(Self {
                ttl_secs,
                expires_at,
            })
        }

        /// [`Self::resolve`] for a handler: a failure is the ready-to-return
        /// 500.
        pub(crate) async fn resolve_or_error(
            ctx: &dyn wafer_run::context::Context,
        ) -> Result<Self, wafer_run::OutputStream> {
            Self::resolve(ctx)
                .await
                .map_err(wafer_run::OutputStream::error)
        }
    }

    /// Outcome of [`issue_tokens_and_cookie`]: the freshly minted token pair,
    /// the access-token lifetime (seconds) and the ready-to-set `auth_token`
    /// cookie. Callers add only their response shape (JSON body vs. 302
    /// redirect). The rotation family is persisted internally (on the refresh
    /// row); no caller needs it back, so it is intentionally not surfaced here.
    pub(crate) struct IssuedLogin {
        pub access_token: String,
        pub refresh_token: String,
        pub access_lifetime: u64,
        pub cookie: String,
    }

    /// Shared token-issuance tail for every login flow (password login, signup,
    /// bootstrap redemption, OAuth callback, and refresh rotation).
    ///
    /// [B12] Record this login family on the userportal device list: touch
    /// the row it already has, or insert one when it has none.
    ///
    /// Touch-then-insert rather than a branch on whether the caller passed a
    /// family, because "no row" is not the same question as "new family". A
    /// family can lose its row — migration 012 drops the pre-B12 table and
    /// re-runs whenever any auth migration changes the block's SQL hash, and
    /// the sweeper removes rows whose expiry has passed. Without the fallback
    /// such a device would rotate its tokens forever while never appearing on
    /// the list again.
    ///
    /// Best-effort by design: a failure here loses a list entry, not a
    /// session, and must not turn a successful login into a 500.
    async fn record_login_family(
        ctx: &dyn wafer_run::context::Context,
        user_id: &str,
        family: &str,
        auth_method: &str,
        expires_at: &str,
    ) {
        use super::repo::sessions;

        match sessions::touch(ctx, family, expires_at).await {
            Ok(0) => {
                if let Err(e) = sessions::insert(
                    ctx,
                    sessions::NewSession {
                        family: family.to_string(),
                        user_id: user_id.to_string(),
                        auth_method: auth_method.to_string(),
                        expires_at: expires_at.to_string(),
                    },
                )
                .await
                {
                    tracing::warn!(
                        user_id = %user_id,
                        auth_method = %auth_method,
                        "failed to persist session row for login: {e}"
                    );
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                user_id = %user_id,
                family = %family,
                "failed to touch session row on rotation: {e}"
            ),
        }
    }

    /// The rotation family an issuance belongs to, and the refresh-row
    /// generation it persists.
    #[derive(Clone, Copy)]
    pub(crate) enum Rotation<'a> {
        /// Initial authentication: a brand-new family at generation `0`.
        NewFamily,
        /// Refresh rotation within an established family (SEC-039);
        /// `generation` is the predecessor's plus one.
        Within { family: &'a str, generation: i64 },
    }

    /// Mints the access + refresh JWTs, persists the refresh-token row,
    /// records the login family on the userportal device list, and builds the
    /// `auth_token` cookie — the exact sequence that was previously
    /// copy-pasted across all five handlers (and which the OAuth copy had
    /// drifted from, silently omitting the session row). Centralising it
    /// guarantees every authentication path is visible on the device list.
    ///
    /// `rotation` says which family the tokens belong to: see [`Rotation`].
    ///
    /// [B12] The session row is keyed by that family, not by the access
    /// token, so a rotation touches the row the device already has instead of
    /// adding a forty-eighth one for the day. Its `expires_at` is the refresh
    /// row's, so the list cannot claim a device is signed in after its refresh
    /// token has expired.
    ///
    /// The SESSION row is the only write here that may fail without aborting
    /// issuance: it is a device-list entry, not a credential, so losing it
    /// costs a row on a UX surface and is logged rather than raised. The
    /// REFRESH-token row is a credential — [`store_refresh_token`] explains
    /// why its failure returns an error and no tokens are handed out.
    pub(crate) async fn issue_tokens_and_cookie(
        ctx: &dyn wafer_run::context::Context,
        lifetime: &SessionLifetime,
        user_id: &str,
        email: &str,
        roles: &[String],
        auth_method: &str,
        rotation: Rotation<'_>,
    ) -> std::result::Result<IssuedLogin, wafer_run::OutputStream> {
        let (family, generation) = match rotation {
            Rotation::NewFamily => (None, 0),
            Rotation::Within { family, generation } => (Some(family), generation),
        };
        let (access_token, refresh_token, issued_family) =
            generate_tokens(ctx, lifetime, user_id, email, roles, auth_method, family).await?;

        store_refresh_token(
            ctx,
            lifetime,
            user_id,
            &refresh_token,
            &issued_family,
            generation,
        )
        .await
        .map_err(|e| {
            crate::blocks::crud::db_error_internal(e, "Could not persist the refresh token")
        })?;
        record_login_family(
            ctx,
            user_id,
            &issued_family,
            auth_method,
            &lifetime.expires_at,
        )
        .await;

        // [B12] Retention runs from here because it is the one path every
        // deployment exercises on its own — the Cloudflare Worker has no
        // `scheduled` handler yet (Phase 4) and no operator has to remember to
        // POST anything. It is throttled to at most once an hour, so a login
        // storm costs one sweep.
        super::maintenance::sweep_if_due(ctx).await;

        let access_lifetime = access_token_lifetime_secs(ctx)
            .await
            .map_err(wafer_run::OutputStream::error)?;
        let cookie = build_auth_cookie(&access_token, access_lifetime, ctx)
            .await
            .map_err(wafer_run::OutputStream::error)?;

        Ok(IssuedLogin {
            access_token,
            refresh_token,
            access_lifetime,
            cookie,
        })
    }

    #[cfg(test)]
    mod access_token_lifetime_tests {
        use super::*;
        use crate::{
            blocks::auth::config::ACCESS_TOKEN_LIFETIME_SECS_KEY, test_support::TestContext,
        };

        #[tokio::test]
        async fn unset_falls_back_to_default() {
            let ctx = TestContext::new().await;
            assert_eq!(
                access_token_lifetime_secs(&ctx).await.expect("config read"),
                config::ACCESS_TOKEN_LIFETIME_SECS_DEFAULT
            );
        }

        #[tokio::test]
        async fn honors_a_value_under_the_cap() {
            let mut ctx = TestContext::new().await;
            ctx.set_config(ACCESS_TOKEN_LIFETIME_SECS_KEY, "60");
            assert_eq!(
                access_token_lifetime_secs(&ctx).await.expect("config read"),
                60
            );
        }

        #[tokio::test]
        async fn clamps_a_value_over_the_cap() {
            // P2c: an admin configuring an absurdly long-lived access token
            // must not be able to defeat the belt-and-suspenders backstop —
            // the resolved lifetime never exceeds the hard cap.
            let mut ctx = TestContext::new().await;
            ctx.set_config(
                ACCESS_TOKEN_LIFETIME_SECS_KEY,
                &(config::ACCESS_TOKEN_LIFETIME_SECS_MAX * 10).to_string(),
            );
            assert_eq!(
                access_token_lifetime_secs(&ctx).await.expect("config read"),
                config::ACCESS_TOKEN_LIFETIME_SECS_MAX
            );
        }
    }

    #[cfg(test)]
    mod generate_tokens_auth_version_tests {
        use super::*;
        use crate::test_support::TestContext;

        /// A `TestContext` with auth migrations applied and a real crypto
        /// block registered, so `generate_tokens`'s `crypto::sign` /
        /// `crypto::random_bytes` calls (and `crypto::verify` in these tests)
        /// have somewhere to dispatch to.
        async fn ctx_with_crypto() -> TestContext {
            TestContext::with_auth_and_crypto().await
        }

        async fn seed_user(ctx: &TestContext) -> String {
            repo::users::insert(
                ctx,
                repo::users::NewUser {
                    email: "mint@example.com".into(),
                    display_name: "Mint".into(),
                    avatar_url: None,
                    role: "user".into(),
                    email_verified: false,
                    verification_token_hash: None,
                },
            )
            .await
            .unwrap()
            .id
        }

        /// The access token carries the same `family` the refresh token does.
        /// Without it the userportal cannot tell which listed device is the
        /// one making the request, and `current_session_family` would have to
        /// read an unverified value off the cookie.
        #[tokio::test]
        async fn minted_access_token_carries_the_refresh_family() {
            let ctx = ctx_with_crypto().await;
            let uid = seed_user(&ctx).await;

            let Ok((access_token, refresh_token, family)) = generate_tokens(
                &ctx,
                &SessionLifetime::resolve(&ctx)
                    .await
                    .expect("session lifetime"),
                &uid,
                "mint@example.com",
                &["user".to_string()],
                "password",
                None,
            )
            .await
            else {
                panic!("mint tokens failed")
            };

            let access = crypto::verify(&ctx, &access_token)
                .await
                .expect("verify access token");
            let refresh = crypto::verify(&ctx, &refresh_token)
                .await
                .expect("verify refresh token");
            assert_eq!(
                access.get("family").and_then(|v| v.as_str()),
                Some(family.as_str()),
                "the access token must carry the family the refresh token anchors"
            );
            assert_eq!(
                refresh.get("family").and_then(|v| v.as_str()),
                Some(family.as_str())
            );
        }

        /// The refresh token carries its own `jti`, not the access token's.
        /// Its nonce is what makes a rotation's output differ from the mint
        /// before it (see `generate_tokens`); a refresh token that reused the
        /// access token's id would still differ per mint, so this pins the
        /// two ids as separate draws rather than one value stamped twice.
        #[tokio::test]
        async fn minted_refresh_token_carries_a_jti_of_its_own() {
            let ctx = ctx_with_crypto().await;
            let uid = seed_user(&ctx).await;

            let Ok((access_token, refresh_token, _family)) = generate_tokens(
                &ctx,
                &SessionLifetime::resolve(&ctx)
                    .await
                    .expect("session lifetime"),
                &uid,
                "mint@example.com",
                &["user".to_string()],
                "password",
                None,
            )
            .await
            else {
                panic!("mint tokens failed")
            };

            let jti = |claims: &BTreeMap<String, serde_json::Value>| {
                claims
                    .get("jti")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            };
            let access = crypto::verify(&ctx, &access_token)
                .await
                .expect("verify access token");
            let refresh = crypto::verify(&ctx, &refresh_token)
                .await
                .expect("verify refresh token");
            let access_jti = jti(&access).expect("the access token carries a jti");
            let refresh_jti = jti(&refresh).expect("the refresh token carries a jti");
            assert_eq!(refresh_jti.len(), 32, "a 16-byte nonce, hex: {refresh_jti}");
            assert_ne!(
                refresh_jti, access_jti,
                "the refresh token's jti must be its own draw, not the access token's"
            );
        }

        #[tokio::test]
        async fn minted_access_token_embeds_the_users_current_auth_version() {
            let ctx = ctx_with_crypto().await;
            let uid = seed_user(&ctx).await;

            // Bump twice before minting — the token must embed 2, not 0.
            bump_auth_version(&ctx, &uid).await.unwrap();
            bump_auth_version(&ctx, &uid).await.unwrap();

            let Ok((access_token, _refresh_token, _family)) = generate_tokens(
                &ctx,
                &SessionLifetime::resolve(&ctx)
                    .await
                    .expect("session lifetime"),
                &uid,
                "mint@example.com",
                &["user".to_string()],
                "password",
                None,
            )
            .await
            else {
                panic!("mint tokens failed")
            };

            let claims = crypto::verify(&ctx, &access_token)
                .await
                .expect("verify minted token");
            assert_eq!(
                claims
                    .get(repo::users::AUTH_VERSION_FIELD)
                    .and_then(|v| v.as_i64()),
                Some(2),
                "minted access token must embed the user's current auth_version at mint time"
            );
        }
    }
}

/// Authenticate a request using an API key.
///
/// Hashes the key with SHA-256, looks it up in the database by key_hash,
/// checks it's not revoked/expired, and sets auth meta on the message.
/// Sets nothing if the key is invalid (the request continues as
/// unauthenticated), matching JWT behavior. A lookup that fails — the key,
/// its user or their roles — is `Err`, as [`credential_check_failed`]
/// classifies it, and the pipeline answers the request with it: demoting the
/// request to anonymous would tell the key's holder it had been revoked.
pub async fn authenticate_api_key(
    ctx: &dyn wafer_run::context::Context,
    api_key: &str,
    msg: &mut wafer_run::Message,
) -> Result<(), WaferError> {
    use wafer_run::*;

    use crate::util::sha256_hex;

    let key_hash = sha256_hex(api_key.as_bytes());

    let Some(key_row) = repo::api_keys::find_by_key_hash(ctx, &key_hash)
        .await
        .map_err(|e| credential_check_failed(e, "auth: api key lookup"))?
    else {
        return Ok(());
    };

    // Reject revoked or expired keys.
    if key_row.is_revoked() {
        return Ok(());
    }
    if key_row.is_expired(chrono::Utc::now()) {
        return Ok(());
    }

    // Look up the user to get email and roles.
    if key_row.user_id.is_empty() {
        return Ok(());
    }
    let Some(user) = repo::users::find_by_id(ctx, &key_row.user_id)
        .await
        .map_err(|e| credential_check_failed(e, "auth: api key user lookup"))?
    else {
        return Ok(());
    };

    // Deleted or disabled accounts must not authenticate, even with a
    // still-valid API key. Login/refresh/OAuth already enforce this on their
    // own row loads; this is the same gate for the key path.
    if !user.is_active() {
        return Ok(());
    }

    // Fetch roles from user_roles collection (roles are not stored on the
    // user record). A failed read must not stamp an empty or wrong roles list
    // on an otherwise-valid key (SB-3), so it refuses the request like the
    // lookups above.
    let roles = helpers::get_user_roles(ctx, &key_row.user_id)
        .await
        .map_err(|e| credential_check_failed(e, "auth: api key roles lookup"))?;
    let roles_str = roles.join(",");

    // Set auth meta (same fields as JWT auth)
    msg.set_meta(META_AUTH_USER_ID, &key_row.user_id);
    msg.set_meta(META_AUTH_USER_EMAIL, &user.email);
    msg.set_meta(META_AUTH_USER_ROLES, &roles_str);
    Ok(())
}

#[cfg(test)]
mod api_key_lifecycle_tests {
    use std::collections::HashMap;

    use serde_json::json;
    use wafer_run::{Message, META_AUTH_USER_ID};

    use super::{
        authenticate_api_key,
        repo::{api_keys, users},
    };
    use crate::{test_support::TestContext, util::sha256_hex};

    async fn seed_user_and_key(ctx: &TestContext, raw_key: &str) -> String {
        let user_id = seed_user(ctx, raw_key).await;
        api_keys::insert(
            ctx,
            api_keys::NewApiKey {
                user_id: &user_id,
                name: "test-key",
                key_hash: &sha256_hex(raw_key.as_bytes()),
                key_prefix: "sb_test",
                expires_at: None,
            },
        )
        .await
        .unwrap();
        user_id
    }

    /// One user per key, because the tests that seed two keys would otherwise
    /// collide on the unique email.
    async fn seed_user(ctx: &TestContext, tag: &str) -> String {
        users::insert(
            ctx,
            users::NewUser {
                email: format!("{tag}@e.co"),
                display_name: "Key".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: false,
                verification_token_hash: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    /// A context whose auth block really can read `user_roles`, so a key that
    /// should authenticate does. Without the grant `get_user_roles` fails and
    /// `authenticate_api_key` stamps no meta — which is what a rejected key
    /// looks like too, so an expiry test on this fixture would pass whatever
    /// the expiry check decided.
    ///
    /// The grant is the one the real `impresspress/admin` `BlockInfo`
    /// declares (`ResourceGrant::read_write(AUTH_BLOCK_ID, user_roles::TABLE)`),
    /// which the fixture carries as the runtime does.
    async fn ctx_that_can_read_roles() -> TestContext {
        TestContext::with_auth().await.running_as("wafer-run/auth")
    }

    /// Write an `api_keys` row with `expires_at` exactly as given, bypassing
    /// `NewApiKey`'s typed expiry. Test-fixture setup: this is the shape of
    /// row a deployment already holds, minted when the endpoint stored the
    /// caller's string as it stood, and nothing in the crate can write one
    /// any more.
    async fn seed_key_with_stored_expiry(
        ctx: &TestContext,
        raw_key: &str,
        expires_at: &str,
    ) -> String {
        let user_id = seed_user(ctx, raw_key).await;
        let mut data: HashMap<String, serde_json::Value> = HashMap::new();
        data.insert("user_id".into(), json!(user_id));
        data.insert("name".into(), json!("legacy-key"));
        data.insert("key_hash".into(), json!(sha256_hex(raw_key.as_bytes())));
        data.insert("key_prefix".into(), json!("sb_test"));
        data.insert("created_at".into(), json!("2026-01-01T00:00:00Z"));
        data.insert("expires_at".into(), json!(expires_at));
        wafer_core::clients::database::create(ctx, api_keys::TABLE, data)
            .await
            .expect("seed a legacy api-key row");
        user_id
    }

    /// The user id `authenticate_api_key` stamps for `raw_key`, or `""` when
    /// it refused the key.
    async fn authenticated_as(ctx: &TestContext, raw_key: &str) -> String {
        let mut msg = Message::new("http");
        authenticate_api_key(ctx, raw_key, &mut msg)
            .await
            .expect("the check completes");
        msg.get_meta(META_AUTH_USER_ID).to_string()
    }

    /// `…T20:00:00+09:00` is 11:00 UTC. Compared as text against the clock's
    /// own `…T12:00:00…` it sorted eight hours into the future, so a key an
    /// hour dead authenticated. The offset is fixed at +09:00 and the
    /// instant an hour ago, so the string this writes is always ahead of the
    /// one the clock reads — the divergence is forced, not sampled.
    #[tokio::test]
    async fn a_stored_offset_expiry_is_read_as_the_instant_it_names() {
        let ctx = ctx_that_can_read_roles().await;
        let offset = chrono::FixedOffset::east_opt(9 * 3600).expect("+09:00");
        let expired_an_hour_ago = (chrono::Utc::now() - chrono::Duration::hours(1))
            .with_timezone(&offset)
            .to_rfc3339();
        let live_in_a_month = (chrono::Utc::now() + chrono::Duration::days(30))
            .with_timezone(&offset)
            .to_rfc3339();
        seed_key_with_stored_expiry(&ctx, "raw-offset-dead", &expired_an_hour_ago).await;
        let live_owner =
            seed_key_with_stored_expiry(&ctx, "raw-offset-live", &live_in_a_month).await;

        assert_eq!(
            authenticated_as(&ctx, "raw-offset-dead").await,
            "",
            "{expired_an_hour_ago} named an instant an hour ago"
        );
        // The control: the same offset spelling, still in the future, is
        // honoured — so the assertion above is the expiry check answering,
        // not the fixture refusing every key.
        assert_eq!(
            authenticated_as(&ctx, "raw-offset-live").await,
            live_owner,
            "{live_in_a_month} is a month out"
        );
    }

    /// An expiry that is not a timestamp is not an expiry. `"never"` sorts
    /// after every clock reading there will ever be, so text comparison gave
    /// it the one property a key must not have.
    #[tokio::test]
    async fn a_stored_expiry_that_is_not_a_timestamp_does_not_authenticate() {
        let ctx = ctx_that_can_read_roles().await;
        seed_key_with_stored_expiry(&ctx, "raw-never-key", "never").await;
        let owner =
            seed_key_with_stored_expiry(&ctx, "raw-readable-key", "2099-01-01T00:00:00Z").await;

        assert_eq!(authenticated_as(&ctx, "raw-never-key").await, "");
        // The control: a readable expiry far out still authenticates.
        assert_eq!(authenticated_as(&ctx, "raw-readable-key").await, owner);
    }

    #[tokio::test]
    async fn active_user_key_authenticates() {
        // SB-3: `get_user_roles` surfaces (rather than swallows) a denied read
        // of the admin-owned user_roles::TABLE, which is why this fixture
        // carries the real grant — see `ctx_that_can_read_roles`.
        let ctx = ctx_that_can_read_roles().await;
        let uid = seed_user_and_key(&ctx, "raw-active-key").await;

        let mut msg = Message::new("http");
        authenticate_api_key(&ctx, "raw-active-key", &mut msg)
            .await
            .expect("the check completes");
        assert_eq!(msg.get_meta(META_AUTH_USER_ID), uid);
    }

    #[tokio::test]
    async fn disabled_user_key_is_rejected() {
        let ctx = TestContext::with_auth().await.running_as("wafer-run/auth");
        let uid = seed_user_and_key(&ctx, "raw-disabled-key").await;

        users::set_disabled(&ctx, &uid, true)
            .await
            .expect("disable the key's owner");

        let mut msg = Message::new("http");
        authenticate_api_key(&ctx, "raw-disabled-key", &mut msg)
            .await
            .expect("the check completes");
        // No auth meta stamped → request stays anonymous.
        assert_eq!(msg.get_meta(META_AUTH_USER_ID), "");
    }
}

// SB-3: `get_user_roles` used to swallow both DB reads with `if let
// Ok(...)`, so a WRAP-grant regression or transient DB error yielded an
// empty/partial roles list indistinguishable from "user genuinely has no
// roles" — silently 403ing every admin (`require_role`), re-inserting a
// duplicate admin row on every login (`ensure_admin_role`), and stamping
// empty roles on API keys (`authenticate_api_key`). These tests pin the
// fix: a denied/failed roles read is now an `Err`, not an empty `Vec`.
#[cfg(test)]
mod get_user_roles_error_surfacing_tests {
    use super::helpers::{ensure_admin_role, get_user_roles};
    use crate::test_support::TestContext;

    #[tokio::test]
    async fn denied_roles_table_read_is_an_error_not_empty_roles() {
        // `impresspress__admin__user_roles` is admin-owned. In production,
        // admin's own block-level grant
        // (`ResourceGrant::read_write(AUTH_BLOCK_ID, user_roles::TABLE)` in
        // `blocks/admin/mod.rs`) makes the auth block's read succeed; a block
        // the deployment grants nothing stands in for that grant regressing.
        let ctx = TestContext::with_auth().await.running_as("test/ungranted");

        let res = get_user_roles(&ctx, "some-user-id").await;
        assert!(
            res.is_err(),
            "a denied/failed roles read must be an Err, not empty roles"
        );
    }

    #[tokio::test]
    async fn ensure_admin_role_propagates_denied_roles_read_instead_of_inserting() {
        // If the roles-table read fails, `ensure_admin_role` must not
        // silently treat that as "no admin row yet" and insert a duplicate
        // — it must propagate the error and skip the insert entirely.
        let ctx = TestContext::with_auth().await.running_as("test/ungranted");

        let res = ensure_admin_role(&ctx, "some-user-id", "admin@example.com").await;
        assert!(
            res.is_err(),
            "ensure_admin_role must propagate a denied roles read instead of \
             proceeding to (possibly duplicate-)insert the admin grant"
        );
    }
}
