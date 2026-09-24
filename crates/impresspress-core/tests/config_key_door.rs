//! A config key is spelled once, in the `const` that declares it, and every
//! other site names that constant.
//!
//! A config variable's name is its wire identity: it is the `variables` row's
//! primary key, the environment variable an operator exports, the `name=` on
//! the admin settings form, and the string `blocks::config` looks up. Every
//! one of those has to be the same bytes. A key respelled at a call site makes
//! a rename a whole-repo find-and-replace that compiles perfectly if you miss
//! one and fails at runtime, silently, as "the setting has no effect" — a
//! config read that finds nothing is indistinguishable from a config nobody
//! set.
//!
//! The rule the crate already states (`config_vars.rs`: "Shared
//! (`WAFER_RUN_SHARED__`) variables are defined here — the single source of
//! truth. Block-scoped variables are declared in each block's `BlockInfo`") is
//! about the `ConfigVar`. This gate is the other half: the key's bytes live in
//! one `const NAME: &str = "…";` beside that declaration, and the `ConfigVar`,
//! every reader, every form and every test names the constant.
//!
//! ## What the gate checks
//!
//! 1. [`every_spelling_is_a_declaration`]: a key literal anywhere in the
//!    scanned tree is the value of a `const … : &str` — never an argument, a
//!    `json!` member, a match arm or a `name=` attribute. This is what stops a
//!    declaring module from also respelling some other module's key: a file
//!    may spell only what it declares.
//! 2. [`no_key_is_declared_twice`]: one constant per key across the crate.
//! 3. [`the_declaring_modules_are_exactly_the_listed_ones`] and
//!    [`the_fixture_declarers_are_exactly_the_listed_ones`]: which files hold
//!    a declaration is written down below, and a new one fails until it is
//!    added with its reason, a dead entry fails until it is removed.
//!
//! ## Test fixtures
//!
//! A test that needs a key no block declares — a retired key it pins as
//! absent, an undeclared key it stores ad hoc — names it in a `const` inside
//! its `#[cfg(test)]` module, and that module's file is listed in
//! [`DECLARES_A_TEST_FIXTURE_KEY`]. A test needing a *declared* key imports
//! the declaring constant like any other site; [`no_key_is_declared_twice`]
//! refuses a fixture `const` that respells one. Bare literals are allowed only
//! where this gate does not look: the crate's own `tests/` directory and the
//! block test directories under `src/**/tests/`, which hold fixtures pinned
//! to the wire name on purpose and never ship.
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
//! * A key assembled rather than written. `blocks/rate_limit.rs` builds
//!   `WAFER_RUN_SHARED__RATE_LIMIT_{CATEGORY}` and the OAuth pages build
//!   `IMPRESSPRESS__AUTH_UI__OAUTH_{PROVIDER}_CLIENT_ID` with `format!`
//!   legitimately; a bypass could do the same deliberately.
//! * A key inside a longer literal that is not quote-delimited — a
//!   form-encoded request body (`&key_var=IMPRESSPRESS__LLM__OPENAI_KEY&…`)
//!   or an error message naming the key an operator should set.
//! * `WAFER_RUN__<BLOCK>__*` keys — the auth block's `WAFER_RUN__AUTH__*`
//!   family and the `WAFER_RUN__{WEB,SQLITE,STRIPE}` block prefixes, which
//!   predate the `IMPRESSPRESS__` convention and are outside the pattern.
//! * The test directories named above, and the other workspace crates.
//!   `CARGO_MANIFEST_DIR` is this one, which is where `config_vars.rs` and
//!   every block's declaration lives.

use impresspress_core::test_support::source_scan::{
    strip_line_comments, strip_test_modules, SourceWalk,
};

/// Every file whose production code declares a config key.
///
/// These are the modules that own a key: `config_vars.rs` for the shared
/// vars and the deploy-time infrastructure keys, and for a block the module
/// holding its `ConfigVar`s (`blocks/<block>/config.rs` or its `mod.rs`) or,
/// for a key that is a record rather than a setting, the block module that
/// writes it (`blocks/dev/seed.rs`, `platform_state/variables.rs`). The rest
/// are not a block's module, each for a stated reason:
///
/// * `llm_target.rs` holds the llm block's max-token key because the vector
///   block names it too, and the two blocks do not share a cargo feature —
///   the reason that module exists.
/// * `builder/boot.rs`, `migration_helper.rs`, `prepared_plan.rs` and
///   `ui/assets.rs` declare `IMPRESSPRESS_*` infrastructure keys beside the
///   code that reads them.
const DECLARES_A_CONFIG_KEY: &[&str] = &[
    "blocks/auth/config.rs",
    "blocks/auth_ui/mod.rs",
    "blocks/dev/seed.rs",
    "blocks/email.rs",
    "blocks/files/cloud.rs",
    "blocks/legalpages/mod.rs",
    "blocks/llm/mod.rs",
    "blocks/products/config.rs",
    "blocks/signal/service.rs",
    "blocks/tickets/config.rs",
    "builder/boot.rs",
    "config_vars.rs",
    "llm_target.rs",
    "migration_helper.rs",
    "platform_state/variables.rs",
    "prepared_plan.rs",
    "ui/assets.rs",
];

