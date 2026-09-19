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
//! own doc comment would otherwise fail it) and with every `#[cfg(test)]`
//! item removed (a test asserting on the old behaviour is not a handler
//! producing it). Trailing comments on code lines are kept, so nothing hides
//! behind a `//` on the same line as code.
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
//! That last blind spot outlived the allowlist. `products/stripe.rs`'s
//! webhook dispatcher still tails roughly forty database and Stripe-API
//! failures into `err_internal` with no `NotFound` arm above them, and
//! `products/purchase.rs`'s refund orchestration does the same; separating
//! the database calls from the Stripe calls there is a reading job per site,
//! not a mechanical one, so it is owed as its own PR rather than smuggled
//! into this gate's scope. An empty list below does NOT mean every products
//! refusal is classified — it means no file writes the *shape*.
//!
//! `auth::repo::RepoError` used to be named here as a site the gate could
//! not help: it was `NotFound | Db(String)`, so the wafer code was gone
//! before a handler ever saw it. PR 2 folded it into `WaferError`, and those
//! sites classify like every other one now.

use impresspress_core::test_support::source_scan::{
    strip_line_comments, strip_test_modules, SourceWalk,
};

/// Files still carrying the shape. **Empty**, and the history of how it got
/// there, because each entry was a place a WRAP refusal shipped as a 500 and
/// the order they came off in is the argument for keeping it at zero.
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
///
/// PR 3 took its ten: `messages/{rest,pages}.rs`, `legalpages/{mod,pages}.rs`,
/// `files/{share,cloud}.rs`, `files/storage/{objects,buckets}.rs`,
/// `llm/mod.rs` and `llm/routes/providers.rs`. Four of those are STORAGE
/// calls rather than database ones (`files/share.rs` and both
/// `files/storage/*`); the codes are the same set and the mapping is the
/// same sentence, so they go through the same door. Only the eight products
/// entries were left after it.
///
/// PR 4 took **none** of them, deliberately. The entries were written on the
/// assumption that the enum work would open these files anyway, and it did —
/// but it opened them to move two published snapshots, the SDK's order and
/// seller types and every products status column at once. Folding a second,
/// unrelated behaviour change (a WRAP denial stops answering 500) into that
/// review would have hidden it. The eight went as their own PR, which is a
/// mechanical diff with a behavioural test and no snapshot movement at all.
///
/// PR 5 (`StripeEventType`) opened two of the eight — `products/stripe.rs`
/// and `products/pages.rs` — and took none either, for the same reason.
///
/// That PR has now landed and the list is **empty**. All 29 `NotFound`
/// classifications across the eight products files classify through
/// `crud::db_error` / `crud::db_error_internal`, or through one of the three
/// block-private helpers (`handlers::product::write_error`,
/// `handlers::offers::domain_error`, `handlers::provider::provider_error`)
/// whose tails now delegate to it while keeping their own domain arms.
/// `products/tests/error_mapping_tests.rs` is the behavioural half: a real
/// `wrap::check_access` denial per file, each paired with the 404 a granted
/// read of a missing row still gives.
///
/// An empty list is the invariant, not a milestone: a file that hand-maps a
/// database error fails this test, and re-listing one takes an edit here and
/// the review that comes with it.
const STILL_HAND_MAPPED: &[(&str, &str)] = &[];

/// The one file allowed to contain the mapping, because it IS the mapping.
const THE_DOOR: &str = "crud.rs";

/// The walk this gate runs over: every block source, with a floor so an empty
/// scan cannot pass as a clean one.
fn scan() -> SourceWalk {
    SourceWalk::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/blocks")).least(100)
}

/// `src` as production code: every `#[cfg(test)]` item dropped, and full-line
/// comments with it.
fn production_code(src: &str) -> Vec<String> {
    strip_line_comments(&strip_test_modules(src))
        .lines()
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

/// The files that carry the shape and are not allowed to, and the entries on
/// the list that no longer carry it — the gate's whole verdict, over whatever
/// tree `walk` reaches.
fn verdict(walk: &SourceWalk) -> (Vec<String>, Vec<String>) {
    let allowed: std::collections::HashMap<&str, &str> =
        STILL_HAND_MAPPED.iter().copied().collect();

    let mut unexpected = Vec::new();
    let mut clean_but_listed = Vec::new();

    for file in walk.collect() {
        if file.rel == THE_DOOR {
            continue;
        }
        let hits = hand_maps_a_database_error(&production_code(&file.text));
        match (hits, allowed.contains_key(file.rel.as_str())) {
            (true, false) => unexpected.push(file.rel),
            (false, true) => clean_but_listed.push(file.rel),
            _ => {}
        }
    }

    unexpected.sort();
    clean_but_listed.sort();
    (unexpected, clean_but_listed)
}

#[test]
fn only_crud_maps_a_database_error_by_hand() {
    let (unexpected, clean_but_listed) = verdict(&scan());

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

    // …but a handler BELOW one still is. `#[cfg(test)]` is not only the
    // trailing `mod tests`: seventeen files in this crate carry it on an early
    // `mod test_support;`, a `use` or a fixture `fn`, and a scope that ran to
    // the first attribute and stopped saw 15 of `blocks/products/mod.rs`'s 422
    // lines. Everything past it was un-gated, and this is the case that says
    // so.
    let after_a_test_module = production_code(
        r#"
        #[cfg(test)]
        mod tests {
            fn nothing() {}
        }

        pub fn handler() {
            Err(e) if e.code == ErrorCode::NotFound => err_not_found("x"),
            Err(e) => err_internal("Database error", e),
        }
        "#,
    );
    assert!(hand_maps_a_database_error(&after_a_test_module));
}

/// The *walk* reaches a planted offender, and the door it exempts is the only
/// thing it lets through.
///
/// `the_gate_catches_the_shape_it_is_looking_for` proves the predicate works;
/// it says nothing about whether the walk ever opens a file. A root that moved
/// or an extension filter that broke would leave the gate above passing on an
/// empty scan — green, and blind to the one thing it exists to catch.
#[test]
fn the_walk_reaches_the_files_it_claims_to_scan() {
    const SHAPE: &str = "match db::get(ctx, TABLE, id).await {\n\
         Err(e) if e.code == ErrorCode::NotFound => err_not_found(\"x\"),\n\
         Err(e) => err_internal(\"Database error\", e),\n\
         }\n";

    let root = std::env::temp_dir().join(format!("error-door-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("nested")).expect("temp tree");
    std::fs::write(root.join("nested/offender.rs"), SHAPE).expect("offender");
    std::fs::write(root.join(THE_DOOR), SHAPE).expect("the door");
    std::fs::write(root.join("notes.txt"), SHAPE).expect("non-rust");

    let (unexpected, clean_but_listed) = verdict(&SourceWalk::new(&root));
    std::fs::remove_dir_all(&root).expect("clean up");

    assert_eq!(
        unexpected,
        vec!["nested/offender.rs".to_string()],
        "expected exactly the planted offender"
    );
    assert!(clean_but_listed.is_empty(), "{clean_but_listed:?}");
}
