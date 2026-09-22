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
//! for more than it is: other workspace crates (the CLI's `boot_lifecycle` and
//! `native_wrap_grants` tests seed fixtures through `DatabaseService`, and
//! the Cloudflare adapter's `D1ConfigSource` reads the variables table on
//! its production config path; all of them name `platform_state::*::TABLE`
//! and decode through the row types, so they are consumers of this module's
//! public surface rather than bypasses of it, but no test in this crate can
//! see them), non-Rust sources (the migrations under
//! `blocks/admin/migrations/` define the tables), and the files on the
//! allowlists below — each listed individually with its reason, so a NEW
//! file naming a table fails the gate and has to justify itself here.

use impresspress_core::test_support::source_scan::{strip_line_comments, SourceWalk};

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

/// The walk this gate runs over: every `.rs` file in the crate, with a floor
/// so an empty scan cannot pass as a clean one.
fn scan() -> SourceWalk {
    SourceWalk::crate_src().least(100)
}

/// Every file the walk reaches, as `(path, code)` — the source with its
/// full-line comments dropped, which is what every scan below matches on.
fn sources(walk: &SourceWalk) -> Vec<(String, String)> {
    walk.collect()
        .into_iter()
        .map(|file| (file.rel, strip_line_comments(&file.text)))
        .collect()
}

/// Whether `path` (relative to `src`) is one of `allowlist`'s entries.
/// Exact matches only — no directory prefixes, so an allowlist can never
/// exempt a file that does not exist yet.
fn matches_allowlist(path: &str, allowlist: &[&str]) -> bool {
    allowlist.contains(&path)
}