/// Every file whose ONLY key declarations are test fixtures, inside a
/// `#[cfg(test)]` module, for keys no block declares.
///
/// * `blocks/admin/ops.rs` — a retired shared key an operator must be able
///   to delete.
/// * `blocks/config.rs` — undeclared keys `CONFIG_SET` stores ad hoc, one of
///   them sensitive by suffix.
/// * `blocks/llm/routes/providers.rs`, `blocks/llm/schema.rs` — `key_var`
///   names an admin chose for a provider, which no block declares by design.
///
/// `config_vars.rs`, `blocks/auth/config.rs` and `platform_state/variables.rs`
/// hold fixture constants of the same kind (the flags `get_bool` is tested on,
/// retired keys pinned as absent, a block-scoped row's key and prefix) and are
/// listed above because they also declare production keys.
const DECLARES_A_TEST_FIXTURE_KEY: &[&str] = &[
    "blocks/admin/ops.rs",
    "blocks/config.rs",
    "blocks/llm/routes/providers.rs",
    "blocks/llm/schema.rs",
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

/// One config key spelled as a literal: the key, and whether that spelling is
/// the value of a `const NAME: &str` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Spelling<'a> {
    key: &'a str,
    declares: bool,
    /// The source line holding the literal, for the failure message.
    line: &'a str,
}

/// Whether `before` — the code up to a key's opening quote — ends in
/// `const NAME: &str =` (or `&'static str`), across any whitespace and line
/// breaks rustfmt puts between them.
fn ends_in_const_str_binding(before: &str) -> bool {
    let Some(rest) = before.trim_end().strip_suffix('=') else {
        return false;
    };
    let rest = rest.trim_end();
    let Some(rest) = rest
        .strip_suffix("&str")
        .or_else(|| rest.strip_suffix("&'static str"))
    else {
        return false;
    };
    let Some(rest) = rest.trim_end().strip_suffix(':') else {
        return false;
    };
    let rest = rest.trim_end();
    let name_start = rest
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .map_or(0, |at| at + 1);
    let name = &rest[name_start..];
    !name.is_empty()
        && rest[..name_start]
            .trim_end()
            .strip_suffix("const")
            .is_some_and(|head| head.is_empty() || head.ends_with(char::is_whitespace))
}

/// Every config key `code` spells as a quoted literal.
///
/// Each `"` is treated as a possible opening quote and the key must run from
/// there to a closing `"` with nothing else between. Deliberately not a
/// scan that pairs quotes across the file: one raw string carrying a `"` of
/// its own (`r#"name="…""#`) desynchronises a pairing scan and hides every
/// literal after it.
///
/// An escaped quote closes a key too. `"name=\"WAFER_RUN_SHARED__APP_NAME\""`
/// is the non-raw spelling of the very `name=` attribute this gate says has to
/// agree with the constant, so a matcher that only accepted a bare `"` would
/// be blind to exactly the case it argues about.
///
/// A spelling `declares` only when it is a whole `const NAME: &str = "KEY";`:
/// the binding before the opening quote and a `;` straight after the closing
/// one. `const X: &str = "KEY".trim()`-style tricks and `static`s do not
/// count.
fn spellings(code: &str) -> Vec<Spelling<'_>> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(offset) = code[at..].find('"') {
        let quote = at + offset;
        let start = quote + 1;
        at = start;
        let tail = &code[start..];
        let end = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        let after = &tail[end..];
        let closed = after.starts_with('"') || after.starts_with("\\\"");
        let key = &tail[..end];
        if closed && is_config_key(key) {
            let line_start = code[..quote].rfind('\n').map_or(0, |at| at + 1);
            let line_end = code[start..].find('\n').map_or(code.len(), |at| start + at);
            found.push(Spelling {
                key,
                declares: after.starts_with("\";") && ends_in_const_str_binding(&code[..quote]),
                line: code[line_start..line_end].trim(),
            });
        }
    }
    found
}

/// One scanned file: its path and its code with full-line comments removed,
/// whole and with the `#[cfg(test)]` items dropped.
struct Scanned {
    rel: String,
    code: String,
    production: String,
}

fn scanned(walk: &SourceWalk) -> Vec<Scanned> {
    walk.collect()
        .into_iter()
        .map(|file| Scanned {
            rel: file.rel,
            code: strip_line_comments(&file.text),
            production: strip_line_comments(&strip_test_modules(&file.text)),
        })
        .collect()
}

