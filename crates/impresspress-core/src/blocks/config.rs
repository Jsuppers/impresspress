//! Impresspress config block — the variables table is the config store.
//!
//! Wraps wafer-core's `ConfigBlock` so `info()` and the interface declaration
//! stay single-sourced, and takes over the two operations that decide whether
//! an admin's saved setting is real: `CONFIG_GET` and `CONFIG_SET`.
//!
//! ## Why this block exists
//!
//! wafer-core's block serves both operations from a `ConfigService`, which is
//! a **synchronous** trait (`fn get(&self, key) -> Option<String>`). A
//! synchronous reader cannot consult a database, so every target answered
//! reads from an in-memory map instead:
//!
//! - native seeded an `EnvConfigService` once at boot from the `variables`
//!   table, so an admin's `PATCH /b/admin/api/settings/{key}` — which writes
//!   only that table — was invisible until the process restarted;
//! - Cloudflare's `HashMapConfigService::set` is a no-op, so the five admin
//!   forms behind `ui::settings_form::save_settings` answered
//!   `200 Settings saved` and changed nothing at all.
//!
//! A `Block`'s `handle` is async, which a `ConfigService` is not. That is the
//! whole reason the fix is a block rather than another service implementation:
//! it can await the database without a sync bridge.
//!
//! ## Read order
//!
//! 1. Runtime-owned keys always come from the boot map — see
//!    `served_only_from_boot_map`. Infrastructure keys (`IMPRESSPRESS_*`
//!    without `__`) and internal adapter-injected keys (`__…__`) are never
//!    variables-table config by the repo's naming rules, so a row carrying one
//!    must not be served: the browser's `__IMPRESSPRESS_RUNTIME_KIND__` marker
//!    is what keeps Stripe secret-key operations off in a visitor's browser.
//!    The JWT secret is the named exception — a table row on native, but the
//!    boot map already holds that value, and a row must not rotate the signing
//!    key out from under a running process. `CONFIG_SET` refuses all of them.
//! 2. Otherwise the `variables` row wins when it holds a non-empty value.
//!    This is the actual fix: an admin write lands in the table and the very
//!    next read sees it.
//! 3. Otherwise the boot map answers. It carries what the table cannot —
//!    worker/env bindings, builder-time vars (CORS, CSP, STRICT_SCHEMA) and
//!    the synthetic block-settings JSON.
//! 4. Otherwise `NotFound`, exactly as wafer-core's block reports it, so
//!    `config::get_default`'s fallback-to-default behaviour is unchanged.
//!
//! An empty row value deliberately falls through to the boot map rather than
//! masking it. Blank means "unset", not "explicitly blank", everywhere in this
//! repo — the boot seeder skips an empty env value and
//! `admin::settings::seed_defaults` skips an empty declared default for
//! exactly that reason — so a row that ended up blank must not shadow what the
//! boot map holds for the key.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use wafer_block::{codec, wire::config as wire, ServiceOp};
use wafer_core::interfaces::{config::service::ConfigService, database::service::DatabaseService};
use wafer_run::{
    context::Context, Block, BlockInfo, ErrorCode, InputStream, Message, OutputStream,
    ResourceType, WaferError,
};

use crate::{
    config_generation::config_write_generation,
    // audit-allow: this block never reaches the table under WRAP — it reads and writes through `variables`' boot-flavour API (`load_all`, `find_by_key`, `set`) over the raw `DatabaseService` that `builder::registration` hands it, the same way `D1ConfigSource` reads this table, so no grant applies (a `ctx`-routed read IS denied: see `the_config_block_reads_the_variables_table_under_wrap`); the audit also derives the caller `impresspress/config` from the file path, while the block registers as `wafer-run/config`
    platform_state::variables,
    util::{is_sensitive_key, validate_url_value},
};

