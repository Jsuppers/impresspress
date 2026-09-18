//! Nothing under `src/` may reach for an unpaged database read directly.
//!
//! `wafer_core::clients::database::list_all` and `list_sorted` send a fixed
//! `limit` and return a plain `Vec<Record>`. A caller cannot tell a complete
//! answer from a truncated one, so a read over a table that grows with
//! traffic silently becomes a prefix — and a total, a count or an
//! act-on-every-row loop built on a prefix is wrong with no symptom. That is
//! exactly the class of bug this guard exists to stop coming back.
//!
//! `crate::db_read` replaces both. Its three shapes each make the caller
//! answer the question the raw call let them skip:
//!
//! * `list_bounded` — you state *why* the matching set is small, as a
//!   `Bound`; if it turns out not to be, the read fails loudly instead of
//!   returning part of the answer.
//! * `list_capped` — you get the rows *and* whether there are more, and the
//!   surface showing them has to say so.
//! * `list_every` / `page_after` — every matching row, by keyset pagination.
//!
//! A source scan is the mechanism because the thing being banned is a call to
//! somebody else's crate: there is no type this crate owns that could refuse
//! it, and a lint would need a custom driver. The scan is cheap, it names the
//! offending file and line, and it cannot be satisfied by a comment.
//!
//! Scope: `crates/impresspress-core/src`. The integration tests under
//! `tests/` are free to call the raw reads — they assert over fixture tables
//! whose contents they just wrote, and they are not what ships.

use std::{fs, path::Path};

/// The module that owns the replacement, and so the one file allowed to hold
/// the pattern (in prose — it does not call either function).
const OWNER: &str = "db_read.rs";

/// Directories named `tests` hold the products block's test modules, which
/// are `#[cfg(test)]` and never compiled into a release.
const TEST_DIR: &str = "tests";

const BANNED: [&str; 2] = ["list_all(", "list_sorted("];

fn is_banned_call(line: &str) -> bool {
    BANNED.iter().any(|call| {
        ["db::", "database::"]
            .iter()
            .any(|prefix| line.contains(&format!("{prefix}{call}")))
    })
}

fn walk(dir: &Path, found: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("read source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == TEST_DIR) {
                continue;
            }
            walk(&path, found);
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "rs")
            || path.file_name().is_some_and(|name| name == OWNER)
        {
            continue;
        }
        let source = fs::read_to_string(&path).expect("read source file");
        for (index, line) in source.lines().enumerate() {
            if is_banned_call(line) {
                found.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
            }
        }
    }
}

#[test]
fn no_unpaged_database_read_outside_db_read() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    walk(&src, &mut found);
    assert!(
        found.is_empty(),
        "these call `db::list_all` / `db::list_sorted`, which truncate at a \
         fixed limit and report nothing. Use `crate::db_read`: \
         `list_bounded` with the reason the set is small, `list_capped` when \
         the surface can say it is showing a prefix, or `list_every` when \
         every row matters.\n  {}",
        found.join("\n  ")
    );
}

/// The guard would pass trivially if its own matcher stopped matching.
#[test]
fn the_matcher_recognises_the_calls_it_bans() {
    assert!(is_banned_call(
        "    let rows = db::list_all(ctx, TABLE, vec![]);"
    ));
    assert!(is_banned_call(
        "    db::list_sorted(ctx, TABLE, vec![], sort)"
    ));
    assert!(is_banned_call(
        "    wafer_core::clients::database::list_all(&ctx, TABLE, vec![])"
    ));
    assert!(!is_banned_call(
        "    db_read::list_every(ctx, TABLE, vec![])"
    ));
    assert!(!is_banned_call("    db::list(ctx, TABLE, &opts)"));
}
