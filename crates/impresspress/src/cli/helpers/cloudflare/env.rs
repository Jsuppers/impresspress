//! Resolve the `[cloudflare]` section from impresspress.toml + env vars.
//!
//! Resolution rule for env-overlayable fields: **env > toml > error**.
//! That makes clone-and-deploy work: a fresh checkout has no deployer
//! identifiers committed, and a `.env` supplies them at deploy time.
//!
//! Bindings (`d1.binding`, `r2.binding`) stay toml-only because they're
//! code contracts — the worker reads `env.DB` / `env.STORAGE` by exact
//! name, so changing one without changing the other breaks the worker.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use super::wrangler::{CloudflareConfig, D1Config, R2Config};

/// Default Workers Observability head sampling rate when neither
/// `IMPRESSPRESS_CLOUDFLARE_HEAD_SAMPLING_RATE` nor `impresspress.toml`'s
/// `[cloudflare].head_sampling_rate` is set. `1.0` (100%) preserves the
/// previous hardcoded behavior for existing consumers.
const DEFAULT_HEAD_SAMPLING_RATE: f64 = 1.0;

/// Safe application-agnostic deploy smoke path for consumers that have not
/// opted into representative dynamic routes.
const DEFAULT_DEPLOY_SMOKE_PATH: &str = "/health";

