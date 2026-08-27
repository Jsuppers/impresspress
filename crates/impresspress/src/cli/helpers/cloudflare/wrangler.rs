//! wrangler.toml generation + override merging.
//!
//! [`CloudflareConfig`] is the *resolved* shape consumed by [`generate`].
//! Parsing impresspress.toml + applying env-var overlays lives in
//! [`super::env`], which produces this type.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
pub use impresspress_core::{
    PREPARED_APPLICATION_BUILD_SHA256_VAR, PREPARED_APPLICATION_ID_VAR, PREPARED_PLAN_HASH_VAR,
    PREPARED_PLAN_MODULE_SHA256_VAR, RELEASE_ASSET_KEYS_SHA256_VAR,
    RELEASE_ASSET_MANIFEST_SHA256_VAR,
};

use super::{
    assets::ReleaseManifest,
    build::WORKER_BUILD_VERSION,
    prepared::{
        ApplicationArtifactIdentity, PreparedModule, PREPARED_MODULE_DIR, PREPARED_SHIM_FILE,
        PREPARED_TEXT_GLOB,
    },
};

/// Version-bound Worker variables exposing the immutable release asset set.
/// Existing applications may continue reading logical R2 keys during the
/// compatibility period; runtimes that understand these bindings can resolve
/// release-managed keys without consulting a mutable global pointer.
pub const RELEASE_ASSET_ID_VAR: &str = "IMPRESSPRESS_RELEASE_ASSET_ID";
pub const RELEASE_ASSET_PREFIX_VAR: &str = "IMPRESSPRESS_RELEASE_ASSET_PREFIX";
pub const RELEASE_ASSET_MANIFEST_VAR: &str = "IMPRESSPRESS_RELEASE_ASSET_MANIFEST";
pub const PREPARED_WAFER_LOCK_IDENTITY_VAR: &str =
    impresspress_core::PREPARED_WAFER_LOCK_IDENTITY_JSON_VAR;

