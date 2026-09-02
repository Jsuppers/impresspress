//! Flow definitions for Impresspress.
//!
//! All API routing is handled by the `impresspress/router` block, which delegates
//! to `impresspress-core`'s shared pipeline. The only flow needed is `site-main`,
//! which dispatches API paths to the router and serves the SPA for everything
//! else. The wafer-core base flows (wafer-run/infra) provide middleware.

pub mod site_main;

use wafer_run::{RuntimeError, Wafer};

/// The middleware blocks whose config this module contributes to, named once.
const CORS_BLOCK: &str = "wafer-run/cors";
const SECURITY_HEADERS_BLOCK: &str = "wafer-run/security-headers";

/// Register the site-main flow (used with impresspress/router).
///
/// `cors_allowed_origins` and `csp_directives` are the operator-configured
/// values of [`crate::config_vars::CORS_ALLOWED_ORIGINS_KEY`] and
/// [`crate::config_vars::CSP_DIRECTIVES_KEY`], resolved from config by the
/// builder. They configure the `wafer-run/cors` and `wafer-run/security-
/// headers` middleware steps of this flow — the two infrastructure blocks
/// have no other config channel, so without this a native/Cloudflare deploy
/// denies every cross-origin request and blocks embedded Stripe.js.
///
/// `existing_block_configs` is what the consumer already declared through
/// [`crate::builder::ImpresspressBuilder::block_config`], in declaration
/// order. It is threaded in rather than read back off the `Wafer` because
/// there is nothing to read it back with: `Wafer::add_block_config` is a map
/// **insert**, the map is `pub(crate)` in the producer, and so a second
/// declaration for one block name silently replaces the first. See
/// [`site_main_block_configs`] for what that cost and what is done about it.
///
/// # Errors
///
/// Returns the underlying `RuntimeError` if the runtime rejects the
/// generated route config or the embedded `site_main::JSON` (a build-time
/// invariant — failure here means the bundled flow JSON drifted from the
/// runtime's flow schema).
pub fn register_site_main(
    w: &mut Wafer,
    cors_allowed_origins: &str,
    csp_directives: &str,
    existing_block_configs: &[(String, serde_json::Value)],
) -> Result<(), RuntimeError> {
    for (name, config) in
        site_main_block_configs(cors_allowed_origins, csp_directives, existing_block_configs)
    {
        w.add_block_config(&name, config);
    }

    w.add_flow_json(site_main::JSON)
}

