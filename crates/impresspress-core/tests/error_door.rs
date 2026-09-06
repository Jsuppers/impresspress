//! A failed database call has exactly one mapping.
//!
//! `blocks::crud::db_error` decides what a `WaferError` from the database
//! client turns into: `NotFound` is the caller's 404, `PermissionDenied` is a
//! **403**, `ResourceExhausted` keeps its 429, everything else is the
//! sanitized 500 with the cause logged. The shape it replaces —
//!
//! ```ignore
//! Err(e) if e.code == ErrorCode::NotFound => err_not_found("X not found"),
//! Err(e) => err_internal("Database error", e),
//! ```
//!
//! — appeared at 62 sites across 27 files, and **not one of them re-checked
//! `PermissionDenied`**. So a block deployed without the `ResourceGrant` its
//! handler needs answered `500 Internal server error (ref: …)`, which an
//! operator cannot tell from a corrupt row, and a caller cannot tell from an
//! outage. That is the regression this gate exists to stop: the shape is easy
//! to write, reads as careful, and silently loses the one code that matters.
//!
//! The gate is a source scan because there is nothing else it could be. The
//! ingredients (`ErrorCode::NotFound`, `err_not_found`, `err_internal`) are
//! all legitimately public, so no type system or lint can see the
//! combination; only reading the source can.
//!
//! Scope: every `.rs` file under `src/blocks/`, with full-line comments
//! removed first (prose describing the shape is not the shape — this file's
//! own doc comment would otherwise fail it) and with everything from the
//! first `#[cfg(test)]` onwards removed (a test asserting on the old
//! behaviour is not a handler producing it). Trailing comments on code lines
//! are kept, so nothing hides behind a `//` on the same line as code.
//!
//! What the gate does NOT see, stated so it is not mistaken for more than it
//! is: a handler that writes the `NotFound` arm and the `err_internal` tail
//! more than six lines apart; a handler whose tail is something other than
//! `err_internal` (`ui::server_error_response`, say); and a handler with no
//! `NotFound` arm at all, whose bare `err_internal` tail turns a refusal
//! into a 500 just as quietly — PR 2 found four of those in
//! `tickets/rest.rs` and two in `vector/pages.rs` only by reading the files
//! the allowlist sent it to.
//!
//! `auth::repo::RepoError` used to be named here as a site the gate could
//! not help: it was `NotFound | Db(String)`, so the wafer code was gone
//! before a handler ever saw it. PR 2 folded it into `WaferError`, and those
//! sites classify like every other one now.

/// Files still carrying the shape, each with the PR that converts it.
///
/// This list is a worklist, not an exemption: every entry is a place a WRAP
/// refusal still ships as a 500. It shrinks to empty over PRs 2–4 of this
/// phase, and a file that comes off it can never go back on without editing
/// this test.
///
/// PR 1 converted the seven sites inside `blocks/crud.rs` — which is what
/// makes the fix reach every block that reads through the CRUD primitives —
/// plus `products/handlers/sellers.rs`, `admin/settings.rs` and
/// `legalpages/mod.rs`'s two `Result<Option<_>>` handlers.
///
/// PR 2 folded `auth::repo::RepoError` into `WaferError` and took the seven
/// entries it had marked for itself off this list: `admin/{ops,mod,iam}.rs`,
/// `vector/pages.rs`, `tickets/{rest,pages}.rs` and
/// `dev/generations_api.rs`, the last through the `dev::no_store_db_error`
/// its entry called for.
const STILL_HAND_MAPPED: &[(&str, &str)] = &[
    // ---- PR 3: content blocks ----
    ("messages/rest.rs", "PR 3 (two context lookups)"),
    (
        "messages/pages.rs",
        "PR 3 (the SSR context read; its 404 is `ui::not_found_response`, so \
         it converts to `db_error` for the tail only)",
    ),
    (
        "files/storage/buckets.rs",
        "PR 3 (a STORAGE call, not a database one: `NotFound` is a no-op and \
         the tail is `err_internal`, so a denied delete_folder ships as 500)",
    ),
    (
        "legalpages/pages.rs",
        "PR 3 (the SSR editor's document read)",
    ),
    (
        "legalpages/mod.rs",
        "PR 3 (the two remaining PATCH/DELETE tails)",
    ),
    ("files/share.rs", "PR 3"),
    ("files/cloud.rs", "PR 3 (two share lookups)"),
    ("files/storage/objects.rs", "PR 3 (the object read)"),
    ("llm/mod.rs", "PR 3 (the override delete)"),
    ("llm/routes/providers.rs", "PR 3 (four provider lookups)"),
    // ---- PR 4: products enums (the same files move for the enum work) ----
    ("products/pages.rs", "PR 4 (three SSR reads)"),
    ("products/purchase.rs", "PR 4 (five purchase lookups)"),
    ("products/stripe.rs", "PR 4 (three catalog reads)"),
    (
        "products/handlers/offers.rs",
        "PR 4 (and `domain_error`'s tail)",
    ),
    (
        "products/handlers/commerce.rs",
        "PR 4 (three storefront reads)",
    ),
    ("products/handlers/catalog.rs", "PR 4"),
    (
        "products/handlers/provider.rs",
        "PR 4 (`provider_error`'s tail)",
    ),
    (
        "products/handlers/product.rs",
        "PR 4 (six lookups plus `write_error`'s tail)",
    ),
];

