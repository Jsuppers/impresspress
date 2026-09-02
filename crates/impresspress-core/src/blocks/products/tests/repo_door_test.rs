//! The products table has exactly one door. A read that goes around
//! `repo::products` skips the `deleted_at` filter, and a soft-deleted
//! product becomes visible again — which for the catalog means purchasable.
//! The gate is a source scan because the table name is necessarily reachable
//! (`mod.rs` registers it in `collections(...)`), so nothing but a test can
//! catch a call site that names it directly.
//!
//! Scope: every `.rs` file in this crate, not just `src/blocks/products`.
//! A table name is a string — any block can spell it out, and the admin
//! block's database explorer and the WRAP layer both work in terms of raw
//! table names. Scanning only the owning block left the whole rest of the
//! crate free to read the products table directly and answer with rows the
//! door would have filtered.
//!
//! What this gate still does NOT cover, stated so it is not mistaken for
//! more than it is:
//!
//!   * other workspace crates (`impresspress-web`, `-browser`,
//!     `-cloudflare`, the CLI). `CARGO_MANIFEST_DIR` is this crate, and the
//!     products block lives entirely here; a cross-crate read would need
//!     `repo` to be public, which it is not.
//!   * non-Rust sources. The migrations under `blocks/products/migrations/`
//!     name the table by design — that is where it is defined.
//!   * the files on the allowlists below. Each is listed individually, so a
//!     NEW file reading the table raw fails the gate and has to justify
//!     itself here; but nothing re-checks that a listed file's reads are
//!     still fixture setup rather than production reads.

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

/// Whether `path` (relative to `src`) matches one of `allowlist`'s entries.
/// Every entry must match `path` exactly — no directory prefixes, so an
/// allowlist can never exempt a file that does not exist yet. A suffix match
/// would additionally let `"mod.rs"` wrongly allow `repo/mod.rs`,
/// `handlers/mod.rs`, etc.
fn matches_allowlist(path: &str, allowlist: &[&str]) -> bool {
    allowlist.contains(&path)
}

const TABLE_LITERAL: &str = "impresspress__products__products";

/// Files allowed to name the products table by its literal string. Listed
/// one by one, deliberately: the previous blanket `tests/` entry exempted
/// every test file that would ever exist, including one that quietly read
/// the table raw and asserted on the unfiltered rows.
const LITERAL_ALLOWED: &[&str] = &[
    // the door itself
    "blocks/products/repo/products.rs",
    // this file: needs the literal in order to scan for it
    "blocks/products/tests/repo_door_test.rs",
    // seeds rows (including soft-deleted ones) straight into the table so the
    // handler tests have something to act on; fixture setup, not a production
    // read that has to respect the soft-delete filter
    "blocks/products/tests/handler_tests.rs",
    // the migration runner's own tests. They necessarily work below the repo
    // layer: migration 020 exists to repair a `deleted_at = ''` row, and the
    // repo layer is precisely what can no longer produce one, so the only way
    // to seed the pre-migration state is against the table. The migration
    // `.sql` files next to it spell the table out for the same reason — that
    // is where the table is defined. (Matches CLAUDE.md's standing exceptions
    // for migration-file runners and test-fixture setup.)
    "blocks/products/migrations/mod.rs",
];

#[test]
fn only_the_repo_module_names_the_products_table() {
    let offenders: Vec<String> = crate_sources()
        .into_iter()
        .filter(|(path, _)| !matches_allowlist(path, LITERAL_ALLOWED))
        .filter(|(_, src)| src.contains(TABLE_LITERAL))
        .map(|(path, _)| path)
        .collect();
    assert!(
        offenders.is_empty(),
        "these files name the products table directly and so bypass the \
         soft-delete filter; route them through repo::products: {offenders:?}"
    );
}

/// The old const is gone. A file that still imports it would compile only by
/// redefining it, which is the same bypass wearing the old name.
///
/// This file is excluded from its own scan (via `LITERAL_ALLOWED`): the
/// literal text this test searches for necessarily appears in its own
/// source below, which would otherwise make the assertion fail against
/// itself forever.
#[test]
fn the_old_products_table_const_is_gone() {
    let offenders: Vec<String> = crate_sources()
        .into_iter()
        .filter(|(path, _)| !matches_allowlist(path, LITERAL_ALLOWED))
        .filter(|(_, src)| src.contains("PRODUCTS_TABLE"))
        .map(|(path, _)| path)
        .collect();
    assert!(
        offenders.is_empty(),
        "PRODUCTS_TABLE still referenced in {offenders:?}"
    );
}

/// The literal-string scan above only catches a call site that spells the
/// table name out by hand. It misses the more likely mistake: naming the
/// table through `repo::products::TABLE` itself (e.g. handing it straight to
/// `db::list_all`) instead of calling a `repo::products` function. That
/// compiles cleanly today because `TABLE` is `pub(crate)` for `mod.rs`'s
/// `collections(...)` registration — so a caller who forgets to also append
/// `repo::products::live_filter()` gets a silent soft-delete bypass with no
/// warning from the compiler. This scan closes that gap: every use of the
/// `products::TABLE` identifier (however many `super::`/`repo::` segments
/// precede it) must be one of the entries on `TABLE_IDENT_ALLOWED`, each
/// justified on why it isn't a soft-delete bypass.
const TABLE_IDENT: &str = "products::TABLE";