/// The files, outside `allowed`, whose code satisfies `names` — every scan
/// below is this shape, and stating it once is what lets the walk's own
/// self-test drive the same filter over a planted tree.
fn offenders<'a>(
    sources: &'a [(String, String)],
    allowed: &[&str],
    names: impl Fn(&str) -> bool,
) -> Vec<&'a String> {
    sources
        .iter()
        .filter(|(path, _)| !matches_allowlist(path, allowed))
        .filter(|(_, src)| names(src))
        .map(|(path, _)| path)
        .collect()
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
    (
        "shares",
        &[
            "blocks/files/repo/shares.rs",
            // the admin SQL explorer's refusal list names these two as
            // literals because their owning module is behind a block
            // feature and the table outlives the build that created it;
            // `secret_tables.rs` pins each literal to this door's own
            // constant in a `#[cfg(feature = ..)]` test, and issues no
            // query against either
            "secret_tables.rs",
        ],
    ),
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
            // the htmx guard's products fixture: the block's repository is
            // private to it, so the rows no API writes without Stripe are
            // test-fixture rows written straight to the table
            "htmx_guard/products.rs",
            "blocks/products/repo/products.rs",
            "blocks/products/migrations/mod.rs",
            "blocks/products/tests/handler_tests.rs",
            "blocks/products/tests/page_link_tests.rs",
            // category 3: a catalog seeded past the unpaged read ceiling, in
            // one `INSERT … SELECT` over a recursive CTE. Ten thousand rows
            // through `db::create` would take a minute of service dispatch,
            // and the size is the whole point of the test.
            "blocks/products/tests/bounded_read_tests.rs",
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
            // the htmx guard's products fixture: the block's repository is
            // private to it, so the rows no API writes without Stripe are
            // test-fixture rows written straight to the table
            "htmx_guard/products.rs",
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
            // the htmx guard's products fixture: the block's repository is
            // private to it, so the rows no API writes without Stripe are
            // test-fixture rows written straight to the table
            "htmx_guard/products.rs",
            // the admin SQL explorer's refusal list names these two as
            // literals because their owning module is behind a block
            // feature and the table outlives the build that created it;
            // `secret_tables.rs` pins each literal to this door's own
            // constant in a `#[cfg(feature = ..)]` test, and issues no
            // query against either
            "secret_tables.rs",
            "blocks/products/repo/purchases.rs",
            "blocks/products/tests/handler_tests.rs",
            "blocks/products/tests/purchase_tests.rs",
            "blocks/products/tests/repo_tests.rs",
            "blocks/products/tests/seller_governance_tests.rs",
            "blocks/products/tests/stripe_tests.rs",
            // seeds orders past the unpaged read ceiling in one
            // `INSERT … SELECT`; see the note on the products door above
            "blocks/products/tests/bounded_read_tests.rs",
        ],
    ),
    (
        "line_items",
        &[
            "blocks/products/repo/purchases.rs",
            "blocks/products/tests/handler_tests.rs",
            "blocks/products/tests/purchase_tests.rs",
            "blocks/products/tests/stripe_tests.rs",
            // seeds a line per order past the unpaged read ceiling; see the
            // note on the products door above
            "blocks/products/tests/bounded_read_tests.rs",
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
            // the htmx guard's products fixture: the block's repository is
            // private to it, so the rows no API writes without Stripe are
            // test-fixture rows written straight to the table
            "htmx_guard/products.rs",
            "blocks/products/repo/seller_accounts.rs",
            "blocks/products/migrations/mod.rs",
            // seeds a seller population past the unpaged read ceiling; see
            // the note on the products door above
            "blocks/products/tests/bounded_read_tests.rs",
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
            // the admin SQL explorer's refusal list names this one as a
            // literal for the same reason as `shares` and `purchases`: its
            // owning module is behind a block feature and the table outlives
            // the build that created it. `secret_tables.rs` pins the literal
            // to this door's own constant in a `#[cfg(feature = ..)]` test,
            // and issues no query against it
            "secret_tables.rs",
        ],
    ),
    ("products_variables", &["blocks/products/repo/variables.rs"]),
    (
        "groups",
        &[
            "blocks/products/repo/groups.rs",
            // the htmx guard's products fixture: the block's repository is
            // private to it, so the rows no API writes without Stripe are
            // test-fixture rows written straight to the table
            "htmx_guard/products.rs",
        ],
    ),
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
    let sources = sources(&scan());
    for (door, literal, _consts, _qualifier) in TABLES {
        let allowed = LITERAL_ALLOWED
            .iter()
            .find(|(m, _)| m == door)
            .map(|(_, files)| *files)
            .unwrap_or(&[]);
        let offenders = offenders(&sources, allowed, |src| src.contains(literal));
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
/// imports `platform_state` and names `<module>::TABLE` — by its path, or by
/// a grouped `<module>::{…, TABLE}` import that leaves only a bare `TABLE` at
/// the call site ([`names_const`]) — must be on the list below, each entry
/// justified on why it is not a query around the door.
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
            // the admin SQL explorer's refusal list: it compares the table
            // name against the text of a submitted query and never issues
            // one. Naming the door's constant is the point — a re-typed
            // literal here would be a second spelling of the table that
            // could drift out of the refusal silently.
            "secret_tables.rs",
            // `BlockInfo::collections(..)` / `grants(..)` are advisory
            // declarations for WRAP and the admin database explorer
            "blocks/admin/mod.rs",
            // the export allowlist/exclusion bookkeeping; its reads go
            // through a generic `db::list_all(ctx, table, ..)` over the
            // allowlist and its import through `seed::import`, and the dev
            // block grants itself those tables (see the audit pragma there)
            "blocks/dev/data_snapshot.rs",
            // A fault injector: the seed's tests fail its one metadata
            // refresh (`database.update`) and its bulk read (`database.list`)
            // on this table, to prove neither stamps the seed hash gate.
            "blocks/admin/settings.rs",
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
        &[
            "blocks/admin/mod.rs",
            "blocks/dev/data_snapshot.rs",
            // A fault injector, the same category as `blocks/admin/pages/blocks.rs`:
            // the race test aims `RendezvousDbOpContext` at the grants read
            // `assign` makes, so the two concurrent assigns both pass it
            // before either inserts. It then drives the revoke through
            // `handle_remove_role`, where the reported bug lived. The role
            // delete test aims `FailingDbOpContext` at the grants read of
            // the revocation pass that runs after the role row is deleted.
            "blocks/admin/iam.rs",
            // A fault injector, the same one: the roles tab's delete with
            // that late revocation pass failing.
            "blocks/admin/pages/users.rs",
            // A test fixture that must write past the door: migration 004's
            // test plants twin grants for the repair to collapse, and the
            // door's only writer (`assign`) refuses to make a twin.
            "blocks/admin/migrations/mod.rs",
        ],
    ),
    (
        "users",
        &[
            // the admin SQL explorer's refusal list; see the note on
            // the `variables` door above
            "secret_tables.rs",
            // the export allowlist/exclusion bookkeeping; its reads go
            // through a generic `db::list_all(ctx, table, ..)` over the
            // allowlist and its import through `seed::import`
            "blocks/dev/data_snapshot.rs",
            // A fault injector, the established second category: refresh
            // reads the tokens table and THEN the users table, and the branch
            // under test is the second read, so `TestContext::break_reads`
            // cannot reach it — it fails the token lookup first and the
            // handler returns before the users read happens. Only
            // `FailingDbOpContext` can fail one table, and it has to be
            // named. The other seven handlers in the same sweep reach their
            // branch on their first read and use `break_reads`, which names
            // no table at all.
            "blocks/auth_ui/api/refresh.rs",
            // A fault injector: a role delete's first write is the
            // auth-version bump of each holder, and the test fails exactly
            // that increment to prove a failed invalidation revokes nothing.
            // `break_reads` cannot reach it — the delete's reads succeed.
            "blocks/admin/iam.rs",
            // A fault injector: the security page reads the provider links
            // and THEN the user's `email_verified` flag, and the branch under
            // test is the flag read. `break_reads` fails the link list first;
            // only `FailingDbOpContext` aimed at this table reaches it.
            "blocks/userportal/pages/security.rs",
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
            // the admin SQL explorer's refusal list; see the note on
            // the `variables` door above
            "secret_tables.rs",
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
            // `("database.aggregate", objects::TABLE)` — the bucket-list
            // page's SECOND read. `break_reads` cannot reach it (the bucket
            // listing fails first), so the outage test names the table to put
            // the fault on the object-count aggregate alone.
            "blocks/files/pages_user/buckets.rs",
            // `("database.list", objects::TABLE)` — the object-list page's
            // "bucket found, listing failed" shape: the ownership check reads
            // the buckets table and must still land.
            "blocks/files/pages_user/objects.rs",
        ],
    ),
    (
        "shares",
        &[
            // the admin SQL explorer's refusal list; see the note on
            // the `variables` door above
            "secret_tables.rs",
            "blocks/files/mod.rs",
            // `("database.get", shares::TABLE)` — the share-delete
            // authorization test: a failed ownership read must stop the
            // request rather than skip the check
            "blocks/files/cloud.rs",
            // `("database.list", shares::TABLE)` — the cloudstorage page's
            // outage test, scoped so the quota reads beside it still land
            "blocks/files/pages_user/cloudstorage.rs",
            // `("database.list"/"database.increment_field_where",
            // shares::TABLE)` — the public link's outage tests: a lookup
            // that failed must not read as a revoked link, and an access
            // the counter could not record must not be served
            "blocks/files/share.rs",
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
            // names `products::TABLE` for the witness assertion that a
            // one-shot read of the seeded catalog stops at the ceiling — the
            // fact the exhaustive read exists to defeat.
            "blocks/products/tests/bounded_read_tests.rs",
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
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            // a fault injector: `FailingDbOpContext` fails the update that
            // records a link Stripe already created, and nothing else
            "blocks/products/tests/stripe_tests.rs",
        ],
    ),
    (
        "checkout_presets",
        &["blocks/products/mod.rs", "blocks/dev/data_snapshot.rs"],
    ),
    (
        "purchases",
        &[
            // the admin SQL explorer's refusal list; see the note on
            // the `variables` door above
            "secret_tables.rs",
            "blocks/products/repo/purchases.rs",
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/page_link_tests.rs",
            "blocks/products/tests/provider_tests.rs",
            "blocks/products/tests/storefront_tests.rs",
            "blocks/products/tests/stripe_tests.rs",
            // names `PURCHASES_TABLE`/`LINE_ITEMS_TABLE` for the witness
            // assertion that a one-shot read of the seeded tables stops at
            // the ceiling.
            "blocks/products/tests/bounded_read_tests.rs",
        ],
    ),
    (
        "line_items",
        &[
            "blocks/products/repo/purchases.rs",
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            // names `LINE_ITEMS_TABLE` for the witness assertion that a
            // one-shot read of the seeded table stops at the ceiling.
            "blocks/products/tests/bounded_read_tests.rs",
        ],
    ),
    (
        "refunds",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/purchase_tests.rs",
            // Test-fixture setup: `refund_reconciliation_keeps_the_provider_response_summary`
            // stages the `provider_succeeded` state an interrupted reconcile
            // leaves behind. No product path parks a row there across requests,
            // and `record_provider_response` refuses once the reconcile has
            // stamped `stripe_event_created`. Also a fault injector:
            // `refund_status_when_the_ledger_read_answers` aims
            // `FailingDbOpContext` at the refund-ledger read.
            "blocks/products/tests/provider_tests.rs",
            // A fault injector: `refund_webhook_status_when_the_ledger_read_answers`
            // aims `FailingDbOpContext` at the webhook's refund-ledger read.
            "blocks/products/tests/stripe_tests.rs",
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
            // the admin SQL explorer's refusal list, which names the constant
            // only to assert its own literal still matches it, and issues no
            // query — see the note on the `variables` door above
            "secret_tables.rs",
        ],
    ),
    (
        "products_variables",
        &[
            // A false attribution, kept rather than silenced: the file
            // names `platform_state::variables::TABLE` (the config
            // store) and, separately, `products::PURCHASES_TABLE`, and
            // the second import is what puts the "products" qualifier in
            // it. It never names this table, and it issues no query at
            // all — see the note on the `variables` door above.
            "secret_tables.rs",
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
            "blocks/products/tests/handler_tests.rs",
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
    // The two template doors carry a fault injector each, the same category
    // as the `llm_settings` entry below: `handler_tests` names the table so
    // `FailingDbOpContext` lands on the default-template lookup a create makes
    // and on nothing else — the point of that test is that the *other* reads
    // in the same create still work.
    (
        "group_templates",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/handler_tests.rs",
        ],
    ),
    (
        "product_templates",
        &[
            "blocks/products/mod.rs",
            "blocks/dev/data_snapshot.rs",
            "blocks/products/tests/handler_tests.rs",
        ],
    ),
    // The llm settings door. One entry, and it is a fault injector: the
    // block's `config_tests` name the table so `FailingDbOpContext` lands on
    // the settings read under test rather than on some other table's. Same
    // category as `blocks/admin/pages/blocks.rs` and `blocks/files/quota.rs`
    // above.
    ("llm_settings", &["blocks/llm/mod.rs"]),
];