/// Toml-shaped, pre-resolution. Every field that can be supplied via env
/// is `Option<String>`.
#[derive(Debug, Deserialize)]
pub struct RawCloudflareConfig {
    pub account_id: Option<String>,
    pub worker_name: Option<String>,
    pub compatibility_date: Option<String>,
    pub d1: RawD1Config,
    pub r2: RawR2Config,
    pub wrangler_overrides_path: Option<PathBuf>,
    /// Workers Observability head sampling rate, `0.0..=1.0`. Optional —
    /// defaults to [`DEFAULT_HEAD_SAMPLING_RATE`] when unset by both toml
    /// and env.
    pub head_sampling_rate: Option<f64>,
    /// Ordinary application paths exercised before promotion. TOML-only;
    /// defaults to `/health` for existing consumers.
    pub deploy_smoke_paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct RawD1Config {
    pub binding: String,
    pub database_name: Option<String>,
    pub database_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RawR2Config {
    pub binding: String,
    pub bucket_name: Option<String>,
    pub release_assets_dir: Option<PathBuf>,
    pub release_assets_prefix: Option<PathBuf>,
    /// Glob patterns, matched against staged logical keys, whose files are
    /// kept out of the release asset set entirely.
    pub release_assets_exclude: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct ImpresspressTomlPartial {
    cloudflare: Option<RawCloudflareConfig>,
}

/// Parse `<repo_root>/impresspress.toml` and return the unresolved `[cloudflare]`
/// section.
///
/// # Errors
///
/// Returns an error if `impresspress.toml` cannot be read, cannot be parsed,
/// or does not declare a `[cloudflare]` table.
pub fn parse(repo_root: &Path) -> Result<RawCloudflareConfig> {
    let path = repo_root.join("impresspress.toml");
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let parsed: ImpresspressTomlPartial =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    parsed.cloudflare.ok_or_else(|| {
        anyhow!(
            "{} is missing a [cloudflare] section — see \
             docs/superpowers/specs/2026-05-06-impresspress-cloudflare-cli-flow-design.md",
            path.display()
        )
    })
}

impl RawCloudflareConfig {
    /// Apply env-var overlay and validate every required field is present.
    /// `env` is a getter so callers (and tests) can inject any source.
    pub fn resolve<F: Fn(&str) -> Option<String>>(self, env: F) -> Result<CloudflareConfig> {
        let account_id = pick(
            env("CLOUDFLARE_ACCOUNT_ID"),
            self.account_id,
            "account_id",
            "CLOUDFLARE_ACCOUNT_ID",
        )?;
        let worker_name = pick(
            env("IMPRESSPRESS_CLOUDFLARE_WORKER_NAME"),
            self.worker_name,
            "worker_name",
            "IMPRESSPRESS_CLOUDFLARE_WORKER_NAME",
        )?;
        let compatibility_date = pick(
            env("IMPRESSPRESS_CLOUDFLARE_COMPATIBILITY_DATE"),
            self.compatibility_date,
            "compatibility_date",
            "IMPRESSPRESS_CLOUDFLARE_COMPATIBILITY_DATE",
        )?;
        let d1_database_name = pick(
            env("IMPRESSPRESS_CLOUDFLARE_D1_DATABASE_NAME"),
            self.d1.database_name,
            "d1.database_name",
            "IMPRESSPRESS_CLOUDFLARE_D1_DATABASE_NAME",
        )?;
        let d1_database_id = pick(
            env("IMPRESSPRESS_CLOUDFLARE_D1_DATABASE_ID"),
            self.d1.database_id,
            "d1.database_id",
            "IMPRESSPRESS_CLOUDFLARE_D1_DATABASE_ID",
        )?;
        let r2_bucket_name = pick(
            env("IMPRESSPRESS_CLOUDFLARE_R2_BUCKET_NAME"),
            self.r2.bucket_name,
            "r2.bucket_name",
            "IMPRESSPRESS_CLOUDFLARE_R2_BUCKET_NAME",
        )?;
        let release_assets_dir = self.r2.release_assets_dir;
        let release_assets_prefix = self.r2.release_assets_prefix.unwrap_or_default();
        if release_assets_dir.is_none() && !release_assets_prefix.as_os_str().is_empty() {
            bail!("cloudflare.r2.release_assets_prefix requires cloudflare.r2.release_assets_dir");
        }
        if let Some(path) = &release_assets_dir {
            validate_relative_path(path, "cloudflare.r2.release_assets_dir", false)?;
        }
        validate_relative_path(
            &release_assets_prefix,
            "cloudflare.r2.release_assets_prefix",
            true,
        )?;
        // Compile the globs here so an unusable pattern is a config error at
        // parse time, not a silently-ineffective filter at stage time.
        let release_assets_exclude = self
            .r2
            .release_assets_exclude
            .unwrap_or_default()
            .into_iter()
            .map(|pattern| {
                glob::Pattern::new(&pattern).with_context(|| {
                    format!("cloudflare.r2.release_assets_exclude entry {pattern:?} is not a valid glob")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let head_sampling_rate = resolve_head_sampling_rate(
            env("IMPRESSPRESS_CLOUDFLARE_HEAD_SAMPLING_RATE"),
            self.head_sampling_rate,
        )?;
        let deploy_smoke_paths = resolve_deploy_smoke_paths(self.deploy_smoke_paths)?;
        Ok(CloudflareConfig {
            account_id,
            worker_name,
            compatibility_date,
            d1: D1Config {
                binding: self.d1.binding,
                database_name: d1_database_name,
                database_id: d1_database_id,
            },
            r2: R2Config {
                binding: self.r2.binding,
                bucket_name: r2_bucket_name,
                release_assets_dir,
                release_assets_prefix,
                release_assets_exclude,
            },
            wrangler_overrides_path: self.wrangler_overrides_path,
            head_sampling_rate,
            deploy_smoke_paths,
        })
    }
}

fn validate_relative_path(path: &Path, key: &str, allow_empty: bool) -> Result<()> {
    use std::path::Component;

    if !allow_empty && path.as_os_str().is_empty() {
        bail!("{key} must not be empty");
    }
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("{key} must be a clean repository-relative path");
    }
    Ok(())
}

/// Resolve the Workers Observability head sampling rate: env > toml >
/// [`DEFAULT_HEAD_SAMPLING_RATE`] (unlike the required identifiers `pick()`
/// handles, an unset sampling rate is not an error — 100% is a reasonable
/// default, just no longer a hardcoded one).
fn resolve_head_sampling_rate(env_val: Option<String>, toml_val: Option<f64>) -> Result<f64> {
    let rate = match env_val {
        Some(s) => s.trim().parse::<f64>().map_err(|_| {
            anyhow!(
                "IMPRESSPRESS_CLOUDFLARE_HEAD_SAMPLING_RATE={s:?} is not a valid \
                 number (expected 0.0-1.0)"
            )
        })?,
        None => toml_val.unwrap_or(DEFAULT_HEAD_SAMPLING_RATE),
    };
    if !(0.0..=1.0).contains(&rate) {
        bail!(
            "cloudflare.head_sampling_rate = {rate} is out of range — \
             must be between 0.0 and 1.0"
        );
    }
    Ok(rate)
}

fn resolve_deploy_smoke_paths(paths: Option<Vec<String>>) -> Result<Vec<String>> {
    let paths = paths.unwrap_or_else(|| vec![DEFAULT_DEPLOY_SMOKE_PATH.to_string()]);
    if paths.is_empty() {
        bail!("cloudflare.deploy_smoke_paths must contain at least one path");
    }
    for (index, path) in paths.iter().enumerate() {
        if !path.starts_with('/') || path.starts_with("//") {
            bail!(
                "cloudflare.deploy_smoke_paths[{index}] must start with exactly one '/': {path:?}"
            );
        }
        if path.contains("://") {
            bail!(
                "cloudflare.deploy_smoke_paths[{index}] must be a path, not a scheme or host: \
                 {path:?}"
            );
        }
        if path.contains('?') {
            bail!(
                "cloudflare.deploy_smoke_paths[{index}] must not contain a query string: {path:?}"
            );
        }
        if path.contains('#') {
            bail!("cloudflare.deploy_smoke_paths[{index}] must not contain a fragment: {path:?}");
        }
    }
    Ok(paths)
}

fn pick(
    env_val: Option<String>,
    toml_val: Option<String>,
    toml_key: &str,
    env_var: &str,
) -> Result<String> {
    env_val.or(toml_val).ok_or_else(|| {
        anyhow!(
            "missing required cloudflare config: set [cloudflare.{toml_key}] in impresspress.toml \
             or env {env_var}"
        )
    })
}

/// Production entry point — parse + resolve via `std::env::var`.
///
/// # Errors
///
/// Returns an error from [`parse`] (missing/unparseable `impresspress.toml`)
/// or [`RawCloudflareConfig::resolve`] (missing required field after env
/// overlay).
pub fn load(repo_root: &Path) -> Result<CloudflareConfig> {
    let raw = parse(repo_root)?;
    raw.resolve(|name| std::env::var(name).ok())
}
