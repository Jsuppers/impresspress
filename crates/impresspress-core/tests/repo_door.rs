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
/// `consts` are the path fragments the second scan looks for. `<module>::TABLE`
/// for a module's primary table and `<module>::<NAME>_TABLE` for a second one
/// cover a caller that spells the whole path; a door whose constant is
/// re-exported under an alias (`products/mod.rs` hands `blocks::dev` a
/// `<NAME>_TABLE` alias for every collection the block declares) lists the
/// alias too, because `use blocks::products::OFFERS_TABLE` is a call site the
/// path spelling never sees.
///
/// `qualifier` is the path fragment a file must ALSO contain for that token
/// to be attributed to this door. It is what keeps a block's own same-named
/// repo module out of the match: products has a `repo::variables::TABLE`, so
/// `variables::TABLE` counts as the platform door only when the file also
/// names `platform_state`. For the auth users door the fragment is `auth`
/// and for the files doors it is `files`, which every path to
/// `blocks::<block>::repo::<module>` necessarily spells.
const TABLES: &[(&str, &str, &[&str], &str)] = &[
    (
        "variables",
        "impresspress__admin__variables",
        &["variables::TABLE"],
        "platform_state",
    ),
    (
        "block_settings",
        "impresspress__admin__block_settings",
        &["block_settings::TABLE"],
        "platform_state",
    ),
    (
        "wrap_grants",
        "impresspress__admin__wrap_grants",
        &["wrap_grants::TABLE"],
        "platform_state",
    ),
    (
        "request_logs",
        "impresspress__admin__request_logs",
        &["request_logs::TABLE"],
        "platform_state",
    ),
    (
        "user_roles",
        "impresspress__admin__user_roles",
        &["user_roles::TABLE"],
        "platform_state",
    ),
    ("users", "wafer_run__auth__users", &["users::TABLE"], "auth"),
    // The three auth doors this PR adds. `sessions` and `tokens` are the
    // pair B12 re-keyed and wired retention for; `maintenance` is the
    // sweeper's singleton, new in migration 012.
    (
        "sessions",
        "wafer_run__auth__sessions",
        &["sessions::TABLE"],
        "auth",
    ),
    (
        "refresh_tokens",
        "wafer_run__auth__tokens",
        &["tokens::TABLE"],
        "auth",
    ),
    (
        "auth_maintenance",
        "wafer_run__auth__maintenance",
        &["maintenance::TABLE"],
        "auth",
    ),
    (
        "buckets",
        "impresspress__files__buckets",
        &["buckets::TABLE"],
        "files",
    ),
    (
        "objects",
        "impresspress__files__objects",
        &["objects::TABLE"],
        "files",
    ),
    (
        "shares",
        "impresspress__files__cloud_shares",
        &["shares::TABLE"],
        "files",
    ),
    (
        "share_access_logs",
        "impresspress__files__cloud_access_logs",
        &["shares::ACCESS_LOGS_TABLE"],
        "files",
    ),
    (
        "quota",
        "impresspress__files__cloud_quotas",
        &["quota::TABLE"],
        "files",
    ),
    (
        "views",
        "impresspress__files__views",
        &["views::TABLE"],
        "files",
    ),
    (
        "documents",
        "impresspress__legalpages__documents",
        &["documents::TABLE"],
        "legalpages",
    ),
    // The products doors. Every table the block declares, each owned by its
    // own `repo/<module>.rs`. The second const on most rows is the alias
    // `blocks/products/mod.rs` re-exports for `blocks::dev::data_snapshot`'s
    // closed-list bookkeeping; `purchases` and `subscriptions` name their
    // constants that way inside the door itself, which is why those doors
    // appear on their own IDENT list below.
    (
        "products",
        "impresspress__products__products",
        &["products::TABLE"],
        "products",
    ),
    (
        "product_versions",
        "impresspress__products__product_versions",
        &["product_versions::TABLE", "PRODUCT_VERSIONS_TABLE"],
        "products",
    ),
    (
        "offers",
        "impresspress__products__offers",
        &["offers::TABLE", "OFFERS_TABLE"],
        "products",
    ),
    (
        "offer_components",
        "impresspress__products__offer_components",
        &["offer_components::TABLE", "OFFER_COMPONENTS_TABLE"],
        "products",
    ),
    (
        "payment_links",
        "impresspress__products__payment_links",
        &["payment_links::TABLE", "PAYMENT_LINKS_TABLE"],
        "products",
    ),
    (
        "checkout_presets",
        "impresspress__products__checkout_presets",
        &["checkout_presets::TABLE", "CHECKOUT_PRESETS_TABLE"],
        "products",
    ),
    (
        "purchases",
        "impresspress__products__purchases",
        &["PURCHASES_TABLE"],
        "products",
    ),
    (
        "line_items",
        "impresspress__products__line_items",
        &["LINE_ITEMS_TABLE"],
        "products",
    ),
    (
        "refunds",
        "impresspress__products__refunds",
        &["refunds::TABLE", "REFUNDS_TABLE"],
        "products",
    ),
    (
        "disputes",
        "impresspress__products__disputes",
        &["disputes::TABLE", "DISPUTES_TABLE"],
        "products",
    ),
    (
        "entitlements",
        "impresspress__products__entitlements",
        &["entitlements::TABLE", "ENTITLEMENTS_TABLE"],
        "products",
    ),
    (
        "subscriptions",
        "impresspress__products__subscriptions",
        &["SUBSCRIPTIONS_TABLE"],
        "products",
    ),
    (
        "subscription_items",
        "impresspress__products__subscription_items",
        &["subscription_items::TABLE", "SUBSCRIPTION_ITEMS_TABLE"],
        "products",
    ),
    (
        "seller_accounts",
        "impresspress__products__seller_accounts",
        &["seller_accounts::TABLE", "SELLER_ACCOUNTS_TABLE"],
        "products",
    ),
    (
        "provider_operations",
        "impresspress__products__provider_operations",
        &["provider_operations::TABLE", "PROVIDER_OPERATIONS_TABLE"],
        "products",
    ),
    (
        "stripe_events",
        "impresspress__products__stripe_events",
        &["stripe_events::TABLE", "STRIPE_EVENTS_TABLE"],
        "products",
    ),
    (
        "products_variables",
        "impresspress__products__variables",
        &["variables::TABLE", "PRODUCTS_VARIABLES_TABLE"],
        "products",
    ),
    (
        "groups",
        "impresspress__products__groups",
        &["groups::TABLE", "GROUPS_TABLE"],
        "products",
    ),
    (
        "types",
        "impresspress__products__types",
        &["types::TABLE", "TYPES_TABLE"],
        "products",
    ),
    (
        "group_templates",
        "impresspress__products__group_templates",
        &["group_templates::TABLE", "GROUP_TEMPLATES_TABLE"],
        "products",
    ),
    (
        "product_templates",
        "impresspress__products__product_templates",
        &["product_templates::TABLE", "PRODUCT_TEMPLATES_TABLE"],
        "products",
    ),
    (
        "llm_settings",
        "impresspress__llm__settings",
        &["settings::TABLE"],
        "llm",
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
    // The auth session / refresh-token / maintenance doors. Same two
    // categories the `users` door above is exempted under, and nothing else:
    // `auth_grants()` spells its grant targets as literals so the WRAP audit
    // script's const-resolver can follow them, and the migration runner's own
    // tests assert against the DDL the `.sql` files next to them define.
    (
        "sessions",
        &[
            "blocks/auth/repo/sessions.rs",
            "blocks/auth/service.rs",
            "blocks/auth/migrations/mod.rs",
        ],
    ),
    (
        "refresh_tokens",
        &["blocks/auth/repo/tokens.rs", "blocks/auth/service.rs"],
    ),
    // Nothing but the door: the sweeper's singleton is granted through
    // auth-ui's existing `wafer_run__auth__*` wildcard, so no grant literal
    // names it, and the migration tests do not assert on its DDL.
    ("auth_maintenance", &["blocks/auth/repo/maintenance.rs"]),
    // The products doors. Three categories, and nothing else:
    //
    // 1. `blocks/products/repo/<module>.rs` — the door itself, where the
    //    name is defined.
    // 2. `blocks/products/migrations/mod.rs` — the migration runner's own
    //    tests, which necessarily work below the repo layer (migration 020
    //    repairs a row the repo layer can no longer produce), and which
    //    assert against the DDL the `.sql` files next to them define.
    // 3. `blocks/products/tests/*.rs` — fixture setup that seeds rows the
    //    repo layer would not write (soft-deleted products, a pre-migration
    //    stripe event) and asserts on the raw stored row.
    //
    // `blocks/products/stripe.rs` is the one production file on this list,
    // for the reason its door already documents: `repo/stripe_events.rs`
    // owns the name only, and the webhook pipeline that is the table's sole
    // reader and writer predates the convention. Moving that pipeline behind
    // the door is a separate change; the entry says so rather than hiding it.
    (
        "products",
        &[
            "blocks/products/repo/products.rs",
            "blocks/products/migrations/mod.rs",
            "blocks/products/tests/handler_tests.rs",
            "blocks/products/tests/page_link_tests.rs",
        ],
    ),
    (
        "product_versions",
        &[
            "blocks/products/repo/product_versions.rs",
            "blocks/products/migrations/mod.rs",
        ],
    ),
    (
        "offers",
        &[
            "blocks/products/repo/offers.rs",
            "blocks/products/migrations/mod.rs",
            "blocks/products/tests/offer_management_tests.rs",
            "blocks/products/tests/repo_tests.rs",
        ],
    ),
    (
        "offer_components",
        &[
            "blocks/products/repo/offer_components.rs",
            "blocks/products/migrations/mod.rs",
        ],
    ),
    (
        "payment_links",
        &[
            "blocks/products/repo/payment_links.rs",
            "blocks/products/migrations/mod.rs",
        ],
    ),
    (
        "checkout_presets",
        &[
            "blocks/products/repo/checkout_presets.rs",
            "blocks/products/migrations/mod.rs",
        ],
    ),
    (
        "purchases",
        &[
            "blocks/products/repo/purchases.rs",
            "blocks/products/tests/handler_tests.rs",
            "blocks/products/tests/purchase_tests.rs",
            "blocks/products/tests/repo_tests.rs",
            "blocks/products/tests/seller_governance_tests.rs",
            "blocks/products/tests/stripe_tests.rs",
        ],
    ),
    (
        "line_items",
        &[
            "blocks/products/repo/purchases.rs",
            "blocks/products/tests/handler_tests.rs",
            "blocks/products/tests/purchase_tests.rs",
            "blocks/products/tests/stripe_tests.rs",
        ],
    ),
    (
        "refunds",
        &[
            "blocks/products/repo/refunds.rs",
            "blocks/products/migrations/mod.rs",
        ],
    ),
    (
        "disputes",
        &[
            "blocks/products/repo/disputes.rs",
            "blocks/products/migrations/mod.rs",
        ],
    ),
    (
        "entitlements",
        &[
            "blocks/products/repo/entitlements.rs",
            "blocks/products/migrations/mod.rs",
        ],
    ),
    (
        "subscriptions",
        &[
            "blocks/products/repo/subscriptions.rs",
            "blocks/products/tests/repo_tests.rs",
        ],
    ),
    (
        "subscription_items",
        &[
            "blocks/products/repo/subscription_items.rs",
            "blocks/products/migrations/mod.rs",
        ],
    ),
    (
        "seller_accounts",
        &[
            "blocks/products/repo/seller_accounts.rs",
            "blocks/products/migrations/mod.rs",
        ],
    ),
    (
        "provider_operations",
        &[
            "blocks/products/repo/provider_operations.rs",
            "blocks/products/migrations/mod.rs",
        ],
    ),
    (
        "stripe_events",
        &[
            "blocks/products/repo/stripe_events.rs",
            "blocks/products/migrations/mod.rs",
            // category 4: the webhook pipeline that predates the convention
            "blocks/products/stripe.rs",
            "blocks/products/tests/handler_tests.rs",
            "blocks/products/tests/stripe_tests.rs",
        ],
    ),
    ("products_variables", &["blocks/products/repo/variables.rs"]),
    ("groups", &["blocks/products/repo/groups.rs"]),
    ("types", &["blocks/products/repo/types.rs"]),
    (
        "group_templates",
        &["blocks/products/repo/group_templates.rs"],
    ),
    (
        "product_templates",
        &[
            "blocks/products/repo/product_templates.rs",
            "blocks/products/migrations/mod.rs",
        ],
    ),
    // The llm settings door. The door itself plus the migration runner's
    // own tests, which assert that the embedded DDL creates the table and
    // its index — reading the name back from the constant they are testing
    // would make them tautological. The block declares no `collections(..)`
    // (its schema is materialised by its migrations, and `mod.rs` says so),
    // so no other non-test file has to name the table.
    (
        "llm_settings",
        &[
            "blocks/llm/repo/settings.rs",
            "blocks/llm/migrations/mod.rs",
        ],
    ),
];

#[test]
fn only_the_door_names_a_platform_table() {
    let sources: Vec<(String, String)> = crate_sources()
        .into_iter()
        .map(|(path, src)| (path, code_only(&src)))
        .collect();
    for (door, literal, _consts, _qualifier) in TABLES {
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
    // The auth doors B12 adds. Two categories, both already established
    // above: the export allowlist's closed-list bookkeeping, and tests naming
    // a table only to aim `FailingDbOpContext` at it so the injected fault
    // lands on the query under test.
    (
        "sessions",
        &[
            "blocks/dev/data_snapshot.rs",
            // `("database.delete_where_count", sessions::TABLE)` — logout's
            // "a failed session-row delete is not a successful logout" test
            "blocks/auth_ui/api/logout.rs",
        ],
    ),
    (
        "refresh_tokens",
        &[
            "blocks/dev/data_snapshot.rs",
            // Five `FailingDbOpContext` fixtures across the flows that revoke
            // refresh rows: logout, password change, password reset, refresh
            // rotation, and the userportal per-device revoke.
            "blocks/auth_ui/api/logout.rs",
            "blocks/auth_ui/api/change_password.rs",
            "blocks/auth_ui/api/reset_password.rs",
            "blocks/auth_ui/api/refresh.rs",
            "blocks/userportal/pages/sessions.rs",
            // `("database.delete_where_count", tokens::TABLE)` — the sweep's
            // "one failing table is named and the others still run" test
            "blocks/auth/maintenance.rs",
        ],
    ),
    (
        "auth_maintenance",
        &[
            // The export decision: the sweep's throttle stamp is scoped to
            // the instance that wrote it, so `TABLE_EXCLUDED` names it. The
            // list is closed, so every table has to be named somewhere in it.
            "blocks/dev/data_snapshot.rs",
            // `("database.get", maintenance::TABLE)` — the throttle's
            // "an unreadable stamp skips rather than sweeps" test
            "blocks/auth/maintenance.rs",
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
    // The products doors. Four categories:
    //
    // 1. `blocks/products/mod.rs` — `BlockInfo::collections(..)` plus the
    //    curated `block-dev`-gated re-export list that lets
    //    `blocks::dev::data_snapshot` name every collection this block
    //    declares without retyping a literal. Advisory declarations, not
    //    queries; the same reason `blocks/admin/mod.rs` and
    //    `blocks/files/mod.rs` are listed above. Every products door needs
    //    it.
    // 2. `blocks/dev/data_snapshot.rs` — the export allowlist/exclusion
    //    bookkeeping and the `DataSnapshot` JSON keys. Its reads go through
    //    a generic `db::list_all(ctx, table, ..)` over the allowlist and its
    //    writes through `seed::import`; already listed for the platform
    //    doors above for exactly this.
    // 3. `blocks/products/tests/*.rs` — fixtures that seed or assert on raw
    //    rows, and fault injectors (`FailingDbOpContext`) that name the
    //    table so the injected failure lands on the query under test.
    // 4. Two production files that pass the constant to a shared helper
    //    rather than building a query on it: `handlers/group.rs` and
    //    `handlers/types.rs` hand `repo::{groups,types}::TABLE` to
    //    `blocks/crud.rs`'s generic `list_page` / `create_record` /
    //    `update_record` / `delete_record` / `verify_owner` /
    //    `{get,update,delete}_owned`, whose table name always comes from the
    //    caller (the same property that made `crud.rs` carry an
    //    `// audit-allow-file:` pragma for the WRAP audit). Folding those
    //    into per-table repo functions moves the HTTP error mapping `crud`
    //    encapsulates and is a separate change. `blocks/products/stripe.rs`
    //    is the fifth, for the reason `repo/stripe_events.rs` documents.
    //
    // `repo/purchases.rs` and `repo/subscriptions.rs` are on their own
    // lists: their constants are named `PURCHASES_TABLE`,
    // `LINE_ITEMS_TABLE` and `SUBSCRIPTIONS_TABLE` rather than `TABLE`, so
    // the door's own uses match the scan.
    (
        "products",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/handler_tests.rs",
            "blocks/products/tests/offer_management_tests.rs",
            "blocks/products/tests/offer_pricing_tests.rs",
            "blocks/products/tests/repo_tests.rs",
            "blocks/products/tests/seller_governance_tests.rs",
            "blocks/products/tests/stripe_tests.rs",
        ],
    ),
    (
        "product_versions",
        &["blocks/products/mod.rs", "blocks/dev/data_snapshot.rs"],
    ),
    (
        "offers",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/handler_tests.rs",
            "blocks/products/tests/offer_pricing_tests.rs",
            "blocks/products/tests/stripe_tests.rs",
        ],
    ),
    (
        "offer_components",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/offer_pricing_tests.rs",
        ],
    ),
    (
        "payment_links",
        &["blocks/products/mod.rs", "blocks/dev/data_snapshot.rs"],
    ),
    (
        "checkout_presets",
        &["blocks/products/mod.rs", "blocks/dev/data_snapshot.rs"],
    ),
    (
        "purchases",
        &[
            "blocks/products/repo/purchases.rs",
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/page_link_tests.rs",
            "blocks/products/tests/provider_tests.rs",
            "blocks/products/tests/storefront_tests.rs",
            "blocks/products/tests/stripe_tests.rs",
        ],
    ),
    (
        "line_items",
        &[
            "blocks/products/repo/purchases.rs",
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
        ],
    ),
    (
        "refunds",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/purchase_tests.rs",
        ],
    ),
    (
        "disputes",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/handler_tests.rs",
            "blocks/products/tests/purchase_tests.rs",
        ],
    ),
    (
        "entitlements",
        &["blocks/products/mod.rs", "blocks/dev/data_snapshot.rs"],
    ),
    (
        "subscriptions",
        &[
            "blocks/products/repo/subscriptions.rs",
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/handler_tests.rs",
            "blocks/products/tests/status_enum_tests.rs",
            "blocks/products/tests/stripe_tests.rs",
        ],
    ),
    (
        "subscription_items",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/stripe_tests.rs",
        ],
    ),
    (
        "seller_accounts",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/handler_tests.rs",
            "blocks/products/tests/page_link_tests.rs",
            "blocks/products/tests/provider_tests.rs",
            "blocks/products/tests/repo_tests.rs",
            "blocks/products/tests/seller_governance_tests.rs",
            "blocks/products/tests/status_enum_tests.rs",
            "blocks/products/tests/stripe_tests.rs",
        ],
    ),
    (
        "provider_operations",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/provider_tests.rs",
        ],
    ),
    (
        "stripe_events",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            // category 4: the webhook pipeline that predates the convention
            "blocks/products/stripe.rs",
        ],
    ),
    (
        "products_variables",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/offer_pricing_tests.rs",
        ],
    ),
    (
        "groups",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            // category 4: `crud::{list_page, create_record, update_record,
            // delete_record, verify_owner, *_owned}` take the table from the
            // caller
            "blocks/products/handlers/group.rs",
            "blocks/products/tests/page_link_tests.rs",
            "blocks/products/tests/repo_tests.rs",
        ],
    ),
    (
        "types",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            // category 4, same as groups
            "blocks/products/handlers/types.rs",
        ],
    ),
    (
        "group_templates",
        &["blocks/products/mod.rs", "blocks/dev/data_snapshot.rs"],
    ),
    (
        "product_templates",
        &["blocks/products/mod.rs", "blocks/dev/data_snapshot.rs"],
    ),
    // The llm settings door. One entry, and it is a fault injector: the
    // block's `config_tests` name the table so `FailingDbOpContext` lands on
    // the settings read under test rather than on some other table's. Same
    // category as `blocks/admin/pages/blocks.rs` and `blocks/files/quota.rs`
    // above.
    ("llm_settings", &["blocks/llm/mod.rs"]),
];

