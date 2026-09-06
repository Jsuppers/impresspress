//! AuthServiceImpl — implements the wafer-core `AuthService` trait.
//!
//! Authenticates the credential impresspress actually issues. Every signed-in
//! request carries an access JWT, as an `auth_token` cookie or as an
//! `Authorization: Bearer` header; a token
//! [`crate::crypto::verify_access_token`] accepts resolves to its `sub`. Any
//! other token is looked up as a personal access token in
//! `wafer_run__auth__personal_access_tokens` (whose `last_used_at` is bumped),
//! and a request carrying neither is unauthenticated.
//!
//! `require_role(Admin)` additionally honours an unexpired bootstrap token
//! presented as a Bearer, which is how the first admin is created.
//!
//! The session table is not a credential store: a row there is a login family
//! for the userportal device list (B12), never something a request presents.
//!
//! See `docs/superpowers/specs/2026-04-21-auth-block-design.md` §4 for the
//! cross-block contract and §6 for the bootstrap-token fallback.

use std::sync::{Arc, OnceLock};

use wafer_core::interfaces::auth::service::{
    AuthError, AuthService, Role, TokenScope, UserId, UserProfile,
};
use wafer_run::{context::Context, Message};

use super::repo::{pats, users};

/// Per-block state. Holds a lazy [`Context`] handle so service methods can
/// dispatch messages to `wafer-run/database` etc.
///
/// `ctx` is populated lazily because `AuthServiceImpl` is constructed at
/// block-registration time (when no `Context` exists yet) and the framework
/// `AuthBlock::lifecycle(Init)` later passes one in via
/// [`AuthService::init`]. The Init hook calls `ctx.clone_arc()` (wafer-run
/// #46) and stores the resulting `Arc<dyn Context>` in the cell.
#[derive(Clone)]
pub struct BlockState {
    /// Lazy auth context handle. Populated by [`AuthServiceImpl::init`] from
    /// the framework AuthBlock's `lifecycle(Init)` hook; tests pre-populate
    /// via [`BlockState::for_test`].
    pub ctx: Arc<OnceLock<Arc<dyn Context>>>,
}

impl BlockState {
    /// Production constructor — context cell starts empty and is populated
    /// later by [`AuthServiceImpl::init`] when the framework AuthBlock fires
    /// its `Init` lifecycle event.
    // `dyn Context` only requires `MaybeSend + MaybeSync` (real `Send + Sync`
    // on native, a no-op marker on wasm32 — see wafer_block::compat), so this
    // `Arc` doesn't promise cross-thread safety on wasm32; it's a shared
    // handle, not a thread-safety claim, and wasm32 is single-threaded.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new() -> Self {
        Self {
            ctx: Arc::new(OnceLock::new()),
        }
    }

    /// Test-only constructor. Pre-populates the context cell so service
    /// methods can run without going through the full `init` lifecycle.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn for_test(ctx: Arc<dyn Context>) -> Self {
        let cell = OnceLock::new();
        let _ = cell.set(ctx);
        Self {
            ctx: Arc::new(cell),
        }
    }
}

impl Default for BlockState {
    fn default() -> Self {
        Self::new()
    }
}

/// `AuthService` implementation backed by the auth block's repo layer.
pub struct AuthServiceImpl {
    state: BlockState,
}

impl AuthServiceImpl {
    pub fn new(state: BlockState) -> Self {
        Self { state }
    }

    /// Borrow the lazy context handle. Returns `Err(Internal)` if `init`
    /// hasn't run yet — callers that hit this path are pre-init dispatches
    /// (a `handle` arriving before the framework AuthBlock's
    /// `lifecycle(Init)`), which would only happen on a runtime bug.
    fn ctx(&self) -> Result<&dyn Context, AuthError> {
        self.state
            .ctx
            .get()
            .map(|arc| arc.as_ref())
            .ok_or_else(|| AuthError::Internal("auth service ctx not initialized".to_string()))
    }
}