/// Keys the boot map always answers, whatever the variables table holds — and
/// that `CONFIG_SET` refuses, since a stored row for one would never be served.
///
/// Derived from the repo's key-naming conventions rather than listed:
/// infrastructure keys ([`crate::config_vars::is_infrastructure_key`]) and
/// internal adapter-injected keys ([`crate::config_vars::is_internal_key`]) are
/// by definition never variables-table config, so a row carrying one is a
/// mistake or a forgery. That matters: the browser adapter's
/// `__IMPRESSPRESS_RUNTIME_KIND__ = "browser"` is what keeps Stripe secret-key
/// operations off inside a visitor's browser, and a table-first read let a
/// `server` row switch them back on.
///
/// The JWT secret is the one named exception. It IS a table row on native, but
/// the boot map already holds that same value, and a row must not rotate the
/// signing key out from under a running process.
fn served_only_from_boot_map(key: &str) -> bool {
    crate::config_vars::is_instance_owned_key(key)
}

/// The `variables` table as a key/value map, shared by every reader holding
/// the snapshot it was built for.
type ConfigRows = Arc<HashMap<String, String>>;

/// A memoized [`ConfigRows`] tagged with the config-write generation it was
/// read at, so a later write makes it visibly stale. See
/// [`VariablesConfigBlock::snapshot`].
type CachedRows = RwLock<Option<(u64, ConfigRows)>>;

/// The config block impresspress registers in place of wafer-core's.
pub struct VariablesConfigBlock {
    /// wafer-core's block, kept for `info()` and any operation this one does
    /// not claim, so a new config op added upstream keeps working here.
    inner: Arc<dyn Block>,
    /// The target's boot map: what the table cannot hold, or must not be
    /// trusted for. Env and worker bindings, builder-time vars (CORS, CSP,
    /// STRICT_SCHEMA), the block-settings JSON, runtime markers, and the JWT
    /// secret. Native and the browser no longer copy the variables table into
    /// it — this block serves stored variables from the table itself.
    boot: Arc<dyn ConfigService>,
    /// The platform database, held as the raw service rather than reached
    /// through `ctx`.
    ///
    /// `impresspress__admin__variables` belongs to the ADMIN block, and
    /// `db::list_all` sends the collection as a WRAP resource, so reaching it
    /// through `ctx` is a cross-block read that WRAP denies by default. This
    /// block would then fall back to the boot map and serve compiled-in
    /// defaults on every page — which is exactly what a live Cloudflare
    /// deploy did before this field existed.
    ///
    /// Reading the platform's own config store through the raw service is the
    /// established shape, not a workaround: `variables`' boot-flavour API is
    /// documented as "over `DatabaseService`, before WRAP", and
    /// `D1ConfigSource` reads the same table the same way for the same
    /// reason.
    db: Arc<dyn DatabaseService>,
    /// The `variables` table, memoized alongside the config-write generation
    /// it was read at. See [`Self::snapshot`].
    snapshot: CachedRows,
}

impl VariablesConfigBlock {
    /// Wrap wafer-core's config block over the same boot-seeded service the
    /// builder already constructed.
    pub fn new(boot: Arc<dyn ConfigService>, db: Arc<dyn DatabaseService>) -> Self {
        let inner: Arc<dyn Block> = Arc::new(wafer_core::service_blocks::config::ConfigBlock::new(
            boot.clone(),
        ));
        Self {
            inner,
            boot,
            db,
            snapshot: RwLock::new(None),
        }
    }

    /// The key a `CONFIG_GET` is asking for: a codec-encoded `GetRequest`
    /// body, or the `key` meta fallback wafer-core's handler also accepts.
    fn get_key(msg: &Message, body: &[u8]) -> Result<String, OutputStream> {
        match codec::decode::<wire::GetRequest>(body) {
            Ok(req) => Ok(req.key),
            Err(_) => {
                let meta_key = msg.get_meta("key");
                if meta_key.is_empty() {
                    return Err(OutputStream::error(WaferError::new(
                        ErrorCode::InvalidArgument,
                        "config.get requires a 'key' in data or meta",
                    )));
                }
                Ok(meta_key.to_string())
            }
        }
    }