/// Files allowed to name the products table via the `repo::products::TABLE`
/// identifier (as opposed to the literal string above). Per-file for the same
/// reason as [`LITERAL_ALLOWED`].
const TABLE_IDENT_ALLOWED: &[&str] = &[
    // the door itself: `TABLE`'s own definition and internal uses
    "blocks/products/repo/products.rs",
    // `BlockInfo::collections(...)` is an advisory table listing for
    // admin/WRAP discovery, not a query — nothing to filter
    "blocks/products/mod.rs",
    // fixtures seed rows (including soft-deleted ones) and assert on raw rows
    // straight against the table; setup and assertion for the tests guarding
    // the door, not production reads to filter
    "blocks/products/tests/handler_tests.rs",
    "blocks/products/tests/offer_management_tests.rs",
    "blocks/products/tests/offer_pricing_tests.rs",
    "blocks/products/tests/repo_door_test.rs",
    "blocks/products/tests/seller_governance_tests.rs",
    "blocks/products/tests/stripe_tests.rs",
];

#[test]
fn only_the_allowlist_names_the_products_table_via_the_const() {
    let offenders: Vec<String> = crate_sources()
        .into_iter()
        .filter(|(path, _)| !matches_allowlist(path, TABLE_IDENT_ALLOWED))
        .filter(|(_, src)| src.contains(TABLE_IDENT))
        .map(|(path, _)| path)
        .collect();
    assert!(
        offenders.is_empty(),
        "these files name the products table via `products::TABLE` instead of \
         calling a repo::products function, silently skipping the soft-delete \
         filter for a caller who does not also remember `live_filter()`: {offenders:?}"
    );
}

/// The three scans above prove that nothing *names the products table*
/// outside `repo::products`. They say nothing about which of that module's
/// own functions a call site picks — and on the write side the choice is the
/// whole correctness question.
///
/// `update_live`, `soft_delete` and `restore` each carry their soft-delete
/// predicate in the write's own `WHERE`, so they cannot land on a row whose
/// state changed since the caller last looked. Two functions deliberately do
/// not: `update_including_deleted` writes with no liveness filter at all, and
/// `purge` hard-deletes the row. Both are `pub(crate)`, both compile cleanly
/// from anywhere in the block, and nothing in the type system distinguishes
/// the handful of deliberate uses from an accidental one — a new handler
/// reaching for `update_including_deleted` writes straight through a
/// soft-deleted product and every other gate in this file still passes.
///
/// So they are distinguished by name, and each call site is listed here with
/// its justification. This is the write-side twin of
/// `only_the_allowlist_names_the_products_table_via_the_const`.
///
/// What it does NOT cover, for the same reasons stated at the top of this
/// file: other crates, non-Rust sources, and whether a listed file's uses are
/// still the ones justified below. It also cannot see a caller that reaches
/// these functions through an alias or a re-export — `use
/// repo::products::purge as p;` defeats a source scan, as it defeats the two
/// above it.
const WRITE_ESCAPE_HATCHES: &[(&str, &[&str])] = &[
    (
        // No liveness filter: writes to a soft-deleted row and reports
        // success.
        "products::update_including_deleted",
        &[
            // this file: needs the identifier in order to scan for it
            "blocks/products/tests/repo_door_test.rs",
            // seller suspension. `seller_every_product` reads through
            // `list_all_including_deleted` on purpose — suspension is a fraud
            // control and has to cover every row the seller owns, because
            // soft delete takes nothing down in Stripe — so filtering the
            // write on liveness would silently exempt exactly those rows.
            "blocks/products/handlers/sellers.rs",
            // the `stripe_product_id` write-back after the Stripe Product has
            // already been created. Refusing to record it because the product
            // was deleted mid-sync leaves that Stripe object orphaned, and it
            // is precisely the id `archive_offer_catalog` later needs to take
            // it down.
            "blocks/products/stripe.rs",
        ],
    ),
    (
        // Hard delete. Orphans every `product_id` reference to the row, which
        // is the bug soft delete exists to fix.
        "products::purge",
        &[
            // this file: needs the identifier in order to scan for it
            "blocks/products/tests/repo_door_test.rs",
            // rolling back a product that failed part-way through creation
            // and was never visible to anyone, so it has no references to
            // orphan. Not the delete a user's action reaches — that is
            // `soft_delete`.
            "blocks/products/handlers/product.rs",
        ],
    ),
];

#[test]
fn write_side_escape_hatches_are_allowlisted() {
    let sources = crate_sources();
    for (ident, allowlist) in WRITE_ESCAPE_HATCHES {
        let offenders: Vec<&String> = sources
            .iter()
            .filter(|(path, _)| !matches_allowlist(path, allowlist))
            .filter(|(_, src)| src.contains(ident))
            .map(|(path, _)| path)
            .collect();
        assert!(
            offenders.is_empty(),
            "these files call `{ident}`, which writes past the soft-delete \
             filter, without being justified in WRITE_ESCAPE_HATCHES. Use the \
             filtered write (`update_live` / `soft_delete`) unless the \
             unfiltered one is genuinely what the operation means, and say why \
             here if it is: {offenders:?}"
        );
    }
}

