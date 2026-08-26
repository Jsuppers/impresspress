use std::fs;

use impresspress::cli::helpers::cloudflare::{
    assets::ReleaseManifest,
    build::WORKER_BUILD_VERSION,
    prepared::{stage_prepared_module, ApplicationArtifactIdentity, PREPARED_TEXT_GLOB},
    wrangler::{
        generate, generate_candidate_upload, generate_final_upload, generate_upload,
        generate_upload_with_release, CloudflareConfig, D1Config, R2Config,
        PREPARED_APPLICATION_BUILD_SHA256_VAR, PREPARED_APPLICATION_ID_VAR, PREPARED_PLAN_HASH_VAR,
        PREPARED_PLAN_MODULE_SHA256_VAR, PREPARED_WAFER_LOCK_IDENTITY_VAR, RELEASE_ASSET_ID_VAR,
        RELEASE_ASSET_KEYS_SHA256_VAR, RELEASE_ASSET_MANIFEST_SHA256_VAR,
        RELEASE_ASSET_MANIFEST_VAR, RELEASE_ASSET_PREFIX_VAR,
    },
};
use impresspress_core::{PreparedRuntimePlan, PreparedRuntimeStructure, WaferLockIdentity};
use tempfile::tempdir;

fn sample_cfg() -> CloudflareConfig {
    CloudflareConfig {
        account_id: "test-acct".into(),
        worker_name: "wafer-site".into(),
        compatibility_date: "2026-05-01".into(),
        d1: D1Config {
            binding: "DB".into(),
            database_name: "wafer-site-prod".into(),
            database_id: "00000000-0000-0000-0000-000000000000".into(),
        },
        r2: R2Config {
            binding: "STORAGE".into(),
            bucket_name: "wafer-site-assets".into(),
            release_assets_dir: None,
            release_assets_prefix: Default::default(),
            release_assets_exclude: Vec::new(),
        },
        wrangler_overrides_path: None,
        head_sampling_rate: 1.0,
        deploy_smoke_paths: vec!["/health".into()],
    }
}