/// sha256 of a raw token string. Exposed so tests and the (future) session
/// issuance helper in Plan A2 agree on the hash format. Thin wrapper over
/// [`crate::util::sha256`] — there is one canonical sha256
/// implementation in `crate::util` (re-exported from `wafer_block::hash`).
pub fn hash_token(raw: &str) -> Vec<u8> {
    crate::util::sha256(raw.as_bytes()).to_vec()
}

/// Extract a Bearer token from the `Authorization` header.
fn bearer_from(msg: &Message) -> Option<String> {
    let v = msg.header("authorization");
    if v.is_empty() {
        return None;
    }
    v.strip_prefix("Bearer ").map(str::to_owned)
}

/// The credential the request presents: the `Authorization: Bearer` token, or
/// failing that the `auth_token` cookie.
///
/// The cookie fallback is the same one `blocks::router` applies
/// (`router.rs:97-107`), and it has to be repeated here because the router
/// resolves the cookie into a Bearer *value it passes to `handle_request`* —
/// it does not restamp the header onto the `Message`. Without this, a service
/// method reached on a cookie-authenticated request would find no credential
/// at all and answer `Unauthorized` to the one credential the browser
/// actually sends.
///
/// Accepting it here adds no trust: the token still has to pass
/// [`crate::crypto::verify_access_token`] or match a PAT row. The CSRF concern
/// the router documents about cookie-sourced credentials — that a cross-site
/// page can ride them ambiently — is answered before dispatch by
/// `csrf::enforce_origin_policy`, and this path is reached from another
/// block's `auth.*` message rather than from a browser navigation.
fn presented_token(msg: &Message) -> Option<String> {
    if let Some(bearer) = bearer_from(msg).filter(|t| !t.is_empty()) {
        return Some(bearer);
    }
    let cookie = msg.cookie("auth_token");
    (!cookie.is_empty()).then(|| cookie.to_owned())
}

/// Returns `true` iff `expires_at` parses as an RFC3339 timestamp earlier
/// than now. Parsing the timestamp avoids the mixed-format trap of string
/// comparison (`+00:00` vs `Z`) — the auth tables intermix both because
/// some repo helpers write `…Z` and others use `to_rfc3339()`.
///
/// Unparseable inputs are treated as "expired" — a malformed expiry on a
/// session row is safer to reject than silently grant.
fn is_expired(expires_at: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(expires_at) {
        Ok(exp) => chrono::Utc::now() >= exp.with_timezone(&chrono::Utc),
        Err(_) => true,
    }
}

/// Internal credential classification used by all three require_* methods.
enum Creds {
    /// A verified access JWT; the payload is its `sub`, already checked
    /// against the signature, `type`, issuer, blocklist and `auth_version`
    /// rules by [`crate::crypto::verify_access_token`].
    Jwt(String),
    /// A presented token that is not an access JWT, as the sha256 of the raw
    /// token — the lookup key in `wafer_run__auth__personal_access_tokens`.
    Pat(Vec<u8>),
}

/// Classify the credential on `msg`.
///
/// A token that verifies as an access JWT is that; any other token is a
/// candidate PAT; no token at all is unauthenticated. The token is whichever
/// of the `Authorization: Bearer` header and the `auth_token` cookie the
/// request carries ([`presented_token`]). The `wafer_session` cookie this used
/// to accept in a branch of its own was issued by nothing.
///
/// The secret comes from the config snapshot (`ctx.config_get`, the pattern
/// `csrf.rs` uses) and the issuer from `helpers::expected_issuer`, so this
/// service applies exactly the deployment's own token policy — the same two
/// values `pipeline.rs` passes to `extract_auth_meta`.
async fn extract_creds(ctx: &dyn Context, msg: &Message) -> Result<Creds, AuthError> {
    let Some(bearer) = presented_token(msg) else {
        return Err(AuthError::Unauthorized);
    };
    let secret = ctx
        .config_get(super::JWT_SECRET_KEY)
        .unwrap_or("")
        .to_string();
    let expected_iss = super::helpers::expected_issuer(ctx).await;
    if let Some(claims) = crate::crypto::verify_access_token(ctx, &bearer, &secret, &expected_iss)
        .await
        .filter(|c| c.sub.as_deref().is_some_and(|s| !s.is_empty()))
    {
        return Ok(Creds::Jwt(claims.sub.unwrap_or_default()));
    }
    Ok(Creds::Pat(hash_token(&bearer)))
}