/// An allowlist entry naming a file that no longer calls the function is a
/// dead exemption: it silently pre-approves whatever that file does next.
/// Every entry must still be a real call site (this file excepted — it holds
/// the identifiers only in order to scan for them).
#[test]
fn no_write_escape_hatch_allowlist_entry_is_dead() {
    let sources = crate_sources();
    for (ident, allowlist) in WRITE_ESCAPE_HATCHES {
        for entry in *allowlist {
            if *entry == "blocks/products/tests/repo_door_test.rs" {
                continue;
            }
            let uses = sources
                .iter()
                .any(|(path, src)| path == entry && src.contains(ident));
            assert!(
                uses,
                "`{entry}` is allowlisted for `{ident}` but no longer calls it; \
                 drop the entry rather than leaving a standing exemption"
            );
        }
    }
}

/// The read-side twin of [`WRITE_ESCAPE_HATCHES`], and the one that matters
/// most for what a customer can see.
///
/// The scans above prove nothing *names the products table* outside
/// `repo::products`, and the write list proves no new call site writes past
/// the soft-delete filter. Neither says anything about the two functions in
/// that module that deliberately *read* past it: `get_including_deleted` and
/// `list_all_including_deleted`. Both are `pub(crate)`, both compile cleanly
/// from anywhere in the block, and both return a soft-deleted product's row
/// as if it were live — so a new handler reaching for either one puts a
/// deleted product back in front of a customer while every other gate in
/// this file stays green. That is the exact argument the write-side list
/// makes for itself; it applies at least as strongly here, because a leaked
/// read is visible to a buyer whereas a stray write is visible to an admin.
///
/// Each call site is listed with its justification, and the same limits
/// apply as above: other crates, non-Rust sources, aliases and re-exports are
/// out of reach, and nothing re-checks that a listed file's uses are still
/// the ones justified here.
const READ_ESCAPE_HATCHES: &[(&str, &[&str])] = &[
    (
        // Reads one row whatever its soft-delete state.
        "products::get_including_deleted",
        &[
            // this file: needs the identifier in order to scan for it
            "blocks/products/tests/repo_door_test.rs",
            // two callers, both of which read a DELETED product on purpose
            // and neither of which shows it to anyone:
            // `archive_offer_catalog` needs `owner_kind`/`owner_id` and
            // `stripe_product_id` to take a deleted product's Prices out of
            // the live Stripe catalog, which is precisely when that most
            // needs doing; and `reconcile_payment_link_session` needs the
            // product name for the buyer's own receipt after Stripe has
            // already captured the money through a Payment Link that soft
            // delete never took down.
            "blocks/products/stripe.rs",
        ],
    ),
    (
        // Lists rows whatever their soft-delete state.
        "products::list_all_including_deleted",
        &[
            // this file: needs the identifier in order to scan for it
            "blocks/products/tests/repo_door_test.rs",
            // seller suspension, which is a fraud control and so has to cover
            // every row the seller owns — soft delete takes nothing down in
            // Stripe, so exempting the deleted rows would leave exactly them
            // still taking money.
            "blocks/products/handlers/sellers.rs",
            // asserts that suspension reached the deleted rows too, which it
            // can only do by reading the set that spans both.
            "blocks/products/tests/seller_governance_tests.rs",
        ],
    ),
];

#[test]
fn read_side_escape_hatches_are_allowlisted() {
    let sources = crate_sources();
    for (ident, allowlist) in READ_ESCAPE_HATCHES {
        let offenders: Vec<&String> = sources
            .iter()
            .filter(|(path, _)| !matches_allowlist(path, allowlist))
            .filter(|(_, src)| src.contains(ident))
            .map(|(path, _)| path)
            .collect();
        assert!(
            offenders.is_empty(),
            "these files call `{ident}`, which reads past the soft-delete \
             filter and so can hand a soft-deleted product to a caller, \
             without being justified in READ_ESCAPE_HATCHES. Use `get` / \
             `list_all` unless reading a deleted row is genuinely what the \
             operation means, and say why here if it is: {offenders:?}"
        );
    }
}

/// [`no_write_escape_hatch_allowlist_entry_is_dead`] for the read list: an
/// entry naming a file that no longer calls the function silently
/// pre-approves whatever that file does next.
#[test]
fn no_read_escape_hatch_allowlist_entry_is_dead() {
    let sources = crate_sources();
    for (ident, allowlist) in READ_ESCAPE_HATCHES {
        for entry in *allowlist {
            if *entry == "blocks/products/tests/repo_door_test.rs" {
                continue;
            }
            let uses = sources
                .iter()
                .any(|(path, src)| path == entry && src.contains(ident));
            assert!(
                uses,
                "`{entry}` is allowlisted for `{ident}` but no longer calls it; \
                 drop the entry rather than leaving a standing exemption"
            );
        }
    }
}