    /// The whole `variables` table as a key/value map, fetched at most once
    /// per config-write generation.
    ///
    /// One query, not one per key. A single page render reads roughly eight
    /// branding keys through `config::get_default`; a row fetch per key would
    /// turn that into eight D1 round-trips per request on Workers, where CPU
    /// and latency are the scarce resources. `D1ConfigSource::cached_snapshot`
    /// solves the same problem the same way and against the same counter, so
    /// the two invalidate together.
    ///
    /// The generation is captured BEFORE the read: a write landing while the
    /// query is in flight must not be masked by the snapshot it raced.
    ///
    /// No lock is held across the `await` — the guard is dropped before the
    /// fetch and re-taken after — so a hard-stopped request cannot strand one.
    async fn snapshot(&self) -> Option<ConfigRows> {
        let generation = config_write_generation();
        {
            let cached = self
                .snapshot
                .read()
                .expect("config snapshot lock poisoned")
                .clone();
            if let Some((cached_generation, rows)) = cached {
                if cached_generation == generation {
                    return Some(rows);
                }
            }
        }

        let rows = match variables::load_all(&self.db).await {
            Ok(rows) => rows,
            Err(e) => {
                // Reported rather than propagated: a config read that cannot
                // reach the table falls back to the boot map, which is what
                // the process was already serving. Failing instead would turn
                // a transient database blip into a blank site.
                tracing::warn!(
                    error = %e,
                    "config read could not reach the variables table; falling back to the boot map"
                );
                return None;
            }
        };
        // `load_all` already returns a key/value map; an empty value falls
        // through to the boot map rather than masking it (see the module docs).
        let map: HashMap<String, String> = rows
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .collect();
        let map = Arc::new(map);

        *self
            .snapshot
            .write()
            .expect("config snapshot lock poisoned") = Some((generation, map.clone()));
        Some(map)
    }

    /// The stored value for `key`, or `None` when no row holds a non-empty
    /// one.
    async fn stored_value(&self, key: &str) -> Option<String> {
        self.snapshot().await?.get(key).cloned()
    }

