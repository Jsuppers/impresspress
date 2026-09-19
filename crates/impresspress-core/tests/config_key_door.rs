//! A config key is spelled where it is declared, and nowhere else.
//!
//! A config variable's name is its wire identity: it is the `variables` row's
//! primary key, the environment variable an operator exports, the `name=` on
//! the admin settings form, and the string `blocks::config` looks up. Every
//! one of those has to be the same bytes. Today 113 distinct keys are spelled
//! as string literals in 425 places across 57 files, so a rename is a
//! whole-repo find-and-replace that compiles perfectly if you miss one and
//! fails at runtime, silently, as "the setting has no effect" — a config read
//! that finds nothing is indistinguishable from a config nobody set.
//!
//! The rule the crate already states (`config_vars.rs`: "Shared
//! (`WAFER_RUN_SHARED__`) variables are defined here — the single source of
//! truth. Block-scoped variables are declared in each block's `BlockInfo`") is
//! about *declaration*. This gate is the other half: a declared key is named
//! through the constant that declares it, not respelled at the call site.
//!
//! ## What this gate is for, and what it is not
//!
//! It is **not** a conversion. The 425 literals are still there; every file
//! holding one is named below. What it stops is the 426th: a new file, or a
//! newly literal-spelled key in a file that had none, fails this test and has
//! to either use the constant or say here why it cannot.
//!
//! The list is a worklist. A later step replaces the literals with constants
//! and deletes entries; [`no_allowlist_entry_is_dead`] is what makes that
//! deletion mandatory rather than optional, so the list can only shrink.
//!
//! ## What the scan matches
//!
//! A string literal whose whole content is `WAFER_RUN_SHARED__…`,
//! `IMPRESSPRESS__…` or `IMPRESSPRESS_…` followed only by `A-Z`, `0-9` and
//! `_` — the three namespaces the repo's `CLAUDE.md` defines
//! (`WAFER_RUN_SHARED__*` shared, `{ORG}__{BLOCK}__*` block-scoped,
//! `IMPRESSPRESS_*` infrastructure). It finds the key at any quote, so a key
//! embedded in a longer raw string (`r#"name="WAFER_RUN_SHARED__APP_NAME""#`)
//! counts, which is the point: the rendered form's `name=` attribute is one of
//! the places that has to agree.
//!
//! Full-line comments are stripped first — prose naming a key is not a
//! spelling of it, and this file's own doc comment would otherwise fail the
//! gate. A trailing comment on a line of code is kept, so nothing hides
//! behind a `//` on the same line as the code using it.
//!
//! ## What it does NOT see, stated so it is not mistaken for more than it is
//!
//! * A key assembled rather than written. `blocks/admin/pages/permissions.rs`
//!   builds a block's key prefix with `format!("{}__", …)` legitimately; a
//!   bypass could do the same deliberately.
//! * `WAFER_RUN__<BLOCK>__*` keys — the auth block's `WAFER_RUN__AUTH__*`
//!   family and the `WAFER_RUN__{WEB,SQLITE,STRIPE}` block prefixes, which
//!   predate the `IMPRESSPRESS__` convention. 46 literals across 11 files
//!   spell one today, and bringing them in means deciding what their doors
//!   are — a decision for the step that converts them, not for the gate that
//!   stops new ones.
//! * The block test modules under `src/**/tests/`, skipped for the same
//!   reason `tests/db_read_guard.rs` skips them: they are `#[cfg(test)]` and
//!   never ship. An inline `#[cfg(test)] mod tests` IS in scope, because it
//!   shares a file with the production code that has to be converted anyway
//!   and the list below is per file.
//! * The other workspace crates. `CARGO_MANIFEST_DIR` is this one, which is
//!   where `config_vars.rs` and every block's declaration lives.

use impresspress_core::test_support::source_scan::{strip_line_comments, SourceWalk};