/// The one file allowed to contain the mapping, because it IS the mapping.
const THE_DOOR: &str = "crud.rs";

fn block_sources() -> Vec<(String, String)> {
    let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/blocks"));
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
        "the scan walks every block; {} files means it lost its way",
        out.len()
    );
    out
}

/// `src` as production code: full-line comments dropped, and everything from
/// the first `#[cfg(test)]` attribute onwards dropped with it.
fn production_code(src: &str) -> Vec<String> {
    src.lines()
        .take_while(|line| !line.trim_start().starts_with("#[cfg(test)]"))
        .filter(|line| !line.trim_start().starts_with("//"))
        .map(str::to_string)
        .collect()
}

/// Whether `line` CLASSIFIES an error as `NotFound`, rather than merely
/// naming the variant.
///
/// The two spellings a handler uses are the match guard
/// (`Err(e) if e.code == ErrorCode::NotFound =>`) and the bare arm of a
/// `match error.code` (`ErrorCode::NotFound =>`). Passing the variant as an
/// argument — `no_store_error(ErrorCode::NotFound, …)`, which constructs a
/// refusal rather than classifying one — is not the shape, and
/// `blocks/dev/files.rs` is why this distinction is drawn: it answers a
/// missing manifest entry with a constructed `NotFound` and, five lines
/// later, `err_internal`s an unrelated blob read.
fn classifies_as_not_found(line: &str) -> bool {
    let trimmed = line.trim();
    (trimmed.contains(".code ==") && trimmed.contains("ErrorCode::NotFound"))
        || trimmed.starts_with("ErrorCode::NotFound =>")
}

/// Whether `lines` pairs a `NotFound` classification with an `err_internal`
/// tail within six lines — the window the shape occupies wherever it appears.
fn hand_maps_a_database_error(lines: &[String]) -> bool {
    lines.iter().enumerate().any(|(i, line)| {
        classifies_as_not_found(line)
            && lines[i..(i + 7).min(lines.len())]
                .iter()
                .any(|window| window.contains("err_internal"))
    })
}

#[test]
fn only_crud_maps_a_database_error_by_hand() {
    let allowed: std::collections::HashMap<&str, &str> =
        STILL_HAND_MAPPED.iter().copied().collect();

    let mut unexpected = Vec::new();
    let mut clean_but_listed = Vec::new();

    for (rel, src) in block_sources() {
        if rel == THE_DOOR {
            continue;
        }
        let hits = hand_maps_a_database_error(&production_code(&src));
        match (hits, allowed.contains_key(rel.as_str())) {
            (true, false) => unexpected.push(rel),
            (false, true) => clean_but_listed.push(rel),
            _ => {}
        }
    }

    unexpected.sort();
    assert!(
        unexpected.is_empty(),
        "these files hand-map a database error instead of calling \
         `crud::db_error`, so a WRAP `PermissionDenied` ships from them as a \
         500: {unexpected:?}\n\
         Use `crud::db_error(error, \"X not found\", \"Database error\")`. If \
         the site genuinely cannot — a block whose responses all carry a \
         header, the way `blocks::dev` does — classify through \
         `crud::classify_db_error` and seal it yourself, as \
         `dev::no_store_db_error` does. Otherwise add it to \
         STILL_HAND_MAPPED with the PR that converts it."
    );

    clean_but_listed.sort();
    assert!(
        clean_but_listed.is_empty(),
        "these files are on STILL_HAND_MAPPED but no longer hand-map \
         anything; take them off the list so it stays a worklist: \
         {clean_but_listed:?}"
    );
}

/// The gate can actually fail. A test that only ever passes proves nothing,
/// and this one's whole value is the day someone re-introduces the shape.
#[test]
fn the_gate_catches_the_shape_it_is_looking_for() {
    let offending = production_code(
        r#"
        match db::get(ctx, TABLE, id).await {
            Ok(row) => ok_json(&row),
            Err(e) if e.code == ErrorCode::NotFound => err_not_found("Thing not found"),
            Err(e) => err_internal("Database error", e),
        }
        "#,
    );
    assert!(hand_maps_a_database_error(&offending));

    let converted = production_code(
        r#"
        match db::get(ctx, TABLE, id).await {
            Ok(row) => ok_json(&row),
            Err(e) => crud::db_error(e, "Thing not found", "Database error"),
        }
        "#,
    );
    assert!(!hand_maps_a_database_error(&converted));

    // Prose describing the shape is not the shape.
    let prose = production_code(
        r#"
        // Err(e) if e.code == ErrorCode::NotFound => ...
        // Err(e) => err_internal("Database error", e),
        "#,
    );
    assert!(!hand_maps_a_database_error(&prose));

    // Neither is a test asserting on it.
    let in_a_test = production_code(
        r#"
        pub fn handler() {}

        #[cfg(test)]
        mod tests {
            Err(e) if e.code == ErrorCode::NotFound => err_not_found("x"),
            Err(e) => err_internal("Database error", e),
        }
        "#,
    );
    assert!(!hand_maps_a_database_error(&in_a_test));
}