/// Static WRAP grants for the framework `wafer-run/auth` block. Returned by
/// both [`AuthService::grants`] (consumed by `AuthBlock::info()` so the
/// runtime registers them at startup) and called directly by userportal
/// pages that reflect over auth's grant list to compose their own WRAP
/// scope. Keep these in sync with the spec at
/// `docs/superpowers/specs/2026-04-21-auth-block-design.md`.
pub fn auth_grants() -> Vec<wafer_block::types::ResourceGrant> {
    // String literals are used (instead of repo::*::TABLE consts) so the
    // static WRAP-grant audit script (scripts/audit-wrap-grants.sh) can
    // resolve every grant target — its const-resolver only follows
    // top-level `super::NAME` paths, not nested module paths like
    // `repo::users::TABLE`. Each literal must stay in sync with the
    // corresponding `pub const TABLE` in repo/*.rs.
    vec![
        // auth-ui owns the SSR / JSON / OAuth handlers and writes every
        // auth table during login, signup, OAuth callback, etc. The
        // wildcard covers users / sessions / pats / provider_links /
        // bootstrap_tokens / orgs / api_keys without enumerating each.
        wafer_run::ResourceGrant::read_write("impresspress/auth-ui", "wafer_run__auth__*"),
        // The pipeline router (ImpresspressRouterBlock, id `impresspress/router`)
        // calls `jwt_blocklist::contains()` from `crate::crypto::extract_auth_meta`
        // during request preprocessing — SEC-042 logout invalidates JWTs
        // via this table. The call runs in the router's context, so the
        // router needs read access. Without it WRAP denies and the
        // contains() fail-closed path treats every JWT as blocklisted,
        // 403-ing every signed-in admin request.
        wafer_run::ResourceGrant::read("impresspress/router", "wafer_run__auth__jwt_blocklist"),
        // P2c: same pipeline-preprocessing shape as the blocklist grant
        // above — `crate::crypto::extract_auth_meta` also calls
        // `blocks::auth::current_auth_version()`, which reads the users row
        // through `repo::users::auth_version()`, in the router's context on
        // every request bearing an access JWT. Without this grant WRAP
        // denies the users-table read and the fail-closed lookup-error
        // branch rejects every token, 403-ing every signed-in request.
        wafer_run::ResourceGrant::read("impresspress/router", "wafer_run__auth__users"),
        // Admin block reads auth tables for the admin dashboards. The
        // wildcard mirrors the legacy AuthBlock grant — admin/pages/users
        // reads users, sessions, AND api_keys (the API-key tab) so the
        // narrower per-table list would regress.
        wafer_run::ResourceGrant::read("impresspress/admin", "wafer_run__auth__*"),
        // Userportal `/b/userportal/sessions` page lists the caller's
        // sessions and revokes individual rows. Read+write because revoke
        // deletes the row; reads are scoped to the caller's user_id by
        // the repo helper.
        wafer_run::ResourceGrant::read_write(
            "impresspress/userportal",
            "wafer_run__auth__sessions",
        ),
        // [B12] The same revoke burns the family's refresh rows before
        // deleting the session row — that is what actually signs the device
        // out. Without this grant every revoke would 500 (or, worse under a
        // swallowing handler, report success while the device kept
        // refreshing). Read+write: `revoke_family` updates the rows, and the
        // ownership check that precedes it reads the session row, not this
        // table.
        wafer_run::ResourceGrant::read_write("impresspress/userportal", "wafer_run__auth__tokens"),
        // Userportal `/b/userportal/security` lists the caller's
        // linked OAuth providers. Read-only — unlinking goes
        // through an auth POST endpoint, not the userportal block.
        wafer_run::ResourceGrant::read(
            "impresspress/userportal",
            "wafer_run__auth__provider_links",
        ),
        wafer_run::ResourceGrant::read_write("impresspress/userportal", "wafer_run__auth__users"),
        wafer_run::ResourceGrant::read("impresspress/products", "wafer_run__auth__users"),
        // Wave 3: rate_limit.rs (called from products + files blocks) writes to
        // wafer_run__auth__rate_limits on the wasm32 (Cloudflare Workers) path.
        // Native uses an in-memory Mutex<HashMap> counter and never touches the DB.
        // auth-ui is already covered by the wildcard grant above.
        wafer_run::ResourceGrant::read_write(
            "impresspress/products",
            "wafer_run__auth__rate_limits",
        ),
        wafer_run::ResourceGrant::read_write("impresspress/files", "wafer_run__auth__rate_limits"),
        // Public ticket submissions fail closed around this durable counter;
        // maintenance also prunes rows older than the bounded retention window.
        wafer_run::ResourceGrant::read_write(
            "impresspress/tickets",
            "wafer_run__auth__rate_limits",
        ),
        // auth-ui's login / signup / refresh handlers read the auth-policy
        // config vars declared by `auth::config` (owned by this block). These
        // are CONFIG resources (uppercase `WAFER_RUN__AUTH__*`), a different
        // namespace from the lowercase `wafer_run__auth__*` DATABASE grant
        // above — so the DB wildcard does not cover them. Without these
        // config-typed grants WRAP denies the reads and auth-ui silently
        // falls back to defaults, so an operator's REQUIRE_VERIFICATION /
        // ALLOWED_EMAIL_DOMAINS / ACCESS_TOKEN_LIFETIME_SECS settings are
        // ignored. `allows_config_key` is an exact-match check, so each key is
        // granted explicitly (no wildcard). Literals kept in sync with
        // `auth::config`'s `*_KEY` consts for the WRAP-grant audit script.
        wafer_run::ResourceGrant::read(
            "impresspress/auth-ui",
            "WAFER_RUN__AUTH__REQUIRE_VERIFICATION",
        )
        .typed(wafer_run::ResourceType::Config),
        wafer_run::ResourceGrant::read(
            "impresspress/auth-ui",
            "WAFER_RUN__AUTH__ALLOWED_EMAIL_DOMAINS",
        )
        .typed(wafer_run::ResourceType::Config),
        wafer_run::ResourceGrant::read(
            "impresspress/auth-ui",
            "WAFER_RUN__AUTH__ACCESS_TOKEN_LIFETIME_SECS",
        )
        .typed(wafer_run::ResourceType::Config),
    ]
}

