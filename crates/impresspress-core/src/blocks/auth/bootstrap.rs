//! First-run bootstrap for the `wafer-run/auth` block.
//!
//! On `Init`, if the `wafer_run__auth__users` table is empty, [`run`] picks
//! one of three paths based on which bootstrap env vars are set:
//!
//! 1. `BOOTSTRAP_ADMIN_EMAIL` + `BOOTSTRAP_ADMIN_PASSWORD` — hash the password
//!    via `wafer-run/crypto` and create an admin user + `local_credentials`.
//! 2. `BOOTSTRAP_ADMIN_TOKEN` — store sha256(token) in `bootstrap_tokens`
//!    with a 24h expiry. The holder later redeems it by presenting the raw
//!    token as a `Bearer` header (see `AuthServiceImpl::require_role`).
//! 3. None set — log and do nothing; leaves the operator to provision via UI
//!    / CLI.
//!
//! If the `users` table is non-empty, [`run`] is a no-op regardless of env —
//! bootstrap is a first-run mechanism only, never a "re-seed" trigger.

use wafer_core::clients::crypto;
use wafer_run::{context::Context, ErrorCode, WaferError};

use super::{
    config::AuthConfig,
    repo::{bootstrap_tokens, local_credentials, users},
    service::hash_token,
};

/// Run the bootstrap step. Idempotent: returns `Ok(())` without side-effects
/// when the `users` table is already populated.
pub async fn run(ctx: &dyn Context, cfg: &AuthConfig) -> Result<(), WaferError> {
    let user_count = users::count(ctx).await.map_err(internal)?;
    if user_count > 0 {
        tracing::debug!("auth: bootstrap skipped, users table already has {user_count} row(s)");
        return Ok(());
    }

    match (
        cfg.bootstrap_admin_email.as_deref(),
        cfg.bootstrap_admin_password.as_deref(),
        cfg.bootstrap_admin_token.as_deref(),
    ) {
        (Some(email), Some(password), _) => {
            bootstrap_with_email_password(ctx, email, password).await?;
            tracing::info!("auth: bootstrapped admin user: {email}");
        }
        (_, _, Some(token)) => {
            bootstrap_with_token(ctx, token).await?;
            tracing::info!("auth: bootstrap token installed; expires in 24h");
        }
        _ => {
            tracing::info!("auth: no bootstrap admin configured (users table empty)");
        }
    }
    Ok(())
}

pub(crate) async fn bootstrap_with_email_password(
    ctx: &dyn Context,
    email: &str,
    password: &str,
) -> Result<(), WaferError> {
    let hash = crypto::hash(ctx, password).await?;

    // One insert through the repo. The hand-built column map this replaced
    // justified itself with "the legacy columns the rest of impresspress
    // still reads (`name`, `disabled`, `deleted_at`)"; migration 006 creates
    // all three — `disabled INTEGER NOT NULL DEFAULT 0` and a nullable
    // `deleted_at`, which are exactly the values the map wrote — and
    // `users::insert` already dual-writes `name` with `display_name`. The
    // only value the map still carried on its own was `email_verified`,
    // which is now a `NewUser` field.
    //
    // A bootstrapped admin is inherently trusted (the operator set
    // BOOTSTRAP_ADMIN_PASSWORD), so it is verified on creation; without
    // that, /b/userportal/security would show them unverified on first
    // login.
    let user = users::insert(
        ctx,
        users::NewUser {
            email: email.to_string(),
            display_name: "Admin".to_string(),
            avatar_url: None,
            role: "admin".to_string(),
            email_verified: true,
            verification_token_hash: None,
        },
    )
    .await
    .map_err(internal)?;
    local_credentials::insert(ctx, &user.id, &hash, false)
        .await
        .map_err(internal)?;
    Ok(())
}

async fn bootstrap_with_token(ctx: &dyn Context, token: &str) -> Result<(), WaferError> {
    let expires = chrono::Utc::now() + chrono::Duration::hours(24);
    let expires_iso = expires.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    bootstrap_tokens::insert(ctx, hash_token(token), &expires_iso)
        .await
        .map_err(internal)?;
    Ok(())
}

fn internal<E: std::fmt::Display>(e: E) -> WaferError {
    WaferError::new(ErrorCode::Internal, format!("auth bootstrap: {e}"))
}
