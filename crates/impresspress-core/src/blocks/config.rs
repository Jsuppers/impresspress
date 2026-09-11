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
//! 1. `PROTECTED_KEYS` always come from the boot map. The JWT secret is a
//!    worker secret on Cloudflare and never lives in D1; on native
//!    `seed_jwt_secret` does put it in the table, but the boot map already
//!    holds that same value. Letting a table row win would also let an admin
//!    edit rotate the signing key out from under a running process
//!    mid-request, which is not a config change but an outage.
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
//! masking it: `seed_and_load` writes rows with empty values for declared
//! vars that have no setting yet, and treating those as "explicitly blank"
//! would blank out env-provided values on the first boot that seeds them.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use wafer_block::{codec, wire::config as wire, ServiceOp};
use wafer_core::interfaces::config::service::ConfigService;
use wafer_run::{
    context::Context, Block, BlockInfo, ErrorCode, InputStream, Message, OutputStream,
    ResourceType, WaferError,
};

use crate::{
    config_generation::config_write_generation,
    platform_state::variables::{self, VariablePatch},
    util::{is_sensitive_key, validate_url_value},
};

/// Keys the boot map always answers, whatever the table holds. See the read
/// order in the module docs.
const PROTECTED_KEYS: &[&str] = &[crate::blocks::auth::JWT_SECRET_KEY];

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
    /// Boot-seeded map: env/worker bindings, builder-time vars, block
    /// settings JSON, and the boot copy of every table row.
    boot: Arc<dyn ConfigService>,
    /// The `variables` table, memoized alongside the config-write generation
    /// it was read at. See [`Self::snapshot`].
    snapshot: CachedRows,
}

impl VariablesConfigBlock {
    /// Wrap wafer-core's config block over the same boot-seeded service the
    /// builder already constructed.
    pub fn new(boot: Arc<dyn ConfigService>) -> Self {
        let inner: Arc<dyn Block> = Arc::new(wafer_core::service_blocks::config::ConfigBlock::new(
            boot.clone(),
        ));
        Self {
            inner,
            boot,
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
    async fn snapshot(&self, ctx: &dyn Context) -> Option<ConfigRows> {
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

        let rows = match variables::list_all(ctx).await {
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
        let map: HashMap<String, String> = rows
            .into_iter()
            .filter(|row| !row.value.is_empty())
            .map(|row| (row.key, row.value))
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
    async fn stored_value(&self, ctx: &dyn Context, key: &str) -> Option<String> {
        self.snapshot(ctx).await?.get(key).cloned()
    }

    /// Persist `key` and let cached readers know the store moved.
    async fn write(ctx: &dyn Context, key: &str, value: &str) -> Result<(), OutputStream> {
        // The same two guards `blocks::admin::ops::update_variable` applies,
        // so the two write surfaces cannot accept divergent input. The
        // sensitive-empty guard reads the stored flag exactly as that path
        // does; a missing row has no stored secret to wipe, so only the
        // suffix rule applies there.
        if value.is_empty() {
            let stored_flag = match variables::get_by_key(ctx, key).await {
                Ok(Some(row)) => i64::from(row.sensitive),
                Ok(None) => 0,
                Err(e) => {
                    return Err(OutputStream::error(WaferError::new(
                        ErrorCode::Internal,
                        format!("config.set could not read {key}: {e}"),
                    )))
                }
            };
            if is_sensitive_key(key, stored_flag) {
                return Err(OutputStream::error(WaferError::new(
                    ErrorCode::InvalidArgument,
                    format!("Cannot set {key} to an empty value"),
                )));
            }
        }
        if key.ends_with("_URL") {
            if let Err(e) = validate_url_value(value) {
                return Err(OutputStream::error(WaferError::new(
                    ErrorCode::InvalidArgument,
                    format!("Invalid value for {key}: {e}"),
                )));
            }
        }

        let patch = VariablePatch {
            value: Some(value.to_string()),
            ..Default::default()
        };
        if let Err(e) = variables::upsert_by_key(ctx, key, patch).await {
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

                let value = if PROTECTED_KEYS.contains(&key.as_str()) {
                    self.boot.get(&key)
                } else {
                    match self.stored_value(ctx, &key).await {
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
                match Self::write(ctx, &req.key, &req.value).await {
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
) -> Result<(), wafer_run::RuntimeError> {
    let block: Arc<dyn Block> = Arc::new(VariablesConfigBlock::new(boot));
    wafer.register_block("wafer-run/config", block)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{unique_config_value, TestContext};

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
}