/// Reject a resolved user id whose account is disabled or soft-deleted.
/// The single lifecycle-state gate shared by every `require_*` credential
/// path — session cookies and PATs resolve a user id without otherwise
/// loading the user row, so without this they would authenticate a
/// deactivated account.
async fn ensure_active(ctx: &dyn Context, user_id: &str) -> Result<(), AuthError> {
    let user = users::find_by_id(ctx, user_id)
        .await
        .map_err(|e| AuthError::Internal(e.to_string()))?
        .ok_or(AuthError::Unauthorized)?;
    if !user.is_active() {
        return Err(AuthError::Unauthorized);
    }
    Ok(())
}

#[wafer_block::wafer_async_trait]
impl AuthService for AuthServiceImpl {
    /// Apply auth migrations and run the bootstrap admin step. Invoked by the
    /// framework `AuthBlock::lifecycle(Init)` (wafer-run #41/#45) once at
    /// startup. Mirrors the body of the custom impresspress `AuthBlock::lifecycle`
    /// so the framework block has a self-sufficient service to delegate to.
    async fn init(&self, ctx: &dyn Context) -> Result<(), AuthError> {
        // Capture an owning `Arc<dyn Context>` so subsequent `require_*`
        // calls have a context handle to dispatch repo lookups through.
        // wafer-run #46 added `Context::clone_arc` for exactly this. `set`
        // returns `Err` if the cell was already populated (e.g. test
        // pre-populated via `for_test`, or a duplicate `Init` event); both
        // cases are harmless — the existing handle keeps pointing at the
        // same shared snapshots.
        let _ = self.state.ctx.set(ctx.clone_arc());

        // Auth's migrations run here (not in a `Block::lifecycle`) because the
        // service `init` needs an `AuthError` return shape, not the
        // `WaferError` that `migration_helper::lifecycle_init` produces — so
        // this calls the shared `apply_migrations` directly with the block's
        // single-source migration consts.
        let sqlite: Vec<&str> = super::migrations::SQLITE_MIGRATIONS
            .iter()
            .map(|(_, sql)| *sql)
            .collect();
        crate::migration_helper::apply_migrations(
            ctx,
            "wafer-run/auth",
            &sqlite,
            super::migrations::POSTGRES_MIGRATIONS,
        )
        .await
        .map_err(|e| AuthError::Internal(format!("auth migrations: {e}")))?;
        let cfg = super::config::AuthConfig::from_ctx(ctx).await;
        super::bootstrap::run(ctx, &cfg)
            .await
            .map_err(|e| AuthError::Internal(e.to_string()))?;
        Ok(())
    }