/// Whether `src` names `ident` — by its path (`user_roles::TABLE`), by a
/// grouped import that brings the constant in under its bare name
/// (`user_roles::{self, TABLE}`, `user_roles::{TABLE as T}`), by a module
/// alias (`user_roles as ur` then `ur::TABLE`), or by a glob import
/// (`user_roles::*` then a bare `TABLE`). The last three never spell the path
/// at a call site. A bare `TABLE` alone is not evidence of anything, every
/// door names its own; the import is what attributes it.
fn names_const(src: &str, ident: &str) -> bool {
    if src.contains(ident) {
        return true;
    }
    let Some((module, name)) = ident.rsplit_once("::") else {
        return false;
    };
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    // Every word-bounded occurrence of `word` in `hay`, as byte offsets.
    let words = |hay: &str, word: &str| -> Vec<usize> {
        hay.match_indices(word)
            .map(|(at, _)| at)
            .filter(|&at| {
                !hay[..at].chars().next_back().is_some_and(is_word)
                    && !hay[at + word.len()..].chars().next().is_some_and(is_word)
            })
            .collect()
    };
    // `module as alias`, at the top level of a `use` or inside a group:
    // `alias::NAME` then names the constant.
    for at in words(src, module) {
        let rest = src[at + module.len()..].trim_start();
        let Some(rest) = rest.strip_prefix("as") else {
            continue;
        };
        if !rest.starts_with(char::is_whitespace) {
            continue; // `module asx`, not an alias
        }
        let alias: String = rest
            .trim_start()
            .chars()
            .take_while(|&c| is_word(c))
            .collect();
        if !alias.is_empty() && !words(src, &format!("{alias}::{name}")).is_empty() {
            return true;
        }
    }
    // `module::*`: every bare `NAME` in the file may be the glob's.
    let globbed = words(src, module).into_iter().any(|at| {
        src[at + module.len()..]
            .trim_start()
            .strip_prefix("::")
            .is_some_and(|rest| rest.trim_start().starts_with('*'))
    });
    if globbed
        && words(src, name)
            .into_iter()
            .any(|at| !src[..at].trim_end().ends_with("::"))
    {
        return true;
    }
    let opener = format!("{module}::{{");
    let mut from = 0;
    while let Some(at) = src[from..].find(&opener) {
        let start = from + at;
        from = start + opener.len();
        if src[..start].chars().next_back().is_some_and(is_word) {
            continue; // `other_user_roles::{`, not this module
        }
        let mut depth = 1;
        let mut end = from;
        for (offset, c) in src[from..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                end = from + offset;
                break;
            }
        }
        // Only the group's own top level imports from `module`: a `TABLE`
        // inside a nested group, or after another `::`, is some other
        // module's (`products::{repo::{groups::TABLE}}` is the groups door).
        let group = &src[from..end];
        let mut depth = 0;
        for (lo, c) in group.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth != 0 || !group[lo..].starts_with(name) {
                continue;
            }
            let hi = lo + name.len();
            let before = group[..lo].chars().next_back();
            let after = group[hi..].chars().next();
            let pathed = group[..lo].trim_end().ends_with("::");
            if !before.is_some_and(is_word) && !after.is_some_and(is_word) && !pathed {
                return true;
            }
        }
    }
    false
}