#[test]
fn only_the_allowlist_names_a_platform_table_via_the_const() {
    let sources: Vec<(String, String)> = crate_sources()
        .into_iter()
        .map(|(path, src)| (path, code_only(&src)))
        .collect();
    for (door, _, consts, qualifier) in TABLES {
        let allowed = IDENT_ALLOWED
            .iter()
            .find(|(m, _)| m == door)
            .map(|(_, files)| *files)
            .unwrap_or(&[]);
        let offenders: Vec<&String> = sources
            .iter()
            .filter(|(path, _)| !matches_allowlist(path, allowed))
            .filter(|(_, src)| {
                src.contains(qualifier) && consts.iter().any(|ident| src.contains(ident))
            })
            .map(|(path, _)| path)
            .collect();
        assert!(
            offenders.is_empty(),
            "these files name the table via one of `{consts:?}` instead of calling a \
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
    for (door, literal, consts, _qualifier) in TABLES {
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
                    sources.iter().any(|(path, src)| path == entry
                        && consts.iter().any(|ident| src.contains(ident))),
                    "`{entry}` is allowlisted for `{consts:?}` but no longer names any \
                     of them; drop the entry rather than leaving a standing exemption"
                );
            }
        }
    }
}

/// The old names are gone: `admin_schema.rs` and the `blocks::admin`
/// re-exports (`BLOCK_SETTINGS_TABLE`, `WRAP_GRANTS_TABLE`,
/// `REQUEST_LOGS_TABLE`, `USER_ROLES_TABLE`, `admin::VARIABLES_TABLE`),
/// `messages_schema.rs` (the module that existed so `blocks/llm` could read
/// the messages block's tables by name), and `PRODUCTS_TABLE` (the products
/// table's pre-`repo` constant, previously guarded by the block's own door
/// test). A file that still imports one would compile only by redefining it,
/// which is the same bypass wearing the old name. (`VARIABLES_TABLE` on its
/// own is not banned: products aliases its own `repo::variables::TABLE` to
/// it.)
#[test]
fn the_old_table_name_shims_are_gone() {
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
        "messages_schema::",
        "mod messages_schema",
        "PRODUCTS_TABLE",
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

/// The messages block's two tables are named only inside the messages block.
///
/// This is the cross-block half of the same rule, and it is stated as a
/// boundary rather than as a door because `messages/rest.rs` genuinely hands
/// `service::{CONTEXTS_TABLE, ENTRIES_TABLE}` to shared helpers
/// (`crud::verify_owner`, `crud::delete_record`) whose table comes from the
/// caller — allowlisting that file would buy a standing exemption for
/// nothing, since the risk this test exists for was never inside the block.
/// It was `blocks/llm/pages.rs`, which listed both tables with `db::list`
/// while the same block wrote through `ctx.call_block`. That direct read is
/// the whole reason `messages_schema.rs` existed and the reason
/// `messages/mod.rs` had to grant `impresspress/llm` read access to two
/// tables it does not own.
#[test]
fn the_messages_tables_are_named_only_inside_the_messages_block() {
    let sources: Vec<(String, String)> = crate_sources()
        .into_iter()
        .map(|(path, src)| (path, code_only(&src)))
        .collect();
    for name in [
        "impresspress__messages__contexts",
        "impresspress__messages__entries",
        "CONTEXTS_TABLE",
        "ENTRIES_TABLE",
    ] {
        let offenders: Vec<&String> = sources
            .iter()
            .filter(|(path, _)| !path.starts_with("blocks/messages/"))
            .filter(|(_, src)| src.contains(name))
            .map(|(path, _)| path)
            .collect();
        assert!(
            offenders.is_empty(),
            "`{name}` belongs to `impresspress/messages`; these files outside \
             `blocks/messages/` name it instead of calling the block through \
             `ctx.call_block(\"impresspress/messages\", ..)`: {offenders:?}"
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