    /// WRAP grants the auth block declares for downstream consumers. The
    /// framework `AuthBlock::info()` embeds these into `BlockInfo::grants`
    /// (wafer-run #45) so the runtime registers them at startup.
    ///
    /// Delegates to the [`auth_grants`] free function so non-trait callers
    /// (e.g. userportal's WRAP-grant reflection in `pages/sessions.rs` and
    /// `pages/security.rs`) can see the same list without instantiating
    /// the framework block.
    fn grants(&self) -> Vec<wafer_block::types::ResourceGrant> {
        auth_grants()
    }

    async fn require_user(&self, msg: &Message) -> Result<UserId, AuthError> {
        let ctx = self.ctx()?;
        match extract_creds(ctx, msg).await? {
            // The token's own expiry, issuer, blocklist status and
            // `auth_version` were checked by `verify_access_token`; what is
            // left is the account lifecycle, which no token can carry.
            Creds::Jwt(sub) => {
                ensure_active(ctx, &sub).await?;
                Ok(UserId(sub))
            }
            Creds::Pat(h) => {
                let row = pats::find_by_token_hash(ctx, &h)
                    .await
                    .map_err(|e| AuthError::Internal(e.to_string()))?
                    .ok_or(AuthError::Unauthorized)?;
                if let Some(exp) = row.expires_at.as_deref() {
                    if is_expired(exp) {
                        return Err(AuthError::Unauthorized);
                    }
                }
                pats::touch_last_used(ctx, &h)
                    .await
                    .map_err(|e| AuthError::Internal(e.to_string()))?;
                ensure_active(ctx, &row.user_id).await?;
                Ok(UserId(row.user_id))
            }
        }
    }

    async fn require_token(&self, msg: &Message, scope: TokenScope) -> Result<UserId, AuthError> {
        let ctx = self.ctx()?;
        let creds = extract_creds(ctx, msg).await?;
        // Scopes live exclusively on PATs. A session access JWT presented here
        // is a category error — treat it as Forbidden so the caller knows the
        // credentials are valid but wrong type, not just missing.
        let h = match creds {
            Creds::Pat(h) => h,
            Creds::Jwt(_) => return Err(AuthError::Forbidden),
        };
        let row = pats::find_by_token_hash(ctx, &h)
            .await
            .map_err(|e| AuthError::Internal(e.to_string()))?
            .ok_or(AuthError::Unauthorized)?;
        if let Some(exp) = row.expires_at.as_deref() {
            if is_expired(exp) {
                return Err(AuthError::Unauthorized);
            }
        }
        let needed = match scope {
            TokenScope::Publish => "publish",
        };
        if !row.scopes.iter().any(|s| s == needed) {
            return Err(AuthError::Forbidden);
        }
        pats::touch_last_used(ctx, &h)
            .await
            .map_err(|e| AuthError::Internal(e.to_string()))?;
        ensure_active(ctx, &row.user_id).await?;
        Ok(UserId(row.user_id))
    }