/// Every file that spells a config key as a literal today, and nothing else.
///
/// Per FILE rather than per key, deliberately. A per-key list would have to
/// pair 113 keys with the files allowed to spell each, which is 425 decisions
/// nobody has made; a per-file list is 57 facts that are all true right now
/// and each of which a later step can delete outright. The cost is that a
/// listed file can add a *different* key's literal without the gate noticing
/// — which is the compromise that gets the gate standing today instead of
/// after the conversion it is meant to protect.
///
/// The entries fall into four groups, and only the first is permanent:
///
///  1. **The declaration sites.** `config_vars.rs` for the shared vars, plus
///     the module that holds each block's own `pub const …: &str = "…"` or
///     `ConfigVar::new("…")`: `blocks/{auth,products,tickets}/config.rs`,
///     `blocks/products/handlers/seller_policy.rs`, `blocks/signal/service.rs`,
///     `blocks/email.rs`, `blocks/{auth_ui,legalpages,llm,products}/mod.rs`,
///     `prepared_plan.rs` and `migration_helper.rs`. A key is a literal
///     exactly once, there; these stay listed for as long as they are where
///     the name is defined.
///  2. **Readers that could name the constant.** The large majority: a page,
///     a handler or a service calling `config::get_default(ctx, "…", …)` with
///     the key written out — `ui/mod.rs`'s `SiteConfig::load` reads seven
///     that way (the file spells ten in all). Every one of these is a
///     mechanical replacement.
///  3. **Serialisation and rendering surfaces.** `ui/settings_form.rs`,
///     `blocks/admin/pages/*`, `blocks/dev/data_snapshot.rs` — places where
///     the key is the name of a form field, a JSON member or an exported row.
///     Mechanical too, but the replacement has to keep the rendered bytes
///     identical, so they are worth converting with their snapshots in view.
///  4. **Inline `#[cfg(test)]` modules.** `platform_state/variables.rs` holds
///     51 literals and exactly one of them — `ENV_PRECEDENCE_TRANSITION_KEY` —
///     is above its `#[cfg(test)]`; `blocks/config.rs`'s nine are all below
///     one. A test that pins the wire name on purpose — the category
///     `tests/repo_door.rs` already documents for table names — is a
///     legitimate reason for a file to stay here; it just has to be written
///     down when the rest of the file is converted.
const SPELLS_A_CONFIG_KEY: &[&str] = &[
    "blocks/admin/mod.rs",
    "blocks/admin/ops.rs",
    "blocks/admin/pages/email.rs",
    "blocks/admin/pages/variables.rs",
    "blocks/admin/settings.rs",
    "blocks/auth/config.rs",
    "blocks/auth/mod.rs",
    "blocks/auth_ui/api/bootstrap.rs",
    "blocks/auth_ui/api/login.rs",
    "blocks/auth_ui/api/mod.rs",
    "blocks/auth_ui/api/signup.rs",
    "blocks/auth_ui/mod.rs",
    "blocks/auth_ui/oauth/callback.rs",
    "blocks/auth_ui/oauth/start.rs",
    "blocks/auth_ui/oauth/state_binding.rs",
    "blocks/auth_ui/pages/login.rs",
    "blocks/auth_ui/pages/mod.rs",
    "blocks/auth_ui/pages/settings.rs",
    "blocks/auth_ui/pages/signup.rs",
    "blocks/config.rs",
    "blocks/dev/data_snapshot.rs",
    "blocks/dev/export.rs",
    "blocks/dev/seed.rs",
    "blocks/email.rs",
    "blocks/files/cloud.rs",
    "blocks/legalpages/mod.rs",
    "blocks/llm/contracts.rs",
    "blocks/llm/mod.rs",
    "blocks/llm/routes/providers.rs",
    "blocks/llm/schema.rs",
    "blocks/llm/ui.rs",
    "blocks/products/config.rs",
    "blocks/products/handlers/commerce.rs",
    "blocks/products/handlers/dispatch.rs",
    "blocks/products/handlers/product.rs",
    "blocks/products/handlers/seller_policy.rs",
    "blocks/products/mod.rs",
    "blocks/products/pages.rs",
    "blocks/products/stripe.rs",
    "blocks/products/stripe_client.rs",
    "blocks/products/stripe_provider.rs",
    "blocks/signal/service.rs",
    "blocks/tickets/config.rs",
    "blocks/tickets/service.rs",
    "blocks/userportal/mod.rs",
    "blocks/vector/ingestion.rs",
    "builder/boot.rs",
    "config_vars.rs",
    "migration_helper.rs",
    "pipeline.rs",
    "platform_state/variables.rs",
    "prepared_plan.rs",
    "routing.rs",
    "ui/assets.rs",
    "ui/mod.rs",
    "ui/settings_form.rs",
    "util.rs",
];