/// Files whose production code declares a key, and files that declare keys
/// only inside `#[cfg(test)]` items.
fn declarers(files: &[Scanned]) -> (Vec<String>, Vec<String>) {
    let declares = |code: &str| spellings(code).iter().any(|spelling| spelling.declares);
    let mut production = Vec::new();
    let mut fixture_only = Vec::new();
    for file in files {
        if declares(&file.production) {
            production.push(file.rel.clone());
        } else if declares(&file.code) {
            fixture_only.push(file.rel.clone());
        }
    }
    (production, fixture_only)
}

/// `listed` and `found` as the two lists a failure message needs: entries
/// the tree has and the list does not, and entries the list has and the
/// tree does not.
fn list_drift(listed: &[&str], found: &[String]) -> (Vec<String>, Vec<String>) {
    let mut missing: Vec<String> = found
        .iter()
        .filter(|rel| !listed.contains(&rel.as_str()))
        .cloned()
        .collect();
    let mut dead: Vec<String> = listed
        .iter()
        .filter(|entry| !found.iter().any(|rel| rel == *entry))
        .map(|entry| (*entry).to_string())
        .collect();
    missing.sort();
    dead.sort();
    (missing, dead)
}

#[test]
fn every_spelling_is_a_declaration() {
    let files = scanned(&scan());
    let mut respelled: Vec<String> = files
        .iter()
        .flat_map(|file| {
            spellings(&file.code)
                .into_iter()
                .filter(|spelling| !spelling.declares)
                .map(|spelling| format!("{}: {}  ({})", file.rel, spelling.key, spelling.line))
                .collect::<Vec<_>>()
        })
        .collect();
    respelled.sort();
    assert!(
        respelled.is_empty(),
        "these sites spell a config key as a string literal instead of naming \
         the constant that declares it:\n{}\n\
         Shared keys are declared in `config_vars.rs`; a block's own keys are \
         declared beside its `ConfigVar`s (`blocks/<block>/config.rs` or its \
         `mod.rs`). Import the constant. A test that needs a key no block \
         declares names it in a `const` inside its test module.",
        respelled.join("\n")
    );
}

#[test]
fn no_key_is_declared_twice() {
    let files = scanned(&scan());
    let mut declared: Vec<(&str, &str)> = files
        .iter()
        .flat_map(|file| {
            spellings(&file.code)
                .into_iter()
                .filter(|spelling| spelling.declares)
                .map(|spelling| (spelling.key, file.rel.as_str()))
                .collect::<Vec<_>>()
        })
        .collect();
    declared.sort_unstable();
    let twice: Vec<String> = declared
        .windows(2)
        .filter(|pair| pair[0].0 == pair[1].0)
        .map(|pair| format!("{} in {} and {}", pair[0].0, pair[0].1, pair[1].1))
        .collect();
    assert!(
        twice.is_empty(),
        "a config key has one declaring constant; import it instead of \
         declaring another: {twice:?}"
    );
}

#[test]
fn the_declaring_modules_are_exactly_the_listed_ones() {
    let (production, _) = declarers(&scanned(&scan()));
    let (missing, dead) = list_drift(DECLARES_A_CONFIG_KEY, &production);
    assert!(
        missing.is_empty() && dead.is_empty(),
        "DECLARES_A_CONFIG_KEY is out of date.\n\
         declaring a key but not listed: {missing:?} — a key belongs to the \
         block whose `ConfigVar` declares it; move it there, or list the file \
         with the reason it cannot live there.\n\
         listed but declaring nothing: {dead:?} — drop the entry."
    );
}

#[test]
fn the_fixture_declarers_are_exactly_the_listed_ones() {
    let (_, fixture_only) = declarers(&scanned(&scan()));
    let (missing, dead) = list_drift(DECLARES_A_TEST_FIXTURE_KEY, &fixture_only);
    assert!(
        missing.is_empty() && dead.is_empty(),
        "DECLARES_A_TEST_FIXTURE_KEY is out of date.\n\
         declaring a fixture key but not listed: {missing:?} — if the key is \
         declared by a block, import that constant; if not, list the file \
         with what the fixture is for.\n\
         listed but declaring nothing: {dead:?} — drop the entry."
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
        // a key bound to a local, which is a spelling, not a declaration
        r#"let key = "WAFER_RUN_SHARED__APP_NAME";"#,
        // a match arm
        r#"match key { "WAFER_RUN_SHARED__APP_NAME" => 1, _ => 0 }"#,
    ] {
        let found = spellings(banned);
        assert_eq!(found.len(), 1, "the matcher stopped seeing: {banned}");
        assert!(
            !found[0].declares,
            "a use site was taken for a declaration: {banned}"
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
            spellings(allowed).is_empty(),
            "the matcher started refusing: {allowed} -> {:?}",
            spellings(allowed)
        );
    }
}

