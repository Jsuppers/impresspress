use std::collections::{BTreeMap, HashMap};

use wafer_core::clients::database as db;
use wafer_run::{context::Context, ConfigVar, InputStream, InputType, Message, OutputStream};

use super::{
    contracts::{AdminSettingView, AdminSettingsResponse},
    ops::{self, MASKED_VALUE},
};
use crate::{
    blocks::crud,
    http::{err_bad_request, err_internal, err_not_found, ok_json},
    platform_state::{
        block_settings::{self, BlockSettingsPatch},
        variables::{self, NewVariable, VariablePatch},
    },
};

/// `GET /b/admin/api/settings/all`.
pub(super) async fn handle_list_full(ctx: &dyn Context) -> OutputStream {
    match variables::list_all(ctx).await {
        Ok(rows) => {
            let vars: Vec<_> = rows
                .iter()
                .map(|row| {
                    let is_sensitive = ops::is_sensitive_key(&row.key, i64::from(row.sensitive));
                    let is_system = row.key.starts_with("WAFER_RUN_SHARED__");
                    // Mask sensitive values even in the "full" listing
                    let value = if is_sensitive {
                        MASKED_VALUE.to_string()
                    } else {
                        row.value.clone()
                    };
                    serde_json::json!({
                        "key": row.key,
                        "name": row.name,
                        "description": row.description,
                        "value": value,
                        "warning": row.warning,
                        "sensitive": is_sensitive,
                        "system": is_system,
                        "updated_at": row.updated_at,
                    })
                })
                .collect();
            ok_json(&vars)
        }
        Err(e) => err_internal("Database error", e),
    }
}

/// `GET /b/admin/api/settings`.
pub(super) async fn handle_list(ctx: &dyn Context) -> OutputStream {
    match variables::list_all(ctx).await {
        Ok(rows) => {
            // Collected into a `BTreeMap` first, then flattened: the
            // response is a public contract, and a randomized key order made
            // two identical reads differ byte for byte.
            let mut by_key = BTreeMap::new();
            for row in &rows {
                let sensitive = ops::is_sensitive_key(&row.key, i64::from(row.sensitive));
                let value = if sensitive {
                    MASKED_VALUE.to_string()
                } else {
                    row.value.clone()
                };
                by_key.insert(
                    row.key.clone(),
                    AdminSettingView {
                        key: row.key.clone(),
                        value: serde_json::Value::String(value),
                        sensitive,
                    },
                );
            }
            ok_json(&AdminSettingsResponse {
                settings: by_key.into_values().collect(),
            })
        }
        Err(e) => err_internal("Database error", e),
    }
}

/// `GET /b/admin/api/settings/{key}`. `{key}` is read only as the route
/// table bound it.
pub(super) async fn handle_get(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let key = match crud::path_var(msg, "key", "Missing setting key") {
        Ok(value) => value,
        Err(response) => return response,
    };

    match variables::get_by_key(ctx, key).await {
        Ok(Some(mut row)) => {
            // SEC-060: mask on the row flag OR the `_SECRET` / `_KEY` suffix —
            // the single-key getter previously masked on the flag alone, so a
            // `*_SECRET` key with the flag unset leaked its value here.
            if ops::is_sensitive_key(key, i64::from(row.sensitive)) {
                row.value = MASKED_VALUE.to_string();
            }
            // The row is echoed in the `{id, data}` record envelope this
            // endpoint has always published; it is declared without a schema
            // until it is typed.
            ok_json(&db::Record {
                id: row.id.clone(),
                data: row.to_data(),
            })
        }
        Ok(None) => err_not_found("Setting not found"),
        Err(e) => err_internal("Database error", e),
    }
}