#[derive(Debug, Clone)]
pub struct CloudflareConfig {
    pub account_id: String,
    pub worker_name: String,
    pub compatibility_date: String,
    pub d1: D1Config,
    pub r2: R2Config,
    /// Path (relative to consumer repo root) to a TOML file whose contents
    /// are deep-merged over the generated defaults. None means "no overrides."
    pub wrangler_overrides_path: Option<PathBuf>,
    /// Workers Observability head sampling rate (`0.0..=1.0`), resolved by
    /// [`super::env::RawCloudflareConfig::resolve`] from
    /// `IMPRESSPRESS_CLOUDFLARE_HEAD_SAMPLING_RATE` / `impresspress.toml`'s
    /// `[cloudflare].head_sampling_rate`, defaulting to `1.0` (100%) when
    /// neither is set. Explicit and configurable rather than hardcoded, so a
    /// deployment that outgrows 100%-capture traffic doesn't have to reach
    /// for a `wrangler_overrides_path` file to dial it down.
    pub head_sampling_rate: f64,
    /// Ordinary routes exercised by the bounded mixed-concurrency gate after
    /// final-version verification and before promotion. Resolution guarantees
    /// a non-empty collection of path-only values.
    pub deploy_smoke_paths: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct D1Config {
    pub binding: String,
    pub database_name: String,
    pub database_id: String,
}

#[derive(Debug, Clone)]
pub struct R2Config {
    pub binding: String,
    pub bucket_name: String,
    /// Optional repository-relative directory containing release-managed
    /// objects to upload to R2 before candidate initialization.
    pub release_assets_dir: Option<PathBuf>,
    /// R2 key prefix under which the contents of `release_assets_dir` land.
    /// Empty means the bucket root.
    pub release_assets_prefix: PathBuf,
    /// Compiled globs, matched against staged logical keys, whose files never
    /// enter the release asset set.
    ///
    /// The key inventory is bound into one 4 KB Worker variable, so files the
    /// runtime never reads are not merely wasted upload — they consume a hard,
    /// shared budget and eventually fail the deploy outright.
    pub release_assets_exclude: Vec<glob::Pattern>,
}

pub fn generate(cfg: &CloudflareConfig, repo_root: &Path, out_dir: &Path) -> Result<PathBuf> {
    generate_named(
        cfg,
        repo_root,
        out_dir,
        "wrangler.toml",
        BuildHook::Include,
        None,
        None,
        None,
    )
}

/// Generate the configuration used to upload an artifact that
/// [`super::build::run`] has already produced.
///
/// This deliberately omits Wrangler's custom `[build]` hook. `versions
/// upload` still bundles the staged `shim.mjs` and its modules, but it must
/// not compile the consumer crate a second time: doing so is both expensive
/// and breaks the guarantee that the locally inspected Wasm is the artifact
/// being uploaded.
pub fn generate_upload(
    cfg: &CloudflareConfig,
    repo_root: &Path,
    out_dir: &Path,
) -> Result<PathBuf> {
    generate_upload_with_release(cfg, repo_root, out_dir, None)
}

/// Generate an upload-only config and bind an immutable release asset identity
/// into the resulting Worker version.
pub fn generate_upload_with_release(
    cfg: &CloudflareConfig,
    repo_root: &Path,
    out_dir: &Path,
    release: Option<&ReleaseManifest>,
) -> Result<PathBuf> {
    generate_named(
        cfg,
        repo_root,
        out_dir,
        "wrangler-upload.toml",
        BuildHook::Omit,
        release,
        None,
        None,
    )
}

/// Upload-only dynamic candidate. It carries the same release identity as the
/// final version plus the Wasm/wafer.lock identity consumed by
/// `/_deploy/prepare`, but no prepared Text module.
pub fn generate_candidate_upload(
    cfg: &CloudflareConfig,
    repo_root: &Path,
    out_dir: &Path,
    release: &ReleaseManifest,
    identity: &ApplicationArtifactIdentity,
) -> Result<PathBuf> {
    generate_named(
        cfg,
        repo_root,
        out_dir,
        "wrangler-candidate.toml",
        BuildHook::Omit,
        Some(release),
        Some(identity),
        None,
    )
}

/// Upload-only final version. Uses the exact same Wasm artifact as the dynamic
/// candidate and changes only the entry shim, Text plan module, and final plan
/// identity vars.
pub fn generate_final_upload(
    cfg: &CloudflareConfig,
    repo_root: &Path,
    out_dir: &Path,
    release: &ReleaseManifest,
    identity: &ApplicationArtifactIdentity,
    prepared: &PreparedModule,
) -> Result<PathBuf> {
    generate_named(
        cfg,
        repo_root,
        out_dir,
        "wrangler-final.toml",
        BuildHook::Omit,
        Some(release),
        Some(identity),
        Some(prepared),
    )
}

#[derive(Clone, Copy)]
enum BuildHook {
    Include,
    Omit,
}

fn generate_named(
    cfg: &CloudflareConfig,
    repo_root: &Path,
    out_dir: &Path,
    file_name: &str,
    build_hook: BuildHook,
    release: Option<&ReleaseManifest>,
    identity: Option<&ApplicationArtifactIdentity>,
    prepared: Option<&PreparedModule>,
) -> Result<PathBuf> {
    let mut value = base_toml(cfg);

    if let Some(rel) = cfg.wrangler_overrides_path.as_ref() {
        let abs = repo_root.join(rel);
        let raw = std::fs::read_to_string(&abs)
            .with_context(|| format!("read overrides {}", abs.display()))?;
        let overrides: toml::Value =
            toml::from_str(&raw).with_context(|| format!("parse overrides {}", abs.display()))?;
        deep_merge(&mut value, overrides);
    }

    // Runtime code relies on this exact internal binding name. Restore it
    // after consumer overrides just like the upload-only build invariant
    // below, so a broad override cannot silently disable cache freshness.
    let mut version_metadata = toml::map::Map::new();
    version_metadata.insert(
        "binding".into(),
        toml::Value::String("CF_VERSION_METADATA".into()),
    );
    value
        .as_table_mut()
        .expect("base wrangler config is a table")
        .insert(
            "version_metadata".into(),
            toml::Value::Table(version_metadata),
        );

    if let Some(release) = release {
        let root = value
            .as_table_mut()
            .expect("base wrangler config is a table");
        let vars = root
            .get_mut("vars")
            .and_then(toml::Value::as_table_mut)
            .context("wrangler overrides replaced [vars] with a non-table")?;
        // These are deployment invariants, so install them after consumer
        // overrides. They are captured with the Worker version and therefore
        // restore the correct immutable asset identity on version rollback.
        vars.insert(
            RELEASE_ASSET_ID_VAR.into(),
            toml::Value::String(release.asset_set_sha256.clone()),
        );
        vars.insert(
            RELEASE_ASSET_PREFIX_VAR.into(),
            toml::Value::String(release.immutable_prefix.clone()),
        );
        vars.insert(
            RELEASE_ASSET_MANIFEST_VAR.into(),
            toml::Value::String(release.manifest_key()),
        );
        vars.insert(
            RELEASE_ASSET_MANIFEST_SHA256_VAR.into(),
            toml::Value::String(release.manifest_sha256()?),
        );
        vars.insert(
            RELEASE_ASSET_KEYS_SHA256_VAR.into(),
            toml::Value::String(release.logical_keys_sha256()?),
        );
    }

    if let Some(identity) = identity {
        let vars = value
            .as_table_mut()
            .expect("base wrangler config is a table")
            .get_mut("vars")
            .and_then(toml::Value::as_table_mut)
            .context("wrangler overrides replaced [vars] with a non-table")?;
        vars.insert(
            PREPARED_APPLICATION_ID_VAR.into(),
            toml::Value::String(identity.application_id.clone()),
        );
        vars.insert(
            PREPARED_APPLICATION_BUILD_SHA256_VAR.into(),
            toml::Value::String(identity.application_build_sha256.clone()),
        );
        vars.insert(
            PREPARED_WAFER_LOCK_IDENTITY_VAR.into(),
            toml::Value::String(identity.dependency_lock_json()?),
        );
    }

    if let Some(prepared) = prepared {
        let root = value
            .as_table_mut()
            .expect("base wrangler config is a table");
        root.insert(
            "main".into(),
            toml::Value::String(format!("{PREPARED_MODULE_DIR}/{PREPARED_SHIM_FILE}")),
        );
        let vars = root
            .get_mut("vars")
            .and_then(toml::Value::as_table_mut)
            .context("wrangler overrides replaced [vars] with a non-table")?;
        vars.insert(
            PREPARED_PLAN_HASH_VAR.into(),
            toml::Value::String(prepared.plan_hash.clone()),
        );
        vars.insert(
            PREPARED_PLAN_MODULE_SHA256_VAR.into(),
            toml::Value::String(prepared.plan_module_sha256.clone()),
        );
        install_prepared_text_rule(root)?;
    }

    // Remove the hook *after* applying consumer overrides. An upload-only
    // config is an invariant of the deploy pipeline, not something an
    // override file may accidentally undo.
    if matches!(build_hook, BuildHook::Omit) {
        value
            .as_table_mut()
            .expect("base wrangler config is a table")
            .remove("build");
    }

    let body = toml::to_string_pretty(&value).context("serialize wrangler.toml")?;
    let header = match build_hook {
        BuildHook::Include => {
            "# Generated by `impresspress build --target cloudflare`. \
             Do not edit. Regenerated each build.\n\n"
        }
        BuildHook::Omit => {
            "# Generated by `impresspress deploy --target cloudflare`. \
             Upload-only: consumes the already-built worker artifact. \
             Do not edit.\n\n"
        }
    };
    let path = out_dir.join(file_name);
    std::fs::write(&path, format!("{header}{body}"))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn install_prepared_text_rule(root: &mut toml::map::Map<String, toml::Value>) -> Result<()> {
    let rules = root
        .entry("rules")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .context("wrangler overrides replaced rules with a non-array")?;
    let already_present = rules.iter().any(|rule| {
        rule.as_table()
            .and_then(|table| table.get("globs"))
            .and_then(toml::Value::as_array)
            .is_some_and(|globs| {
                globs
                    .iter()
                    .any(|glob| glob.as_str() == Some(PREPARED_TEXT_GLOB))
            })
    });
    if !already_present {
        let mut text = toml::map::Map::new();
        text.insert("type".into(), toml::Value::String("Text".into()));
        text.insert(
            "globs".into(),
            toml::Value::Array(vec![toml::Value::String(PREPARED_TEXT_GLOB.into())]),
        );
        text.insert("fallthrough".into(), toml::Value::Boolean(true));
        rules.push(toml::Value::Table(text));
    }
    Ok(())
}

fn base_toml(cfg: &CloudflareConfig) -> toml::Value {
    use toml::Value;

    let mut root = toml::map::Map::new();
    root.insert("name".into(), Value::String(cfg.worker_name.clone()));
    root.insert("account_id".into(), Value::String(cfg.account_id.clone()));
    root.insert(
        "main".into(),
        Value::String("../../build/worker/shim.mjs".into()),
    );
    root.insert(
        "compatibility_date".into(),
        Value::String(cfg.compatibility_date.clone()),
    );
    // Required so `wrangler versions upload` prints a "Version Preview
    // URL" line — `impresspress deploy` parses that URL to call the new
    // version preview URLs needed for `/_deploy/prepare`, final verification,
    // and health checks before promotion.
    root.insert("preview_urls".into(), Value::Boolean(true));

    // Gives the runtime a cheap, request-current identity for the complete
    // Worker version. Cloudflare may reuse an isolate when only bindings or
    // secrets change; comparing this ID before returning an isolate-cached
    // Wafer prevents services constructed from the previous request's Env
    // from surviving that change.
    let mut version_metadata = toml::map::Map::new();
    version_metadata.insert(
        "binding".into(),
        Value::String("CF_VERSION_METADATA".into()),
    );
    root.insert("version_metadata".into(), Value::Table(version_metadata));

    let mut build = toml::map::Map::new();
    // Pinned to an exact version (`WORKER_BUILD_VERSION`, shared with the
    // local `impresspress build --target cloudflare` path in
    // `build::ensure_worker_build_installed`) — worker-build 0.8.x rejects
    // consumers using `worker < 0.8` (hard version check) and changed its
    // output layout from `build/worker/shim.mjs` (which `main` points to)
    // to `build/index.js`. Until we upgrade the `worker` crate, lock the
    // toolchain to this exact version for both the local build and the
    // wrangler-driven rebuild during deploy.
    //
    // The `command -v ... && [ ... ] ||` guard skips `cargo install`
    // outright when the pinned version is already on `PATH` (e.g. a build
    // environment that persists `~/.cargo/bin` across runs), instead of
    // reinstalling unconditionally on every build.
    build.insert(
        "command".into(),
        Value::String(format!(
            "((command -v worker-build >/dev/null 2>&1 && \
             [ \"$(worker-build --version)\" = \"{WORKER_BUILD_VERSION}\" ]) || \
             cargo install worker-build --version \"={WORKER_BUILD_VERSION}\" --quiet) && \
             worker-build --no-default-features \
             --features target-cloudflare"
        )),
    );
    root.insert("build".into(), Value::Table(build));

    let mut d1_entry = toml::map::Map::new();
    d1_entry.insert("binding".into(), Value::String(cfg.d1.binding.clone()));
    d1_entry.insert(
        "database_name".into(),
        Value::String(cfg.d1.database_name.clone()),
    );
    d1_entry.insert(
        "database_id".into(),
        Value::String(cfg.d1.database_id.clone()),
    );
    root.insert(
        "d1_databases".into(),
        Value::Array(vec![Value::Table(d1_entry)]),
    );

    let mut r2_entry = toml::map::Map::new();
    r2_entry.insert("binding".into(), Value::String(cfg.r2.binding.clone()));
    r2_entry.insert(
        "bucket_name".into(),
        Value::String(cfg.r2.bucket_name.clone()),
    );
    root.insert(
        "r2_buckets".into(),
        Value::Array(vec![Value::Table(r2_entry)]),
    );

    // Plain worker vars (`env.var(...)`), read at runtime by the CF adapter.
    // STRICT_SCHEMA on: the D1 database service trusts its migrated schema and
    // skips the per-op table-exists probe + lazy ADD COLUMN, removing a network
    // round-trip per logical op on D1. This is a deploy-time operational
    // decision (not an admin-editable runtime toggle), so it lives here rather
    // than in the D1 `variables` table. The adapter's `build_runtime` threads
    // this into the config snapshot so the `wafer-run/database` block applies
    // it at Init. A consumer that needs the self-healing lazy-schema behavior
    // can flip it back off via `wrangler_overrides_path`.
    let mut vars = toml::map::Map::new();
    vars.insert(
        wafer_core::interfaces::database::handler::STRICT_SCHEMA_CONFIG_KEY.into(),
        Value::String("true".into()),
    );
    root.insert("vars".into(), Value::Table(vars));

    // Workers Logs (a.k.a. Workers Observability). Off by default at the
    // platform; we turn it on so dashboard logs + the `wrangler tail`
    // request envelope are populated. `head_sampling_rate` is an explicit,
    // configurable knob (`cfg.head_sampling_rate`, see
    // `CloudflareConfig::head_sampling_rate`) rather than hardcoded — set
    // `[cloudflare].head_sampling_rate` in impresspress.toml or
    // `IMPRESSPRESS_CLOUDFLARE_HEAD_SAMPLING_RATE` for a deployment whose
    // traffic has outgrown 100% capture.
    let mut obs = toml::map::Map::new();
    obs.insert("enabled".into(), Value::Boolean(true));
    obs.insert(
        "head_sampling_rate".into(),
        Value::Float(cfg.head_sampling_rate),
    );
    root.insert("observability".into(), Value::Table(obs));

    Value::Table(root)
}

/// Deep-merge `overrides` into `base`. Tables merge recursively;
/// arrays and primitives are replaced wholesale.
fn deep_merge(base: &mut toml::Value, overrides: toml::Value) {
    use toml::Value;
    match (base, overrides) {
        (Value::Table(b), Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, replacement) => *slot = replacement,
    }
}