/// The declaration recogniser accepts every shape rustfmt writes a key
/// constant in, and nothing that merely resembles one.
#[test]
fn a_declaration_is_a_whole_const_str_binding() {
    for declaration in [
        r#"pub const APP_NAME_KEY: &str = "WAFER_RUN_SHARED__APP_NAME";"#,
        r#"const KEY: &str = "WAFER_RUN_SHARED__FLAG";"#,
        r#"pub(crate) const FROM: &'static str = "IMPRESSPRESS__EMAIL__FROM";"#,
        // rustfmt's wrap when the line is too long
        "pub(crate) const ALLOWED_RECIPIENT_PATTERNS: &str =\n    \"IMPRESSPRESS__EMAIL__ALLOWED_RECIPIENT_PATTERNS\";",
        "    pub(super) const X: &str = \"IMPRESSPRESS_REQUEST_LOG\";",
    ] {
        let found = spellings(declaration);
        assert_eq!(found.len(), 1, "not seen at all: {declaration}");
        assert!(found[0].declares, "not taken as a declaration: {declaration}");
    }
    for use_site in [
        // a static is not the constant a reader can import in a const context
        r#"static KEY: &str = "WAFER_RUN_SHARED__APP_NAME";"#,
        // the value is an expression built on the literal, not the literal
        r#"const KEY: &str = "WAFER_RUN_SHARED__APP_NAME".trim_ascii();"#,
        // the literal is an argument inside a const initialiser
        r#"const V: ConfigVar = ConfigVar::new("WAFER_RUN_SHARED__APP_NAME");"#,
        // a `let` with a type annotation
        r#"let key: &str = "WAFER_RUN_SHARED__APP_NAME";"#,
        // an identifier merely ending in `const`
        r#"notconst KEY: &str = "WAFER_RUN_SHARED__APP_NAME";"#,
    ] {
        let found = spellings(use_site);
        assert_eq!(found.len(), 1, "not seen at all: {use_site}");
        assert!(
            !found[0].declares,
            "a use site was taken for a declaration: {use_site}"
        );
    }
}

/// The *walk* reaches a planted offender, skips a block test module, reads
/// only Rust and sorts a declaration inside a test module from one outside it.
///
/// `the_matcher_recognises_the_keys_it_bans` proves the predicate works; it
/// says nothing about whether the walk ever opens a file. A root that moved
/// or an extension filter that broke would leave every gate above passing on
/// an empty scan — green, and blind to the one thing it exists to catch. The
/// floor on [`scan`] is the other half: it fails when the real tree comes
/// back short.
#[test]
fn the_walk_reaches_the_files_it_claims_to_scan() {
    const USE: &str = "fn f() { read(\"WAFER_RUN_SHARED__APP_NAME\"); }\n";

    let root = std::env::temp_dir().join(format!("config-key-door-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("blocks/tests")).expect("temp tree");
    std::fs::write(root.join("blocks/offender.rs"), USE).expect("offender");
    std::fs::write(root.join("blocks/tests/fixture.rs"), USE).expect("exempt test module");
    std::fs::write(
        root.join("declares.rs"),
        "pub const APP_NAME_KEY: &str = \"WAFER_RUN_SHARED__APP_NAME\";\n",
    )
    .expect("declaring module");
    std::fs::write(
        root.join("fixture.rs"),
        "pub fn f() {}\n\n#[cfg(test)]\nmod tests {\n    const FLAG: &str = \"WAFER_RUN_SHARED__FLAG\";\n}\n",
    )
    .expect("fixture declarer");
    std::fs::write(
        root.join("prose.rs"),
        "// \"WAFER_RUN_SHARED__APP_NAME\" named in a comment, not spelled\n",
    )
    .expect("prose only");
    std::fs::write(root.join("notes.txt"), USE).expect("non-rust");

    let files = scanned(&SourceWalk::new(&root).skip_dir("tests"));
    std::fs::remove_dir_all(&root).expect("clean up");

    let respelled: Vec<&str> = files
        .iter()
        .filter(|file| {
            spellings(&file.code)
                .iter()
                .any(|spelling| !spelling.declares)
        })
        .map(|file| file.rel.as_str())
        .collect();
    assert_eq!(
        respelled,
        vec!["blocks/offender.rs"],
        "expected exactly the planted offender"
    );
    let (production, fixture_only) = declarers(&files);
    assert_eq!(production, vec!["declares.rs".to_string()]);
    assert_eq!(fixture_only, vec!["fixture.rs".to_string()]);
}