    async fn require_role(&self, msg: &Message, role: Role) -> Result<UserId, AuthError> {
        let ctx = self.ctx()?;

        // Bootstrap-token fast path: if the caller presents a Bearer token
        // that matches an unexpired row in `bootstrap_tokens`, grant Admin.
        // Bootstrap tokens are not tied to a user — use a sentinel id.
        // Admin-gated handlers read `role`, not id, at this stage (user-id
        // coupling lands in Plan A2 when bootstrap consumption creates the
        // first real admin user).
        if matches!(role, Role::Admin) {
            if let Some(bearer) = bearer_from(msg) {
                let h = hash_token(&bearer);
                let valid = super::repo::bootstrap_tokens::is_valid(ctx, &h)
                    .await
                    .map_err(|e| AuthError::Internal(e.to_string()))?;
                if valid {
                    return Ok(UserId("bootstrap".to_string()));
                }
            }
        }

        let uid = self.require_user(msg).await?;
        // Admin is determined by the SAME merged role resolution the HTTP
        // `is_admin` path uses — `get_user_roles` merges the inline `users.role`
        // with `user_roles::TABLE` rows. So a user granted admin via the roles
        // table (the admin IAM UI) is admin to trait consumers too, not only to
        // `/b/admin` routes. [F19]
        let has = match role {
            Role::Admin => crate::blocks::auth::helpers::get_user_roles(ctx, &uid.0)
                .await
                .map_err(|e| AuthError::Internal(e.to_string()))?
                .iter()
                .any(|r| r == "admin"),
            Role::User => true, // any authenticated user
        };
        if has {
            Ok(uid)
        } else {
            Err(AuthError::Forbidden)
        }
    }

    async fn user_profile(&self, user: UserId) -> Result<UserProfile, AuthError> {
        let ctx = self.ctx()?;
        let row = users::find_by_id(ctx, &user.0)
            .await
            .map_err(|e| AuthError::Internal(e.to_string()))?
            .ok_or(AuthError::NotFound)?;
        let role = match row.role.as_str() {
            "admin" => Role::Admin,
            _ => Role::User,
        };
        Ok(UserProfile {
            id: UserId(row.id),
            email: row.email,
            display_name: row.display_name,
            avatar_url: row.avatar_url,
            role,
            orgs: Vec::new(), // populated by Plan C
        })
    }
}

#[cfg(test)]
mod tests {
    //! Trait-level dispatch tests for `init` + `grants`. The underlying
    //! `migrations::apply` and `bootstrap::run` helpers have their own
    //! integration tests in `tests/auth/` — what we exercise here is that
    //! `<AuthServiceImpl as AuthService>::init` actually calls them, and
    //! that `grants()` returns the expected consumer set.
    use std::sync::Arc;

    use super::*;
    use crate::test_support::{access_token_for, TestContext, TEST_JWT_SECRET};

    /// A `Message` presenting `token` the way the router presents the
    /// `auth_token` cookie: as an `Authorization: Bearer` header.
    fn bearer_msg(token: &str) -> Message {
        let mut msg = Message::new("auth.require_role");
        msg.set_meta("http.header.authorization", format!("Bearer {token}"));
        msg
    }

    /// `TestContext::with_admin` plus the JWT master secret in the config
    /// snapshot: `extract_creds` reads it synchronously to verify the access
    /// token the request carries.
    async fn with_admin_and_jwt_secret() -> TestContext {
        let mut ctx = TestContext::with_admin().await;
        ctx.set_config(super::super::JWT_SECRET_KEY, TEST_JWT_SECRET);
        ctx
    }

    #[tokio::test]
    async fn init_applies_migrations_and_runs_bootstrap_on_fresh_ctx() {
        // Admin migrations are pre-applied so the `block_settings` tracking
        // table exists — `apply_if_blessed` requires it to upsert the
        // `current_hash` row. In production `register_all_static_blocks`
        // registers admin first, so its Init runs before auth's.
        let ctx = Arc::new(TestContext::with_admin().await);
        let service = AuthServiceImpl::new(BlockState::for_test(ctx.clone()));

        service
            .init(&*ctx)
            .await
            .expect("init applies migrations and runs bootstrap");

        // Migrations applied → users table exists and is queryable.
        // No bootstrap admin env vars → bootstrap no-ops, table stays empty.
        assert_eq!(
            users::count(&*ctx)
                .await
                .expect("users table exists after init"),
            0
        );
    }

