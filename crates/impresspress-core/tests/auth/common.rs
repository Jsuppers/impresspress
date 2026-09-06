//! Shared test helpers for the `wafer-run/auth` integration tests.
//!
//! `MigrationTestCtx` routes:
//! - `call_block("wafer-run/database", ...)` to a real `DatabaseBlock` wrapping
//!   an in-memory SQLite service.
//! - `call_block("wafer-run/crypto", ...)` to a real `CryptoBlock` wrapping
//!   `Argon2JwtCryptoService`, so tests exercising `crypto::random_bytes` and
//!   `crypto::hash` see the same wire contract as production.
//!
//! Any other block call returns `NotFound` — including `wafer-run/config`,
//! which makes `config::get_default(..., "sqlite")` fall back to the default.
//!
//! [`MigrationTestCtx::config_get`] serves exactly one key,
//! `WAFER_RUN__AUTH__JWT_SECRET`, because the auth service reads it
//! synchronously to verify an access token; it is the same master secret the
//! fixture's crypto service signs with, so a token minted through
//! [`MigrationTestCtx::mint_access_token`] verifies against the key
//! `crypto::verify_access_token` derives.

use std::{collections::HashMap, sync::Arc, time::Duration};

use wafer_run::{context::Context, Block, InputStream, Message, OutputStream, WaferError};

/// The fixture crypto service's master secret. Long enough for the HMAC-SHA256
/// minimum-length check.
pub const TEST_MASTER_SECRET: &str = "test-jwt-secret-padded-to-min-32-bytes-aaaa";

/// The issuer every fixture-minted token carries: `expected_issuer` reads
/// `WAFER_RUN_SHARED__FRONTEND_URL` through the config client, which this
/// fixture does not register, so the declared default is what the verifier
/// compares against.
pub const TEST_ISSUER: &str = "http://localhost:5173";

#[derive(Clone)]
pub struct MigrationTestCtx {
    db_block: Arc<dyn Block>,
    crypto_block: Arc<dyn Block>,
}

impl MigrationTestCtx {
    /// Construct a test context with admin migrations pre-applied.
    ///
    /// Admin's migrations create `impresspress__admin__block_settings`, the
    /// tracking table every other block's `apply_if_blessed` upserts into.
    /// In production this is guaranteed by registration order
    /// (`register_all_static_blocks` puts admin first); here we enforce it
    /// in the fixture so auth tests can call `migrations::apply` without
    /// the call failing on a missing tracking table.
    pub async fn new() -> Self {
        let ctx = Self::raw();
        impresspress_core::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations (bootstraps block_settings)");
        ctx
    }

    fn raw() -> Self {
        let svc = Arc::new(
            wafer_block_sqlite::service::SQLiteDatabaseService::open_in_memory()
                .expect("open in-memory sqlite"),
        );
        let db_block: Arc<dyn Block> = Arc::new(
            wafer_core::service_blocks::database::DatabaseBlock::new(svc),
        );
        let crypto_svc = Arc::new(
            wafer_block_crypto::service::Argon2JwtCryptoService::new(
                // ≥ 32 bytes for HMAC-SHA256 minimum-length check.
                "test-jwt-secret-padded-to-min-32-bytes-aaaa".to_string(),
            )
            .expect("test secret is long enough"),
        );
        let crypto_block: Arc<dyn Block> = Arc::new(
            wafer_core::service_blocks::crypto::CryptoBlock::new(crypto_svc),
        );
        Self {
            db_block,
            crypto_block,
        }
    }

    /// Mint an access JWT the way `auth_ui` mints one: through the fixture's
    /// real crypto service, under the `impresspress/auth-ui` caller identity,
    /// so the service signs with `sign_for(AUTH_UI_BLOCK_ID, ..)` — the
    /// derived key `crypto::verify_access_token` verifies against.
    ///
    /// `sub`, `type`, and `iss` are filled in; `extra` adds or overrides
    /// anything else the case needs (`family`, `roles`, `auth_version`, ...).
    pub async fn mint_access_token(
        &self,
        sub: &str,
        extra: &[(&str, serde_json::Value)],
        ttl: Duration,
    ) -> String {
        let mut claims: HashMap<String, serde_json::Value> = HashMap::new();
        claims.insert("sub".to_string(), serde_json::json!(sub));
        claims.insert("type".to_string(), serde_json::json!("access"));
        claims.insert("iss".to_string(), serde_json::json!(TEST_ISSUER));
        for (k, v) in extra {
            claims.insert((*k).to_string(), v.clone());
        }
        let as_auth_ui = AsAuthUi(self.clone());
        wafer_core::clients::crypto::sign(&as_auth_ui, &claims, ttl)
            .await
            .expect("fixture crypto service signs the access token")
    }
}

