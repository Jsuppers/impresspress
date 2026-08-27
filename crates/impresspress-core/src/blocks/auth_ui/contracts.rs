//! Request/response types for the `/b/auth/api/*` JSON surface.
//!
//! These are the *only* source of the OpenAPI schemas declared in
//! [`super::AuthUiBlock`]'s `BlockInfo::endpoints` — `.input::<T>()` /
//! `.output::<T>()` derive them from the same types the handlers deserialize
//! into and serialize out of. Before this module existed, each handler built
//! its response with an ad-hoc `serde_json::json!` literal and the schema was
//! hand-written alongside it in `mod.rs`, so the two could (and did) drift
//! with nothing to catch it.
//!
//! # Nothing credential-bearing belongs here
//!
//! Every type in this module is serialized straight to an HTTP response. The
//! password hash (`local_credentials.password_hash`), the refresh-token row's
//! `token_hash`, the verification-token digest and the JWT blocklist `jti` all
//! live on rows these handlers read — and none of them appears on any type
//! below. That is why the user-facing shapes are hand-written view types
//! ([`AuthenticatedUser`], [`SignupUser`], [`MeUser`]) rather than
//! `repo::users::UserRow` re-exported: a view type cannot silently grow a
//! column when the table does.
//!
//! The access/refresh tokens on [`LoginResponse`] / [`SignupResponse`] /
//! [`RefreshResponse`] are the *product* of these endpoints, not a leak — they
//! are what the caller came for, and they were already in the hand-written
//! schemas.
//!
//! # `#[schemars(required)]` on `Option<T>` is not decoration
//!
//! Same rule as `blocks::products::contracts`: `.output::<T>()` generates
//! under schemars' **serialize** contract, which gets `required` right but not
//! nullability — `Option<T>`'s `JsonSchema` impl calls `allow_null`
//! unconditionally. `#[schemars(required)]` is the one lever that drops the
//! `null` branch, and on an `Option<T>` paired with
//! `skip_serializing_if = "Option::is_none"` it does *not* also force the
//! property into `required`. That is exactly the shape the optional halves of
//! [`SignupResponse`] have: absent on one code path, never `null` on either.
//! Do not strip these.

use serde::{Deserialize, Serialize};

// Modelled as a single-variant enum rather than a `String` so a handler cannot
// emit anything else: the schema's constant and the Rust value are the same
// fact. Renders as `{"type": "string", "enum": ["Bearer"]}` — schemars' spelling
// of the `{"const": "Bearer"}` the hand-written schema used, same meaning.
/// The only `token_type` this API issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum TokenType {
    Bearer,
}

/// `POST /b/auth/api/login` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LoginRequest {
    #[schemars(extend("format" = "email"))]
    pub email: String,
    pub password: String,
}

// A deliberate projection of `repo::users::UserRow` — `disabled`, `deleted_at`,
// `email_verified`, `updated_at` and the auth-version counter are
// account-lifecycle state that no authenticated caller needs and that a row
// re-export would have published. Keep this a view type.
/// The caller's identity, as returned by a successful authentication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AuthenticatedUser {
    pub id: String,
    pub email: String,
    pub roles: Vec<String>,
    pub name: String,
}

/// `POST /b/auth/api/login` response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: TokenType,
    /// Access token lifetime in seconds
    pub expires_in: u64,
    /// Role-aware post-login redirect path
    pub default_redirect: String,
    pub user: AuthenticatedUser,
}

/// `POST /b/auth/api/signup` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SignupRequest {
    #[schemars(extend("format" = "email"))]
    pub email: String,
    pub password: String,
    /// Optional display name
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

// Distinct from `AuthenticatedUser` because the verification-required and
// already-registered paths answer before any role lookup happens. On the
// already-registered path `id` is deliberately the empty string — [SEC-035]
// forbids confirming that the address exists.
/// The new account. `roles` and `name` are present only on the auto-login
/// path; the verification-required reply carries `id` and `email` alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SignupUser {
    pub id: String,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub roles: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub name: Option<String>,
}

/// `POST /b/auth/api/signup` response body.
///
/// Auto-logs in (issues tokens) unless email verification is required, in
/// which case only email_verified/message/user are returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SignupResponse {
    pub email_verified: bool,
    /// Present when verification is required or the email is already registered
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub token_type: Option<TokenType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub expires_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub default_redirect: Option<String>,
    pub user: SignupUser,
}

/// `POST /b/auth/api/logout` response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LogoutResponse {
    pub message: String,
}

// Same projection rule as `AuthenticatedUser` — a view type, never the row.
// `avatar_url` is flattened from the row's `Option<String>` to `""`, so the
// wire type is a plain string and the key is always present.
/// The caller's own profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MeUser {
    pub id: String,
    pub email: String,
    pub name: String,
    pub roles: Vec<String>,
    #[schemars(extend("format" = "date-time"))]
    pub created_at: String,
    pub avatar_url: String,
}

/// `GET /b/auth/api/me` and `PATCH /b/auth/api/me` response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MeResponse {
    pub user: MeUser,
}

/// `PATCH /b/auth/api/me` request body. Every field is optional and only the
/// ones present are applied. `name` and `avatar_url` are the only
/// user-editable profile fields: email changes go through verification and
/// roles through the admin API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UpdateMeRequest {
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

/// `POST /b/auth/api/refresh` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// `POST /b/auth/api/refresh` response body: the rotated token pair only.
/// Refresh does not re-resolve `default_redirect` or the user projection, so
/// neither is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefreshResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: TokenType,
    /// Access token lifetime in seconds
    pub expires_in: u64,
}