#[test]
fn generate_writes_wrangler_toml_with_required_fields() {
    let tmp = tempdir().unwrap();
    let repo_root = tmp.path();
    let out = repo_root.join("target/impresspress-cloudflare");
    fs::create_dir_all(&out).unwrap();

    let path = generate(&sample_cfg(), repo_root, &out).unwrap();
    assert!(path.exists(), "wrangler.toml should be created");
    let body = fs::read_to_string(&path).unwrap();

    assert!(body.contains(r#"name = "wafer-site""#));
    assert!(body.contains(r#"account_id = "test-acct""#));
    assert!(body.contains(r#"compatibility_date = "2026-05-01""#));
    assert!(body.contains(r#"binding = "DB""#));
    assert!(body.contains(r#"database_name = "wafer-site-prod""#));
    assert!(body.contains(r#"binding = "STORAGE""#));
    assert!(body.contains(r#"bucket_name = "wafer-site-assets""#));
    assert!(body.contains(r#"main = "../../build/worker/shim.mjs""#));
    assert!(
        body.contains(r#"binding = "CF_VERSION_METADATA""#),
        "runtime cache invalidation needs the Worker version identity:\n{body}"
    );
    assert!(
        body.contains("preview_urls = true"),
        "preview_urls must be enabled so `wrangler versions upload` prints a \
         Version Preview URL for `impresspress deploy` to parse:\n{body}"
    );
    assert!(
        !body.contains("migrations_dir"),
        "deploy toml must not declare a wrangler migrations ledger — \
         schema funnels through /_deploy/init"
    );
    assert!(
        body.contains(r#"WAFER_RUN__DATABASE__STRICT_SCHEMA = "true""#),
        "deploy toml must enable STRICT_SCHEMA so the D1 adapter trusts its \
         migrated schema and skips per-op introspection round-trips:\n{body}"
    );
}

#[test]
fn generate_writes_configured_head_sampling_rate() {
    let tmp = tempdir().unwrap();
    let repo_root = tmp.path();
    let out = repo_root.join("target/impresspress-cloudflare");
    fs::create_dir_all(&out).unwrap();

    let mut cfg = sample_cfg();
    cfg.head_sampling_rate = 0.1;
    let path = generate(&cfg, repo_root, &out).unwrap();
    let body = fs::read_to_string(&path).unwrap();

    assert!(
        body.contains("head_sampling_rate = 0.1"),
        "generated toml should reflect the configured sampling rate, not a \
         hardcoded 1.0:\n{body}"
    );
}

#[test]
fn generate_pins_worker_build_and_skips_reinstall_when_present() {
    let tmp = tempdir().unwrap();
    let repo_root = tmp.path();
    let out = repo_root.join("target/impresspress-cloudflare");
    fs::create_dir_all(&out).unwrap();

    let path = generate(&sample_cfg(), repo_root, &out).unwrap();
    let body = fs::read_to_string(&path).unwrap();

    let expected_pin = format!(r#"--version "={WORKER_BUILD_VERSION}""#);
    assert!(
        body.contains(&expected_pin),
        "build command must pin the exact worker-build version, not a \
         floating semver range:\n{body}"
    );
    assert!(
        body.contains(&format!(r#"= "{WORKER_BUILD_VERSION}""#)),
        "build command must check the installed version before deciding \
         whether to reinstall:\n{body}"
    );
    assert!(
        body.contains("worker-build --no-default-features"),
        "build command must still run worker-build after the check:\n{body}"
    );
}

#[test]
fn generate_merges_overrides_with_consumer_winning() {
    let tmp = tempdir().unwrap();
    let repo_root = tmp.path();
    let out = repo_root.join("target/impresspress-cloudflare");
    fs::create_dir_all(&out).unwrap();

    let overrides_path = repo_root.join("wrangler.overrides.toml");
    fs::write(
        &overrides_path,
        r#"
compatibility_date = "2099-01-01"

[[routes]]
pattern = "wafer.run/*"
zone_name = "wafer.run"
"#,
    )
    .unwrap();

    let mut cfg = sample_cfg();
    cfg.wrangler_overrides_path = Some(
        overrides_path
            .strip_prefix(repo_root)
            .unwrap()
            .to_path_buf(),
    );

    let path = generate(&cfg, repo_root, &out).unwrap();
    let body = fs::read_to_string(&path).unwrap();

    assert!(
        body.contains(r#"compatibility_date = "2099-01-01""#),
        "override primitive should win:\n{body}"
    );
    assert!(
        body.contains(r#"pattern = "wafer.run/*""#),
        "new array entry should be present:\n{body}"
    );
    assert!(
        body.contains(r#"name = "wafer-site""#),
        "non-overridden default should remain:\n{body}"
    );
    assert!(
        body.contains(r#"binding = "DB""#),
        "non-overridden d1 binding should remain:\n{body}"
    );
}

#[test]
fn generate_upload_uses_prebuilt_artifact_without_build_hook() {
    let tmp = tempdir().unwrap();
    let repo_root = tmp.path();
    let out = repo_root.join("target/impresspress-cloudflare");
    fs::create_dir_all(&out).unwrap();

    // Even a consumer-provided build override must not re-enable compilation
    // in the upload-only configuration.
    let overrides_path = repo_root.join("wrangler.overrides.toml");
    fs::write(
        &overrides_path,
        r#"
[build]
command = "must-not-run"

[[routes]]
pattern = "wafer.run/*"
zone_name = "wafer.run"
"#,
    )
    .unwrap();
    let mut cfg = sample_cfg();
    cfg.wrangler_overrides_path = Some("wrangler.overrides.toml".into());

    let developer_path = generate(&cfg, repo_root, &out).unwrap();
    let upload_path = generate_upload(&cfg, repo_root, &out).unwrap();
    let developer: toml::Value =
        toml::from_str(&fs::read_to_string(developer_path).unwrap()).unwrap();
    let upload: toml::Value = toml::from_str(&fs::read_to_string(&upload_path).unwrap()).unwrap();

    assert_eq!(
        upload_path.file_name().unwrap(),
        "wrangler-upload.toml",
        "the upload config must not overwrite the developer config"
    );
    assert!(
        developer.get("build").is_some(),
        "developer config still needs its build hook"
    );
    assert!(
        upload.get("build").is_none(),
        "upload config must consume the artifact already built by ImpressPress"
    );
    for key in [
        "main",
        "name",
        "compatibility_date",
        "d1_databases",
        "r2_buckets",
        "vars",
        "routes",
        "version_metadata",
    ] {
        assert_eq!(
            upload.get(key),
            developer.get(key),
            "upload and developer configs diverged at {key}"
        );
    }
}

#[test]
fn generate_upload_binds_release_identity_and_exact_key_set_after_overrides() {
    let tmp = tempdir().unwrap();
    let repo_root = tmp.path();
    let out = repo_root.join("target/impresspress-cloudflare");
    let staged = out.join("assets");
    fs::create_dir_all(staged.join("site/media")).unwrap();
    fs::write(staged.join("site/media/hero.webp"), b"hero").unwrap();
    fs::write(staged.join("site/app.js"), b"app").unwrap();
    let release = ReleaseManifest::from_staged_dir(&staged).unwrap();

    // A consumer override cannot detach this Worker version from the release
    // identity the deployer is about to upload and verify.
    fs::write(
        repo_root.join("wrangler.overrides.toml"),
        format!(
            "[vars]\n{RELEASE_ASSET_ID_VAR} = \"wrong\"\n{RELEASE_ASSET_PREFIX_VAR} = \"wrong\"\n"
        ),
    )
    .unwrap();
    let mut cfg = sample_cfg();
    cfg.wrangler_overrides_path = Some("wrangler.overrides.toml".into());

    let path = generate_upload_with_release(&cfg, repo_root, &out, Some(&release)).unwrap();
    let generated: toml::Value = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    let vars = generated["vars"].as_table().unwrap();

    assert_eq!(
        vars[RELEASE_ASSET_ID_VAR].as_str(),
        Some(release.asset_set_sha256.as_str())
    );
    assert_eq!(
        vars[RELEASE_ASSET_PREFIX_VAR].as_str(),
        Some(release.immutable_prefix.as_str())
    );
    assert_eq!(
        vars[RELEASE_ASSET_MANIFEST_VAR].as_str(),
        Some(release.manifest_key().as_str())
    );
    assert!(!vars.contains_key("IMPRESSPRESS_RELEASE_ASSET_KEYS_JSON"));
}

#[test]
fn release_key_sets_larger_than_the_old_cap_are_accepted() {
    // >4KB of logical keys — the old Worker-var budget must be gone.
    let staged = tempdir().unwrap();
    let media = staged.path().join("site/media");
    fs::create_dir_all(&media).unwrap();
    for i in 0..200 {
        fs::write(media.join(format!("image-{i:04}.webp")), b"x").unwrap();
    }
    let release = ReleaseManifest::from_staged_dir(staged.path()).unwrap();
    assert!(release.logical_keys_json().unwrap().len() > 4 * 1024);

    let tmp = tempdir().unwrap();
    let out = tmp.path().join("target/impresspress-cloudflare");
    fs::create_dir_all(&out).unwrap();
    let path =
        generate_upload_with_release(&sample_cfg(), tmp.path(), &out, Some(&release)).unwrap();
    let generated: toml::Value = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    let vars = generated["vars"].as_table().unwrap();

    assert!(!vars.contains_key("IMPRESSPRESS_RELEASE_ASSET_KEYS_JSON"));
    assert_eq!(
        vars[RELEASE_ASSET_KEYS_SHA256_VAR].as_str(),
        Some(release.logical_keys_sha256().unwrap().as_str())
    );
}

#[test]
fn candidate_and_final_configs_reuse_identity_but_only_final_loads_text_plan() {
    let tmp = tempdir().unwrap();
    let out = tmp.path().join("target/impresspress-cloudflare");
    let staged = out.join("assets");
    fs::create_dir_all(&staged).unwrap();
    fs::write(staged.join("app.js"), b"app").unwrap();
    let release = ReleaseManifest::from_staged_dir(&staged).unwrap();
    let identity = ApplicationArtifactIdentity {
        application_id: "wafer-site".into(),
        application_build_sha256: format!("sha256:{}", "a".repeat(64)),
        dependency_lock: WaferLockIdentity::absent(),
    };
    let plan = PreparedRuntimePlan::new_with_config_generation(
        "wafer-site",
        identity.application_build_sha256.clone(),
        "3".repeat(32),
        identity.dependency_lock.clone(),
        release.prepared_identity().unwrap(),
        PreparedRuntimeStructure {
            application_blocks: Vec::new(),
            routes: Vec::new(),
            built_in_route_count: 0,
            block_settings: Default::default(),
            block_configs: Default::default(),
            final_block_configs: Default::default(),
            wrap_grants: Vec::new(),
            deployment_wrap_grants: Vec::new(),
        },
    )
    .unwrap();
    let prepared = stage_prepared_module(&out, &plan).unwrap();

    let candidate_path =
        generate_candidate_upload(&sample_cfg(), tmp.path(), &out, &release, &identity).unwrap();
    let final_path = generate_final_upload(
        &sample_cfg(),
        tmp.path(),
        &out,
        &release,
        &identity,
        &prepared,
    )
    .unwrap();
    let candidate: toml::Value =
        toml::from_str(&fs::read_to_string(candidate_path).unwrap()).unwrap();
    let final_cfg: toml::Value = toml::from_str(&fs::read_to_string(final_path).unwrap()).unwrap();

    assert!(candidate.get("build").is_none());
    assert!(final_cfg.get("build").is_none());
    assert_eq!(
        candidate["main"].as_str(),
        Some("../../build/worker/shim.mjs")
    );
    assert_eq!(
        final_cfg["main"].as_str(),
        Some("prepared-runtime/shim.mjs")
    );
    assert!(candidate.get("rules").is_none());
    assert!(final_cfg["rules"].as_array().unwrap().iter().any(|rule| {
        rule["type"].as_str() == Some("Text")
            && rule["fallthrough"].as_bool() == Some(true)
            && rule.to_string().contains(PREPARED_TEXT_GLOB)
    }));

    let candidate_vars = candidate["vars"].as_table().unwrap();
    let final_vars = final_cfg["vars"].as_table().unwrap();
    for key in [
        RELEASE_ASSET_ID_VAR,
        RELEASE_ASSET_PREFIX_VAR,
        RELEASE_ASSET_MANIFEST_VAR,
        RELEASE_ASSET_MANIFEST_SHA256_VAR,
        RELEASE_ASSET_KEYS_SHA256_VAR,
        PREPARED_APPLICATION_ID_VAR,
        PREPARED_APPLICATION_BUILD_SHA256_VAR,
        PREPARED_WAFER_LOCK_IDENTITY_VAR,
    ] {
        assert_eq!(
            candidate_vars[key], final_vars[key],
            "identity drift at {key}"
        );
    }
    assert!(candidate_vars.get(PREPARED_PLAN_HASH_VAR).is_none());
    assert_eq!(
        final_vars[PREPARED_PLAN_HASH_VAR].as_str(),
        Some(plan.plan_hash.as_str())
    );
    assert_eq!(
        final_vars[PREPARED_PLAN_MODULE_SHA256_VAR].as_str(),
        Some(prepared.plan_module_sha256.as_str())
    );
}

#[test]
fn generate_errors_on_missing_overrides_file() {
    let tmp = tempdir().unwrap();
    let mut cfg = sample_cfg();
    cfg.wrangler_overrides_path = Some("does-not-exist.toml".into());
    let out = tmp.path().join("target/impresspress-cloudflare");
    fs::create_dir_all(&out).unwrap();

    let err = generate(&cfg, tmp.path(), &out).unwrap_err();
    assert!(
        err.to_string().contains("does-not-exist.toml"),
        "error should mention the missing path. got: {err}"
    );
}
