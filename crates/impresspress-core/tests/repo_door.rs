//! Every platform table has exactly one door.
//!
//! `src/platform_state/<module>.rs` owns one `impresspress__admin__*` table
//! each, and `src/blocks/<block>/repo/<module>.rs` owns its block's tables
//! the same way: the name, the column names and the row shape. Every other
//! module reaches the table through that module's functions, so a column is
//! spelled in one Rust file and a read cannot skip whatever the door
//! enforces (decoding, the seed hash gate, the single `assign` writer, the
//! one decode of `files.public`, the three writers of
//! `legalpages.documents.status`). The gate is a source scan because the
//! table name is necessarily reachable — a block's `collections(..)` and
//! `grants(..)` registrations name it — so nothing but a test can catch a
//! call site that names it directly. It generalises
//! `blocks/products/tests/repo_door_test.rs`; the block repos join it one PR
//! at a time.
//!
//! Scope: every `.rs` file under this crate's `src/`, with full-line
//! comments removed first — prose naming a table is not a query, and a
//! dozen doc comments describe these tables by name. Trailing comments on
//! code lines are kept, so nothing hides behind a `//` on the same line as
//! code. What this gate still does NOT cover, stated so it is not mistaken
//! for more than it is: other workspace crates (the CLI's `deploy_init` and
//! `native_wrap_grants` tests seed fixtures through `DatabaseService`, and
//! the Cloudflare adapter's `D1ConfigSource` reads the variables table on
//! its production config path; all of them name `platform_state::*::TABLE`
//! and decode through the row types, so they are consumers of this module's
//! public surface rather than bypasses of it, but no test in this crate can
//! see them), non-Rust sources (the migrations under
//! `blocks/admin/migrations/` define the tables), and the files on the
//! allowlists below — each listed individually with its reason, so a NEW
//! file naming a table fails the gate and has to justify itself here.

/// `(door, table, const, qualifier)` for every door this gate covers.
///
/// `door` names the door in the failure message and keys the two allowlists.
/// It is the owning module's name wherever a module owns one table; where a
/// module owns two (`files::repo::shares` owns the share rows and their
/// child access log, because a log row is meaningless without its share)
/// each table is its own door, so an exemption for one is not an exemption
/// for the other.
///
/// `const` is the path fragment the second scan looks for — `<module>::TABLE`
/// for a module's primary table, `<module>::<NAME>_TABLE` for a second one.
///
/// `qualifier` is the path fragment a file must ALSO contain for that token
/// to be attributed to this door. It is what keeps a block's own same-named
/// repo module out of the match: products has a `repo::variables::TABLE`, so
/// `variables::TABLE` counts as the platform door only when the file also
/// names `platform_state`. For the auth users door the fragment is `auth`
/// and for the files doors it is `files`, which every path to
/// `blocks::<block>::repo::<module>` necessarily spells.
const TABLES: &[(&str, &str, &str, &str)] = &[
    (
        "variables",
        "impresspress__admin__variables",
        "variables::TABLE",
        "platform_state",
    ),
    (
        "block_settings",
        "impresspress__admin__block_settings",
        "block_settings::TABLE",
        "platform_state",
    ),
    (
        "wrap_grants",
        "impresspress__admin__wrap_grants",
        "wrap_grants::TABLE",
        "platform_state",
    ),
    (
        "request_logs",
        "impresspress__admin__request_logs",
        "request_logs::TABLE",
        "platform_state",
    ),
    (
        "user_roles",
        "impresspress__admin__user_roles",
        "user_roles::TABLE",
        "platform_state",
    ),
    ("users", "wafer_run__auth__users", "users::TABLE", "auth"),
    (
        "buckets",
        "impresspress__files__buckets",
        "buckets::TABLE",
        "files",
    ),
    (
        "objects",
        "impresspress__files__objects",
        "objects::TABLE",
        "files",
    ),
    (
        "shares",
        "impresspress__files__cloud_shares",
        "shares::TABLE",
        "files",
    ),
    (
        "share_access_logs",
        "impresspress__files__cloud_access_logs",
        "shares::ACCESS_LOGS_TABLE",
        "files",
    ),
    (
        "quota",
        "impresspress__files__cloud_quotas",
        "quota::TABLE",
        "files",
    ),
    (
        "views",
        "impresspress__files__views",
        "views::TABLE",
        "files",
    ),
    (
        "documents",
        "impresspress__legalpages__documents",
        "documents::TABLE",
        "legalpages",
    ),
];

fn crate_sources() -> Vec<(String, String)> {
    let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read source dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, std::fs::read_to_string(&path).expect("read source")));
            }
        }
    }
    assert!(
        out.len() > 100,
        "the scan walks the whole crate; {} files means it lost its way",
        out.len()
    );
    out
}

