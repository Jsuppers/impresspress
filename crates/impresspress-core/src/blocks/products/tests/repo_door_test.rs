//! The products table has exactly one door. A read that goes around
//! `repo::products` skips the `deleted_at` filter, and a soft-deleted
//! product becomes visible again — which for the catalog means purchasable.
//! The gate is a source scan because the table name is necessarily reachable
//! (`mod.rs` registers it in `collections(...)`), so nothing but a test can
//! catch a call site that names it directly.

fn block_sources() -> Vec<(String, String)> {
    let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/blocks/products"));
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read block dir") {
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
    out
}

/// Whether `path` (relative to `src/blocks/products`) matches one of
/// `allowlist`'s entries. An entry ending in `/` allows every file under
/// that directory; any other entry must match `path` exactly — a suffix
/// match would let `"mod.rs"` wrongly allow `repo/mod.rs`, `handlers/mod.rs`,
/// etc. too.
fn matches_allowlist(path: &str, allowlist: &[&str]) -> bool {
    allowlist.iter().any(|a| match a.strip_suffix('/') {
        Some(dir) => path == dir || path.starts_with(a),
        None => path == *a,
    })
}

const TABLE_LITERAL: &str = "impresspress__products__products";

/// Files/directories allowed to name the products table by its literal
/// string.
const LITERAL_ALLOWED: &[&str] = &[
    "repo/products.rs",        // the door itself
    "tests/repo_door_test.rs", // this file: needs the literal to scan for it
    "tests/",                  // test fixtures seed rows (including soft-deleted ones) directly
                               // against the table for setup; that's fixture setup, not a
                               // production read that must respect the soft-delete filter
];

#[test]
fn only_the_repo_module_names_the_products_table() {
    let offenders: Vec<String> = block_sources()
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
    let offenders: Vec<String> = block_sources()
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