/// The three namespaces a config key can be in. `IMPRESSPRESS_` comes last so
/// the longer `IMPRESSPRESS__` is tried first; both are accepted, because
/// `IMPRESSPRESS_*` (no double underscore) is the infrastructure namespace
/// and `IMPRESSPRESS__<BLOCK>__*` the block-scoped one, and the gate wants
/// every literal of either.
const PREFIXES: [&str; 3] = ["WAFER_RUN_SHARED__", "IMPRESSPRESS__", "IMPRESSPRESS_"];

/// The walk this gate runs over, stated once so its self-test plants its
/// offender behind the same filters the real scan uses.
fn scan() -> SourceWalk {
    SourceWalk::crate_src().skip_dir("tests").least(100)
}

/// Whether `literal` is a config key: one of the three prefixes, at least one
/// more character, and nothing but `A-Z`, `0-9` and `_` throughout.
fn is_config_key(literal: &str) -> bool {
    PREFIXES.iter().any(|prefix| {
        literal
            .strip_prefix(prefix)
            .is_some_and(|tail| !tail.is_empty())
    }) && literal
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Every config key `code` spells as a quoted literal.
///
/// Each `"` is treated as a possible opening quote and the key must run from
/// there to a closing `"` with nothing else between. Deliberately not a
/// scan that pairs quotes across the file: one raw string carrying a `"` of
/// its own (`r#"name="…""#` in `ui/settings_form.rs`) desynchronises a
/// pairing scan and hides every literal after it — which it did, by 32.
///
/// An escaped quote closes a key too. `"name=\"WAFER_RUN_SHARED__APP_NAME\""`
/// is the non-raw spelling of the very `name=` attribute this gate says has to
/// agree with the constant, so a matcher that only accepted a bare `"` would
/// be blind to exactly the case it argues about.
fn config_keys_in(code: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(offset) = code[at..].find('"') {
        let start = at + offset + 1;
        at = start;
        let tail = &code[start..];
        let end = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        let closed = tail[end..].starts_with('"') || tail[end..].starts_with("\\\"");
        if closed && is_config_key(&tail[..end]) {
            found.push(&tail[..end]);
        }
    }
    found
}

/// The files the walk reaches that spell at least one config key.
fn spellers(walk: &SourceWalk) -> Vec<String> {
    walk.collect()
        .into_iter()
        .filter(|file| !config_keys_in(&strip_line_comments(&file.text)).is_empty())
        .map(|file| file.rel)
        .collect()
}

#[test]
fn no_new_file_spells_a_config_key() {
    let mut unexpected: Vec<String> = spellers(&scan())
        .into_iter()
        .filter(|rel| !SPELLS_A_CONFIG_KEY.contains(&rel.as_str()))
        .collect();
    unexpected.sort();
    assert!(
        unexpected.is_empty(),
        "these files spell a config key as a string literal instead of naming \
         the constant that declares it: {unexpected:?}\n\
         Shared keys are declared in `config_vars.rs`; a block's own keys are \
         declared beside its `ConfigVar`s (`blocks/<block>/config.rs` or its \
         `mod.rs`). Import the constant. If the key genuinely has to be a \
         literal here — a test pinning the wire name, say — add the file to \
         SPELLS_A_CONFIG_KEY with the reason."
    );
}

/// An entry naming a file that no longer spells a key is a dead exemption:
/// it silently pre-approves whatever that file does next, and it is what
/// makes this list a worklist rather than a permanent carve-out.
#[test]
fn no_allowlist_entry_is_dead() {
    let spelling = spellers(&scan());
    let mut stale: Vec<&str> = SPELLS_A_CONFIG_KEY
        .iter()
        .copied()
        .filter(|entry| !spelling.iter().any(|rel| rel == entry))
        .collect();
    stale.sort_unstable();
    assert!(
        stale.is_empty(),
        "these files are on SPELLS_A_CONFIG_KEY but no longer spell a config \
         key; drop the entries so the list keeps shrinking: {stale:?}"
    );
}

/// The matcher still recognises what it bans, and still lets through what it
/// does not.
///
/// A gate whose matcher has quietly stopped matching passes for the same
/// reason a converted codebase does, which is the failure mode that makes
/// most source gates worthless.
#[test]
fn the_matcher_recognises_the_keys_it_bans() {
    for banned in [
        r#"config::get_default(ctx, "WAFER_RUN_SHARED__APP_NAME", "").await"#,
        r#"ConfigVar::new("IMPRESSPRESS__PRODUCTS__STRIPE_SECRET_KEY")"#,
        r#"env.secret("IMPRESSPRESS_DEPLOY_TOKEN")"#,
        r##"assert!(s.contains(r#"name="WAFER_RUN_SHARED__APP_NAME""#));"##,
        r#"json!({"IMPRESSPRESS__EMAIL__FROM": "a@b.c"})"#,
        // the non-raw spelling of the `name=` attribute, closed by an
        // ESCAPED quote — the case the raw-string one above only covers
        // when the author happened to reach for `r#".."#`
        r#"write!(f, "name=\"WAFER_RUN_SHARED__APP_NAME\"")"#,
    ] {
        assert_eq!(
            config_keys_in(banned).len(),
            1,
            "the matcher stopped seeing: {banned}"
        );
    }
    for allowed in [
        // the constant, which is the whole point
        r#"config::get_default(ctx, config_vars::APP_NAME_KEY, "").await"#,
        // assembled, not spelled — a stated blind spot, not a false negative
        r#"format!("{prefix}__{name}")"#,
        // a table name, not a config key: lowercase
        r#"const TABLE: &str = "impresspress__admin__variables";"#,
        // the namespace prefix on its own carries no key after it
        r#"let p = "IMPRESSPRESS_";"#,
        // `WAFER_RUN__<BLOCK>__*` is out of the stated pattern
        r#"const JWT: &str = "WAFER_RUN__AUTH__JWT_SECRET";"#,
        // a shouty constant that is not in any config namespace
        r#"header("X-IMPRESSPRESS", "1")"#,
    ] {
        assert!(
            config_keys_in(allowed).is_empty(),
            "the matcher started refusing: {allowed} -> {:?}",
            config_keys_in(allowed)
        );
    }
}

/// The *walk* reaches a planted offender, honours an allowlist entry, skips a
/// block test module and reads only Rust.
///
/// `the_matcher_recognises_the_keys_it_bans` proves the predicate works; it
/// says nothing about whether the walk ever opens a file. A root that moved
/// or an extension filter that broke would leave the gate above passing on an
/// empty scan — green, and blind to the one thing it exists to catch. The
/// floor on [`scan`] is the other half: it fails when the real tree comes
/// back short.
#[test]
fn the_walk_reaches_the_files_it_claims_to_scan() {
    const LINE: &str = "let name = \"WAFER_RUN_SHARED__APP_NAME\";\n";

    let root = std::env::temp_dir().join(format!("config-key-door-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("blocks/tests")).expect("temp tree");
    std::fs::write(root.join("blocks/offender.rs"), LINE).expect("offender");
    std::fs::write(root.join("blocks/tests/fixture.rs"), LINE).expect("exempt test module");
    std::fs::write(root.join("listed.rs"), LINE).expect("allowlisted");
    std::fs::write(
        root.join("prose.rs"),
        "// \"WAFER_RUN_SHARED__APP_NAME\" named in a comment, not spelled\n",
    )
    .expect("prose only");
    std::fs::write(root.join("notes.txt"), LINE).expect("non-rust");

    let found = spellers(&SourceWalk::new(&root).skip_dir("tests"));
    std::fs::remove_dir_all(&root).expect("clean up");

    let unexpected: Vec<&String> = found.iter().filter(|rel| *rel != "listed.rs").collect();
    assert_eq!(
        unexpected,
        vec![&"blocks/offender.rs".to_string()],
        "expected exactly the planted offender; the whole scan saw {found:?}"
    );
}