#[test]
fn a_grouped_import_of_the_const_is_naming_it() {
    for (src, named) in [
        ("use crate::platform_state::user_roles::TABLE;", true),
        (
            "use crate::platform_state::user_roles::{self, UserRoleRow, TABLE};",
            true,
        ),
        (
            "use crate::platform_state::{user_roles::{TABLE as T}, variables};",
            true,
        ),
        (
            "use crate::platform_state::user_roles::{\n    self,\n    TABLE,\n};",
            true,
        ),
        (
            "use crate::platform_state::user_roles::{self, UserRoleRow};",
            false,
        ),
        (
            "use crate::platform_state::user_roles::{self, OTHER_TABLE};",
            false,
        ),
        ("use crate::blocks::x::not_user_roles::{TABLE};", false),
        // another module's constant inside the group
        (
            "use crate::platform_state::user_roles::{self, other::TABLE};",
            false,
        ),
        (
            "use crate::platform_state::user_roles::{self, other::{TABLE}};",
            false,
        ),
        // a module alias, at the top level and inside a group
        (
            "use crate::platform_state::user_roles as ur;\nfn f() { ur::TABLE; }",
            true,
        ),
        (
            "use crate::platform_state::{user_roles as ur, variables};\nfn f() { ur::TABLE; }",
            true,
        ),
        (
            "use crate::platform_state::user_roles as ur;\nfn f() { ur::UserRoleRow; }",
            false,
        ),
        (
            "use crate::x::not_user_roles as ur;\nfn f() { ur::TABLE; }",
            false,
        ),
        // a glob import, then the bare name
        (
            "use crate::platform_state::user_roles::*;\nfn f() { db::list(ctx, TABLE); }",
            true,
        ),
        (
            "use crate::platform_state::user_roles::*;\nfn f() { db::list(ctx, OTHER_TABLE); }",
            false,
        ),
        (
            "use crate::platform_state::user_roles::*;\nfn f() { other::TABLE; }",
            false,
        ),
        (
            "use crate::x::not_user_roles::*;\nfn f() { db::list(ctx, TABLE); }",
            false,
        ),
    ] {
        assert_eq!(names_const(src, "user_roles::TABLE"), named, "{src}");
    }
}