    /// Persist `key` and let cached readers know the store moved.
    ///
    /// Writes through the raw service for the same reason reads do: a
    /// `ctx`-routed write to the admin block's table is a cross-block write
    /// WRAP denies.
    async fn write(&self, key: &str, value: &str) -> Result<(), OutputStream> {
        // The same masked-submission, sensitive-empty and `_URL` guards
        // `blocks::admin::ops::update_variable` applies, spelled from the same
        // helpers, so the two write surfaces agree on the RULES.
        //
        // They do not agree on the whole of the sensitive-empty EXEMPTION, and
        // deliberately: this surface exempts only the static
        // `config_vars::is_provisioning_only_key` (the bootstrap password),
        // while the admin PUT calls `ops::is_clearable_provisioning_credential`,
        // which additionally exempts a bootstrap TOKEN that has already been
        // redeemed. So `CONFIG_SET` refuses a clear the admin PUT accepts. The
        // narrower rule is the correct one here and `ops.rs` says why: "is the
        // token redeemed" is answered by counting admin users, and this
        // operation runs over the raw `DatabaseService` with no `Context` to
        // count through. Widening it here would exempt an UNREDEEMED token on
        // the one surface that cannot check, and clearing a live token is a
        // lockout with no way back on Cloudflare.
        //
        // The divergence is not reachable today, and the reason is simpler than
        // it looks. It concerns exactly one key — `BOOTSTRAP_ADMIN_TOKEN`,
        // which the admin path exempts once redeemed and this one never does —
        // and that key is in no `save_settings` allowlist at all. The only
        // settings form carrying bootstrap keys is `auth_ui::pages::settings`,
        // whose "Admin" section is the bootstrap EMAIL and PASSWORD; nothing
        // renders the token. So this caller cannot submit a value for it,
        // empty or otherwise.
        //
        // Note the narrowness: that is an argument about ONE key, not about
        // empty submissions in general. `save_settings` decides sensitivity
        // from the declared var and this function decides it from the stored
        // row, so a declared-plain key whose row an operator flagged sensitive
        // DOES reach the guard below with an empty value and is refused here —
        // which is correct, and which `save_settings` now forwards as the 400
        // it is rather than the 500 it used to flatten it into. A
        // `MASKED_VALUE` submission is handled before it can get here: that
        // caller's pre-pass refuses a mask that would replace a value, for
        // every allowlisted var and not just the ones it can see are sensitive,
        // deliberately covering more than this guard does because it cannot
        // read the flag this one reads.
        //
        // KNOWN GAP, recorded rather than fixed: the parity stops at the
        // create path. `variables::set`'s create branch builds its own
        // `NewVariable` and so bypasses `VariablePatch::into_new`, which is
        // where `is_sensitive_by_default_when_created` protects an undeclared,
        // suffix-less ad hoc key. A `config.set` creating one therefore stores
        // it unflagged where the admin PUT would flag it.
        //
        // Not reachable through THIS operation's only caller: every var
        // `ui::settings_form` renders comes from a `ConfigVar` allowlist, and a
        // declared key is settled by `into_row`. Note that "declared" has to
        // mean what `config_vars::collect_all_config_vars` says it means —
        // `auth_ui::pages::settings` renders
        // `auth::config::auth_identity_config_vars`, which belongs to no
        // `BlockInfo`, and an earlier version of that collector missed them and
        // so called two ordinary admin toggles ad hoc. The gap is latent
        // because of the allowlist, not because nothing undeclared can reach a
        // settings form. The
        // runtime-owned refusal below is deliberately NOT symmetric: this
        // surface refuses the JWT secret (no caller legitimately writes it
        // here — `ui::settings_form` writes declared block and shared vars
        // only), while the admin variables API accepts it, because on native
        // that row IS the next boot's secret. See
        // `blocks::admin::ops::reject_runtime_owned_key`. The
        // sensitive-empty guard reads the stored flag exactly as that path
        // does; a missing row contributes no flag, so for it the guard rests
        // on `is_sensitive_key`'s key half alone — the key's declaration or
        // its `_SECRET`/`_KEY` spelling.
        let existing = match variables::find_by_key(&self.db, key).await {
            Ok(row) => row,
            Err(e) => {
                return Err(OutputStream::error(WaferError::new(
                    ErrorCode::Internal,
                    format!("config.set could not read {key}: {e}"),
                )))
            }
        };
        // The row's own flag, shared by the two guards below: neither of them
        // may decide off the key's spelling alone, or an ad hoc row an admin
        // marked sensitive in the UI would be judged as if it were plain.
        let stored_flag = existing.as_ref().map_or(0, |row| i64::from(row.sensitive));
        // The mask is never a value, on this surface as on the admin ones.
        //
        // This operation is reachable by ANY block through
        // `wafer_core::clients::config::set`, so without the guard here the
        // rule held only because the three surfaces that exist today each
        // enforce it themselves — an invariant that breaks silently the day a
        // block adds a call. Nothing stops it being enforced here: the stored
        // row is already in hand for the empty guard below, which is the only
        // thing the mask check needs. (Contrast the provisioning EXEMPTION
        // discussed above, which genuinely cannot be mirrored here because it
        // asks a question only a `Context` can answer.)
        //
        // Narrowed to a mask that would REPLACE something, for the reason
        // `ui::settings_form`'s pre-pass is: a write that changes nothing
        // destroys nothing, and refusing it only strands whoever is holding a
        // value that already is those eight characters.
        //
        // "Already" has to mean what a READER would answer, which is this
        // block's own read order — a non-empty row, else the boot map — and not
        // the row alone. The pre-pass asks `config::get_default`, which is that
        // order; comparing against `existing.value` here made the two disagree
        // in exactly the case the row cannot speak for: absent or empty, where
        // `CONFIG_GET` drops it and the boot map answers. For a key whose boot
        // value is the mask the pre-pass then allowed and this guard refused,
        // mid-loop, with the rest of the page already written — reachable from
        // an env-seeded credential that is literally `********`, cleared on the
        // Variables page and typed again on a settings form. One question,
        // asked the same way on both sides, is what makes that impossible
        // rather than merely unlikely.
        if crate::util::is_masked_submission(key, stored_flag, value) {
            let boot_value = self.boot.get(key);
            let current = existing
                .as_ref()
                .map(|row| row.value.as_str())
                .filter(|stored| !stored.is_empty())
                .or(boot_value.as_deref())
                .unwrap_or("");
            if current != value {
                return Err(OutputStream::error(WaferError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "{} is the mask {key} reads back as, not its value: storing it would \
                         destroy the secret",
                        crate::util::MASKED_VALUE
                    ),
                )));
            }
        }
        // The static provisioning-only exemption — the narrower of the two, per
        // the note above. It has to be here at all for the reason it exists on
        // the admin path: a spent bootstrap password must stay clearable
        // because `delete_variable` and `key_is_deletable` both refuse to
        // delete a declared `WAFER_RUN_SHARED__*` row, so without it the
        // deployment keeps a plaintext admin password by every route.
        if value.is_empty()
            && !crate::config_vars::is_provisioning_only_key(key)
            && is_sensitive_key(key, stored_flag)
        {
            return Err(OutputStream::error(WaferError::new(
                ErrorCode::InvalidArgument,
                format!("Cannot set {key} to an empty value"),
            )));
        }
        if key.ends_with("_URL") {
            if let Err(e) = validate_url_value(value) {
                return Err(OutputStream::error(WaferError::new(
                    ErrorCode::InvalidArgument,
                    format!("Invalid value for {key}: {e}"),
                )));
            }
        }

        // What this asserts ON TOP of the key's own declaration, which
        // `variables::set` and `NewVariable::into_row` settle themselves: an
        // existing row's stored flag, so an ad hoc row an admin marked
        // sensitive in the UI stays that way. Deriving it here from the key's
        // spelling alone is what let a `Password`-typed declared var with no
        // row yet —
        // `WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_PASSWORD`, spelled neither
        // `_SECRET` nor `_KEY` — land unflagged, after which the settings API
        // served it verbatim and `cache_key::row_is_sensitive` judged it
        // eligible for the edge cache.
        let sensitive = existing.as_ref().is_some_and(|row| row.sensitive);
        if let Err(e) = variables::set(&self.db, key, value, "", "", sensitive).await {
            return Err(OutputStream::error(WaferError::new(
                ErrorCode::Internal,
                format!("config.set could not write {key}: {e}"),
            )));
        }
        // The generation bump lives in `variables::upsert_by_key`, not here.
        // `PATCH /b/admin/api/settings/{key}` writes the table through
        // `ops::update_variable` without ever entering this block, so a bump
        // placed here would leave a warm snapshot stale for the life of the
        // process on exactly the path the admin uses — see
        // `an_admin_write_invalidates_an_already_warm_snapshot`.
        Ok(())
    }
}