/// Every block config `register_site_main` installs, given what the consumer
/// already declared.
///
/// Split out from the registration itself so the decision — in particular
/// which keys survive a second declaration — is testable without a `Wafer`,
/// and so the untested remainder is a three-line loop.
///
/// # Why two of these merge and two do not
///
/// `Wafer::add_block_config` replaces a block's whole config. For
/// `wafer-run/router` and `wafer-run/web` that is the intent: `site_main`
/// owns the route table and the SPA root, and a consumer that means to
/// override them says so with
/// [`crate::builder::ImpresspressBuilder::final_block_config`], which runs
/// after this.
///
/// For the two middleware blocks it was a bug. Their config is not one
/// setting but a bag of them, and this function is not their only author:
/// the browser sandbox declares
/// `{"csp": "… worker-src 'self' blob:; frame-src 'self'", "frame_ancestors": "self"}`
/// on `wafer-run/security-headers` so the `/b/dev` page can spawn its
/// compiler worker and frame its own live preview. Replacing that with
/// `{"csp": <shared directives>}` dropped both — silently, and only visible
/// as a browser refusing to start a worker. Plan 1 Task 10's e2e measured the
/// two bundles' `Content-Security-Policy` side by side and found them
/// byte-identical, which is what this fixes.
///
/// So: every key the consumer set is preserved, and `csp` — a `;`-separated
/// directive list that the security-headers block itself merges
/// directive-by-directive over its hard baseline — is **concatenated**, the
/// consumer's first. `allowed_origins` is not concatenated: it is a
/// comma-separated allow-list with its own separator, and joining two of them
/// with `; ` would produce a value neither author meant.
pub fn site_main_block_configs(
    cors_allowed_origins: &str,
    csp_directives: &str,
    existing_block_configs: &[(String, serde_json::Value)],
) -> Vec<(String, serde_json::Value)> {
    let mut out = vec![
        // Inject default routes into the router block config.
        (
            "wafer-run/router".to_string(),
            serde_json::json!({ "routes": site_main::default_routes() }),
        ),
        // Serve from the "site" storage bucket as an SPA.
        (
            "wafer-run/web".to_string(),
            serde_json::json!({ "web_root": "site", "web_spa": "true", "web_index": "index.html" }),
        ),
    ];

    // Feed the CORS allow-list to the middleware step. Empty stays fail-closed
    // (the block denies all cross-origin requests); a value or `*` opens it.
    // An empty value adds nothing at all rather than an empty config, so a
    // consumer-declared `wafer-run/cors` config is left exactly as it was.
    if !cors_allowed_origins.is_empty() {
        out.push((
            CORS_BLOCK.to_string(),
            merged(
                declared(existing_block_configs, CORS_BLOCK),
                serde_json::json!({ "allowed_origins": cors_allowed_origins }),
                &[],
            ),
        ));
    }

    // Feed extra CSP directives to the security-headers step. The block merges
    // these over its hard baseline (widen-only), so this can grant the Stripe
    // origins embedded Checkout needs without weakening the default policy.
    if !csp_directives.is_empty() {
        out.push((
            SECURITY_HEADERS_BLOCK.to_string(),
            merged(
                declared(existing_block_configs, SECURITY_HEADERS_BLOCK),
                serde_json::json!({ "csp": csp_directives }),
                &["csp"],
            ),
        ));
    }

    out
}

/// The config a consumer declared for `name`, or `None`.
///
/// The **last** declaration wins, because that is what `add_block_config`'s
/// map insert does with the same list — this reads the map the builder is
/// about to build, not the list.
fn declared<'a>(
    block_configs: &'a [(String, serde_json::Value)],
    name: &str,
) -> Option<&'a serde_json::Value> {
    block_configs
        .iter()
        .rev()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, config)| config)
}