#[test]
fn only_the_allowlist_names_a_platform_table_via_the_const() {
    let sources = sources(&scan());
    for (door, _, consts, qualifier) in TABLES {
        let allowed = IDENT_ALLOWED
            .iter()
            .find(|(m, _)| m == door)
            .map(|(_, files)| *files)
            .unwrap_or(&[]);
        let offenders = offenders(&sources, allowed, |src| {
            src.contains(qualifier) && consts.iter().any(|ident| names_const(src, ident))
        });
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
    let sources = sources(&scan());
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
                        && consts.iter().any(|ident| names_const(src, ident))),
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
    let sources = sources(&scan());
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
        let offenders = offenders(&sources, &[], |src| src.contains(old));
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
    let sources = sources(&scan());
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

/// The *walk* reaches a planted offender, honours an allowlist entry, and
/// reads only Rust — over the same `sources` pipeline every scan above runs.
///
/// Each scan above proves what it matches; none of them proves the walk ever
/// opened a file. A root that moved or an extension filter that broke would
/// leave every assertion here passing on an empty list of sources, which is
/// the failure mode that makes most source gates worthless. The floor on
/// [`scan`] is the other half: it fails when the real tree comes back short.
#[test]
fn the_walk_reaches_the_files_it_claims_to_scan() {
    const LITERAL: &str = "impresspress__admin__variables";

    let root = std::env::temp_dir().join(format!("repo-door-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("nested")).expect("temp tree");
    std::fs::write(
        root.join("nested/offender.rs"),
        format!("let t = \"{LITERAL}\";\n"),
    )
    .expect("offender");
    std::fs::write(
        root.join("door.rs"),
        format!("pub const T: &str = \"{LITERAL}\";\n"),
    )
    .expect("the allowlisted door");
    std::fs::write(
        root.join("prose.rs"),
        format!("// {LITERAL} named in a comment, not queried\n"),
    )
    .expect("prose only");
    std::fs::write(root.join("notes.txt"), LITERAL).expect("non-rust");

    let sources = sources(&SourceWalk::new(&root));
    std::fs::remove_dir_all(&root).expect("clean up");

    let found = offenders(&sources, &["door.rs"], |src| src.contains(LITERAL));
    assert_eq!(
        found,
        vec![&"nested/offender.rs".to_string()],
        "expected exactly the planted offender"
    );
}
