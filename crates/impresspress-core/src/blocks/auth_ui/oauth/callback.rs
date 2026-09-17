//! GET /b/auth/oauth/callback — relocated from auth/oauth.rs::handle_oauth_callback
//! in Task 5.

use std::collections::HashMap;

use wafer_core::clients::{config, network};
use wafer_run::{context::Context, Message, OutputStream};

use crate::{
    blocks::{
        auth::{
            config::REQUIRE_VERIFICATION_KEY,
            helpers::{
                email_domain_allowed, ensure_admin_role, initial_role_for, issue_tokens_and_cookie,
                signup_allowed,
            },
            repo::{oauth_pkce, provider_links, users},
        },
        auth_ui::redirect::{default_post_login_redirect, is_safe_local_redirect},
        errors::{error_response, ErrorCode},
    },
    http::{err_bad_request, err_forbidden, err_internal, err_internal_no_cause, ResponseBuilder},
};

pub async fn handle(ctx: &dyn Context, msg: &Message) -> OutputStream {
    // Check ENABLE_OAUTH flag
    let enable_oauth =
        crate::config_vars::get_bool(ctx, "WAFER_RUN_SHARED__ENABLE_OAUTH", false).await;
    if !enable_oauth {
        return err_forbidden("OAuth login is not enabled");
    }

    let code = msg.query("code");
    let state = msg.query("state");
    if code.is_empty() || state.is_empty() {
        return err_bad_request("Missing code or state parameter");
    }

    // The `state` is only half the proof. It says a flow this deployment
    // started is being completed; the binding cookie says it is being
    // completed by the browser that started it. Checked BEFORE the take, so a
    // forged callback cannot burn someone else's single-use state either.
    if !super::state_binding::matches(msg, state) {
        return err_bad_request("OAuth state does not belong to this browser");
    }

    // SEC-040: look up the server-side PKCE state by the opaque `state_id`
    // the provider echoed back. `take` is single-use (DELETE … RETURNING),
    // so a replayed callback or a stolen state_id can only redeem once,
    // and a state past `expires_at` is treated as missing.
    let pkce_row = match oauth_pkce::take(ctx, state).await {
        Ok(Some(row)) => row,
        Ok(None) => return err_bad_request("Invalid or expired OAuth state"),
        Err(e) => return err_internal("OAuth state lookup failed", e),
    };
    let provider = pkce_row.provider.clone();
    let code_verifier = pkce_row.code_verifier.clone();
    // Use the redirect_uri stored at start-time so the provider's exact-
    // match check passes even if the live config changed mid-flow.
    let redirect_uri = pkce_row.redirect_uri.clone();

    let Some(spec) = super::spec::lookup(&provider) else {
        return err_bad_request("Unsupported OAuth provider");
    };

    let client_id = config::get_default(
        ctx,
        &format!(
            "IMPRESSPRESS__AUTH_UI__OAUTH_{}_CLIENT_ID",
            provider.to_uppercase()
        ),
        "",
    )
    .await;
    let client_secret = config::get_default(
        ctx,
        &format!(
            "IMPRESSPRESS__AUTH_UI__OAUTH_{}_CLIENT_SECRET",
            provider.to_uppercase()
        ),
        "",
    )
    .await;

    if client_id.is_empty() || client_secret.is_empty() {
        return err_internal_no_cause("OAuth provider not fully configured");
    }

    // Phase 1: exchange the authorization code for a provider access token.
    let oauth_token = match exchange_code(
        ctx,
        spec,
        code,
        &client_id,
        &client_secret,
        &redirect_uri,
        &code_verifier,
    )
    .await
    {
        Ok(t) => t,
        Err(r) => return r,
    };

    // Phase 2: fetch the user's profile, and what the provider promises
    // about the address on it.
    let info = match fetch_user_info(ctx, spec, &oauth_token).await {
        Ok(i) => i,
        Err(r) => return r,
    };

    // Phase 3: resolve the local user (link / email-merge / create), enforcing
    // the signup, disabled-account and email-verification gates, and upsert
    // the provider link.
    let user_id = match resolve_user(ctx, &provider, &oauth_token, &info).await {
        Ok(id) => id,
        Err(r) => return r,
    };

    // Update last_login_at on the users row (best-effort).
    if let Err(e) = users::touch_last_login(ctx, &user_id).await {
        tracing::warn!("Failed to update last_login_at: {e}");
    }

    let email = info.email;

    // A WRAP denial or DB error here must not silently resolve to "no
    // roles" — that would 403 an admin or double-grant on the next login
    // (SB-3).
    let roles = match ensure_admin_role(ctx, &user_id, &email).await {
        Ok(r) => r,
        Err(e) => return err_internal("Failed to resolve user roles", e),
    };

    // Mint tokens, persist the refresh + session rows, build the cookie via
    // the shared issuance tail. Previously this flow hand-rolled token minting
    // and *omitted* the session row, so OAuth logins were invisible on the
    // userportal device list; routing through `issue_tokens_and_cookie` fixes
    // that by construction.
    let issued = match issue_tokens_and_cookie(
        ctx,
        &user_id,
        &email,
        &roles,
        &format!("oauth.{provider}"),
        None,
        0,
    )
    .await
    {
        Ok(i) => i,
        Err(r) => return r,
    };

    // Redirect to frontend — token is set via HttpOnly cookie only (not URL)
    let frontend_url = config::get_default(
        ctx,
        "WAFER_RUN_SHARED__FRONTEND_URL",
        "http://localhost:5173",
    )
    .await;
    // [SEC-036] Validate FRONTEND_URL before plugging it into a Location
    // header — a misconfigured (or attacker-controlled) value here would
    // turn every OAuth callback into an open redirect.
    if !is_safe_frontend_url(&frontend_url) {
        tracing::error!(
            frontend_url = %frontend_url,
            "WAFER_RUN_SHARED__FRONTEND_URL failed validation; refusing OAuth redirect"
        );
        return err_internal_no_cause("Frontend URL is not configured correctly");
    }
    let post_login_raw =
        config::get_default(ctx, "WAFER_RUN_SHARED__POST_LOGIN_REDIRECT", "/b/admin/").await;
    let admin_default = if is_safe_local_redirect(&post_login_raw) {
        post_login_raw
    } else {
        "/b/admin/".to_string()
    };
    // Role-aware default (#1 onboarding bug fix): a non-admin OAuth login
    // must never default into the admin-only destination above — see
    // `redirect::default_post_login_redirect`.
    let is_admin = roles.iter().any(|r| r == "admin");
    let post_login = default_post_login_redirect(is_admin, &admin_default);
    let redirect_url = format!("{}{}", frontend_url.trim_end_matches('/'), post_login);

    ResponseBuilder::new()
        .status(302)
        .set_cookie(&issued.cookie)
        // The binding cookie has done its job for this flow; a single-use
        // binding must not outlive the state it was minted for.
        .set_cookie(&super::state_binding::clear(ctx).await)
        .set_header("Location", &redirect_url)
        .json(&serde_json::json!({"redirect": redirect_url}))
}

