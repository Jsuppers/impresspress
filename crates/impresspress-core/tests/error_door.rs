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
//! That last blind spot is closed for the two files where it was
//! concentrated: `products/stripe.rs` (the webhook dispatcher and the offer
//! checkout) and `products/purchase.rs` (the order reads and the refund
//! orchestration). Whether an `err_internal(label, cause)` there wraps a
//! database call is a reading job per site, so the second gate below does not
//! guess: every `err_internal` tail left in those files is inventoried by its
//! label, with the reason it is not a database failure, and a tail that is not
//! on the inventory fails. Everywhere else the blind spot stands — an empty
//! `STILL_HAND_MAPPED` means no file writes the *shape*, not that every
//! refusal is classified.
//!
//! `auth::repo::RepoError` used to be named here as a site the gate could
//! not help: it was `NotFound | Db(String)`, so the wafer code was gone
//! before a handler ever saw it. PR 2 folded it into `WaferError`, and those
//! sites classify like every other one now.

use impresspress_core::test_support::source_scan::{
    code_before_comment, strip_line_comments, strip_test_modules, SourceWalk,
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

// ---------------------------------------------------------------------------
// Inventoried `err_internal` tails
// ---------------------------------------------------------------------------

/// Every `err_internal(label, cause)` left in a gated file, by label, with how
/// many times it appears and why it is not a database failure.
///
/// A database failure in these files goes through `crud::db_error_internal`
/// (or `crud::db_error` where a `NotFound` is the caller's row), so a WRAP
/// denial is 403 and a quota is 429. What is left here is one of two things:
///
/// - **Stripe**: the cause is a Stripe API call. `stripe_client::classify`
///   gives it `Internal` or `FailedPrecondition`, never a WRAP code, and a
///   Stripe 429 is Stripe's rate limit, not the database's.
/// - **Invariant**: the cause is this process, not a service — a row outside
///   its contract, a setting outside its range (`config::get_default` answers
///   the default, never a database error), a serialization or the OS RNG.
///
/// The gate compares counts both ways. A label that is not listed, or that
/// appears more often than listed, is a new tail: route it through the door
/// or add it here with its reason. A label that appears less often than
/// listed is a stale entry: lower the count or delete the line, so the list
/// never grants more than the files use.
///
/// The behavioural half is fault injection through `FailingDbOpContext`:
/// `webhook_database_denial_is_403_and_the_delivery_is_retried` and its
/// siblings in `products/tests/stripe_tests.rs`, and
/// `refund_ledger_denial_is_403` and its siblings in
/// `products/tests/provider_tests.rs`.
const INVENTORIED_TAILS: &[(&str, &[Tail])] = &[
    (
        "products/stripe.rs",
        &[
            Tail {
                label: "Stripe API error",
                count: 1,
                why: "Stripe: the Checkout Session create",
            },
            Tail {
                label: "Platform application fee is misconfigured",
                count: 1,
                why: "invariant: the fee setting is outside 0..=10000",
            },
            Tail {
                label: "Platform country is misconfigured",
                count: 1,
                why: "invariant: the country setting is not a two-letter code",
            },
            Tail {
                label: "Could not snapshot checkout inputs",
                count: 1,
                why: "invariant: serializing the evaluated inputs",
            },
            Tail {
                label: "Could not snapshot checkout condition",
                count: 1,
                why: "invariant: serializing a component condition",
            },
            Tail {
                label: "Could not create checkout receipt",
                count: 1,
                why: "invariant: the OS random source",
            },
            Tail {
                label: "Purchase row is outside the contract",
                count: 1,
                why: "invariant: an undecodable order status",
            },
        ],
    ),
    (
        "products/purchase.rs",
        &[
            Tail {
                label: "Stripe refund could not be completed",
                count: 1,
                why: "Stripe: the refund create",
            },
            Tail {
                label: "Order row is outside the contract",
                count: 4,
                why: "invariant: an undecodable order row",
            },
            Tail {
                label: "Refund row is outside the contract",
                count: 1,
                why: "invariant: an undecodable refund status",
            },
            Tail {
                label: "&format!(\"{entity} row is outside the contract\")",
                count: 1,
                why: "invariant: an undecodable child row",
            },
            Tail {
                label: "Purchase has invalid refund accounting",
                count: 1,
                why: "invariant: the order's own totals disagree",
            },
        ],
    ),
];

/// One inventoried tail: its label as [`err_internal_labels`] reads it, how
/// many times the file uses it, and why it is not a database failure —
/// `"Stripe: …"` or `"invariant: …"`, the only two reasons there are.
struct Tail {
    label: &'static str,
    count: usize,
    why: &'static str,
}

/// A label whose use disagrees with its inventory: `(label, used, listed)`.
type Miscount = (String, usize, usize);

/// The first argument of every `err_internal(` call in `src`'s production
/// code: a string literal without its quotes, any other expression as
/// written, with its whitespace collapsed.
///
/// A call is `err_internal(` not preceded by an identifier character, so
/// `err_internal_no_cause(` and `crud::db_error_internal(` are not calls of
/// it. Comments are cut at `//` first: a call named in prose is not a call.
fn err_internal_labels(src: &str) -> Vec<String> {
    const CALL: &str = "err_internal(";
    let code = strip_test_modules(src)
        .lines()
        .map(code_before_comment)
        .collect::<Vec<_>>()
        .join("\n");
    let mut labels = Vec::new();
    let mut from = 0;
    while let Some(offset) = code[from..].find(CALL) {
        let start = from + offset;
        from = start + CALL.len();
        let preceded_by_ident = code[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if preceded_by_ident {
            continue;
        }
        labels.push(first_argument(&code[from..]));
    }
    labels
}

/// The text of a call's first argument, up to the first comma outside any
/// bracket or string.
fn first_argument(args: &str) -> String {
    let args = args.trim_start();
    if let Some(literal) = args.strip_prefix('"') {
        let mut escaped = false;
        for (i, c) in literal.char_indices() {
            match c {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => return literal[..i].to_string(),
                _ => escaped = false,
            }
        }
        panic!("unterminated string literal in an err_internal call");
    }
    let (mut depth, mut in_string, mut escaped) = (0_i32, false, false);
    for (i, c) in args.char_indices() {
        if in_string {
            match c {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => in_string = false,
                _ => escaped = false,
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth > 0 => depth -= 1,
            ',' | ')' if depth == 0 => {
                return args[..i].split_whitespace().collect::<Vec<_>>().join(" ");
            }
            _ => {}
        }
    }
    panic!("unterminated err_internal call");
}

/// `src` against its inventory: the labels used more often than listed (new
/// tails), and the labels listed more often than used (stale entries), each
/// with the two counts.
fn tails_verdict(src: &str, inventory: &[Tail]) -> (Vec<Miscount>, Vec<Miscount>) {
    let mut used: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for label in err_internal_labels(src) {
        *used.entry(label).or_default() += 1;
    }
    let listed: std::collections::BTreeMap<String, usize> = inventory
        .iter()
        .map(|tail| (tail.label.to_string(), tail.count))
        .collect();
    assert_eq!(
        listed.len(),
        inventory.len(),
        "a label is listed twice in its file's inventory"
    );

    let unlisted = used
        .iter()
        .filter_map(|(label, &n)| {
            let allowed = listed.get(label).copied().unwrap_or(0);
            (n > allowed).then(|| (label.clone(), n, allowed))
        })
        .collect();
    let stale = listed
        .iter()
        .filter_map(|(label, &allowed)| {
            let n = used.get(label).copied().unwrap_or(0);
            (n < allowed).then(|| (label.clone(), n, allowed))
        })
        .collect();
    (unlisted, stale)
}

#[test]
fn gated_files_tail_only_inventoried_non_database_failures() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src/blocks");
    for (rel, inventory) in INVENTORIED_TAILS {
        for tail in *inventory {
            assert!(
                tail.why.starts_with("Stripe: ") || tail.why.starts_with("invariant: "),
                "{rel}: `{}` must say whether it is a Stripe call or an invariant",
                tail.label
            );
        }
        let src = std::fs::read_to_string(format!("{root}/{rel}"))
            .unwrap_or_else(|error| panic!("{rel} must exist to be gated: {error}"));
        let (unlisted, stale) = tails_verdict(&src, inventory);
        assert!(
            unlisted.is_empty(),
            "{rel} has `err_internal` tails that are not on its inventory \
             (label, used, listed): {unlisted:?}\n\
             If the cause is a database call, use \
             `crud::db_error_internal(error, \"<label>\")` so a WRAP denial \
             stays a 403 and a quota a 429. If it genuinely is not — a Stripe \
             call, or a fault of this process — list it in INVENTORIED_TAILS \
             with that reason."
        );
        assert!(
            stale.is_empty(),
            "{rel}'s inventory lists more `err_internal` tails than the file \
             has (label, used, listed): {stale:?}\n\
             Lower the count or delete the entry, so the inventory never \
             grants a tail the file does not use."
        );
    }
}

/// The inventory gate can fail, on the real file: a planted tail that wraps a
/// database call under a new label, a second copy of a listed label, and a
/// listed tail that is gone are each reported.
#[test]
fn the_inventory_gate_catches_a_planted_tail() {
    let (rel, inventory) = INVENTORIED_TAILS[0];
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/blocks/products/stripe.rs"
    ))
    .expect("stripe.rs");
    assert_eq!(rel, "products/stripe.rs");
    assert_eq!(tails_verdict(&src, inventory), (Vec::new(), Vec::new()));

    let planted = format!(
        "{src}\nasync fn planted(ctx: &dyn Context) -> OutputStream {{\n    \
         match repo::refunds::get_by_idempotency_key(ctx, \"k\").await {{\n        \
         Ok(_) => ok_json(&()),\n        \
         Err(error) => err_internal(\n            \"Could not load the thing\",\n            error,\n        ),\n    \
         }}\n}}\n"
    );
    assert_eq!(
        tails_verdict(&planted, inventory).0,
        vec![("Could not load the thing".to_string(), 1, 0)]
    );

    let reused = format!(
        "{src}\nfn reused(error: WaferError) -> OutputStream {{ err_internal(\"Stripe API error\", error) }}\n"
    );
    assert_eq!(
        tails_verdict(&reused, inventory).0,
        vec![("Stripe API error".to_string(), 2, 1)]
    );

    let removed = src.replacen(
        "err_internal(\"Stripe API error\", error)",
        "crud::db_error_internal(error, \"Stripe API error\")",
        1,
    );
    assert_ne!(removed, src, "the listed Stripe tail must be in the file");
    assert_eq!(
        tails_verdict(&removed, inventory).1,
        vec![("Stripe API error".to_string(), 0, 1)]
    );

    // Neither prose, a test, nor a different function is a tail.
    let not_tails = "// err_internal(\"prose\", e)\n\
         fn a() { err_internal_no_cause(\"x\"); crud::db_error_internal(e, \"y\"); }\n\
         #[cfg(test)]\nmod tests { fn t() { err_internal(\"in a test\", e); } }\n";
    assert!(err_internal_labels(not_tails).is_empty());
}