    #[tokio::test]
    async fn init_is_idempotent() {
        // Running init twice must be safe — migrations track applied
        // versions and bootstrap short-circuits when users already exist.
        // Admin pre-applied for the same reason as above.
        let ctx = Arc::new(TestContext::with_admin().await);
        let service = AuthServiceImpl::new(BlockState::for_test(ctx.clone()));

        service.init(&*ctx).await.expect("first init");
        service
            .init(&*ctx)
            .await
            .expect("second init is idempotent");
    }

    #[test]
    fn grants_declares_expected_consumers() {
        // We don't need a context for grants(); construct the service with
        // a stub ctx and inspect the returned vec directly.
        let rt = tokio::runtime::Runtime::new().expect("tokio rt");
        let ctx = rt.block_on(async { Arc::new(TestContext::new().await) });
        let service = AuthServiceImpl::new(BlockState::for_test(ctx));

        let grants = service.grants();

        // The grant struct exposes grantee + resource as public fields;
        // we just check coverage of the four consumers.
        let consumers: Vec<&str> = grants.iter().map(|g| g.grantee.as_str()).collect();
        assert!(
            consumers.contains(&"impresspress/admin"),
            "grants must include admin: {consumers:?}"
        );
        assert!(
            consumers.contains(&"impresspress/userportal"),
            "grants must include userportal: {consumers:?}"
        );
        assert!(
            consumers.contains(&"impresspress/products"),
            "grants must include products: {consumers:?}"
        );
        // The pipeline router (impresspress/router) calls
        // jwt_blocklist::contains() during extract_auth_meta to honour
        // SEC-042 (logout invalidates JWT). Without a grant, WRAP denies
        // the read, jwt_blocklist::contains fails closed → true, and
        // every signed-in request is treated as anonymous.
        assert!(
            consumers.contains(&"impresspress/router"),
            "grants must include router (SEC-042 blocklist read): {consumers:?}"
        );
        // Sanity: at least one grant exists per consumer (non-empty list).
        assert!(
            grants.len() >= 5,
            "expected ≥5 grants, got {}",
            grants.len()
        );
    }

    #[tokio::test]
    async fn get_user_roles_reflects_admin_granted_only_via_roles_table() {
        // The scenario F19 fixes: a user whose inline `users.role` is NOT
        // "admin" but who has an admin row in user_roles::TABLE must resolve
        // to admin via the merged resolver that `require_role` now uses.
        use crate::platform_state::user_roles;

        let ctx = Arc::new(TestContext::with_admin().await);
        // Apply auth migrations so the `users` table exists.
        AuthServiceImpl::new(BlockState::for_test(ctx.clone()))
            .init(&*ctx)
            .await
            .expect("auth init applies user-table migrations");

        // A non-admin user (inline role = "user").
        let user = users::insert(
            &*ctx,
            users::NewUser {
                email: "roletable@e.co".into(),
                display_name: "RT".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: false,
                verification_token_hash: None,
            },
        )
        .await
        .expect("insert user");

        // Grant admin ONLY via the roles table.
        user_roles::assign(&*ctx, &user.id, "admin", "")
            .await
            .expect("assign admin via roles table");

        let roles = crate::blocks::auth::helpers::get_user_roles(&*ctx, &user.id)
            .await
            .expect(
                "get_user_roles must succeed (TestContext::with_admin has no WRAP enforcement)",
            );
        assert!(
            roles.iter().any(|r| r == "admin"),
            "merged resolver must see the roles-table admin grant: {roles:?}"
        );
    }

