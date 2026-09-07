//! CryptoService for Cloudflare Workers, on top of wafer-block-crypto.
//!
//! JWT policy — HS256, `exp` required on verify, per-block HKDF-derived
//! keys for `sign_for`/`verify_for`, minimum secret length — delegates to
//! [`Argon2JwtCryptoService`], the same engine the native runtime uses, so
//! tokens are interchangeable across deployment targets. (Historically this
//! service silently dropped per-block key derivation by inheriting the
//! trait's master-key fallbacks, which is exactly the kind of drift behind
//! the PR #155 → #170 production auth regression.)
//!
//! Password hashing is the one deliberate platform divergence: argon2id at
//! [`Argon2Cost::Constrained`] (4 MiB / 2 iters), because Workers'
//! CPU/memory limits rule out the default cost. Written through the shared
//! [`PasswordScheme`] selector so the choice is one value rather than a
//! `hash_password` call site, and verified through
//! [`primitives::verify_password_any_scheme`], which dispatches on the scheme
//! the STORED hash names: the cost comes out of the PHC string, and a PBKDF2
//! credential written by the browser target — a dev-sandbox export, a
//! workspace opened under two runtimes — verifies here instead of being
//! reported as a wrong password.

use std::{collections::HashMap, time::Duration};

use wafer_block_crypto::{
    primitives::{self, Argon2Cost, PasswordScheme},
    service::Argon2JwtCryptoService,
};
use wafer_core::interfaces::crypto::service::{CryptoError, CryptoService};

/// CryptoService backing the CF Worker runtime. See module docs for policy.
pub struct ImpresspressCryptoService {
    jwt_secret: String,
    /// Lazily constructed once, on first use, instead of once per
    /// sign/verify/sign_for/verify_for call — `Argon2JwtCryptoService::new`
    /// re-clones `jwt_secret` and re-validates its length every time it's
    /// called, which is pure overhead after the first (successful or
    /// failed) construction. `OnceLock` degrades to a plain guarded cell on
    /// wasm32 (single-threaded), so this is safe despite every
    /// `CryptoService` method taking `&self`.
    ///
    /// The error side is stored pre-stringified (`String`, not
    /// `CryptoError`) because `CryptoError` isn't `Clone`; a fresh
    /// `CryptoError::Other` is rebuilt from the cached message on every
    /// call after the first, so a missing/short secret still fails
    /// consistently rather than only on the first call.
    jwt_engine: std::sync::OnceLock<Result<Argon2JwtCryptoService, String>>,
}

impl ImpresspressCryptoService {
    pub fn new(jwt_secret: String) -> Self {
        Self {
            jwt_secret,
            jwt_engine: std::sync::OnceLock::new(),
        }
    }

    /// Borrow the shared JWT engine, constructing it on first use. Fails
    /// when the secret is missing or shorter than `MIN_JWT_SECRET_LEN` —
    /// surfaced per operation rather than at worker boot, because the
    /// worker constructs this service before config is necessarily
    /// complete and a broken-auth deployment beats a boot-looping one.
    fn jwt(&self) -> Result<&Argon2JwtCryptoService, CryptoError> {
        self.jwt_engine
            .get_or_init(|| {
                Argon2JwtCryptoService::new(self.jwt_secret.clone()).map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|msg| CryptoError::Other(msg.clone()))
    }
}

/// What this target writes when it hashes a new password. Verification
/// ignores it — see [`CryptoService::compare_hash`] below.
const PASSWORD_SCHEME: PasswordScheme = PasswordScheme::Argon2(Argon2Cost::Constrained);

impl CryptoService for ImpresspressCryptoService {
    fn hash(&self, password: &str) -> Result<String, CryptoError> {
        primitives::hash_password_with(password, PASSWORD_SCHEME)
    }

    /// Verify against whichever scheme the **stored hash** names, not the one
    /// this target writes.
    ///
    /// `primitives::verify_password` (what this called before) is argon2-only
    /// and maps every parse failure to `PasswordMismatch`, so a credential
    /// written by the browser target — PBKDF2, because argon2id is
    /// unaffordable in single-threaded wasm — could never be verified here and
    /// the logs said the user kept mistyping their password. The shared
    /// dispatcher recognises both schemes and answers a distinct
    /// `VerifyError` for one it does not know, which is never an accept.
    fn compare_hash(&self, password: &str, hash: &str) -> Result<(), CryptoError> {
        primitives::verify_password_any_scheme(password, hash)
    }

    fn sign(
        &self,
        claims: HashMap<String, serde_json::Value>,
        expiry: Duration,
    ) -> Result<String, CryptoError> {
        self.jwt()?.sign(claims, expiry)
    }

