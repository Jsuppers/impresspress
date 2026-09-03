//! The two templates, compiled for real and run for real.
//!
//! `wafer_guest_parity.rs` proves the JSON the guest module *renders* is the
//! JSON the host's types parse. That is a check of shapes, and it can be true
//! of a module that never compiles to wasm, never negotiates the JSON host
//! codec, and never reaches a database. This file closes the rest of the
//! loop:
//!
//! 1. copy a template out of the tree (dereferencing the `wafer_guest.rs`
//!    symlink) and build it with **plain `cargo`** for `wasm32-wasip1`,
//!    `--offline`, which is what proves the crate has no dependencies — the
//!    browser toolchain the sandbox actually uses has no registry at all;
//! 2. load the module into a real `Wafer` under wasmi, beside the real
//!    `wafer-run/database` block over in-memory SQLite;
//! 3. start the runtime, which runs the guest's `Init` — and therefore its
//!    `db::ensure_table` — under the capabilities its own `BlockInfo`
//!    declared;
//! 4. drive HTTP requests through it and assert on the bytes that come back.
//!
//! Everything between the template's source and the response body is the
//! production path: the ABI exports, the JSON host codec, WRAP's
//! own-namespace rule, the `schema` capability, and the real database
//! handler.
//!
//! # When it does not run
//!
//! A machine without `cargo` or without the `wasm32-wasip1` target cannot do
//! step 1. The test then prints why and returns — but with
//! `IMPRESSPRESS_GUEST_GOLDEN=1` set (as CI does) the same condition is a
//! **failure**, so a CI job that lost the target reports it instead of
//! quietly testing nothing.
#![cfg(all(feature = "block-dev", feature = "wasm"))]

use std::{path::Path, process::Command, sync::Arc};

use wafer_block::{http_codec, streams::input::InputStream, Message};
use wafer_block_sqlite::service::SQLiteDatabaseService;
use wafer_run::{wasm::WasmiBlock, Wafer};

// ---------------------------------------------------------------------------
// Building a template
// ---------------------------------------------------------------------------

/// Whether this machine can build a template, and why not when it cannot.
fn toolchain_ready() -> Result<(), String> {
    let probe = Command::new("cargo").arg("--version").output();
    match probe {
        Ok(out) if out.status.success() => {}
        Ok(out) => return Err(format!("`cargo --version` failed: {}", out.status)),
        Err(e) => return Err(format!("`cargo` is not runnable: {e}")),
    }
    // `--print target-libdir` succeeds only when the target's std is actually
    // installed, which is the thing the build needs — `rustc --print
    // target-list` would answer yes for every target rustc knows about.
    let std_probe = Command::new("rustc")
        .args(["--print", "target-libdir", "--target", "wasm32-wasip1"])
        .output();
    match std_probe {
        Ok(out) if out.status.success() => {
            let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if Path::new(&dir).is_dir() {
                Ok(())
            } else {
                Err(format!(
                    "the wasm32-wasip1 std is not installed ({dir} is missing) — \
                     `rustup target add wasm32-wasip1`"
                ))
            }
        }
        Ok(_) => Err(
            "rustc does not know the wasm32-wasip1 target — `rustup target add wasm32-wasip1`"
                .to_string(),
        ),
        Err(e) => Err(format!("`rustc` is not runnable: {e}")),
    }
}

/// Gate every test in this file on [`toolchain_ready`].
///
/// Returns `false` to skip. `IMPRESSPRESS_GUEST_GOLDEN=1` turns the skip into
/// a panic, so the environment that is supposed to run this (CI) cannot pass
/// by not running it.
fn buildable() -> bool {
    match toolchain_ready() {
        Ok(()) => true,
        Err(why) if std::env::var_os("IMPRESSPRESS_GUEST_GOLDEN").is_some() => {
            panic!("IMPRESSPRESS_GUEST_GOLDEN=1 but the template cannot be built: {why}")
        }
        Err(why) => {
            eprintln!(
                "SKIPPED wafer_guest_golden: {why}. Set IMPRESSPRESS_GUEST_GOLDEN=1 to make this \
                 a failure instead."
            );
            false
        }
    }
}