/// `PATCH /b/admin/api/settings/{key}`. `{key}` is read only as the route
/// table bound it.
pub(super) async fn handle_set(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let key = match crud::path_var(msg, "key", "Missing setting key") {
        Ok(value) => value,
        Err(response) => return response,
    };

    #[derive(serde::Deserialize)]
    struct Req {
        value: serde_json::Value,
    }
    let raw = input.collect_to_bytes().await;
    let body: Req = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    // The `value` column is TEXT; a string value is stored verbatim, anything
    // else as its JSON form (the prior validation already read it via
    // `as_str().unwrap_or("")`, so non-string values were treated as empty).
    let value = match &body.value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };

    // Guards (sensitive-empty + URL/SSRF), audit-log write, and upsert live in
    // the shared ops layer so the SSR variable surface can't diverge.
    match ops::update_variable(
        ctx,
        msg,
        key,
        ops::VariableUpdate {
            value: Some(&value),
            description: None,
        },
    )
    .await
    {
        // Echoed in the `{id, data}` record envelope this endpoint has
        // always published; declared without a schema until it is typed.
        Ok(row) => ok_json(&db::Record {
            id: row.id.clone(),
            data: row.to_data(),
        }),
        Err(out) => out,
    }
}

/// `POST /b/admin/api/settings`.
pub(super) async fn handle_create(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    #[derive(serde::Deserialize)]
    struct Req {
        key: String,
        value: Option<String>,
        name: Option<String>,
        description: Option<String>,
        sensitive: Option<bool>,
    }
    let raw = input.collect_to_bytes().await;
    let body: Req = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };
    // Key-empty guard, URL/SSRF validation, audit-log write, and the create
    // live in the shared ops layer so the SSR variable surface can't diverge.
    match ops::create_variable(
        ctx,
        msg,
        &body.key,
        body.value.as_deref().unwrap_or(""),
        body.name.as_deref(),
        body.description.as_deref(),
        // Absent means sensitive. A caller that does not say is protected;
        // one that wants a plain-text variable says `false`. The old `false`
        // default published any key without a `_SECRET`/`_KEY` suffix in
        // plain text unless the operator remembered the box.
        body.sensitive.unwrap_or(true),
    )
    .await
    {
        // Echoed in the `{id, data}` record envelope this endpoint has
        // always published; declared without a schema until it is typed.
        Ok(row) => ok_json(&db::Record {
            id: row.id.clone(),
            data: row.to_data(),
        }),
        Err(out) => out,
    }
}

/// `DELETE /b/admin/api/settings/{key}`. `{key}` is read only as the route
/// table bound it.
pub(super) async fn handle_delete(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let key = match crud::path_var(msg, "key", "Missing setting key") {
        Ok(value) => value,
        Err(response) => return response,
    };

    if key.starts_with("WAFER_RUN_SHARED__") {
        return err_bad_request(&format!("Cannot delete shared system variable: {key}"));
    }

    match variables::get_by_key(ctx, key).await {
        Ok(Some(row)) => match variables::delete(ctx, &row.id).await {
            Ok(()) => ok_json(&serde_json::json!({"deleted": key})),
            Err(e) => err_internal("Database error", e),
        },
        Ok(None) => err_not_found("Setting not found"),
        Err(e) => err_internal("Database error", e),
    }
}

/// Full block name of the admin block — the `block_settings` row whose
/// `seed_defaults_hash` column gates this function.
const ADMIN_BLOCK_NAME: &str = "impresspress/admin";

/// Compute a deterministic SHA-256 hex digest over the declared shared
/// config vars. Anything that affects the seed outcome (key, name,
/// description, default, warning, sensitive flag) feeds the hash; sort by
/// key so map ordering can't make two equivalent inputs hash differently.
///
/// `var.auto_generate` / `var.optional` don't affect what `seed_defaults`
/// writes — they're consumed by `seed_auto_generated` (CF runner) and the
/// startup validator respectively — so they're intentionally omitted.
fn seed_payload_hash(vars: &[ConfigVar]) -> String {
    use std::fmt::Write as _;
    let mut keys: Vec<&ConfigVar> = vars.iter().collect();
    keys.sort_by(|a, b| a.key.cmp(&b.key));
    let mut buf = String::with_capacity(vars.len() * 128);
    for v in keys {
        let sensitive = if v.input_type == InputType::Password {
            1
        } else {
            0
        };
        // Fixed shape per var: `key\x1fname\x1fdescription\x1fdefault\x1fwarning\x1fsensitive\x1e`.
        // ASCII unit-separator (0x1f) + record-separator (0x1e) bracket
        // each field so embedded newlines / colons in description text
        // can't collide field boundaries across different var shapes.
        let _ = write!(
            &mut buf,
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1e}",
            v.key, v.name, v.description, v.default, v.warning, sensitive,
        );
    }
    crate::migration_helper::sha256_hex_bytes(buf.as_bytes())
}