#[wafer_block::wafer_async_trait]
impl Block for VariablesConfigBlock {
    fn info(&self) -> BlockInfo {
        self.inner.info()
    }

    async fn handle(&self, ctx: &dyn Context, msg: Message, input: InputStream) -> OutputStream {
        match msg.kind.as_str() {
            ServiceOp::CONFIG_GET => {
                let body = input.collect_to_bytes().await;
                let key = match Self::get_key(&msg, &body) {
                    Ok(key) => key,
                    Err(out) => return out,
                };
                if let Err(e) = ctx.check_resource_access(&key, ResourceType::Config, false) {
                    return OutputStream::error(e);
                }

                let value = if served_only_from_boot_map(&key) {
                    self.boot.get(&key)
                } else {
                    match self.stored_value(&key).await {
                        Some(value) => Some(value),
                        None => self.boot.get(&key),
                    }
                };

                value.map_or_else(
                    || {
                        OutputStream::error(WaferError::new(
                            ErrorCode::NotFound,
                            format!("config key not found: {key}"),
                        ))
                    },
                    |value| match codec::encode(&wire::GetResponse { value }) {
                        Ok(bytes) => OutputStream::respond(bytes),
                        Err(e) => OutputStream::error(e),
                    },
                )
            }
            ServiceOp::CONFIG_SET => {
                let body = input.collect_to_bytes().await;
                let req = match codec::decode::<wire::SetRequest>(&body) {
                    Ok(req) => req,
                    Err(e) => {
                        return OutputStream::error(WaferError::new(
                            ErrorCode::InvalidArgument,
                            format!("config.set: {e}"),
                        ))
                    }
                };
                if let Err(e) = ctx.check_resource_access(&req.key, ResourceType::Config, true) {
                    return OutputStream::error(e);
                }
                // A runtime-owned key is never served from the table, so
                // storing one would report success for a value no reader can
                // ever see. Refuse it instead of writing an unservable row.
                if served_only_from_boot_map(&req.key) {
                    return OutputStream::error(WaferError::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "{} is set by the runtime, not stored config; it cannot be written",
                            req.key
                        ),
                    ));
                }
                match self.write(&req.key, &req.value).await {
                    Ok(()) => OutputStream::respond(vec![]),
                    Err(out) => out,
                }
            }
            _ => self.inner.handle(ctx, msg, input).await,
        }
    }
}