    /// The other direction, and the evidence spec 2.2.3 rests on: a user
    /// whose ONLY admin claim is the inline `users.role` column — no
    /// `user_roles` row at all — still resolves as admin everywhere
    /// authorization is decided. Both signup paths write exactly this shape
    /// (`helpers::initial_role_for` feeds `NewUser.role`; neither writes a
    /// `user_roles` row), which is why the OAuth callback's roles insert
    /// could be deleted rather than copied into password signup. If this
    /// test ever fails, an OAuth-created admin has stopped being an admin.
    #[tokio::test]
    async fn inline_admin_role_alone_satisfies_require_role() {
        use crate::platform_state::user_roles;

        let ctx = Arc::new(with_admin_and_jwt_secret().await);
        let service = AuthServiceImpl::new(BlockState::for_test(ctx.clone()));
        service
            .init(&*ctx)
            .await
            .expect("auth init applies user-table migrations");

        // Exactly what `resolve_user`'s brand-new-user branch and
        // `signup::handle` write for a bootstrap-admin email.
        let user = users::insert(
            &*ctx,
            users::NewUser {
                email: "inline-admin@e.co".into(),
                display_name: "Inline".into(),
                avatar_url: None,
                role: "admin".into(),
                email_verified: false,
                verification_token_hash: None,
            },
        )
        .await
        .expect("insert user");

        assert!(
            user_roles::list_for_user(&*ctx, &user.id)
                .await
                .expect("list grants")
                .is_empty(),
            "signup writes no user_roles row — the initial role is the inline column"
        );

        let roles = crate::blocks::auth::helpers::get_user_roles(&*ctx, &user.id)
            .await
            .expect("merged resolver must succeed");
        assert_eq!(
            roles,
            vec!["admin".to_string()],
            "the inline role is the first thing the merged resolver reads"
        );

        // The token claims no roles at all; the inline column is what has to
        // carry the grant.
        let got = service
            .require_role(&bearer_msg(&access_token_for(&user.id, &[])), Role::Admin)
            .await
            .expect("an inline-role admin must satisfy Role::Admin with no user_roles row");
        assert_eq!(got.0, user.id);
    }

    #[tokio::test]
    async fn require_role_admin_grants_via_roles_table_only() {
        // End-to-end version of the resolver test above: a real access-JWT
        // Message for a user whose inline `users.role` is "user" but who has
        // an admin row in user_roles::TABLE must satisfy
        // `require_role(Role::Admin)`.
        use crate::platform_state::user_roles;

        let ctx = Arc::new(with_admin_and_jwt_secret().await);
        let service = AuthServiceImpl::new(BlockState::for_test(ctx.clone()));
        service
            .init(&*ctx)
            .await
            .expect("auth init applies user-table migrations");

        let user = users::insert(
            &*ctx,
            users::NewUser {
                email: "roletable-e2e@e.co".into(),
                display_name: "RT2".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: false,
                verification_token_hash: None,
            },
        )
        .await
        .expect("insert user");

        user_roles::assign(&*ctx, &user.id, "admin", "")
            .await
            .expect("assign admin via roles table");

        let got = service
            .require_role(&bearer_msg(&access_token_for(&user.id, &[])), Role::Admin)
            .await
            .expect("roles-table-only admin must satisfy Role::Admin");
        assert_eq!(got.0, user.id);
    }

    #[tokio::test]
    async fn ensure_active_rejects_disabled_and_deleted() {
        use crate::test_support::TestContext;

        let ctx = TestContext::with_auth().await.with_wrap(
            "wafer-run/auth",
            vec![],
            "impresspress/admin",
        );

        let live = users::insert(
            &ctx,
            users::NewUser {
                email: "live@e.co".into(),
                display_name: "Live".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: false,
                verification_token_hash: None,
            },
        )
        .await
        .unwrap();
        // Active user → Ok.
        assert!(ensure_active(&ctx, &live.id).await.is_ok());

        // Disabled → Unauthorized.
        users::set_disabled(&ctx, &live.id, true).await.unwrap();
        assert!(matches!(
            ensure_active(&ctx, &live.id).await,
            Err(AuthError::Unauthorized)
        ));

        // Missing user → Unauthorized (not a 500).
        assert!(matches!(
            ensure_active(&ctx, "does-not-exist").await,
            Err(AuthError::Unauthorized)
        ));
    }
}
