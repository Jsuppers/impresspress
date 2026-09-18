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
//! ## What the scan does and does not see
//!
//! It matches the name as a whole path segment after `db::` or `database::`,
//! tolerating whitespace before the parenthesis, and it matches any `use`
//! that names either segment — which is how an alias or a bare call would
//! have to enter a file in the first place. A multi-line call is still caught
//! because rustfmt keeps the opening parenthesis on the name's own line, and
//! a `//` comment is stripped before matching so prose about the ban is not
//! itself a violation.
//!
//! It cannot see a call assembled by a macro, one whose opening parenthesis a
//! human put on the next line, or one reached through a module alias bound
//! somewhere other than a `use` in the same file. It covers
//! `impresspress-core/src` only, which is where every one of the sixty call
//! sites this replaced lived. Those are the limits, written down rather than
//! implied.
//!
//! The integration tests under `tests/` and the block test modules under
//! `src/**/tests/` are exempt: they assert over fixture tables whose contents
//! they just wrote, several of them *as* the witness that the raw read
//! truncates, and none of them ships.

use std::{fs, path::Path};

/// The module that owns the replacement, and so the one file allowed to hold
/// the pattern (in prose — it does not call either function).
const OWNER: &str = "db_read.rs";

/// Directories named `tests` hold block test modules, which are
/// `#[cfg(test)]` and never compiled into a release.
const TEST_DIR: &str = "tests";

const BANNED: [&str; 2] = ["list_all", "list_sorted"];

/// Whether `source` reaches the banned read. One hit per line at most — the
/// line number is the useful part, not how many ways it offends.
///
/// Two shapes count. A call: the name as a whole path segment after `::`,
/// with any whitespace before the parenthesis, on a module path ending in
/// `db` or `database`. An import: a `use` naming the banned segment at all,
/// which is the only way an alias or a bare call could enter a file.
/// `list_all_including_deleted` is a different segment and matches neither.
fn offending_lines(source: &str) -> Vec<usize> {
    let mut hits = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let code = line.split("//").next().unwrap_or(line);
        let is_use = {
            let head = code.trim_start();
            head.starts_with("use ") || head.starts_with("pub use ")
        };
        if BANNED.iter().any(|name| segment_at(code, name, is_use)) {
            hits.push(index + 1);
        }
    }
    hits
}

/// Whether `code` names `name` as its own path segment, in a position that
/// reaches the function: after `::` and before `(` for a call, anywhere on a
/// `use` line for an import.
fn segment_at(code: &str, name: &str, is_use: bool) -> bool {
    let mut from = 0;
    while let Some(at) = code[from..].find(name) {
        let start = from + at;
        from = start + name.len();
        let tail = &code[from..];
        // A whole segment: what follows must not continue the identifier.
        if tail
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let before = &code[..start];
        if is_use {
            return true;
        }
        let owner = before
            .strip_suffix("::")
            .map(|head| head.rsplit(|c: char| !(c.is_alphanumeric() || c == '_')))
            .and_then(|mut segments| segments.next());
        if matches!(owner, Some("db") | Some("database")) && tail.trim_start().starts_with('(') {
            return true;
        }
    }
    false
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
        for line in offending_lines(&source) {
            found.push(format!("{}:{}", path.display(), line));
        }
    }
}

const REMEDY: &str = "these call `db::list_all` / `db::list_sorted`, which truncate at a fixed \
                      limit and report nothing. Use `crate::db_read`: `list_bounded` with the \
                      reason the set is small, `list_capped` when the surface can say it is \
                      showing a prefix, or `list_every` when every row matters.";

#[test]
fn no_unpaged_database_read_outside_db_read() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    walk(&src, &mut found);
    assert!(found.is_empty(), "{REMEDY}\n  {}", found.join("\n  "));
}

/// The matcher still recognises what it bans, and still lets through what it
/// does not.
///
/// A guard whose matcher has quietly stopped matching passes for the same
/// reason a clean codebase does, which is the failure mode that makes most
/// source guards worthless.
#[test]
fn the_matcher_recognises_the_calls_it_bans() {
    for banned in [
        "    let rows = db::list_all(ctx, TABLE, vec![]);",
        "    db::list_sorted(ctx, TABLE, vec![], sort)",
        "    wafer_core::clients::database::list_all(&ctx, TABLE, vec![])",
        "    db::list_all (ctx, TABLE, vec![])",
        "    let rows = db::list_all(",
        "use wafer_core::clients::database::list_all;",
        "    use wafer_core::clients::database::{list_all, list_sorted};",
    ] {
        assert_eq!(
            offending_lines(banned).len(),
            1,
            "the matcher stopped seeing: {banned}"
        );
    }
    for allowed in [
        "    db_read::list_every(ctx, TABLE, vec![])",
        "    db::list(ctx, TABLE, &opts)",
        "    repo::products::list_all_including_deleted(ctx, filters)",
        "    // db::list_all( is named in a comment, not called",
        "    let rows = db::list_recent(ctx, TABLE, 20);",
    ] {
        assert!(
            offending_lines(allowed).is_empty(),
            "the matcher started refusing: {allowed}"
        );
    }
}

/// The *walk* finds a planted offender, and honours the two exemptions it
/// claims.
///
/// `the_matcher_recognises_the_calls_it_bans` proves the predicate works; it
/// says nothing about whether `walk` ever reaches a file. If the directory
/// filter or the extension filter broke so that nothing was scanned, the
/// guard above would pass on an empty result — green, and blind to exactly
/// the thing it exists to catch.
#[test]
fn the_walk_reaches_the_files_it_claims_to_scan() {
    let root = std::env::temp_dir().join(format!("db-read-guard-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("blocks/tests")).expect("temp tree");
    fs::write(
        root.join("blocks/offender.rs"),
        "async fn f() { db::list_all(ctx, T, vec![]).await }\n",
    )
    .expect("offender");
    fs::write(
        root.join("blocks/tests/fixture.rs"),
        "async fn f() { db::list_all(ctx, T, vec![]).await }\n",
    )
    .expect("exempt test module");
    fs::write(
        root.join(OWNER),
        "//! db::list_all and db::list_sorted are what this module replaces.\n",
    )
    .expect("owner");
    fs::write(root.join("notes.txt"), "db::list_all(ctx, T, vec![])\n").expect("non-rust");

    let mut found = Vec::new();
    walk(&root, &mut found);
    fs::remove_dir_all(&root).expect("clean up");

    assert_eq!(
        found.len(),
        1,
        "expected exactly the planted offender: {found:?}"
    );
    assert!(found[0].ends_with("offender.rs:1"), "{found:?}");
}