/// `src` without its full-line comments (`//`, `///`, `//!` lines). A
/// trailing comment on a code line stays, so it is still scanned.
fn code_only(src: &str) -> String {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `path` (relative to `src`) is one of `allowlist`'s entries.
/// Exact matches only — no directory prefixes, so an allowlist can never
/// exempt a file that does not exist yet.
fn matches_allowlist(path: &str, allowlist: &[&str]) -> bool {
    allowlist.contains(&path)
}

/// Files allowed to spell a table's literal name, per table. Each entry is
/// a place the name is *defined* or a test fixture that must pin the wire
/// name rather than read it back from the constant it is testing.
const LITERAL_ALLOWED: &[(&str, &[&str])] = &[
    (
        "variables",
        &[
            // the door itself
            "platform_state/variables.rs",
            // the migration runner's tests assert that the embedded DDL
            // carries the index names, which are derived from the table
            // name; the `.sql` files next to it define the table
            "blocks/admin/migrations/mod.rs",
            // the KV row cache's tests pin the wire names its cache keys
            // are derived from — reading them back from the constant would
            // make the test tautological
            "cache_key.rs",
            // a Postgres error-message fixture (`column "block" of relation
            // "…" already exists`) for the duplicate-column detector
            "migration_helper.rs",
            // a guest capability fixture: a sandbox block declaring a
            // foreign table must be refused, and this is the foreign table
            "blocks/dev/validation.rs",
        ],
    ),
    (
        "block_settings",
        &["platform_state/block_settings.rs", "cache_key.rs"],
    ),
    (
        "wrap_grants",
        &["platform_state/wrap_grants.rs", "cache_key.rs"],
    ),
    ("request_logs", &["platform_state/request_logs.rs"]),
    ("user_roles", &["platform_state/user_roles.rs"]),
    // The files doors. Each is its own `repo/<module>.rs` and nothing else,
    // with one exception on the objects table: the WRAP-grant loader's
    // fixture seeds a grant whose target IS a table name on the wire, and
    // resolving it back through `files::repo::objects::TABLE` would make the
    // platform-state test depend on the files block to say what it is
    // testing.
    ("buckets", &["blocks/files/repo/buckets.rs"]),
    (
        "objects",
        &[
            "blocks/files/repo/objects.rs",
            "platform_state/wrap_grants.rs",
        ],
    ),
    ("shares", &["blocks/files/repo/shares.rs"]),
    ("share_access_logs", &["blocks/files/repo/shares.rs"]),
    ("quota", &["blocks/files/repo/quota.rs"]),
    ("views", &["blocks/files/repo/views.rs"]),
    // The legalpages door. Nothing but the door itself: the block declares
    // no `collections(..)` and no `grants(..)` (it owns the one table it
    // touches, so WRAP has nothing to cross-check), which is what leaves this
    // list at one entry.
    ("documents", &["blocks/legalpages/repo/documents.rs"]),
    (
        "users",
        &[
            // the door itself
            "blocks/auth/repo/users.rs",
            // `auth_grants()` spells its grant targets as literals on
            // purpose: the WRAP audit script's const-resolver follows
            // top-level `super::NAME` paths only, not `repo::users::TABLE`
            // (the reason is written out above `auth_grants`)
            "blocks/auth/service.rs",
            // the migration-runner tests assert against the DDL the `.sql`
            // files define; reading the name back from the constant they are
            // testing would make them tautological
            "blocks/auth/migrations/mod.rs",
            // `seed_auth_user` — the ONE raw-SQL users fixture in the crate
            // (a test needs a user under a caller-chosen id that its own
            // authenticated `Message` names; `users::insert` mints a UUID) —
            // plus the WRAP tests whose grant target IS the wire name
            "test_support.rs",
            // the KV row cache classifies tables by wire name; its tests pin
            // the name rather than read it back from the constant
            "cache_key.rs",
            // the fail-closed diagnostic on the router's auth_version read
            // names the grant an operator has to go and add
            "crypto.rs",
        ],
    ),
];

#[test]
fn only_the_door_names_a_platform_table() {
    let sources: Vec<(String, String)> = crate_sources()
        .into_iter()
        .map(|(path, src)| (path, code_only(&src)))
        .collect();
    for (door, literal, _ident, _qualifier) in TABLES {
        let allowed = LITERAL_ALLOWED
            .iter()
            .find(|(m, _)| m == door)
            .map(|(_, files)| *files)
            .unwrap_or(&[]);
        let offenders: Vec<&String> = sources
            .iter()
            .filter(|(path, _)| !matches_allowlist(path, allowed))
            .filter(|(_, src)| src.contains(literal))
            .map(|(path, _)| path)
            .collect();
        assert!(
            offenders.is_empty(),
            "these files name `{literal}` directly and so bypass \
             the `{door}` door; route them through its functions: {offenders:?}"
        );
    }
}

/// The literal scan catches a call site that spells the name by hand. The
/// likelier mistake is naming the table through the constant — handing
/// `platform_state::variables::TABLE` to `db::list_all` — which compiles
/// cleanly because the constant is `pub` for `blocks/admin`'s
/// `collections(..)` registration. This scan closes that gap: a file that
/// imports `platform_state` and names `<module>::TABLE` must be on the list
/// below, each entry justified on why it is not a query around the door.
///
/// The `platform_state` condition is what keeps a block's own
/// `repo::variables::TABLE` (products has one) out of the match. The doors
/// themselves are not listed: inside `platform_state/<module>.rs` the
/// constant is plain `TABLE`, never `<module>::TABLE`.
const IDENT_ALLOWED: &[(&str, &[&str])] = &[
    (
        "variables",
        &[
            // the KV row cache classifies tables by name; it never queries
            "cache_key.rs",
            // the config-snapshot invalidation predicate compares names
            "config_generation.rs",
            // `BlockInfo::collections(..)` / `grants(..)` are advisory
            // declarations for WRAP and the admin database explorer
            "blocks/admin/mod.rs",
            // the export allowlist/exclusion bookkeeping; its reads go
            // through a generic `db::list_all(ctx, table, ..)` over the
            // allowlist and its import through `seed::import`, and the dev
            // block grants itself those tables (see the audit pragma there)
            "blocks/dev/data_snapshot.rs",
        ],
    ),
    (
        "block_settings",
        &[
            "cache_key.rs",
            "config_generation.rs",
            "blocks/admin/mod.rs",
            // names the table only to aim the fault injector
            // (`FailingDbOpContext`) at it in the toggle handler's tests
            "blocks/admin/pages/blocks.rs",
            "blocks/dev/data_snapshot.rs",
        ],
    ),
    (
        "wrap_grants",
        &[
            "cache_key.rs",
            "blocks/admin/mod.rs",
            "blocks/dev/data_snapshot.rs",
        ],
    ),
    (
        "request_logs",
        &[
            // the queued audit row carries the table name for the platform
            // drain (`create_many`) to persist off the response path; the
            // inline path calls `request_logs::insert`
            "pipeline.rs",
            "blocks/admin/mod.rs",
            "blocks/dev/data_snapshot.rs",
        ],
    ),
    (
        "user_roles",
        &["blocks/admin/mod.rs", "blocks/dev/data_snapshot.rs"],
    ),
    (
        "users",
        &[
            // the export allowlist/exclusion bookkeeping; its reads go
            // through a generic `db::list_all(ctx, table, ..)` over the
            // allowlist and its import through `seed::import`
            "blocks/dev/data_snapshot.rs",
        ],
    ),
    // The files block's two categories, both of which the admin doors above
    // are already exempted under:
    //
    // 1. `blocks/files/mod.rs` — `BlockInfo::collections(..)`. Advisory
    //    declarations for WRAP and the admin database explorer, the same
    //    reason `blocks/admin/mod.rs` is listed for the platform tables.
    //    Every files door needs it; there is no way to declare a collection
    //    without naming it.
    // 2. A test naming the table only to aim `FailingDbOpContext` at it, so
    //    the fault lands on the query under test and not on some other
    //    table's. The same reason `blocks/admin/pages/blocks.rs` is listed.
    //    These are not queries around the door; the door is what runs.
    (
        "buckets",
        &[
            "blocks/files/mod.rs",
            // `("database.delete_where", buckets::TABLE)` — the bucket-delete
            // handler's two compensating-failure tests
            "blocks/files/storage/buckets.rs",
        ],
    ),
    (
        "objects",
        &[
            "blocks/files/mod.rs",
            // `("database.delete_where"/"delete_where_count", objects::TABLE)`
            // — the object-delete metadata-cleanup failure test
            "blocks/files/storage/objects.rs",
            // `("database.sum", objects::TABLE)` — the quota fail-closed test
            "blocks/files/quota.rs",
            // `("database.sum", objects::TABLE)` — the same, through the
            // `/b/cloudstorage/quota` handler
            "blocks/files/cloud.rs",
        ],
    ),
    (
        "shares",
        &[
            "blocks/files/mod.rs",
            // `("database.get", shares::TABLE)` — the share-delete
            // authorization test: a failed ownership read must stop the
            // request rather than skip the check
            "blocks/files/cloud.rs",
        ],
    ),
    (
        "documents",
        // Category 2 only. The block declares no `collections(..)`, so there
        // is no non-test file that has to name the table at all; this entry
        // is the four `FailingDbOpContext` fixtures in the block's
        // `write_loss_tests`, which name the table so the injected fault
        // lands on the query under test. Same reason
        // `blocks/admin/pages/blocks.rs` is listed above.
        &["blocks/legalpages/mod.rs"],
    ),
    ("share_access_logs", &["blocks/files/mod.rs"]),
    (
        "quota",
        &[
            "blocks/files/mod.rs",
            // `("database.list", quota::TABLE)` — the quota fail-closed test
            "blocks/files/quota.rs",
        ],
    ),
    ("views", &["blocks/files/mod.rs"]),
];

#[test]
fn only_the_allowlist_names_a_platform_table_via_the_const() {
    let sources: Vec<(String, String)> = crate_sources()
        .into_iter()
        .map(|(path, src)| (path, code_only(&src)))
        .collect();
    for (door, _, ident, qualifier) in TABLES {
        let allowed = IDENT_ALLOWED
            .iter()
            .find(|(m, _)| m == door)
            .map(|(_, files)| *files)
            .unwrap_or(&[]);
        let offenders: Vec<&String> = sources
            .iter()
            .filter(|(path, _)| !matches_allowlist(path, allowed))
            .filter(|(_, src)| src.contains(qualifier) && src.contains(ident))
            .map(|(path, _)| path)
            .collect();
        assert!(
            offenders.is_empty(),
            "these files name the table via `{ident}` instead of calling a \
             `{door}` repo function: {offenders:?}"
        );
    }
}

/// An allowlist entry naming a file that no longer names the table is a
/// dead exemption: it silently pre-approves whatever that file does next.
#[test]
fn no_allowlist_entry_is_dead() {
    let sources: Vec<(String, String)> = crate_sources()
        .into_iter()
        .map(|(path, src)| (path, code_only(&src)))
        .collect();
    for (door, literal, ident, _qualifier) in TABLES {
        for (m, files) in LITERAL_ALLOWED {
            if m != door {
                continue;
            }
            for entry in *files {
                assert!(
                    sources
                        .iter()
                        .any(|(path, src)| path == entry && src.contains(literal)),
                    "`{entry}` is allowlisted for the `{literal}` literal but no longer \
                     names it; drop the entry rather than leaving a standing exemption"
                );
            }
        }
        for (m, files) in IDENT_ALLOWED {
            if m != door {
                continue;
            }
            for entry in *files {
                assert!(
                    sources
                        .iter()
                        .any(|(path, src)| path == entry && src.contains(ident)),
                    "`{entry}` is allowlisted for `{ident}` but no longer names it; \
                     drop the entry rather than leaving a standing exemption"
                );
            }
        }
    }
}

/// The old names are gone: `admin_schema.rs` and the `blocks::admin`
/// re-exports (`BLOCK_SETTINGS_TABLE`, `WRAP_GRANTS_TABLE`,
/// `REQUEST_LOGS_TABLE`, `USER_ROLES_TABLE`, `admin::VARIABLES_TABLE`). A
/// file that still imports one would compile only by redefining it, which
/// is the same bypass wearing the old name. (`VARIABLES_TABLE` on its own
/// is not banned: products aliases its own `repo::variables::TABLE` to it.)
#[test]
fn the_old_admin_table_names_are_gone() {
    let sources: Vec<(String, String)> = crate_sources()
        .into_iter()
        .map(|(path, src)| (path, code_only(&src)))
        .collect();
    for old in [
        "admin_schema::",
        "mod admin_schema",
        "BLOCK_SETTINGS_TABLE",
        "WRAP_GRANTS_TABLE",
        "REQUEST_LOGS_TABLE",
        "USER_ROLES_TABLE",
        "admin::VARIABLES_TABLE",
    ] {
        let offenders: Vec<&String> = sources
            .iter()
            .filter(|(_, src)| src.contains(old))
            .map(|(path, _)| path)
            .collect();
        assert!(
            offenders.is_empty(),
            "`{old}` still referenced in {offenders:?}"
        );
    }
}

#[test]
fn code_only_drops_full_line_comments_and_keeps_trailing_ones() {
    let src = "//! doc\nlet a = 1; // trailing impresspress__admin__variables\n/// more\n  // indented\nlet b = 2;\n";
    assert_eq!(
        code_only(src),
        "let a = 1; // trailing impresspress__admin__variables\nlet b = 2;"
    );
}