/// The profile fields the callback needs from a provider's userinfo response,
/// already normalised (email lowercased, missing strings empty).
struct OAuthUserInfo {
    /// Lowercased email address the provider returned.
    email: String,
    /// Whether the provider asserted that the account holder controls
    /// [`email`](Self::email) — resolved from the provider's
    /// [`EmailAssertion`](super::spec::EmailAssertion). `false` means "not
    /// asserted", which is also what an absent or malformed claim produces.
    email_verified: bool,
    /// Display name, empty if the provider omitted it.
    name: String,
    /// Avatar URL, empty if the provider omitted it.
    avatar: String,
    /// Stable provider-side user id (`sub` for Google and Microsoft, `id` for
    /// GitHub), coerced to a string.
    provider_ref: String,
    /// Per-provider login handle (GitHub `login`, else the email local-part).
    provider_login: String,
}

/// Phase 1 — exchange the authorization `code` for a provider access token.
///
/// POSTs the token-exchange body (PKCE verifier included where the provider
/// uses it) and returns the `access_token` string. Any transport / parse
/// failure or a missing token is mapped to a ready-to-return [`OutputStream`].
async fn exchange_code(
    ctx: &dyn Context,
    spec: &super::spec::OAuthProviderSpec,
    code: &str,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<String, OutputStream> {
    let token_body_str =
        spec.build_token_body(code, client_id, client_secret, redirect_uri, code_verifier);

    let mut headers = HashMap::new();
    headers.insert(
        "Content-Type".to_string(),
        "application/x-www-form-urlencoded".to_string(),
    );
    headers.insert("Accept".to_string(), "application/json".to_string());

    let token_body_bytes = token_body_str.into_bytes();
    let token_resp = match network::do_request(
        ctx,
        "POST",
        spec.token_url,
        &headers,
        Some(&token_body_bytes),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return Err(err_internal("Token exchange failed", e)),
    };

    let token_data: serde_json::Value = match serde_json::from_slice(&token_resp.body) {
        Ok(d) => d,
        Err(_) => return Err(err_internal_no_cause("Failed to parse token response")),
    };

    let access_token_oauth = token_data
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if access_token_oauth.is_empty() {
        return Err(err_internal_no_cause("No access token in OAuth response"));
    }
    Ok(access_token_oauth.to_string())
}

/// Phase 2 — fetch and normalise the user's profile.
///
/// Calls the provider userinfo endpoint and resolves the address together with
/// what the provider promises about it, per the provider's
/// [`EmailAssertion`](super::spec::EmailAssertion): a boolean claim in the
/// same payload (Google), a separate per-address list (GitHub `/user/emails`),
/// or nothing at all (Microsoft). Returns the normalised [`OAuthUserInfo`]; a
/// missing email or stable id is an error, an unverifiable one is not — it is
/// reported as such and the caller decides what it may be used for.
async fn fetch_user_info(
    ctx: &dyn Context,
    spec: &super::spec::OAuthProviderSpec,
    oauth_token: &str,
) -> Result<OAuthUserInfo, OutputStream> {
    // Shared header set for every provider API call. GitHub's REST API rejects
    // requests without a User-Agent header (returns 403 + an HTML error body);
    // other providers accept it.
    let api_headers = || {
        let mut h = HashMap::new();
        h.insert(
            "Authorization".to_string(),
            spec.userinfo_auth_header(oauth_token),
        );
        h.insert("Accept".to_string(), "application/json".to_string());
        h.insert(
            "User-Agent".to_string(),
            concat!("impresspress-auth/", env!("CARGO_PKG_VERSION")).to_string(),
        );
        h
    };

    let info_resp =
        match network::do_request(ctx, "GET", spec.userinfo_url, &api_headers(), None).await {
            Ok(r) => r,
            Err(e) => return Err(err_internal("User info request failed", e)),
        };

    let user_info: serde_json::Value = match serde_json::from_slice(&info_resp.body) {
        Ok(d) => d,
        Err(e) => {
            // Log the SHA-256 hash of the body instead of the body itself —
            // a parse failure is rare and the raw body typically contains
            // the upstream email / provider IDs that we don't want to drop
            // into the error log surface.
            let body_hash = crate::util::sha256_hex(&info_resp.body);
            return Err(err_internal(
                "Failed to parse OAuth user info",
                format!(
                    "status={} parse={} body_len={} body_sha256={}",
                    info_resp.status_code,
                    e,
                    info_resp.body.len(),
                    body_hash
                ),
            ));
        }
    };

    let mut email = user_info
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    // Only a provider that declares a verification claim gets to assert one,
    // and only a literal `true` counts: an absent, `false` or non-boolean
    // value is "not asserted".
    let mut email_verified = match spec.email_assertion {
        super::spec::EmailAssertion::Claim(field) => user_info
            .get(field)
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        super::spec::EmailAssertion::AddressList(_) | super::spec::EmailAssertion::None => false,
    };
    let name = user_info
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let avatar = user_info
        .get("picture")
        .or_else(|| user_info.get("avatar_url"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // For an `AddressList` provider the userinfo payload is not authoritative
    // about the address at all: GitHub's `/user` returns the *public profile*
    // address, which is null when the user keeps it private and carries no
    // confirmation either way. `/user/emails` (granted by the `user:email`
    // scope) is the list that does, so it is always consulted, and only an
    // entry flagged `verified` is taken — preferring the primary one. If the
    // call fails or yields nothing verified, the profile address stands as an
    // unverified address rather than a verified one.
    if let super::spec::EmailAssertion::AddressList(emails_url) = spec.email_assertion {
        if let Ok(emails_resp) =
            network::do_request(ctx, "GET", emails_url, &api_headers(), None).await
        {
            if let Ok(arr) = serde_json::from_slice::<serde_json::Value>(&emails_resp.body) {
                if let Some(entries) = arr.as_array() {
                    // Prefer primary+verified; fall back to any verified.
                    let pick = entries
                        .iter()
                        .find(|e| {
                            e.get("primary").and_then(|v| v.as_bool()).unwrap_or(false)
                                && e.get("verified").and_then(|v| v.as_bool()).unwrap_or(false)
                        })
                        .or_else(|| {
                            entries.iter().find(|e| {
                                e.get("verified").and_then(|v| v.as_bool()).unwrap_or(false)
                            })
                        });
                    if let Some(e) = pick {
                        if let Some(s) = e.get("email").and_then(|v| v.as_str()) {
                            email = s.to_lowercase();
                            email_verified = true;
                        }
                    }
                }
            }
        }
    }

    if email.is_empty() {
        return Err(err_internal_no_cause("No email returned by OAuth provider"));
    }

    // Extract the stable provider-side user identifier.
    // GitHub returns `id` as a JSON number; Google and Microsoft return the
    // OIDC `sub` (string). Coerce to string in all cases.
    let provider_ref = match user_info.get("sub").or_else(|| user_info.get("id")) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    if provider_ref.is_empty() {
        return Err(err_internal_no_cause(
            "OAuth provider did not return a stable user id",
        ));
    }

    // Stable per-provider handle (GitHub `login`, others fall back to email local-part).
    let provider_login = user_info
        .get("login")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| email.split('@').next().unwrap_or(""))
        .to_string();

    Ok(OAuthUserInfo {
        email,
        email_verified,
        name,
        avatar,
        provider_ref,
        provider_login,
    })
}

/// Phase 3 — resolve the local user id for this OAuth identity.
///
/// Tries, in order: an existing `(provider, provider_ref)` link; an
/// email-matched local account (only when both sides have proven the address —
/// see below); otherwise creates a new user (subject to the shared signup
/// gates). Every branch then passes the lifecycle and verification gates
/// before the provider link is upserted. Returns the resolved local user id.
async fn resolve_user(
    ctx: &dyn Context,
    provider: &str,
    oauth_token: &str,
    info: &OAuthUserInfo,
) -> Result<String, OutputStream> {
    // --- Step 1: look up existing link by (provider, provider_ref) ---
    let existing_link =
        match provider_links::find_by_provider_ref(ctx, provider, &info.provider_ref).await {
            Ok(l) => l,
            Err(e) => return Err(err_internal("provider_links lookup failed", e)),
        };

    // --- Step 2 / 3: resolve user_id ---
    let user_id: String = if let Some(link) = existing_link {
        // Known provider link — reuse the bound user.
        link.user_id
    } else {
        // No link yet. An account already holding this address may be adopted
        // only when BOTH sides have proven control of the mailbox.
        //
        // Without that, the address is just a string two parties happen to
        // type, and matching on it hands the account to whichever of them
        // arrives second: an attacker who signs up locally with
        // `victim@example.com` (never confirming it) inherits the victim's
        // real account the moment the victim signs in with a provider — the
        // classic pre-account takeover. It runs the other way too: a provider
        // that will hand out a token for an address it never confirmed
        // (Microsoft, see `spec::EmailAssertion`) lets its own users claim any
        // local account by address alone.
        //
        // A refusal here does tell the caller that an account with this
        // address exists. That is unavoidable — `users.email` is UNIQUE, so
        // the alternative is not silence but a failed insert — and it is the
        // lesser disclosure by a wide margin.
        match users::find_by_email(ctx, &info.email).await {
            Ok(Some(existing_user)) => {
                if !info.email_verified {
                    return Err(error_response(
                        ErrorCode::EmailAlreadyExists,
                        &format!(
                            "An account already uses this email address, and {provider} does \
                             not confirm that this address belongs to you. Sign in to that \
                             account instead."
                        ),
                    ));
                }
                if !existing_user.email_verified {
                    return Err(error_response(
                        ErrorCode::EmailAlreadyExists,
                        "An account already uses this email address but has never confirmed \
                         it. Sign in to that account and verify the address first.",
                    ));
                }
                existing_user.id
            }
            Ok(None) => {
                // Brand-new user — enforce signup gates. Shared with the JSON
                // signup handler so the ALLOW_SIGNUP / ALLOWED_EMAIL_DOMAINS /
                // bootstrap-admin rules can't drift between the two flows.
                if !signup_allowed(ctx).await {
                    return Err(err_forbidden("Signups are currently disabled"));
                }

                if !email_domain_allowed(ctx, &info.email).await {
                    return Err(err_forbidden(
                        "Signups from this email domain are not allowed",
                    ));
                }

                // Determine role: admin if email matches bootstrap email.
                let role = initial_role_for(ctx, &info.email).await;

                let display_name = if info.name.is_empty() {
                    info.email.clone()
                } else {
                    info.name.clone()
                };
                let new_user = users::NewUser {
                    email: info.email.clone(),
                    display_name,
                    avatar_url: if info.avatar.is_empty() {
                        None
                    } else {
                        Some(info.avatar.clone())
                    },
                    role: role.to_string(),
                    // The provider's own assertion IS the verification: a
                    // confirmation email would ask the user to prove exactly
                    // what Google (or GitHub's verified address list) just
                    // proved. A provider that asserts nothing produces an
                    // unverified row, which the gate below then holds to the
                    // deployment's verification policy like any other.
                    email_verified: info.email_verified,
                    verification_token_hash: None,
                };
                // The initial role is the inline `users.role` that
                // `get_user_roles` reads first; a `user_roles` row means a
                // grant beyond it, so none is written at signup — the same
                // rows password signup produces.
                match users::insert(ctx, new_user).await {
                    Ok(u) => u.id,
                    Err(e) => return Err(err_internal("Failed to create user", e)),
                }
            }
            Err(e) => return Err(err_internal("User lookup failed", e)),
        }
    };

    // --- Lifecycle + verification gates (single enforcement point) ---
    // Every branch above (existing link, email merge, new signup) converges
    // here — including the existing-link branch, which authenticates a user
    // it never looked up. Verify the resolved account may authenticate BEFORE
    // mutating the provider link or issuing tokens. `is_active()` covers both
    // `disabled` and soft-delete (`deleted_at`).
    let account = match users::find_by_id(ctx, &user_id).await {
        Ok(Some(u)) if u.is_active() => u,
        Ok(Some(_)) => return Err(err_forbidden("Account is disabled")),
        Ok(None) => return Err(err_forbidden("Account not found")),
        Err(e) => return Err(err_internal("User lookup failed", e)),
    };

    // A provider assertion about the account's own address is verification,
    // so an account that has one records it. This is what carries a row
    // created by an earlier sign-in — when the flow ignored the assertion and
    // stored `email_verified = 0` — over to verified on the next sign-in,
    // instead of stranding it against the policy below forever.
    let verified_now =
        if info.email_verified && !account.email_verified && account.email == info.email {
            if let Err(e) = users::set_email_verified(ctx, &user_id, true).await {
                return Err(err_internal("Failed to record the verified email", e));
            }
            true
        } else {
            account.email_verified
        };

    // The same verification policy `api::login` and `api::refresh` enforce.
    // A callback that issues tokens to an unverified account while refresh
    // rejects it at the first rotation is not a lenient sign-in; it is a
    // sign-in that logs the user back out minutes later, every time, with no
    // way out of the loop.
    let require_verification =
        crate::config_vars::get_bool(ctx, REQUIRE_VERIFICATION_KEY, false).await;
    if require_verification && !verified_now {
        return Err(error_response(
            ErrorCode::EmailNotVerified,
            &format!(
                "This site requires a verified email address, and this account does not have \
                 one — {provider} does not confirm the address it returned."
            ),
        ));
    }

    // --- Step 4: upsert the provider_links row ---
    if let Err(e) = provider_links::upsert(
        ctx,
        provider_links::NewLink {
            provider,
            provider_ref: &info.provider_ref,
            user_id: &user_id,
            provider_login: &info.provider_login,
            access_token: oauth_token,
        },
    )
    .await
    {
        // Log but don't fail — the user is authenticated; link persistence
        // is best-effort metadata. A failed upsert means the next sign-in
        // resolves the account by address again, which only succeeds under
        // the adoption rule above.
        tracing::warn!("Failed to upsert provider_links: {e}");
    }

    Ok(user_id)
}

/// [SEC-036] Validates `WAFER_RUN_SHARED__FRONTEND_URL` before it is used as
/// the origin half of an OAuth callback redirect.
///
/// The OAuth flow ends by issuing a `302 Location: {frontend_url}{post_login}`.
/// If `frontend_url` is attacker-controlled (admin UI mistake, env-var
/// injection, copy-paste of a phishing URL) this becomes an open redirect that
/// piggybacks on the trusted authentication step.
///
/// Accept only:
/// - `https://<host>` (any non-empty host), OR
/// - `http://localhost[:port]` / `http://127.0.0.1[:port]` for local dev.
///
/// Reject anything with a path beyond `/`, any query, any fragment, anything
/// containing CRLF/tab/other control characters, or any non-http(s) scheme.
fn is_safe_frontend_url(s: &str) -> bool {
    // Reject control characters outright — they enable header-injection even
    // if the rest of the URL parses cleanly.
    if s.chars().any(|c| c.is_control()) {
        return false;
    }
    let Ok(parsed) = url::Url::parse(s) else {
        return false;
    };
    let host = match parsed.host_str() {
        Some(h) if !h.is_empty() => h,
        _ => return false,
    };
    match parsed.scheme() {
        "https" => {}
        "http" => {
            if !(host == "localhost" || host == "127.0.0.1" || host == "[::1]") {
                return false;
            }
        }
        _ => return false,
    }
    // Forbid an embedded path — the redirect formats as
    // `{frontend_url}{post_login}` where post_login already starts with `/`.
    // Allowing a path on frontend_url would invite double-slashes and
    // injection of an unexpected prefix.
    if !(parsed.path().is_empty() || parsed.path() == "/") {
        return false;
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::is_safe_frontend_url;

    #[test]
    fn accepts_https_origins() {
        assert!(is_safe_frontend_url("https://app.example.com"));
        assert!(is_safe_frontend_url("https://app.example.com/"));
        assert!(is_safe_frontend_url("https://app.example.com:8443"));
    }

    #[test]
    fn accepts_http_localhost_for_dev() {
        assert!(is_safe_frontend_url("http://localhost:5173"));
        assert!(is_safe_frontend_url("http://localhost"));
        assert!(is_safe_frontend_url("http://127.0.0.1:3000"));
        assert!(is_safe_frontend_url("http://[::1]:5173"));
    }

    #[test]
    fn rejects_http_non_localhost() {
        assert!(!is_safe_frontend_url("http://evil.com"));
        assert!(!is_safe_frontend_url("http://example.com"));
    }

    #[test]
    fn rejects_non_http_schemes() {
        assert!(!is_safe_frontend_url("javascript:alert(1)"));
        assert!(!is_safe_frontend_url("data:text/html,<script>x</script>"));
        assert!(!is_safe_frontend_url("file:///etc/passwd"));
        assert!(!is_safe_frontend_url("ftp://example.com"));
    }

    #[test]
    fn rejects_paths_and_queries_and_fragments() {
        assert!(!is_safe_frontend_url("https://example.com/path"));
        assert!(!is_safe_frontend_url("https://example.com/?q=1"));
        assert!(!is_safe_frontend_url("https://example.com/#frag"));
    }

    #[test]
    fn rejects_empty_host() {
        assert!(!is_safe_frontend_url(""));
        assert!(!is_safe_frontend_url("https://"));
        assert!(!is_safe_frontend_url("not a url"));
    }

    #[test]
    fn rejects_control_characters() {
        assert!(!is_safe_frontend_url(
            "https://example.com\r\nLocation: https://evil.com"
        ));
        assert!(!is_safe_frontend_url("https://example.com\n"));
    }
}

/// End-to-end tests for the OAuth callback's security gates.
///
/// Every test drives the real [`handle`] through a mock `wafer-run/network`
/// block serving canned provider responses, so the assertions are about the
/// shipped flow rather than a re-implementation of it. What they hold in
/// place:
///
/// * **Browser binding** — a callback is only honoured in the browser that
///   ran the start endpoint, so a `code`/`state` pair captured in the
///   attacker's browser cannot be replayed at the victim's (login CSRF).
/// * **Account adoption** — an OAuth identity joins an existing local account
///   only when the provider asserts a verified address AND the local row is
///   itself verified (pre-account takeover).
/// * **Verification policy** — the callback applies
///   `WAFER_RUN__AUTH__REQUIRE_VERIFICATION` exactly as login and refresh do,
///   instead of issuing tokens that the first rotation rejects.
/// * **Provider wiring** — Microsoft signs in through the OIDC userinfo
///   endpoint; Graph `/v1.0/me`, which returns no `email` at all, cannot
///   produce a sign-in.
/// * **Lifecycle** — disabled and soft-deleted accounts are rejected on every
///   branch, and a successful login leaves exactly one session row.
#[cfg(test)]
mod security_regression_tests {
    use std::{collections::HashMap, sync::Arc};

    use async_trait::async_trait;
    use wafer_core::interfaces::network::service::{
        NetworkError, NetworkService, Request, Response,
    };
    use wafer_run::{Block, Message};

    use super::handle;
    use crate::{
        blocks::auth::repo::{oauth_pkce, provider_links, sessions, users},
        test_support::TestContext,
    };

    /// The `state_id` every fixture seeds, and the one `callback_msg` binds to.
    const STATE_ID: &str = "state-xyz";

    /// Stable provider-side user ids the mock returns, per provider.
    const GOOGLE_SUB: &str = "google-user-123";
    const MICROSOFT_SUB: &str = "microsoft-user-456";
    const GITHUB_ID: &str = "4242";

    /// Mock network block serving one provider's token + profile endpoints.
    ///
    /// Any other URL is an error, which is what makes these tests able to say
    /// *which* endpoint the flow talks to: a handler pointed at the wrong
    /// userinfo URL gets an error or an unusable payload, never a pass.
    struct MockOAuthNetwork {
        email: String,
        /// Google: the value of the `verified_email` claim. GitHub: the
        /// `verified` flag on the `/user/emails` entry. Microsoft asserts
        /// nothing, so it is unused there.
        provider_verified: bool,
        /// GitHub only: whether `/user` exposes the address. A user who keeps
        /// their address private gets `null` there, which is what makes
        /// `/user/emails` the only place an address can be found.
        profile_email_public: bool,
    }

    #[async_trait]
    impl NetworkService for MockOAuthNetwork {
        async fn do_request(&self, req: &Request) -> Result<Response, NetworkError> {
            let url = req.url.as_str();
            let body = if url.ends_with("/token") || url.ends_with("oauth/access_token") {
                serde_json::json!({ "access_token": "mock-access-token" })
            } else if url == "https://www.googleapis.com/oauth2/v2/userinfo" {
                serde_json::json!({
                    "sub": GOOGLE_SUB,
                    "email": self.email,
                    "verified_email": self.provider_verified,
                    "name": "Mock Google User",
                })
            } else if url == "https://graph.microsoft.com/oidc/userinfo" {
                // Microsoft's OIDC userinfo claims. No `email_verified`:
                // Microsoft does not make that assertion.
                serde_json::json!({
                    "sub": MICROSOFT_SUB,
                    "email": self.email,
                    "name": "Mock Microsoft User",
                })
            } else if url == "https://graph.microsoft.com/v1.0/me" {
                // Graph's own user resource, for comparison: `mail` and
                // `userPrincipalName`, and no `email` key anywhere.
                serde_json::json!({
                    "id": MICROSOFT_SUB,
                    "displayName": "Mock Microsoft User",
                    "mail": self.email,
                    "userPrincipalName": self.email,
                })
            } else if url == "https://api.github.com/user" {
                // The public profile address — null for a user who keeps it
                // private, and in no case something GitHub vouches for.
                serde_json::json!({
                    "id": GITHUB_ID.parse::<i64>().unwrap(),
                    "login": "mockgh",
                    "email": if self.profile_email_public {
                        serde_json::Value::String(self.email.clone())
                    } else {
                        serde_json::Value::Null
                    },
                    "avatar_url": "https://avatars.example/mockgh.png",
                })
            } else if url == "https://api.github.com/user/emails" {
                serde_json::json!([{
                    "email": self.email,
                    "primary": true,
                    "verified": self.provider_verified,
                }])
            } else {
                return Err(NetworkError::Other(format!("unexpected URL: {url}")));
            };
            Ok(Response {
                status_code: 200,
                headers: HashMap::new(),
                body: serde_json::to_vec(&body).unwrap(),
            })
        }
    }

    /// One OAuth sign-in attempt, described: which provider answers, with
    /// which address, whether that provider vouches for it, and any extra
    /// config the deployment carries.
    struct OauthFlow {
        provider: &'static str,
        email: &'static str,
        provider_verified: bool,
        profile_email_public: bool,
        config: Vec<(String, String)>,
    }

    impl OauthFlow {
        fn google(email: &'static str) -> Self {
            Self {
                provider: "google",
                email,
                provider_verified: true,
                profile_email_public: false,
                config: Vec::new(),
            }
        }

        fn github(email: &'static str) -> Self {
            Self {
                provider: "github",
                email,
                provider_verified: true,
                profile_email_public: false,
                config: Vec::new(),
            }
        }

        fn microsoft(email: &'static str) -> Self {
            Self {
                provider: "microsoft",
                email,
                // Microsoft returns no verification claim whatever the
                // account looks like; the field is inert here.
                provider_verified: false,
                profile_email_public: false,
                config: Vec::new(),
            }
        }

        /// Whether the provider vouches for the address (Google's
        /// `verified_email`, GitHub's per-address `verified`).
        fn provider_verified(mut self, verified: bool) -> Self {
            self.provider_verified = verified;
            self
        }

        /// GitHub only: publish the address on the profile endpoint, as a
        /// user who has not made it private does.
        fn profile_email_public(mut self) -> Self {
            self.profile_email_public = true;
            self
        }

        fn config(mut self, key: &str, value: &str) -> Self {
            self.config.push((key.to_string(), value.to_string()));
            self
        }

        /// Build a ctx with auth migrations, a crypto block (token minting), a
        /// mock network block for this provider, OAuth enabled, and a seeded
        /// PKCE state row so the callback's single-use state redemption
        /// succeeds.
        ///
        /// The extra config is folded into the same `wafer-run/config` block
        /// as the OAuth flags — it can't be layered on afterward via
        /// `TestContext::set_config`, which would replace this block wholesale
        /// and drop the OAuth flags the callback needs to get past its own
        /// gates.
        async fn ctx(&self) -> TestContext {
            let mut ctx = TestContext::with_auth().await;

            // Crypto block — issue_tokens_and_cookie signs JWTs and pulls
            // random bytes for the rotation family / jti.
            let crypto_svc = Arc::new(
                wafer_block_crypto::service::Argon2JwtCryptoService::new(
                    "test-jwt-secret-padded-to-min-32-bytes-aaaa".to_string(),
                )
                .expect("test secret is long enough"),
            );
            let crypto_block: Arc<dyn Block> = Arc::new(
                wafer_core::service_blocks::crypto::CryptoBlock::new(crypto_svc),
            );
            ctx.register_block("wafer-run/crypto", crypto_block);

            // Mock network block under the production block id.
            let net: Arc<dyn Block> =
                Arc::new(wafer_core::service_blocks::network::NetworkBlock::new(
                    Arc::new(MockOAuthNetwork {
                        email: self.email.to_string(),
                        provider_verified: self.provider_verified,
                        profile_email_public: self.profile_email_public,
                    }),
                ));
            ctx.register_block("wafer-run/network", net);

            // Config block — the handler reads OAuth flags / client
            // credentials via `config::get_default`, which dispatches to the
            // `wafer-run/config` block (NOT the TestContext config_get
            // snapshot).
            use wafer_core::{
                interfaces::config::service::ConfigService,
                service_blocks::config::{ConfigBlock, EnvConfigService},
            };
            let cfg_svc = EnvConfigService::new();
            cfg_svc.set("WAFER_RUN_SHARED__ENABLE_OAUTH", "true");
            let upper = self.provider.to_uppercase();
            cfg_svc.set(
                &format!("IMPRESSPRESS__AUTH_UI__OAUTH_{upper}_CLIENT_ID"),
                "client-id",
            );
            cfg_svc.set(
                &format!("IMPRESSPRESS__AUTH_UI__OAUTH_{upper}_CLIENT_SECRET"),
                "client-secret",
            );
            for (k, v) in &self.config {
                cfg_svc.set(k, v);
            }
            let cfg_block: Arc<dyn Block> = Arc::new(ConfigBlock::new(Arc::new(cfg_svc)));
            ctx.register_block("wafer-run/config", cfg_block);

            seed_state(&ctx, STATE_ID, self.provider).await;
            ctx
        }
    }

    /// Seed a single-use PKCE state row keyed by `state_id`.
    async fn seed_state(ctx: &TestContext, state_id: &str, provider: &str) {
        let expires = (chrono::Utc::now() + chrono::Duration::minutes(10))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        oauth_pkce::insert(
            ctx,
            oauth_pkce::NewPkceState {
                state_id,
                provider,
                code_verifier: "verifier-abc",
                redirect_uri: "https://app.example.com/b/auth/oauth/callback",
                expires_at: &expires,
            },
        )
        .await
        .expect("seed pkce state");
    }

    /// The `Cookie` header a browser sends back for a `Set-Cookie` value —
    /// the name/value pair, without the attributes.
    fn cookie_header_for(set_cookie: &str) -> &str {
        set_cookie.split(';').next().unwrap_or("")
    }

    /// The callback request a browser makes on its way back from the
    /// provider: `code` + `state` query params, plus the binding cookie the
    /// start endpoint set for `state_id`.
    async fn callback_msg(ctx: &TestContext) -> Message {
        let set_cookie =
            crate::blocks::auth_ui::oauth::state_binding::issue(ctx, STATE_ID, 600).await;
        let mut msg = callback_msg_unbound();
        msg.set_meta("http.header.cookie", cookie_header_for(&set_cookie));
        msg
    }

    /// The same callback with no binding cookie at all — the shape a forged
    /// callback arrives in, since the attacker's cookie is in the attacker's
    /// browser.
    fn callback_msg_unbound() -> Message {
        let mut msg = Message::new("auth.oauth.callback");
        msg.set_meta("req.query.code", "auth-code-123");
        msg.set_meta("req.query.state", STATE_ID);
        msg
    }

    /// Seed a local account as a password signup would leave it.
    async fn seed_user(ctx: &TestContext, email: &str, email_verified: bool) -> users::UserRow {
        users::insert(
            ctx,
            users::NewUser {
                email: email.to_string(),
                display_name: "Seeded User".to_string(),
                avatar_url: None,
                role: "user".to_string(),
                email_verified,
                verification_token_hash: None,
            },
        )
        .await
        .expect("seed user")
    }

    // ---------------------------------------------------------------
    // Browser binding (login CSRF)
    // ---------------------------------------------------------------

    /// The two halves agree: the cookie the real start handler sets is the
    /// one the real callback demands. A round trip through both handlers, so
    /// neither can drift from the other's idea of the binding.
    #[tokio::test]
    async fn start_binds_the_flow_and_the_callback_accepts_it() {
        let email = "roundtrip@example.com";
        let ctx = OauthFlow::google(email).ctx().await;

        let mut start_msg = Message::new("auth.oauth.login");
        start_msg.set_meta("req.query.provider", "google");
        let started = wafer_block::http_codec::collect_http_response(
            crate::blocks::auth_ui::oauth::start::handle(&ctx, &start_msg).await,
        )
        .await;
        let set_cookie = started
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, v)| v.clone())
            .expect("the start endpoint must bind the flow to this browser");

        let body: serde_json::Value = serde_json::from_slice(&started.body).expect("start json");
        let auth_url = body["auth_url"].as_str().expect("auth_url");
        let state = auth_url
            .split("&state=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .expect("authorize URL carries the state");

        let mut msg = Message::new("auth.oauth.callback");
        msg.set_meta("req.query.code", "auth-code-123");
        msg.set_meta("req.query.state", state);
        msg.set_meta("http.header.cookie", cookie_header_for(&set_cookie));

        let status = crate::test_support::output_status(handle(&ctx, &msg).await).await;
        assert_eq!(
            status, 302,
            "a callback carrying the cookie the start endpoint set must complete"
        );
    }

    /// Login CSRF: an attacker runs the authorize step in their own browser
    /// and hands the victim the resulting callback URL. The victim's browser
    /// has no binding cookie, so the callback must refuse — otherwise the
    /// victim is silently signed in to the attacker's account.
    #[tokio::test]
    async fn callback_without_the_binding_cookie_is_refused() {
        let email = "csrf-victim@example.com";
        let ctx = OauthFlow::google(email).ctx().await;

        let out = handle(&ctx, &callback_msg_unbound()).await;
        assert!(
            crate::test_support::output_is_error(out, "InvalidArgument").await,
            "a callback from a browser that never started the flow must be refused"
        );

        assert!(
            users::find_by_email(&ctx, email)
                .await
                .expect("user lookup ok")
                .is_none(),
            "no account may be created by an unbound callback"
        );
        // The refusal happens before the take, so the victim's own pending
        // flow (if any) is not burned by the forgery.
        assert!(
            oauth_pkce::take(&ctx, STATE_ID)
                .await
                .expect("take ok")
                .is_some(),
            "an unbound callback must not consume the single-use state"
        );
    }

    /// A cookie from some other flow is not a binding either.
    #[tokio::test]
    async fn callback_with_a_foreign_binding_cookie_is_refused() {
        let ctx = OauthFlow::google("csrf-victim2@example.com").ctx().await;

        let foreign =
            crate::blocks::auth_ui::oauth::state_binding::issue(&ctx, "some-other-state", 600)
                .await;
        let mut msg = callback_msg_unbound();
        msg.set_meta("http.header.cookie", cookie_header_for(&foreign));

        assert!(
            crate::test_support::output_is_error(handle(&ctx, &msg).await, "InvalidArgument").await,
            "a binding cookie minted for another state must not redeem this one"
        );
    }

    // ---------------------------------------------------------------
    // Account adoption (pre-account takeover)
    // ---------------------------------------------------------------

    /// Pre-account takeover: an attacker signs up locally with the victim's
    /// address and never confirms it. When the victim later signs in with a
    /// provider, matching on the address alone would hand the victim's
    /// session to the attacker's row. The callback must refuse instead.
    #[tokio::test]
    async fn an_unverified_local_account_is_not_adopted() {
        let email = "preclaimed@example.com";
        let ctx = OauthFlow::google(email).ctx().await;
        let squatted = seed_user(&ctx, email, false).await;

        let out = handle(&ctx, &callback_msg(&ctx).await).await;
        assert!(
            crate::test_support::output_is_error(out, "AlreadyExists").await,
            "an OAuth identity must not adopt an account whose address was never confirmed"
        );

        assert!(
            sessions::list_for_user(&ctx, &squatted.id)
                .await
                .expect("list sessions ok")
                .is_empty(),
            "no session may be minted for the account that was not adopted"
        );
        assert!(
            provider_links::find_by_provider_ref(&ctx, "google", GOOGLE_SUB)
                .await
                .expect("link lookup ok")
                .is_none(),
            "no provider link may be written for a refused adoption"
        );
    }

    /// The legitimate case: both sides have proven the address, so the
    /// identity joins the existing account rather than failing on the UNIQUE
    /// email or creating a second one.
    #[tokio::test]
    async fn a_verified_local_account_is_adopted_once() {
        let email = "verified-local@example.com";
        let ctx = OauthFlow::google(email).ctx().await;
        let existing = seed_user(&ctx, email, true).await;

        let status =
            crate::test_support::output_status(handle(&ctx, &callback_msg(&ctx).await).await).await;
        assert_eq!(status, 302, "a verified account may be adopted");

        let link = provider_links::find_by_provider_ref(&ctx, "google", GOOGLE_SUB)
            .await
            .expect("link lookup ok")
            .expect("the adopted account is linked to the provider identity");
        assert_eq!(
            link.user_id, existing.id,
            "the link must bind to the existing account, not a new one"
        );
        assert_eq!(
            users::find_by_email(&ctx, email)
                .await
                .expect("user lookup ok")
                .expect("user present")
                .id,
            existing.id,
            "adoption must not duplicate the account"
        );
    }

    /// A provider that makes no verification assertion cannot adopt an
    /// account either, however well-confirmed the local row is: its `email`
    /// claim is a mutable profile attribute, not proof of the mailbox.
    #[tokio::test]
    async fn a_provider_without_an_assertion_cannot_adopt_an_account() {
        let email = "ms-adopt@example.com";
        let ctx = OauthFlow::microsoft(email).ctx().await;
        let existing = seed_user(&ctx, email, true).await;

        let out = handle(&ctx, &callback_msg(&ctx).await).await;
        assert!(
            crate::test_support::output_is_error(out, "AlreadyExists").await,
            "Microsoft asserts nothing about the address, so it cannot claim an account by it"
        );
        assert!(
            sessions::list_for_user(&ctx, &existing.id)
                .await
                .expect("list sessions ok")
                .is_empty(),
            "no session for a refused adoption"
        );
    }

    /// Same refusal when the provider does assert, but says the address is
    /// NOT verified.
    #[tokio::test]
    async fn an_unverified_provider_address_cannot_adopt_an_account() {
        let email = "unverified-google@example.com";
        let ctx = OauthFlow::google(email)
            .provider_verified(false)
            .ctx()
            .await;
        seed_user(&ctx, email, true).await;

        let out = handle(&ctx, &callback_msg(&ctx).await).await;
        assert!(
            crate::test_support::output_is_error(out, "AlreadyExists").await,
            "an address the provider itself flags unverified cannot claim an account"
        );
    }

    // ---------------------------------------------------------------
    // Provider wiring
    // ---------------------------------------------------------------

    /// Microsoft sign-in works, and works through the OIDC userinfo endpoint.
    /// Graph `/v1.0/me` — which the mock also serves, in its real shape —
    /// carries `mail` / `userPrincipalName` and no `email`, so a flow pointed
    /// at it ends at "No email returned by OAuth provider" for every user.
    #[tokio::test]
    async fn microsoft_signs_in_through_the_oidc_userinfo_endpoint() {
        let email = "msuser@example.com";
        let ctx = OauthFlow::microsoft(email).ctx().await;

        let status =
            crate::test_support::output_status(handle(&ctx, &callback_msg(&ctx).await).await).await;
        assert_eq!(status, 302, "a Microsoft sign-in must complete");

        let user = users::find_by_email(&ctx, email)
            .await
            .expect("user lookup ok")
            .expect("the Microsoft callback created the account");
        assert!(
            !user.email_verified,
            "Microsoft asserts nothing about the address, so the row is not verified"
        );
        let link = provider_links::find_by_provider_ref(&ctx, "microsoft", MICROSOFT_SUB)
            .await
            .expect("link lookup ok")
            .expect("the link keys on the OIDC `sub`");
        assert_eq!(link.user_id, user.id);
    }

    /// GitHub's profile address is not authoritative: the flow reads
    /// `/user/emails` and takes the verified primary entry, which is also
    /// what makes the resulting account verified.
    #[tokio::test]
    async fn github_takes_the_verified_primary_address() {
        let email = "ghuser@example.com";
        let ctx = OauthFlow::github(email).ctx().await;

        let status =
            crate::test_support::output_status(handle(&ctx, &callback_msg(&ctx).await).await).await;
        assert_eq!(status, 302, "a GitHub sign-in must complete");

        let user = users::find_by_email(&ctx, email)
            .await
            .expect("user lookup ok")
            .expect("the GitHub callback created the account");
        assert!(
            user.email_verified,
            "a GitHub address flagged verified on /user/emails is verified"
        );
    }

    /// The same address with the `verified` flag cleared creates an account
    /// that is NOT verified — the list is read for the flag, not merely for
    /// an address.
    #[tokio::test]
    async fn github_unverified_address_creates_an_unverified_account() {
        let email = "gh-unverified@example.com";
        let ctx = OauthFlow::github(email)
            .provider_verified(false)
            .profile_email_public()
            .ctx()
            .await;

        let status =
            crate::test_support::output_status(handle(&ctx, &callback_msg(&ctx).await).await).await;
        assert_eq!(status, 302);

        let user = users::find_by_email(&ctx, email)
            .await
            .expect("user lookup ok")
            .expect("account created");
        assert!(
            !user.email_verified,
            "an unverified GitHub address must not produce a verified account"
        );
    }

    // ---------------------------------------------------------------
    // Verification policy
    // ---------------------------------------------------------------

    /// With `REQUIRE_VERIFICATION` on, the callback must apply the policy
    /// login and refresh apply. Issuing tokens here to an unverified account
    /// only produced a sign-in that the first refresh rotation threw out.
    #[tokio::test]
    async fn require_verification_refuses_an_unasserted_login() {
        let email = "needs-verification@example.com";
        let ctx = OauthFlow::microsoft(email)
            .config("WAFER_RUN__AUTH__REQUIRE_VERIFICATION", "true")
            .ctx()
            .await;

        let out = handle(&ctx, &callback_msg(&ctx).await).await;
        assert!(
            crate::test_support::output_is_error(out, "PermissionDenied").await,
            "an unverified account must be refused, not signed in and logged out again"
        );

        let user = users::find_by_email(&ctx, email)
            .await
            .expect("user lookup ok")
            .expect("the signup itself is allowed");
        assert!(
            sessions::list_for_user(&ctx, &user.id)
                .await
                .expect("list sessions ok")
                .is_empty(),
            "no session may be minted for an account the policy refuses"
        );
    }

    /// And the provider's assertion satisfies that same policy: a Google
    /// sign-in under `REQUIRE_VERIFICATION` completes, because the row it
    /// creates records the assertion.
    #[tokio::test]
    async fn require_verification_admits_a_provider_verified_login() {
        let email = "google-verified@example.com";
        let ctx = OauthFlow::google(email)
            .config("WAFER_RUN__AUTH__REQUIRE_VERIFICATION", "true")
            .ctx()
            .await;

        let status =
            crate::test_support::output_status(handle(&ctx, &callback_msg(&ctx).await).await).await;
        assert_eq!(
            status, 302,
            "a provider-verified address satisfies the verification policy"
        );

        let user = users::find_by_email(&ctx, email)
            .await
            .expect("user lookup ok")
            .expect("account created");
        assert!(
            user.email_verified,
            "the provider's assertion must be recorded on the row"
        );
    }

    /// A row left unverified by an earlier sign-in is carried over on the
    /// next one rather than stranded: the provider still asserts the address,
    /// so the account records it and the policy admits it.
    #[tokio::test]
    async fn a_provider_assertion_upgrades_an_unverified_linked_account() {
        let email = "legacy-oauth@example.com";
        let ctx = OauthFlow::google(email)
            .config("WAFER_RUN__AUTH__REQUIRE_VERIFICATION", "true")
            .ctx()
            .await;
        let user = seed_user(&ctx, email, false).await;
        provider_links::upsert(
            &ctx,
            provider_links::NewLink {
                provider: "google",
                provider_ref: GOOGLE_SUB,
                user_id: &user.id,
                provider_login: "legacy",
                access_token: "old-token",
            },
        )
        .await
        .expect("seed provider link");

        let status =
            crate::test_support::output_status(handle(&ctx, &callback_msg(&ctx).await).await).await;
        assert_eq!(status, 302, "the linked account signs in");
        assert!(
            users::find_by_id(&ctx, &user.id)
                .await
                .expect("user lookup ok")
                .expect("user present")
                .email_verified,
            "the provider's assertion must be recorded on the existing row"
        );
    }

    // ---------------------------------------------------------------
    // Identity resolution
    // ---------------------------------------------------------------

    /// First sign-in: the callback creates the account AND the provider link,
    /// and persists a session row — OAuth logins are visible on the
    /// userportal device list like every other login.
    #[tokio::test]
    async fn first_login_creates_the_user_the_link_and_a_session() {
        let email = "newoauth@example.com";
        let ctx = OauthFlow::google(email).ctx().await;

        let status =
            crate::test_support::output_status(handle(&ctx, &callback_msg(&ctx).await).await).await;
        assert_eq!(status, 302, "successful OAuth callback should 302-redirect");

        let user = users::find_by_email(&ctx, email)
            .await
            .expect("user lookup ok")
            .expect("OAuth callback created the user");

        let link = provider_links::find_by_provider_ref(&ctx, "google", GOOGLE_SUB)
            .await
            .expect("link lookup ok")
            .expect("OAuth callback created the provider link");
        assert_eq!(link.user_id, user.id);

        let session_rows = sessions::list_for_user(&ctx, &user.id)
            .await
            .expect("list sessions ok");
        assert_eq!(
            session_rows.len(),
            1,
            "OAuth login must persist exactly one session row"
        );
    }

    /// Second sign-in with the same provider identity reuses the linked
    /// account: one user row, one link row, a fresh access token on it.
    #[tokio::test]
    async fn re_login_reuses_the_linked_account() {
        let email = "returning@example.com";
        let ctx = OauthFlow::google(email).ctx().await;

        let first =
            crate::test_support::output_status(handle(&ctx, &callback_msg(&ctx).await).await).await;
        assert_eq!(first, 302);
        let user = users::find_by_email(&ctx, email)
            .await
            .expect("user lookup ok")
            .expect("user present");

        // A second flow needs its own single-use state.
        seed_state(&ctx, STATE_ID, "google").await;
        let second =
            crate::test_support::output_status(handle(&ctx, &callback_msg(&ctx).await).await).await;
        assert_eq!(second, 302, "a returning user signs in again");

        assert_eq!(
            users::find_by_email(&ctx, email)
                .await
                .expect("user lookup ok")
                .expect("user present")
                .id,
            user.id,
            "re-login must not create a second account"
        );
        let link = provider_links::find_by_provider_ref(&ctx, "google", GOOGLE_SUB)
            .await
            .expect("link lookup ok")
            .expect("link present");
        assert_eq!(link.user_id, user.id);
        assert_eq!(
            sessions::list_for_user(&ctx, &user.id)
                .await
                .expect("list sessions ok")
                .len(),
            2,
            "each login leaves its own session row"
        );
    }

    /// #1 onboarding bug fix: a brand-new non-admin OAuth login must default
    /// into `/b/userportal/`, not the admin-only `/b/admin/` default.
    #[tokio::test]
    async fn oauth_login_non_admin_redirects_to_userportal() {
        let email = "oauthuser@example.com";
        let ctx = OauthFlow::google(email).ctx().await;

        let location = crate::test_support::output_header(
            handle(&ctx, &callback_msg(&ctx).await).await,
            "Location",
        )
        .await
        .expect("302 redirect must set a Location header");
        assert!(
            location.ends_with("/b/userportal/"),
            "non-admin OAuth login must default to the user portal, not the \
             admin-only route: {location}"
        );
    }

    /// Companion to the above: an admin (email matches the configured
    /// bootstrap admin email) still gets the operator-configured admin
    /// default — the fix is role-aware, not a blanket redirect change.
    #[tokio::test]
    async fn oauth_login_admin_email_redirects_to_admin_home() {
        let email = "oauthadmin@example.com";
        let ctx = OauthFlow::google(email)
            .config("WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL", email)
            .ctx()
            .await;

        let location = crate::test_support::output_header(
            handle(&ctx, &callback_msg(&ctx).await).await,
            "Location",
        )
        .await
        .expect("302 redirect must set a Location header");
        assert!(
            location.ends_with("/b/admin/"),
            "admin OAuth login must still default to the admin home: {location}"
        );
    }

    // ---------------------------------------------------------------
    // Lifecycle
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn disabled_user_cannot_oauth_in() {
        // A DISABLED account holding the address the provider will return.
        // It is `email_verified`, so the adoption gate lets the flow reach
        // the lifecycle gate under test.
        let email = "disabled@example.com";
        let ctx = OauthFlow::google(email).ctx().await;

        let user = seed_user(&ctx, email, true).await;
        users::set_disabled(&ctx, &user.id, true)
            .await
            .expect("disable user");
        assert!(
            users::find_by_id(&ctx, &user.id)
                .await
                .unwrap()
                .unwrap()
                .disabled,
            "fixture user must be disabled"
        );

        // The callback rejects with a PermissionDenied error stream (mapped to
        // HTTP 403 at the boundary).
        let out = handle(&ctx, &callback_msg(&ctx).await).await;
        assert!(
            crate::test_support::output_is_error(out, "PermissionDenied").await,
            "disabled account must be rejected at the OAuth callback"
        );

        // And no session row was minted for the disabled user.
        let session_rows = sessions::list_for_user(&ctx, &user.id)
            .await
            .expect("list sessions ok");
        assert!(
            session_rows.is_empty(),
            "no session may be created for a disabled OAuth login"
        );
    }

    /// Credential *issuance* paths (login / refresh / OAuth) must gate on
    /// soft-delete too, not just `disabled`. `db::soft_delete` leaves
    /// `local_credentials` and refresh tokens intact, so a soft-deleted user
    /// could otherwise authenticate by address and mint fresh tokens.
    #[tokio::test]
    async fn soft_deleted_user_cannot_oauth_in() {
        let email = "softdeleted@example.com";
        let ctx = OauthFlow::google(email).ctx().await;

        let user = seed_user(&ctx, email, true).await;
        // Soft-delete (stamps `deleted_at`) — NOT `disabled`. Mirrors the
        // lifecycle tests in `auth/repo/users.rs`.
        users::soft_delete(&ctx, &user.id)
            .await
            .expect("soft-delete user");
        let row = users::find_by_id(&ctx, &user.id).await.unwrap().unwrap();
        assert!(row.is_deleted(), "fixture user must be soft-deleted");
        assert!(!row.disabled, "fixture user must not be `disabled`");
        assert!(!row.is_active(), "soft-deleted user must not be active");

        let out = handle(&ctx, &callback_msg(&ctx).await).await;
        assert!(
            crate::test_support::output_is_error(out, "PermissionDenied").await,
            "soft-deleted account must be rejected at the OAuth callback"
        );

        let session_rows = sessions::list_for_user(&ctx, &user.id)
            .await
            .expect("list sessions ok");
        assert!(
            session_rows.is_empty(),
            "no session may be created for a soft-deleted OAuth login"
        );
    }

    /// The existing-provider-link path reuses `link.user_id`, which is the
    /// one branch that can authenticate a user it never read. A disabled user
    /// who already has a link must still be rejected by the shared gate.
    #[tokio::test]
    async fn disabled_pre_linked_user_cannot_oauth_in() {
        let email = "disabled-linked@example.com";
        let ctx = OauthFlow::google(email).ctx().await;

        let user = seed_user(&ctx, email, true).await;

        // Pre-existing provider link → callback takes the existing-link
        // branch. provider_ref must match the mock userinfo `sub`.
        provider_links::upsert(
            &ctx,
            provider_links::NewLink {
                provider: "google",
                provider_ref: GOOGLE_SUB,
                user_id: &user.id,
                provider_login: "disabled-linked",
                access_token: "old-token",
            },
        )
        .await
        .expect("seed provider link");

        // Disable AFTER linking.
        users::set_disabled(&ctx, &user.id, true)
            .await
            .expect("disable user");

        let out = handle(&ctx, &callback_msg(&ctx).await).await;
        assert!(
            crate::test_support::output_is_error(out, "PermissionDenied").await,
            "disabled pre-linked account must be rejected"
        );
        let session_rows = sessions::list_for_user(&ctx, &user.id)
            .await
            .expect("list sessions ok");
        assert!(
            session_rows.is_empty(),
            "no session for disabled pre-linked login"
        );
    }

    /// Same, but soft-deleted (deleted_at set, disabled=false) and pre-linked.
    #[tokio::test]
    async fn soft_deleted_pre_linked_user_cannot_oauth_in() {
        let email = "softdel-linked@example.com";
        let ctx = OauthFlow::google(email).ctx().await;

        let user = seed_user(&ctx, email, true).await;
        provider_links::upsert(
            &ctx,
            provider_links::NewLink {
                provider: "google",
                provider_ref: GOOGLE_SUB,
                user_id: &user.id,
                provider_login: "softdel-linked",
                access_token: "old-token",
            },
        )
        .await
        .expect("seed provider link");

        users::soft_delete(&ctx, &user.id)
            .await
            .expect("soft-delete user");

        let out = handle(&ctx, &callback_msg(&ctx).await).await;
        assert!(
            crate::test_support::output_is_error(out, "PermissionDenied").await,
            "soft-deleted pre-linked account must be rejected"
        );
        let session_rows = sessions::list_for_user(&ctx, &user.id)
            .await
            .expect("list sessions ok");
        assert!(
            session_rows.is_empty(),
            "no session for soft-deleted pre-linked login"
        );
    }
}