/// Sign claims verbatim with the auth-ui-derived key, stamping no `iat`/`exp`.
///
/// The one thing [`MigrationTestCtx::mint_access_token`] cannot express: a
/// token whose `exp` is already in the past. `jwt_sign` always stamps `exp` as
/// `now + expiry` and `Duration` cannot be negative, so an already-expired
/// token has to be assembled from the primitives.
pub fn sign_access_token_expired(sub: &str, exp_unix: i64) -> String {
    use wafer_block_crypto::primitives;

    let derived = primitives::derive_block_key(
        TEST_MASTER_SECRET.as_bytes(),
        impresspress_core::blocks::auth_ui::AUTH_UI_BLOCK_ID,
    );
    let payload = serde_json::json!({
        "sub": sub,
        "type": "access",
        "iss": TEST_ISSUER,
        "exp": exp_unix,
    });
    let header_b64 = primitives::b64url_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload_b64 = primitives::b64url_encode(payload.to_string().as_bytes());
    let signing_input = format!("{header_b64}.{payload_b64}");
    let sig = primitives::hmac_sha256(derived.as_bytes(), signing_input.as_bytes());
    format!("{signing_input}.{}", primitives::b64url_encode(&sig))
}

/// A [`MigrationTestCtx`] that reports `impresspress/auth-ui` as the calling
/// block, which is what selects the per-block HKDF key in the crypto handler
/// (`caller_id: None` would sign with the master key instead, and no
/// production token is ever signed that way). Used only for minting.
struct AsAuthUi(MigrationTestCtx);

#[async_trait::async_trait]
impl Context for AsAuthUi {
    async fn call_block(&self, block_name: &str, msg: Message, input: InputStream) -> OutputStream {
        match block_name {
            "wafer-run/database" => self.0.db_block.handle(self, msg, input).await,
            "wafer-run/crypto" => self.0.crypto_block.handle(self, msg, input).await,
            _ => OutputStream::error(WaferError::new(
                wafer_run::ErrorCode::NotFound,
                format!("block '{block_name}' not registered in test ctx"),
            )),
        }
    }

    fn caller_id(&self) -> Option<&str> {
        Some(impresspress_core::blocks::auth_ui::AUTH_UI_BLOCK_ID)
    }

    fn is_cancelled(&self) -> bool {
        false
    }

    fn config_get(&self, key: &str) -> Option<&str> {
        self.0.config_get(key)
    }

    fn check_resource_access(
        &self,
        _resource: &str,
        _resource_type: wafer_run::ResourceType,
        _is_write: bool,
    ) -> Result<(), WaferError> {
        Ok(())
    }

    fn clone_arc(&self) -> Arc<dyn Context> {
        Arc::new(AsAuthUi(self.0.clone()))
    }
}

#[async_trait::async_trait]
impl Context for MigrationTestCtx {
    async fn call_block(&self, block_name: &str, msg: Message, input: InputStream) -> OutputStream {
        match block_name {
            "wafer-run/database" => self.db_block.handle(self, msg, input).await,
            "wafer-run/crypto" => self.crypto_block.handle(self, msg, input).await,
            _ => OutputStream::error(WaferError::new(
                wafer_run::ErrorCode::NotFound,
                format!("block '{block_name}' not registered in test ctx"),
            )),
        }
    }

    fn is_cancelled(&self) -> bool {
        false
    }

    /// Serves the JWT master secret and nothing else. `AuthServiceImpl` reads
    /// it synchronously (the `csrf.rs` pattern) to verify the access token a
    /// request carries; every other key stays unset so `config::get_default`
    /// falls back to the declared default.
    fn config_get(&self, key: &str) -> Option<&str> {
        (key == impresspress_core::blocks::auth::JWT_SECRET_KEY).then_some(TEST_MASTER_SECRET)
    }

    /// This fixture has no caller identity or WRAP grants, so there is
    /// nothing to enforce — explicitly permissive, overriding the
    /// fail-closed trait default (which exists so an enforcing runtime
    /// can never silently fall back to permissive). Mirrors the pre-WRAP
    /// behaviour of this harness; WRAP-behaviour tests use
    /// `impresspress_core::test_support::TestContext::with_wrap` instead.
    fn check_resource_access(
        &self,
        _resource: &str,
        _resource_type: wafer_run::ResourceType,
        _is_write: bool,
    ) -> Result<(), WaferError> {
        Ok(())
    }

    fn clone_arc(&self) -> Arc<dyn Context> {
        Arc::new(self.clone())
    }
}