/// Copy `from` to `to`, following symlinks.
///
/// Following them is the point: `src/wafer_guest.rs` in each template is a
/// symlink to the canonical module, and the copy has to be a real file so the
/// build is of the same bytes `dev_create_block` writes.
fn copy_dir_all(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        // `metadata` follows symlinks, so a symlinked file reports as a file.
        if entry.metadata()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// The `name = "…"` line of a template's `Cargo.toml`.
///
/// The crate name is not the directory name — `templates/table` is the
/// `newsletter` crate — and the artifact is named after the crate, with
/// hyphens turned into underscores by the linker.
fn package_name(manifest: &str) -> String {
    manifest
        .lines()
        .find_map(|line| {
            line.strip_prefix("name = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .expect("the template's Cargo.toml declares a package name")
        .to_string()
}

/// Build `templates/{name}` for `wasm32-wasip1` and return the module.
fn build_template(name: &str) -> Vec<u8> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/blocks/dev/templates")
        .join(name);
    let out = tempfile::tempdir().expect("tempdir");
    copy_dir_all(&source, out.path()).expect("copy the template");

    let package = package_name(
        &std::fs::read_to_string(out.path().join("Cargo.toml")).expect("read Cargo.toml"),
    );
    // `--offline` is the assertion, not an optimization: a template with a
    // single dependency would fail here rather than quietly working on a
    // machine with a warm registry cache. `--target-dir` is explicit so an
    // ambient `CARGO_TARGET_DIR` cannot move the artifact out from under the
    // read below.
    let target_dir = out.path().join("target");
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip1",
            "--offline",
        ])
        .arg("--target-dir")
        .arg(&target_dir)
        .current_dir(out.path())
        .status()
        .expect("run cargo");
    assert!(
        status.success(),
        "the {name} template must build with plain cargo and no dependencies"
    );

    let artifact = target_dir
        .join("wasm32-wasip1/release")
        .join(format!("{}.wasm", package.replace('-', "_")));
    let bytes =
        std::fs::read(&artifact).unwrap_or_else(|e| panic!("read {}: {e}", artifact.display()));
    assert!(
        bytes.len() <= impresspress_core::blocks::dev::validation::MAX_ARTIFACT_BYTES,
        "the {name} template must fit the sandbox's {} byte artifact limit; it is {} bytes",
        impresspress_core::blocks::dev::validation::MAX_ARTIFACT_BYTES,
        bytes.len(),
    );
    bytes
}

// ---------------------------------------------------------------------------
// The runtime the block runs in
// ---------------------------------------------------------------------------

/// An unstarted `Wafer` carrying the real `wafer-run/database` block over an
/// in-memory SQLite.
///
/// The shape `wafer-run`'s own `json_host_codec_e2e` uses: no admin block and
/// no WRAP grants, so the guest is an ordinary unprivileged caller that
/// reaches its own namespace and nothing else.
fn golden_wafer() -> Wafer {
    let mut wafer = Wafer::builder()
        .disable_inventory()
        .disable_lockfile()
        .build()
        .expect("build a Wafer");
    let sqlite = Arc::new(SQLiteDatabaseService::open_in_memory().expect("in-memory sqlite"));
    wafer_core::service_blocks::database::register_with_tables(&mut wafer, sqlite, vec![])
        .expect("register wafer-run/database");
    wafer
}

/// The `Message` the HTTP boundary builds for a request, plus the auth meta
/// the auth layer would have added.
fn http_msg(method: &str, path: &str, auth: &[(&str, &str)]) -> Message {
    let mut msg = http_codec::build_http_message(
        method,
        path,
        "",
        "127.0.0.1",
        [("content-type", "application/json")],
    );
    for (key, value) in auth {
        msg.set_meta(*key, *value);
    }
    msg
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// The `table` template creates its table on `Init` and serves all three of
/// its endpoints over the JSON host codec.
#[tokio::test]
async fn table_template_creates_its_table_and_serves_its_endpoints() {
    if !buildable() {
        return;
    }
    let wasm = build_template("table");
    let mut wafer = golden_wafer();
    let block = WasmiBlock::load_from_bytes(&wasm).expect("load the newsletter module");
    wafer
        .register_block("site/newsletter", Arc::new(block))
        .expect("register site/newsletter");
    // `start` runs the guest's `Init`, which is where `db::ensure_table` is —
    // so a failure here is the schema capability, WRAP, or the wire shape of
    // `database.ensure_table`, and nothing else.
    let wafer = wafer.start().await.expect("start the runtime");

    // A public POST from an anonymous caller.
    let out = wafer
        .run_block(
            "site/newsletter",
            http_msg("POST", "/b/newsletter/subscribe", &[("auth.user_id", "")]),
            InputStream::from_bytes(br#"{"email":"a@b.c"}"#.to_vec()),
        )
        .await
        .collect_buffered()
        .await
        .expect("a buffered response");
    assert_eq!(http_codec::resolve_status(&out.meta, 200), 200);
    let body: serde_json::Value = serde_json::from_slice(&out.body).unwrap_or_else(|e| {
        panic!(
            "subscribe body ({e}): {:?}",
            String::from_utf8_lossy(&out.body)
        )
    });
    assert_eq!(body["ok"], true, "{body}");

    // The row is really there: an admin read comes back through the database.
    let out = wafer
        .run_block(
            "site/newsletter",
            http_msg(
                "GET",
                "/b/newsletter/subscribers",
                &[("auth.user_id", "admin_1"), ("auth.user_roles", "admin")],
            ),
            InputStream::empty(),
        )
        .await
        .collect_buffered()
        .await
        .expect("a buffered response");
    let listing: serde_json::Value = serde_json::from_slice(&out.body).unwrap_or_else(|e| {
        panic!(
            "listing body ({e}): {:?}",
            String::from_utf8_lossy(&out.body)
        )
    });
    assert_eq!(listing["subscribers"][0]["email"], "a@b.c", "{listing}");
    let id = listing["subscribers"][0]["id"]
        .as_str()
        .expect("the subscriber's id")
        .to_string();
    assert!(
        !listing["subscribers"][0]["created_at"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "the host stamps created_at: {listing}"
    );

    // The `{id}` route: the guest's own router binds the parameter.
    let out = wafer
        .run_block(
            "site/newsletter",
            http_msg(
                "GET",
                &format!("/b/newsletter/subscribers/{id}"),
                &[("auth.user_id", "admin_1"), ("auth.user_roles", "admin")],
            ),
            InputStream::empty(),
        )
        .await
        .collect_buffered()
        .await
        .expect("a buffered response");
    let one: serde_json::Value = serde_json::from_slice(&out.body).expect("by-id body");
    assert_eq!(one["email"], "a@b.c", "{one}");

    // The duplicate check the template makes explicitly, rather than leaning
    // on the UNIQUE constraint.
    let out = wafer
        .run_block(
            "site/newsletter",
            http_msg("POST", "/b/newsletter/subscribe", &[("auth.user_id", "")]),
            InputStream::from_bytes(br#"{"email":"a@b.c"}"#.to_vec()),
        )
        .await
        .collect_buffered()
        .await
        .expect("a buffered response");
    assert_eq!(http_codec::resolve_status(&out.meta, 200), 409);

    // A malformed body is the template's own 400, not a trap.
    let out = wafer
        .run_block(
            "site/newsletter",
            http_msg("POST", "/b/newsletter/subscribe", &[("auth.user_id", "")]),
            InputStream::from_bytes(br#"{"email":"nope"}"#.to_vec()),
        )
        .await
        .collect_buffered()
        .await
        .expect("a buffered response");
    assert_eq!(http_codec::resolve_status(&out.meta, 200), 400);

    // A path the block declares no endpoint for is the guest's own 404.
    let out = wafer
        .run_block(
            "site/newsletter",
            http_msg("GET", "/b/newsletter/nothing", &[]),
            InputStream::empty(),
        )
        .await
        .collect_buffered()
        .await
        .expect("a buffered response");
    assert_eq!(http_codec::resolve_status(&out.meta, 200), 404);
}

/// The `hello` template answers, and reports itself as `site/hello`.
#[tokio::test]
async fn hello_template_answers() {
    if !buildable() {
        return;
    }
    let wasm = build_template("hello");
    let mut wafer = golden_wafer();
    let block = WasmiBlock::load_from_bytes(&wasm).expect("load the hello module");
    wafer
        .register_block("site/hello", Arc::new(block))
        .expect("register site/hello");
    let wafer = wafer.start().await.expect("start the runtime");

    let out = wafer
        .run_block(
            "site/hello",
            http_msg("GET", "/b/hello/", &[]),
            InputStream::empty(),
        )
        .await
        .collect_buffered()
        .await
        .expect("a buffered response");
    assert_eq!(http_codec::resolve_status(&out.meta, 200), 200);
    assert!(
        String::from_utf8_lossy(&out.body).contains("Hello from site/hello"),
        "{:?}",
        String::from_utf8_lossy(&out.body)
    );
}

/// The `BlockInfo` a real compiled template reports is the one the sandbox's
/// static rules accept — the same check `wafer_guest_parity` makes against
/// the natively-rendered string, but against the bytes wasmi read out of the
/// module.
#[tokio::test]
async fn a_compiled_template_reports_the_block_info_the_sandbox_accepts() {
    if !buildable() {
        return;
    }
    let wasm = build_template("table");
    let block = WasmiBlock::load_from_bytes(&wasm).expect("load the newsletter module");
    let info = wafer_run::Block::info(&block);

    let spec = impresspress_core::blocks::dev::validation::validate_static(
        "newsletter",
        &info,
        "sha",
        &impresspress_core::blocks::dev::validation::builtin_route_prefixes(),
        &[],
        &std::collections::BTreeSet::new(),
    )
    .unwrap_or_else(|found| panic!("the compiled template was refused: {found:?}"));
    assert_eq!(spec.name, "site/newsletter");
    assert_eq!(spec.routes[0].prefix, "/b/newsletter/");
    assert!(spec.capabilities.schema);
    assert!(!spec.capabilities.ddl);
    assert!(spec
        .capabilities
        .allows_collection("site__newsletter__subscribers"));
}