/// `existing` with `additions` applied: keys in `concatenated` are joined
/// `"<existing>; <addition>"`, every other key is overwritten, and keys only
/// `existing` has are kept.
///
/// A non-object `existing` (or `None`) starts from an empty object — there is
/// nothing to preserve in a config that is not a map of settings, and
/// silently returning it unchanged would drop the additions instead.
fn merged(
    existing: Option<&serde_json::Value>,
    additions: serde_json::Value,
    concatenated: &[&str],
) -> serde_json::Value {
    let mut out = existing
        .and_then(|value| value.as_object())
        .cloned()
        .unwrap_or_default();
    let serde_json::Value::Object(additions) = additions else {
        return serde_json::Value::Object(out);
    };

    for (key, addition) in additions {
        let joined = concatenated.contains(&key.as_str()).then(|| {
            let before = out.get(&key).and_then(|v| v.as_str()).unwrap_or("");
            // Trim the separator off the left part so `"a;"` + `"b"` is
            // `"a; b"` and not `"a;; b"`; the block's parser tolerates the
            // empty directive, but the served header is a thing people read.
            let before = before.trim().trim_end_matches(';').trim_end();
            let after = addition.as_str().unwrap_or("").trim();
            match (before.is_empty(), after.is_empty()) {
                (true, _) => after.to_string(),
                (false, true) => before.to_string(),
                (false, false) => format!("{before}; {after}"),
            }
        });
        match joined {
            Some(joined) => out.insert(key, serde_json::Value::String(joined)),
            None => out.insert(key, addition),
        };
    }
    serde_json::Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `impresspress-web`'s `RuntimeFactory` declares on a
    /// `browser-devtools` build with `[dev] enabled` (`runtime_factory.rs`):
    /// the app CSP plus the sandbox's two additions, and the frame-ancestors
    /// relaxation its live-site preview iframe needs.
    fn sandbox_security_headers() -> Vec<(String, serde_json::Value)> {
        vec![(
            SECURITY_HEADERS_BLOCK.to_string(),
            serde_json::json!({
                "csp": "default-src 'self'; frame-ancestors 'none'; \
                        worker-src 'self' blob:; frame-src 'self'",
                "frame_ancestors": "self",
            }),
        )]
    }

    /// The shared `WAFER_RUN_SHARED__CSP_DIRECTIVES` value a deployment that
    /// embeds Stripe Checkout carries.
    const STRIPE: &str = "script-src https://js.stripe.com; frame-src https://js.stripe.com";

    fn config_for<'a>(
        configs: &'a [(String, serde_json::Value)],
        name: &str,
    ) -> &'a serde_json::Value {
        configs
            .iter()
            .rev()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, config)| config)
            .unwrap_or_else(|| panic!("no config for {name} in {configs:?}"))
    }

    /// The regression this whole function exists for: the shared directives
    /// must not take the consumer's config with them.
    ///
    /// Before the fix this was one `add_block_config` — a map insert — so the
    /// result was `{"csp": <Stripe only>}`: no `worker-src`, no
    /// `frame_ancestors`, and a `/b/dev` page whose compiler worker the
    /// browser would refuse to start. Plan 1 Task 10's e2e measured a sandbox
    /// bundle's `Content-Security-Policy` as byte-identical to a feature-off
    /// one, which is the same fact from the outside.
    #[test]
    fn shared_directives_merge_into_a_declared_security_headers_config() {
        let declared = sandbox_security_headers();
        let out = site_main_block_configs("", STRIPE, &declared);
        let merged = config_for(&out, SECURITY_HEADERS_BLOCK);

        let csp = merged["csp"].as_str().expect("csp is a string");
        // The consumer's directives survive…
        assert!(csp.contains("worker-src 'self' blob:"), "{csp}");
        assert!(csp.contains("frame-src 'self'"), "{csp}");
        assert!(csp.contains("default-src 'self'"), "{csp}");
        // …the shared ones are added…
        assert!(csp.contains("script-src https://js.stripe.com"), "{csp}");
        assert!(csp.contains("frame-src https://js.stripe.com"), "{csp}");
        // …in that order, joined by exactly one separator.
        assert!(
            csp.find("worker-src").unwrap() < csp.find("script-src https://js.stripe.com").unwrap(),
            "the consumer's directives come first: {csp}"
        );
        assert!(!csp.contains(";;"), "no doubled separator: {csp}");

        // …and every other key the consumer set is still there. This is the
        // one that actually moves `frame-ancestors` off `'none'`: the
        // security-headers block rewrites that directive from the
        // `frame_ancestors` key at request time, whatever `csp` says.
        assert_eq!(merged["frame_ancestors"], serde_json::json!("self"));
    }

    /// With nothing declared, the result is what it always was.
    #[test]
    fn with_no_declared_config_the_shared_directives_stand_alone() {
        let out = site_main_block_configs("", STRIPE, &[]);
        assert_eq!(
            config_for(&out, SECURITY_HEADERS_BLOCK),
            &serde_json::json!({ "csp": STRIPE }),
        );
    }

    /// A deployment with no shared directives must not lose the consumer's
    /// config either — the empty case adds nothing at all rather than an
    /// empty config that would replace it.
    #[test]
    fn empty_shared_directives_leave_a_declared_config_untouched() {
        let declared = sandbox_security_headers();
        let out = site_main_block_configs("", "", &declared);
        assert!(
            !out.iter().any(|(name, _)| name == SECURITY_HEADERS_BLOCK),
            "nothing to add means nothing is added: {out:?}"
        );
    }

    /// `allowed_origins` is a comma-separated list with its own separator, so
    /// it is replaced rather than joined — while the consumer's other keys on
    /// the same block survive.
    #[test]
    fn the_cors_allow_list_replaces_but_keeps_its_neighbours() {
        let declared = vec![(
            CORS_BLOCK.to_string(),
            serde_json::json!({ "allowed_origins": "https://old.example", "max_age": "600" }),
        )];
        let out = site_main_block_configs("https://new.example", "", &declared);
        assert_eq!(
            config_for(&out, CORS_BLOCK),
            &serde_json::json!({ "allowed_origins": "https://new.example", "max_age": "600" }),
        );
    }

    /// The router and the SPA root are `site_main`'s to own; a consumer that
    /// means to override them uses `final_block_config`, which runs after
    /// this. Pinned so "merge everything" is never applied here by analogy.
    #[test]
    fn the_router_and_web_configs_are_still_replacements() {
        let declared = vec![
            (
                "wafer-run/web".to_string(),
                serde_json::json!({ "web_root": "somewhere-else", "cache_mode": "no-cache" }),
            ),
            (
                "wafer-run/router".to_string(),
                serde_json::json!({ "routes": [] }),
            ),
        ];
        let out = site_main_block_configs("", "", &declared);
        assert_eq!(config_for(&out, "wafer-run/web")["web_root"], "site");
        assert!(
            config_for(&out, "wafer-run/web")
                .get("cache_mode")
                .is_none(),
            "web config is replaced wholesale, not merged"
        );
        assert_ne!(
            config_for(&out, "wafer-run/router")["routes"],
            serde_json::json!([]),
        );
    }

    /// `add_block_config` is a map insert, so of two declarations for one
    /// block the *last* is the one that will be in the map this merges into.
    #[test]
    fn the_last_declaration_for_a_block_is_the_one_merged_into() {
        let declared = vec![
            (
                SECURITY_HEADERS_BLOCK.to_string(),
                serde_json::json!({ "csp": "worker-src 'self'", "frame_ancestors": "self" }),
            ),
            (
                SECURITY_HEADERS_BLOCK.to_string(),
                serde_json::json!({ "csp": "img-src data:" }),
            ),
        ];
        let merged = config_for(
            &site_main_block_configs("", STRIPE, &declared),
            SECURITY_HEADERS_BLOCK,
        )
        .clone();
        let csp = merged["csp"].as_str().unwrap();
        assert!(csp.contains("img-src data:"), "{csp}");
        assert!(
            !csp.contains("worker-src"),
            "the first declaration is gone: {csp}"
        );
        assert!(merged.get("frame_ancestors").is_none());
    }

    /// A trailing separator on the consumer's value must not produce `";;"`,
    /// and an empty half must not produce a leading or dangling one.
    #[test]
    fn joining_never_produces_an_empty_directive() {
        let with_trailing = vec![(
            SECURITY_HEADERS_BLOCK.to_string(),
            serde_json::json!({ "csp": "worker-src 'self' blob:;  " }),
        )];
        let csp = config_for(
            &site_main_block_configs("", STRIPE, &with_trailing),
            SECURITY_HEADERS_BLOCK,
        )["csp"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(csp, format!("worker-src 'self' blob:; {STRIPE}"));

        let empty_declared = vec![(
            SECURITY_HEADERS_BLOCK.to_string(),
            serde_json::json!({ "csp": "", "frame_ancestors": "self" }),
        )];
        let merged = config_for(
            &site_main_block_configs("", STRIPE, &empty_declared),
            SECURITY_HEADERS_BLOCK,
        )
        .clone();
        assert_eq!(merged["csp"], serde_json::json!(STRIPE));
        assert_eq!(merged["frame_ancestors"], serde_json::json!("self"));
    }

    /// A config that is not an object has no settings to preserve; the
    /// additions must still be applied rather than dropped.
    #[test]
    fn a_non_object_declared_config_is_replaced_not_honoured() {
        let declared = vec![(
            SECURITY_HEADERS_BLOCK.to_string(),
            serde_json::json!("not a config"),
        )];
        assert_eq!(
            config_for(
                &site_main_block_configs("", STRIPE, &declared),
                SECURITY_HEADERS_BLOCK
            ),
            &serde_json::json!({ "csp": STRIPE }),
        );
    }
}