/// Register impresspress's config block under the name every caller uses.
///
/// Replaces `wafer_core::service_blocks::config::register_with`, which binds
/// the same name to a block serving both operations from the in-memory map.
pub fn register_with(
    wafer: &mut wafer_run::Wafer,
    boot: Arc<dyn ConfigService>,
    db: Arc<dyn DatabaseService>,
) -> Result<(), wafer_run::RuntimeError> {
    let block: Arc<dyn Block> = Arc::new(VariablesConfigBlock::new(boot, db));
    wafer.register_block("wafer-run/config", block)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        platform_state::variables::VariablePatch,
        test_support::{unique_config_value, TestContext},
    };

    /// An admin write must be visible to a reader whose snapshot is ALREADY
    /// warm.
    ///
    /// The reproduction tests in `blocks::admin::settings` and
    /// `ui::settings_form` both read for the first time after writing, so the
    /// snapshot is built fresh and they pass however invalidation behaves.
    /// A real server reads config while serving its first request and keeps
    /// the snapshot; if an admin's `PATCH` does not invalidate it, that
    /// write is invisible for the life of the process — the original defect,
    /// reintroduced by the cache that was supposed to make the fix affordable.
    ///
    /// The automatic bump lives in `impresspress-cloudflare`'s `kv_cached_db`
    /// and so covers only that target. This asserts the behaviour on native,
    /// where the `variables` repo has to supply it.
    #[tokio::test]
    async fn an_admin_write_invalidates_an_already_warm_snapshot() {
        const KEY: &str = "WAFER_RUN_SHARED__PRIMARY_COLOR";

        let mut ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");
        ctx.boot_config_service().await;

        // Warm the snapshot, the way serving one request does.
        let first = unique_config_value();
        variables::upsert_by_key(
            &ctx,
            KEY,
            VariablePatch {
                value: Some(first.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("seed the first value");
        assert_eq!(
            wafer_core::clients::config::get_default(&ctx, KEY, "unset").await,
            first,
            "precondition: the first read populates the snapshot"
        );

        // Now a write lands that did NOT go through this block — which is
        // what `PATCH /b/admin/api/settings/{key}` does: `handle_set` calls
        // `ops::update_variable`, which calls exactly this. The repo write is
        // the shared choke point, so asserting on it covers the admin
        // endpoint and every other direct writer at once.
        let second = unique_config_value();
        variables::upsert_by_key(
            &ctx,
            KEY,
            VariablePatch {
                value: Some(second.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("the admin write lands in the table");

        assert_eq!(
            wafer_core::clients::config::get_default(&ctx, KEY, "unset").await,
            second,
            "an admin write must invalidate a warm config snapshot, or the \
             change stays invisible for the life of the process"
        );
    }

    /// The login page shows branding an admin saved, without a restart.
    ///
    /// This is the requirement read surface 2 stood for. It could not be
    /// asserted while `blocks::auth_ui::pages::site_config` read
    /// `ctx.config_get`: that snapshot is filled at boot and never refilled,
    /// so the assertion would have failed however correct the config store
    /// was. Step 3 of the decision moved those reads onto the async client,
    /// which is what makes this expressible at all.
    ///
    /// Asserted through `SiteConfig::load_for_auth` — what the login, signup,
    /// bootstrap, change-password, reset-password and verify pages all build
    /// their chrome from — rather than by rendering one page, so it covers
    /// every one of them.
    #[tokio::test]
    async fn the_auth_pages_show_branding_saved_after_boot() {
        const KEY: &str = "WAFER_RUN_SHARED__PRIMARY_COLOR";

        let mut ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");
        ctx.boot_config_service().await;

        // Boot is over; the snapshot the auth pages used to read is now fixed.
        let saved = unique_config_value();
        variables::upsert_by_key(
            &ctx,
            KEY,
            VariablePatch {
                value: Some(saved.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("the admin write lands in the table");

        let site = crate::ui::SiteConfig::load_for_auth(&ctx).await;
        assert_eq!(
            site.primary_color, saved,
            "the auth pages must render the brand colour an admin saved, not \
             the one that happened to be in the table when the process booted"
        );
    }

    /// The config block must be able to read the variables table under WRAP.
    ///
    /// Found by deploying to a live Cloudflare Worker, not by any test here.
    /// `impresspress__admin__variables` belongs to the ADMIN block, and
    /// `db::list_all` sends the collection as a WRAP resource
    /// (`svc!(.., Some(collection), .., Some("db"))`). A cross-block read is
    /// denied by default, so on a runtime that enforces WRAP this block's
    /// table read failed, it fell back to the boot map, and every page served
    /// the compiled-in default — the exact defect the fix was meant to remove,
    /// still live, with a green test suite behind it.
    ///
    /// The suite was green because `TestContext` leaves WRAP off unless a test
    /// opts in, which is the same blind spot that let the files block ship a
    /// `wafer-run/crypto` call it had not declared. This test opts in.
    #[tokio::test]
    async fn the_config_block_reads_the_variables_table_under_wrap() {
        const KEY: &str = "WAFER_RUN_SHARED__PRIMARY_COLOR";

        let mut ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");
        ctx.boot_config_service().await;

        let saved = unique_config_value();
        variables::upsert_by_key(
            &ctx,
            KEY,
            VariablePatch {
                value: Some(saved.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("seed the value");

        // Act as the config block itself, on the same gates production
        // applies: no grants, and the admin block owns the table.
        let ctx = ctx.with_wrap(
            "wafer-run/config",
            Vec::new(),
            Vec::new(),
            crate::blocks::admin::ADMIN_BLOCK_ID,
        );

        assert_eq!(
            wafer_core::clients::config::get_default(&ctx, KEY, "unset").await,
            saved,
            "the config block must read the variables table under WRAP; if it \
             cannot it falls back to the boot map and every page serves the \
             compiled-in default while the admin's saved value sits in the table"
        );
    }
}

/// Keys the runtime sets must not be overridable from the variables table.
#[cfg(test)]
mod boot_owned_key_tests {
    use super::*;
    use crate::{platform_state::variables::NewVariable, test_support::TestContext};

    async fn booted_with(adapter_values: &[(&str, &str)]) -> TestContext {
        let mut ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");
        ctx.boot_config_service_with(adapter_values).await;
        ctx
    }

    async fn store_row(ctx: &TestContext, key: &str, value: &str) {
        variables::insert(
            ctx,
            NewVariable {
                key: key.to_string(),
                value: value.to_string(),
                name: String::new(),
                description: String::new(),
                warning: String::new(),
                sensitive: false,
                updated_by: String::new(),
                block: variables::block_for_key(key),
            },
        )
        .await
        .expect("store the row");
    }

    /// The browser adapter's runtime marker must beat a variables row.
    ///
    /// `products::RUNTIME_KIND_CONFIG_KEY` is documented as set by the browser
    /// adapter "after loading persisted variables, so an admin database value
    /// cannot accidentally turn a public browser runtime into a trusted
    /// secret holder", and `products::stripe_secret_operations_allowed` reads
    /// it through the config client. When this block started answering
    /// table-first, a row holding `server` under that key would have switched
    /// Stripe secret-key operations on inside a visitor's browser — a row that
    /// the admin variables API accepts for any key, and that a dev-sandbox data
    /// import can carry.
    #[cfg(feature = "block-products")]
    #[tokio::test]
    async fn a_table_row_cannot_override_an_internal_adapter_key() {
        let key = crate::blocks::products::RUNTIME_KIND_CONFIG_KEY;
        let ctx = booted_with(&[(key, "browser")]).await;
        store_row(&ctx, key, "server").await;

        assert_eq!(
            wafer_core::clients::config::get_default(&ctx, key, "server").await,
            "browser",
            "an adapter-injected internal key must come from the boot map, never the variables table"
        );
    }

    /// Infrastructure keys follow the same rule: `IMPRESSPRESS_*` without a
    /// `__` separator is "infrastructure, never in DB" by the repo's naming
    /// convention, so a row carrying one must not be what a reader sees.
    #[tokio::test]
    async fn a_table_row_cannot_override_an_infrastructure_key() {
        let key = crate::migration_helper::RUN_MIGRATIONS_KEY;
        let ctx = booted_with(&[(key, "1")]).await;
        store_row(&ctx, key, "0").await;

        assert_eq!(
            wafer_core::clients::config::get_default(&ctx, key, "unset").await,
            "1",
            "an infrastructure key must come from the boot map, never the variables table"
        );
    }

    /// A write to a runtime-owned key is refused, not silently stored.
    ///
    /// The read side never serves such a row, so accepting the write would
    /// report success for a value no reader can ever see — the silent no-op
    /// this block exists to remove.
    #[tokio::test]
    async fn config_set_refuses_a_runtime_owned_key() {
        let key = crate::features::BLOCK_SETTINGS_CONFIG_KEY;
        let ctx = booted_with(&[(key, "{}")]).await;

        let result = wafer_core::clients::config::set(&ctx, key, r#"{"forged":true}"#).await;
        assert!(
            result.is_err(),
            "CONFIG_SET of a runtime-owned key must fail rather than store an unservable row"
        );
        assert!(
            variables::get_by_key(&ctx, key)
                .await
                .expect("read back")
                .is_none(),
            "a refused write must not leave a row behind"
        );
    }

    /// `CONFIG_SET` refuses the mask, like every other write surface.
    ///
    /// This is the FOURTH writer into the `variables` table — any block can
    /// reach it through `wafer_core::clients::config::set` — and it is the one
    /// that makes the claim in `util::is_masked_submission` true by
    /// construction rather than by accident of who happens to call what today.
    /// Without it the guard held only because the two admin surfaces and
    /// `ui::settings_form` all enforce it themselves, which is the kind of
    /// invariant that breaks silently the day a block adds a call.
    #[tokio::test]
    async fn config_set_refuses_the_mask_for_a_sensitive_key() {
        const KEY: &str = "WAFER_RUN_SHARED__AUTH__OAUTH_GOOGLE_CLIENT_SECRET";
        let ctx = booted_with(&[]).await;
        store_row(&ctx, KEY, "real-client-secret").await;

        let result = wafer_core::clients::config::set(&ctx, KEY, crate::util::MASKED_VALUE).await;
        assert!(
            result.is_err(),
            "storing the mask over a secret must fail, not report success"
        );
        assert_eq!(
            variables::get_by_key(&ctx, KEY)
                .await
                .expect("read back")
                .expect("the row is still there")
                .value,
            "real-client-secret",
            "and the stored secret must survive the refusal"
        );
    }

    /// The mask refusal is for a mask that REPLACES something. A row already
    /// holding those eight characters is a no-op write, and refusing it would
    /// strand whoever holds such a row — and, since
    /// `ui::settings_form`'s pre-pass allows a submission equal to the current
    /// value, would do it mid-loop with part of the page already saved.
    #[tokio::test]
    async fn config_set_allows_the_mask_when_it_replaces_nothing() {
        const KEY: &str = "WAFER_RUN_SHARED__AUTH__OAUTH_GOOGLE_CLIENT_SECRET";
        let ctx = booted_with(&[]).await;
        store_row(&ctx, KEY, crate::util::MASKED_VALUE).await;

        wafer_core::clients::config::set(&ctx, KEY, crate::util::MASKED_VALUE)
            .await
            .expect("a write that changes nothing must not be refused");
        assert_eq!(
            variables::get_by_key(&ctx, KEY)
                .await
                .expect("read back")
                .expect("the row is still there")
                .value,
            crate::util::MASKED_VALUE,
        );
    }

    /// Ordinary shared keys are unaffected: the table still wins.
    #[tokio::test]
    async fn a_shared_key_is_still_served_from_the_table() {
        const KEY: &str = "WAFER_RUN_SHARED__APP_NAME";
        let ctx = booted_with(&[(KEY, "boot-value")]).await;
        let saved = crate::test_support::unique_config_value();
        store_row(&ctx, KEY, &saved).await;

        assert_eq!(
            wafer_core::clients::config::get_default(&ctx, KEY, "unset").await,
            saved,
            "an admin-editable shared key must still come from the variables table"
        );
    }
}
