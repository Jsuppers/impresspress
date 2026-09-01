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