pub async fn seed_defaults(ctx: &dyn Context) {
    let vars = crate::config_vars::shared_config_vars();

    // Hash-gate: if the cached `block_settings.seed_defaults_hash` row for
    // the admin block already matches the current declared-vars hash, every
    // shared var was seeded against the same metadata last time — there is
    // no outcome change possible and we can skip the entire seed (zero D1
    // queries). Mirrors `migration_helper::apply_if_blessed`'s gate; reads
    // the same in-memory snapshot the migration helper does, so warm cold
    // starts cost zero round-trips. See 2026-05-14 config-snapshot spec
    // § "Hash-gate seed_defaults like migrations" (PR 3).
    let code_hash = seed_payload_hash(&vars);
    let json = ctx
        .config_get(crate::features::BLOCK_SETTINGS_CONFIG_KEY)
        .unwrap_or("{}");
    let cached_hash =
        crate::features::BlockSettings::state_for(json, ADMIN_BLOCK_NAME).seed_defaults_hash;
    if cached_hash == code_hash && !code_hash.is_empty() {
        return;
    }

    // Single bulk fetch of every existing variable, then in-memory diff
    // per declared shared var. Replaces the per-var `get_by_field` loop
    // that issued 2× D1 queries per shared var × cold isolate (~5k D1
    // reads/day in prod — see 2026-05-14 config-snapshot spec). On a
    // bulk failure we treat every key as missing, which falls into the
    // create-with-INSERT-OR-IGNORE-equivalent path; consistent with the
    // prior code's silent-on-error stance.
    let existing: HashMap<String, _> = variables::list_all(ctx)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|row| (row.key.clone(), row))
        .collect();

    for var in &vars {
        let sensitive = var.input_type == InputType::Password;
        let name = if var.name.is_empty() {
            &var.key
        } else {
            &var.name
        };

        match existing.get(&var.key) {
            Some(row) => {
                // A stored value pointing at our own `/b/static/` route for a
                // file this build does not serve is a stale pointer *this
                // function wrote*: built-in asset URLs carry a content hash
                // and are seeded into the database as defaults, so any release
                // that changes the artwork — new logo, new favicon, or the
                // removed raster wordmark whose route is gone outright —
                // leaves every existing deployment pointing at a 404. Left
                // alone it renders a broken image on every page that shows
                // the brand.
                //
                // Repaired here rather than in an admin migration because
                // migrations are gated (`--run-migrations`, see RELEASE.md)
                // and a broken logo gives an operator no signal to opt in,
                // whereas `seed_defaults` runs on every boot's `Init`. It
                // costs one manifest scan per stored value per boot and is
                // idempotent: once repaired the value names a served file, so
                // it no longer matches. Scoped to the built-in route by
                // `is_stale_builtin_asset_url`, so a white-labelled URL is
                // never touched.
                let stale_builtin_asset = crate::ui::assets::is_stale_builtin_asset_url(&row.value);

                // Only refresh metadata when at least one declared field
                // actually differs. Without this guard every isolate cold-start
                // re-writes every shared config var (~80 vars × cold-starts/day
                // ≈ ~900 useless UPDATEs/day in prod).
                let same_name = row.name == *name;
                let same_desc = row.description == var.description;
                let same_warn = row.warning == var.warning;
                let same_sens = row.sensitive == sensitive;
                if same_name && same_desc && same_warn && same_sens && !stale_builtin_asset {
                    continue;
                }
                let mut patch = VariablePatch {
                    name: Some(name.clone()),
                    description: Some(var.description.clone()),
                    warning: Some(var.warning.clone()),
                    sensitive: Some(sensitive),
                    ..Default::default()
                };
                if stale_builtin_asset {
                    tracing::warn!(
                        key = %var.key,
                        stale = %row.value,
                        repaired_to = %var.default,
                        "repaired a persisted URL for a built-in asset this build \
                         no longer serves; reset to the declared default"
                    );
                    patch.value = Some(var.default.clone());
                }
                let _ = variables::upsert_by_key(ctx, &var.key, patch).await;
            }
            None => {
                // Seed from process env when set (lets `.env` bootstrap a
                // fresh deployment), otherwise fall back to the declared
                // default. Empty env values are treated as unset so that
                // `FOO=` doesn't accidentally clear a meaningful default.
                let seed_value = std::env::var(&var.key)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| var.default.clone());
                if !seed_value.is_empty() {
                    let _ = variables::insert(
                        ctx,
                        NewVariable {
                            key: var.key.clone(),
                            value: seed_value,
                            name: name.clone(),
                            description: var.description.clone(),
                            warning: var.warning.clone(),
                            sensitive,
                            updated_by: String::new(),
                            block: variables::block_for_key(&var.key),
                        },
                    )
                    .await;
                }
            }
        }
    }

    // Stamp the new hash on the admin block_settings row so the next cold
    // start short-circuits before issuing `list_all`. Failures here are
    // logged but non-fatal — the seed itself succeeded, and the worst case
    // is that the next isolate re-runs the bulk `list_all` (the same cost
    // we paid this run). Matches the "silent on error" stance of the
    // per-var upsert/create calls above; the `block_settings` row may not
    // exist yet (admin migrations create it on the same `Init` pass),
    // which is why we use `upsert_fields` rather than assuming a row.
    let patch = BlockSettingsPatch {
        seed_defaults_hash: Some(code_hash),
        ..Default::default()
    };
    if let Err(e) = block_settings::upsert_fields(ctx, ADMIN_BLOCK_NAME, patch).await {
        tracing::warn!(
            err = %e,
            "seed_defaults: failed to stamp seed_defaults_hash; next cold start will re-run the bulk list_all"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestContext;

    /// Seed one `variables` row with an explicit `sensitive` flag.
    async fn seed_var(ctx: &dyn Context, key: &str, value: &str, sensitive: bool) {
        variables::insert(
            ctx,
            NewVariable {
                key: key.to_string(),
                value: value.to_string(),
                name: key.to_string(),
                description: String::new(),
                warning: String::new(),
                sensitive,
                updated_by: String::new(),
                block: variables::block_for_key(key),
            },
        )
        .await
        .expect("seed variable");
    }

    /// `GET /b/admin/api/settings` never publishes a secret value.
    ///
    /// The endpoint's OpenAPI description promises exactly this, and the
    /// promise rests on two independent halves of `is_sensitive_key`: the
    /// row's `sensitive` flag, and the `_SECRET` / `_KEY` suffix convention.
    /// A key needs only one of them. Nothing tested this before the endpoint
    /// was documented, which is the worst order to do it in.
    #[tokio::test]
    async fn list_masks_every_sensitive_value() {
        let ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");

        // Not sensitive: no flag, no suffix.
        seed_var(&ctx, "SITE_NAME", "Acme", false).await;
        // Sensitive by suffix alone (SEC-060): the flag is clear.
        seed_var(&ctx, "STRIPE_SECRET", "sk_live_realsecret", false).await;
        seed_var(&ctx, "MAILGUN_API_KEY", "key-realsecret", false).await;
        // Sensitive by flag alone: `InputType::Password` vars carry neither
        // suffix, and `seed_defaults` is what sets their flag.
        seed_var(&ctx, "BOOTSTRAP_ADMIN_PASSWORD", "hunter2", true).await;

        let body = crate::test_support::output_json(handle_list(&ctx).await).await;
        let by_key: std::collections::HashMap<&str, &serde_json::Value> = body["settings"]
            .as_array()
            .expect("settings is an array")
            .iter()
            .map(|entry| (entry["key"].as_str().expect("key is a string"), entry))
            .collect();

        assert_eq!(
            by_key["SITE_NAME"]["value"],
            serde_json::json!("Acme"),
            "a non-sensitive value must be published unchanged"
        );
        assert_eq!(
            by_key["SITE_NAME"]["sensitive"],
            serde_json::json!(false),
            "a non-sensitive variable must say so"
        );
        for masked in [
            "STRIPE_SECRET",
            "MAILGUN_API_KEY",
            "BOOTSTRAP_ADMIN_PASSWORD",
        ] {
            assert_eq!(
                by_key[masked]["value"],
                serde_json::json!(MASKED_VALUE),
                "{masked} must be masked"
            );
            assert_eq!(
                by_key[masked]["sensitive"],
                serde_json::json!(true),
                "{masked} must be flagged sensitive, or a reader cannot tell \
                 the mask from a literal value"
            );
        }

        let raw = body.to_string();
        for secret in ["sk_live_realsecret", "key-realsecret", "hunter2"] {
            assert!(
                !raw.contains(secret),
                "GET /b/admin/api/settings leaked `{secret}`: {raw}"
            );
        }
    }

    /// `seed_payload_hash` is independent of input order (sorts by `key`).
    #[test]
    fn payload_hash_independent_of_input_order() {
        let a = ConfigVar::new("AAA", "first", "1");
        let b = ConfigVar::new("BBB", "second", "2");
        let h1 = seed_payload_hash(&[a.clone(), b.clone()]);
        let h2 = seed_payload_hash(&[b, a]);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    /// Hash changes whenever any seed-relevant field changes.
    #[test]
    fn payload_hash_sensitive_to_each_field() {
        let base = vec![ConfigVar::new("KEY", "desc", "def")];
        let h_base = seed_payload_hash(&base);

        let mut name_changed = base.clone();
        name_changed[0].name = "label".into();
        assert_ne!(h_base, seed_payload_hash(&name_changed));

        let mut desc_changed = base.clone();
        desc_changed[0].description = "different".into();
        assert_ne!(h_base, seed_payload_hash(&desc_changed));

        let mut default_changed = base.clone();
        default_changed[0].default = "other".into();
        assert_ne!(h_base, seed_payload_hash(&default_changed));

        let mut warning_changed = base.clone();
        warning_changed[0].warning = "careful".into();
        assert_ne!(h_base, seed_payload_hash(&warning_changed));

        let mut sensitive_changed = base;
        sensitive_changed[0].input_type = InputType::Password;
        assert_ne!(h_base, seed_payload_hash(&sensitive_changed));
    }

    /// End-to-end: after `seed_defaults` runs once, the admin
    /// `block_settings` row carries the current hash. Wiring that hash into
    /// the next cold-start's config snapshot short-circuits `seed_defaults`
    /// before it can touch the `variables` table — even if every row was
    /// deleted between starts.
    #[tokio::test]
    async fn second_call_with_matching_snapshot_hash_short_circuits() {
        let ctx = TestContext::new().await;

        // 1. Run admin migrations so the block_settings + variables tables
        //    exist (with the new seed_defaults_hash column).
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");

        // 2. First seed run — populates variables + stamps the hash row.
        seed_defaults(&ctx).await;
        let var_count_after_first = variables::list_all(&ctx)
            .await
            .expect("list variables")
            .len();
        assert!(
            var_count_after_first > 0,
            "first seed_defaults should populate at least one variable"
        );

        // 3. Read the stamped hash from the block_settings row directly.
        let admin_rows: Vec<_> = block_settings::list_all(&ctx)
            .await
            .expect("list block_settings")
            .into_iter()
            .filter(|row| row.block_name == ADMIN_BLOCK_NAME)
            .collect();
        assert_eq!(
            admin_rows.len(),
            1,
            "admin block_settings row should be present after first seed_defaults"
        );
        let stamped_hash = admin_rows[0].seed_defaults_hash.clone();
        let code_hash = seed_payload_hash(&crate::config_vars::shared_config_vars());
        assert_eq!(
            stamped_hash, code_hash,
            "stamped seed_defaults_hash should match current declared vars",
        );

        // 4. Simulate a fresh cold start: build a new TestContext (fresh
        //    in-memory DB — no variables, no block_settings row), but
        //    pre-populate the config snapshot with the stamped hash. This
        //    mirrors what the production loader does on the next boot.
        let mut next_ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&next_ctx)
            .await
            .expect("apply admin migrations on next ctx");
        let snapshot = serde_json::json!({
            ADMIN_BLOCK_NAME: { "enabled": true, "seed_defaults_hash": stamped_hash }
        })
        .to_string();
        next_ctx.set_config(crate::features::BLOCK_SETTINGS_CONFIG_KEY, &snapshot);

        // 5. seed_defaults should short-circuit before any list_all on
        //    variables — leaving the (empty) variables table untouched.
        seed_defaults(&next_ctx).await;
        let var_count_after_second = variables::list_all(&next_ctx)
            .await
            .expect("list variables on next ctx")
            .len();
        assert_eq!(
            var_count_after_second, 0,
            "seed_defaults should short-circuit when snapshot hash matches; \
             expected 0 rows in fresh variables table, got {var_count_after_second}"
        );
    }

    /// When the snapshot's cached hash differs from the current code hash
    /// (e.g. a new shared var was declared), `seed_defaults` runs again
    /// and re-stamps the row.
    #[tokio::test]
    async fn mismatched_snapshot_hash_re_runs_seed() {
        let mut ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");

        // Pre-populate the snapshot with a deliberately-wrong hash.
        let snapshot = serde_json::json!({
            ADMIN_BLOCK_NAME: {
                "enabled": true,
                "seed_defaults_hash": "deadbeef".to_string(),
            }
        })
        .to_string();
        ctx.set_config(crate::features::BLOCK_SETTINGS_CONFIG_KEY, &snapshot);

        seed_defaults(&ctx).await;
        let count = variables::list_all(&ctx)
            .await
            .expect("list variables")
            .len();
        assert!(
            count > 0,
            "mismatched snapshot hash should still run the seed; got 0 rows"
        );
    }

    /// SEC-060 regression: the single-key getter must mask a `*_SECRET` value
    /// even when its `sensitive` flag is 0 (the prior code masked on the flag
    /// alone, leaking the secret here).
    #[tokio::test]
    async fn handle_get_masks_secret_suffix_without_flag() {
        use crate::test_support::{admin_msg, output_json};

        let ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");

        // Insert a *_SECRET row with the sensitive flag explicitly unset.
        seed_var(&ctx, "STRIPE_SECRET", "sk_live_supersecret", false).await;

        let msg = crate::blocks::admin::test_support::routed(admin_msg(
            "retrieve",
            "/b/admin/api/settings/STRIPE_SECRET",
        ));
        let body = output_json(handle_get(&ctx, &msg).await).await;
        // `Record` serializes as `{ id, data: { value, ... } }`.
        assert_eq!(
            body.get("data")
                .and_then(|d| d.get("value"))
                .and_then(|v| v.as_str()),
            Some(MASKED_VALUE),
            "a *_SECRET value must be masked even with the sensitive flag unset"
        );
    }

    /// Read one variable row's `value` column.
    async fn stored_value(ctx: &dyn Context, key: &str) -> Option<String> {
        variables::get_by_key(ctx, key)
            .await
            .expect("get variable")
            .map(|row| row.value)
    }

    /// Releases before the pixel-art mark seeded `LOGO_URL` with the built-in
    /// raster wordmark's content-hashed URL. That asset and its route are
    /// gone, so a deployment still carrying the seeded value renders a broken
    /// image on every auth card, sidebar and account card. `seed_defaults`
    /// must clear it back to blank so the app-name fallback takes over.
    #[tokio::test]
    async fn seed_defaults_clears_the_removed_builtin_wordmark_url() {
        let ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");

        // Exactly what an older release's `seed_defaults` wrote: the route
        // prefix plus that release's content hash.
        seed_var(
            &ctx,
            crate::config_vars::LOGO_URL_KEY,
            "/b/static/impresspress-logo-long-1f4c8ab2.png",
            false,
        )
        .await;

        seed_defaults(&ctx).await;

        assert_eq!(
            stored_value(&ctx, crate::config_vars::LOGO_URL_KEY)
                .await
                .as_deref(),
            Some(""),
            "a persisted pointer at the removed built-in wordmark must be \
             cleared so the app-name fallback renders"
        );
    }

    /// The repair reaches a *real* upgrade, not just a blank slate.
    ///
    /// The seed short-circuits when the stamped `seed_defaults_hash` matches
    /// the current declared vars, so the repair only ever runs if that gate
    /// opens. It does: this release changed `LOGO_URL`'s declared default
    /// *and* its description, both of which feed `seed_payload_hash`, so any
    /// older release's stamped hash necessarily differs. This pins that —
    /// stamp a prior release's hash, then assert the stale value is still
    /// repaired.
    #[tokio::test]
    async fn stale_wordmark_is_repaired_through_a_prior_releases_stamped_hash() {
        let mut ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");

        // A prior release's declared LOGO_URL: the old description, and the
        // built-in wordmark URL as the default. Its hash is what that release
        // would have stamped.
        let mut prior = crate::config_vars::shared_config_vars();
        let logo = prior
            .iter_mut()
            .find(|v| v.key == crate::config_vars::LOGO_URL_KEY)
            .expect("LOGO_URL must be a declared shared var");
        logo.description = "Logo shown in header and emails".into();
        logo.default = "/b/static/impresspress-logo-long-1f4c8ab2.png".into();
        let prior_hash = seed_payload_hash(&prior);

        ctx.set_config(
            crate::features::BLOCK_SETTINGS_CONFIG_KEY,
            &serde_json::json!({
                ADMIN_BLOCK_NAME: { "enabled": true, "seed_defaults_hash": prior_hash }
            })
            .to_string(),
        );
        seed_var(
            &ctx,
            crate::config_vars::LOGO_URL_KEY,
            "/b/static/impresspress-logo-long-1f4c8ab2.png",
            false,
        )
        .await;

        seed_defaults(&ctx).await;

        assert_eq!(
            stored_value(&ctx, crate::config_vars::LOGO_URL_KEY)
                .await
                .as_deref(),
            Some(""),
            "the hash gate must open on upgrade so the repair runs"
        );
    }

    /// The repair above is scoped to the built-in wordmark's own route. An
    /// operator's white-label logo is their data and must survive untouched.
    #[tokio::test]
    async fn seed_defaults_keeps_an_operator_configured_logo_url() {
        let ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");

        seed_var(
            &ctx,
            crate::config_vars::LOGO_URL_KEY,
            "https://acme.example/wordmark.png",
            false,
        )
        .await;

        seed_defaults(&ctx).await;

        assert_eq!(
            stored_value(&ctx, crate::config_vars::LOGO_URL_KEY)
                .await
                .as_deref(),
            Some("https://acme.example/wordmark.png"),
            "a white-labelled logo URL must not be cleared"
        );
    }

    /// The repair is not specific to the removed wordmark. Any release that
    /// changes an asset's bytes changes its content hash, and the previous
    /// hash is already seeded into every existing deployment's database —
    /// so the sidebar mark and the browser tab icon go dead on upgrade
    /// exactly like the wordmark did. Unlike the wordmark, these assets still
    /// exist, so the repair must point them at the *current* default rather
    /// than blank.
    #[tokio::test]
    async fn seed_defaults_repairs_stale_builtin_logo_and_favicon_urls() {
        let ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");

        // What a release built before the artwork changed would have seeded:
        // the right route, a hash this build no longer serves.
        seed_var(
            &ctx,
            "WAFER_RUN_SHARED__LOGO_ICON_URL",
            "/b/static/impresspress-logo-5e884a3a.png",
            false,
        )
        .await;
        seed_var(
            &ctx,
            "WAFER_RUN_SHARED__FAVICON_URL",
            "/b/static/favicon-2845a6ac.ico",
            false,
        )
        .await;

        seed_defaults(&ctx).await;

        assert_eq!(
            stored_value(&ctx, "WAFER_RUN_SHARED__LOGO_ICON_URL")
                .await
                .as_deref(),
            Some(crate::ui::assets::logo_icon_url().as_str()),
            "a stale built-in logo URL must be repaired to the current asset"
        );
        assert_eq!(
            stored_value(&ctx, "WAFER_RUN_SHARED__FAVICON_URL")
                .await
                .as_deref(),
            Some(crate::ui::assets::favicon_url().as_str()),
            "a stale built-in favicon URL must be repaired to the current asset"
        );
    }

    /// A row already naming the current asset is left alone — the repair is
    /// idempotent, and must not churn a write on every boot.
    #[tokio::test]
    async fn seed_defaults_leaves_a_current_builtin_logo_url_untouched() {
        let ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");

        let current = crate::ui::assets::logo_icon_url();
        seed_var(&ctx, "WAFER_RUN_SHARED__LOGO_ICON_URL", &current, false).await;

        seed_defaults(&ctx).await;

        assert_eq!(
            stored_value(&ctx, "WAFER_RUN_SHARED__LOGO_ICON_URL")
                .await
                .as_deref(),
            Some(current.as_str()),
        );
    }
}

#[cfg(test)]
mod create_tests {
    use wafer_run::InputStream;

    use super::*;
    use crate::test_support::{admin_msg, collect_or_panic, TestContext};

    async fn admin_ctx() -> TestContext {
        let ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");
        ctx
    }

    async fn sensitive_flag(ctx: &dyn Context, key: &str) -> bool {
        variables::get_by_key(ctx, key)
            .await
            .expect("get variable")
            .unwrap_or_else(|| panic!("{key} was not created"))
            .sensitive
    }

    async fn create(ctx: &dyn Context, body: serde_json::Value) {
        let out = handle_create(
            ctx,
            &admin_msg("create", "/b/admin/api/settings"),
            InputStream::from_bytes(serde_json::to_vec(&body).unwrap()),
        )
        .await;
        collect_or_panic(out).await;
    }

    /// An ad hoc variable created without saying whether it is sensitive is
    /// stored as sensitive. Masking an innocuous value costs the operator one
    /// click to undo; publishing a secret in plain text — which is what the
    /// old `false` default did for any key without a `_SECRET`/`_KEY`
    /// suffix — cannot be undone.
    #[tokio::test]
    async fn create_defaults_to_sensitive_when_the_flag_is_omitted() {
        let ctx = admin_ctx().await;
        create(
            &ctx,
            serde_json::json!({"key": "SITE_MOTTO", "value": "move fast"}),
        )
        .await;
        assert!(sensitive_flag(&ctx, "SITE_MOTTO").await);
    }

    #[tokio::test]
    async fn create_honours_an_explicit_not_sensitive() {
        let ctx = admin_ctx().await;
        create(
            &ctx,
            serde_json::json!({"key": "SITE_MOTTO", "value": "move fast", "sensitive": false}),
        )
        .await;
        assert!(!sensitive_flag(&ctx, "SITE_MOTTO").await);
    }
}