    fn verify(&self, token: &str) -> Result<HashMap<String, serde_json::Value>, CryptoError> {
        self.jwt()?.verify(token)
    }

    fn sign_for(
        &self,
        block_id: &str,
        claims: HashMap<String, serde_json::Value>,
        expiry: Duration,
    ) -> Result<String, CryptoError> {
        self.jwt()?.sign_for(block_id, claims, expiry)
    }

    fn verify_for(
        &self,
        block_id: &str,
        token: &str,
    ) -> Result<HashMap<String, serde_json::Value>, CryptoError> {
        self.jwt()?.verify_for(block_id, token)
    }

    fn random_bytes(&self, n: usize) -> Result<Vec<u8>, CryptoError> {
        primitives::random_bytes(n)
    }
}

// ─── Password-hash parity ────────────────────────────────────────────────────
//
// Run by the `cloudflare-wasm-test` CI job; they touch no `worker::Env`.
#[cfg(all(test, target_arch = "wasm32"))]
mod password_parity {
    use wafer_core::interfaces::crypto::service::{CryptoError, CryptoService};
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::ImpresspressCryptoService;

    fn svc() -> ImpresspressCryptoService {
        ImpresspressCryptoService::new("test-secret-padded-to-32-bytes-or-more".to_string())
    }

    /// A credential this Worker wrote before the change still verifies, and
    /// this target keeps writing argon2id at the constrained cost — the
    /// Workers CPU/memory limits rule out the default one.
    #[wasm_bindgen_test]
    fn a_worker_written_hash_is_constrained_argon2id_and_verifies() {
        let svc = svc();
        let hash = svc.hash("correct horse battery staple").expect("hash");
        assert!(
            hash.starts_with("$argon2id$v=19$m=4096,t=2,p=1$"),
            "the Worker must keep writing constrained argon2id: {hash}"
        );
        svc.compare_hash("correct horse battery staple", &hash)
            .expect("a freshly written credential must verify");
        match svc.compare_hash("wrong", &hash) {
            Err(CryptoError::PasswordMismatch) => {}
            other => panic!("expected PasswordMismatch, got {other:?}"),
        }
    }

    /// **Fails on the pre-change tree.** `primitives::verify_password` is
    /// argon2-only, so a PBKDF2 credential — what the browser target writes,
    /// and what a dev-sandbox export carries — was reported here as the user
    /// mistyping their password. Fixture: password `correct horse battery
    /// staple`, salt `00..0f`, `i=10000`.
    #[wasm_bindgen_test]
    fn a_pbkdf2_hash_written_by_the_browser_target_verifies_here() {
        const BROWSER_HASH: &str =
            "$pbkdf2-sha256$i=10000$AAECAwQFBgcICQoLDA0ODw==$2flfZcLfnShdJogjAMpb4p4+1QBVZmODXExi4nBRUCI=";
        svc()
            .compare_hash("correct horse battery staple", BROWSER_HASH)
            .expect("a PBKDF2 credential from the browser target must verify");
        match svc().compare_hash("wrong", BROWSER_HASH) {
            Err(CryptoError::PasswordMismatch) => {}
            other => panic!("expected PasswordMismatch, got {other:?}"),
        }
    }

    /// **Fails on the pre-change tree**, which answered `PasswordMismatch`.
    /// A scheme this runtime cannot check is not a wrong password: reporting
    /// it as one tells the logs that a user who can never sign in keeps
    /// mistyping.
    #[wasm_bindgen_test]
    fn an_unknown_scheme_is_a_verify_error_not_a_mismatch() {
        match svc().compare_hash("pw", "$scrypt$ln=16,r=8,p=1$c2FsdA$aGFzaA") {
            Err(CryptoError::VerifyError(msg)) => assert!(
                msg.contains("unrecognised password hash scheme"),
                "unexpected message: {msg}"
            ),
            other => panic!("expected a VerifyError for an unknown scheme, got {other:?}"),
        }
    }

    /// Password hashing must not start depending on the JWT secret: the
    /// Worker constructs this service before config is necessarily complete,
    /// and `jwt()` fails on a missing or short secret by design.
    #[wasm_bindgen_test]
    fn hashing_works_without_a_usable_jwt_secret() {
        let svc = ImpresspressCryptoService::new(String::new());
        let hash = svc.hash("pw").expect("hash without a JWT secret");
        svc.compare_hash("pw", &hash)
            .expect("verify without a JWT secret");
        assert!(
            svc.sign(
                std::collections::HashMap::new(),
                std::time::Duration::from_secs(60)
            )
            .is_err(),
            "signing, unlike hashing, must still refuse an empty secret"
        );
    }
}
