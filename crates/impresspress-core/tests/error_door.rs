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
//! That last blind spot is closed for every block in `GATED_BLOCKS` — the
//! whole `products` block today. Whether an `err_internal(label, cause)` there
//! wraps a database call is a reading job per site, so the second gate below
//! does not guess: every `err_internal` tail left in a gated file is
//! inventoried by its label, with the reason it is not a database failure, and
//! a tail that is not on the inventory fails. A third gate stops the inventory
//! being walked around: in a gated file `err_internal` may only be called by
//! name, never renamed, stored, wrapped in a macro, or wrapped in a function
//! that forwards its caller's error. Everywhere else the blind spot stands —
//! an empty `STILL_HAND_MAPPED` means no file writes the *shape*, not that
//! every refusal is classified — and `NOT_YET_GATED` lists where it stands.
//!
//! What the inventory does NOT see, in a gated file or anywhere: a database
//! failure answered through `err_internal_no_cause` (the cause, and its code,
//! is dropped before the call), or through a response helper other than
//! `err_internal` (`ui::server_error_response`). The gate covers
//! `err_internal(label, cause)` only.
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

/// The blocks whose every file is held to [`INVENTORIED_TAILS`] and to
/// [`evasions`]: a file under one of these that is not on the inventory may
/// have no `err_internal` tail at all. Test code (`tests/` directories and
/// `#[cfg(test)]` items) is not gated.
const GATED_BLOCKS: &[&str] = &["products"];

/// The walk the gated-file checks run over: every block source outside a
/// `tests/` directory, floored like [`scan`].
fn gated_scan() -> SourceWalk {
    SourceWalk::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/blocks"))
        .skip_dir("tests")
        .least(100)
}

/// The gated block `rel` belongs to, if any.
fn gated_block(rel: &str) -> Option<&'static str> {
    GATED_BLOCKS.iter().copied().find(|block| {
        rel.strip_prefix(block)
            .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Every `err_internal(label, cause)` left in a gated file, by label, with how
/// many times it appears and why it is not a database failure. A gated file
/// that is not listed here has an empty inventory.
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
/// siblings in `products/tests/stripe_tests.rs`,
/// `refund_ledger_denial_is_403` and its siblings in
/// `products/tests/provider_tests.rs`, and one real route per products file in
/// `products/tests/error_mapping_tests.rs`.
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
    (
        "products/pages.rs",
        &[
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
                label: "Seller account error",
                count: 2,
                why: "invariant: an undecodable seller account row",
            },
        ],
    ),
    (
        "products/handlers/commerce.rs",
        &[
            Tail {
                label: "Could not encode storefront config",
                count: 1,
                why: "invariant: serializing the storefront config",
            },
            Tail {
                label: "Order has invalid currency",
                count: 1,
                why: "invariant: a stored order currency that is not a currency",
            },
            Tail {
                label: "Order row is outside the contract",
                count: 1,
                why: "invariant: an undecodable order or reconciliation status",
            },
            Tail {
                label: "Could not encode order status",
                count: 1,
                why: "invariant: serializing the guest order status",
            },
            Tail {
                label: "Could not decode product",
                count: 2,
                why: "invariant: undecodable product tags or fulfillment kind",
            },
        ],
    ),
    (
        "products/handlers/sellers.rs",
        &[
            Tail {
                label: "&format!(\"{outcome}{SELLER_APPLICATION_FEE_BPS} cannot be read\")",
                count: 1,
                why: "invariant: the fee setting is outside 0..=10000",
            },
            Tail {
                label: "Product row is outside the contract",
                count: 3,
                why: "invariant: an undecodable product row",
            },
        ],
    ),
    (
        "products/handlers/product.rs",
        &[Tail {
            label: "Product row is outside the contract",
            count: 3,
            why: "invariant: an undecodable product row",
        }],
    ),
    (
        "products/handlers/catalog.rs",
        &[Tail {
            label: "Product row is outside the contract",
            count: 1,
            why: "invariant: an undecodable product row",
        }],
    ),
    (
        "products/handlers/provider.rs",
        &[Tail {
            label: "Provider operation row is outside the contract",
            count: 1,
            why: "invariant: an undecodable provider operation row",
        }],
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
    let files = gated_scan().collect();
    for (rel, inventory) in INVENTORIED_TAILS {
        assert!(
            gated_block(rel).is_some(),
            "{rel} is inventoried but not under a GATED_BLOCKS block"
        );
        assert!(
            files.iter().any(|file| file.rel == *rel),
            "{rel} is inventoried but the gated walk does not reach it"
        );
        for tail in *inventory {
            assert!(
                tail.why.starts_with("Stripe: ") || tail.why.starts_with("invariant: "),
                "{rel}: `{}` must say whether it is a Stripe call or an invariant",
                tail.label
            );
        }
    }

    let mut gated = 0;
    for file in files.iter().filter(|file| gated_block(&file.rel).is_some()) {
        gated += 1;
        let rel = file.rel.as_str();
        let inventory = INVENTORIED_TAILS
            .iter()
            .find(|(listed, _)| *listed == rel)
            .map_or(&[][..], |(_, inventory)| *inventory);
        let (unlisted, stale) = tails_verdict(&file.text, inventory);
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
    // `products` alone is 40-odd files; a prefix that stopped matching would
    // otherwise leave this loop green over nothing.
    assert!(
        gated >= 30,
        "the gated walk reached only {gated} gated files"
    );
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

// ---------------------------------------------------------------------------
// Ways around the inventory
// ---------------------------------------------------------------------------

/// One way around the tail inventory; see [`evasions`].
#[derive(Debug, PartialEq, Eq)]
enum Evasion {
    /// `use …::err_internal as <name>`.
    Renamed(String),
    /// `err_internal` named somewhere other than the callee of a call.
    AsValue,
    /// A `macro_rules!` with this name whose body mentions `err_internal`.
    Macro(String),
    /// A function or `let`-bound closure with this name that passes its
    /// caller's error to `err_internal`.
    Wrapper(String),
}

/// The wrappers a gated file may keep, each with why nothing its callers can
/// pass is a database failure — `"invariant: …"`, the one reason there is.
/// Checked both ways, like [`INVENTORIED_TAILS`]: an unlisted wrapper fails,
/// and so does a listed one that is gone.
///
/// A wrapper's own `err_internal` call is on its file's tail inventory once,
/// however many callers it has; this list is what says the callers were read.
const LISTED_WRAPPERS: &[(&str, &str, &str)] = &[(
    "products/purchase.rs",
    "child_rows",
    "invariant: every caller passes `…View::from_record` decodes of rows it already holds",
)];

/// Every way `src` could send a database failure through `err_internal`
/// without [`err_internal_labels`] seeing the call site, one per finding.
/// Empty for a gated file, apart from its [`LISTED_WRAPPERS`].
///
/// The inventory counts `err_internal(` calls by label, so it is only as good
/// as the assumption that every tail is such a call, written where the error
/// arises. Four shapes break that, and each is a finding:
///
/// - **a renamed import** — `use crate::http::err_internal as fail;` — whose
///   calls are `fail(`, invisible to a scan for `err_internal(`;
/// - **the function as a value** — `let fail = err_internal;`, or
///   `.map_err(err_internal)`-style passing — for the same reason;
/// - **a `macro_rules!`** whose expansion calls it: the inventory counts the
///   one call in the macro body however many sites expand it;
/// - **a wrapper**: a function (or a `let`-bound closure) whose `err_internal`
///   cause comes from its caller. The inventory counts the one call in the
///   wrapper, and every call site of the wrapper — each of which can pass a
///   database error — is invisible. A cause comes from the caller when it
///   names a parameter of the closure, or a parameter of the function whose
///   type is an error (`…Error`), a `Result`, a `Display`/`Debug` bound or a
///   generic, or a binding destructured from one (`match result { Err(e) =>
///   … }`, `result.map_err(|e| …)`, `let e = …`).
///
/// Parsed, not grepped: `use` trees nest and rustfmt wraps them, and a wrapper
/// is a question about scopes. `#[cfg(test)]` items are skipped. A wrapper that
/// launders the error through something else — a struct field, a call — is not
/// followed; the bet is that nobody writes one by accident, and one written on
/// purpose is a review finding.
fn evasions(src: &str) -> Vec<Evasion> {
    use syn::visit::Visit;

    const NAME: &str = "err_internal";

    fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
        attrs.iter().any(|attr| {
            attr.path().is_ident("cfg")
                && matches!(&attr.meta, syn::Meta::List(list) if list.tokens.to_string() == "test")
        })
    }

    fn names_err_internal(path: &syn::Path) -> bool {
        path.segments.last().is_some_and(|seg| seg.ident == NAME)
    }

    /// Single-segment paths an expression mentions.
    fn mentions(expr: &syn::Expr) -> std::collections::HashSet<String> {
        struct Idents(std::collections::HashSet<String>);
        impl<'ast> Visit<'ast> for Idents {
            fn visit_path(&mut self, path: &'ast syn::Path) {
                if let Some(ident) = path.get_ident() {
                    self.0.insert(ident.to_string());
                }
                syn::visit::visit_path(self, path);
            }
        }
        let mut idents = Idents(Default::default());
        idents.visit_expr(expr);
        idents.0
    }

    /// Identifiers a pattern binds.
    fn binds(pat: &syn::Pat) -> Vec<String> {
        struct Bound(Vec<String>);
        impl<'ast> Visit<'ast> for Bound {
            fn visit_pat_ident(&mut self, pat: &'ast syn::PatIdent) {
                self.0.push(pat.ident.to_string());
                syn::visit::visit_pat_ident(self, pat);
            }
        }
        let mut bound = Bound(Vec::new());
        bound.visit_pat(pat);
        bound.0
    }

    /// Whether a parameter's type can carry a caller's error.
    fn carries_an_error(ty: &syn::Type, generics: &[String]) -> bool {
        struct Names(Vec<String>);
        impl<'ast> Visit<'ast> for Names {
            fn visit_ident(&mut self, ident: &'ast proc_macro2::Ident) {
                self.0.push(ident.to_string());
            }
        }
        let mut names = Names(Vec::new());
        names.visit_type(ty);
        names.0.iter().any(|name| {
            name.ends_with("Error")
                || matches!(name.as_str(), "Result" | "Display" | "Debug")
                || generics.contains(name)
        })
    }

    /// A function's body, or a closure's.
    #[derive(Clone, Copy)]
    enum Body<'a> {
        Block(&'a syn::Block),
        Expr(&'a syn::Expr),
    }

    impl<'a> Body<'a> {
        fn walk(self, visitor: &mut impl Visit<'a>) {
            match self {
                Body::Block(block) => visitor.visit_block(block),
                Body::Expr(expr) => visitor.visit_expr(expr),
            }
        }
    }

    /// The identifiers in `body` that hold the caller's error, grown from
    /// `sources` through every binding destructured from one. Nested `fn`
    /// items are their own scope and are not entered.
    fn tainted(body: Body<'_>, sources: Vec<String>) -> std::collections::HashSet<String> {
        struct Taint {
            held: std::collections::HashSet<String>,
            grew: bool,
        }
        impl Taint {
            fn from(&self, expr: &syn::Expr) -> bool {
                mentions(expr).iter().any(|ident| self.held.contains(ident))
            }
            fn hold(&mut self, pat: &syn::Pat) {
                for ident in binds(pat) {
                    self.grew |= self.held.insert(ident);
                }
            }
        }
        impl<'ast> Visit<'ast> for Taint {
            fn visit_item_fn(&mut self, _: &'ast syn::ItemFn) {}
            fn visit_local(&mut self, local: &'ast syn::Local) {
                if local
                    .init
                    .as_ref()
                    .is_some_and(|init| self.from(&init.expr))
                {
                    self.hold(&local.pat);
                }
                syn::visit::visit_local(self, local);
            }
            fn visit_expr_match(&mut self, expr: &'ast syn::ExprMatch) {
                if self.from(&expr.expr) {
                    for arm in &expr.arms {
                        self.hold(&arm.pat);
                    }
                }
                syn::visit::visit_expr_match(self, expr);
            }
            fn visit_expr_let(&mut self, expr: &'ast syn::ExprLet) {
                if self.from(&expr.expr) {
                    self.hold(&expr.pat);
                }
                syn::visit::visit_expr_let(self, expr);
            }
            fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
                if self.from(&call.receiver) {
                    for arg in &call.args {
                        if let syn::Expr::Closure(closure) = arg {
                            for input in &closure.inputs {
                                self.hold(input);
                            }
                        }
                    }
                }
                syn::visit::visit_expr_method_call(self, call);
            }
        }
        let mut taint = Taint {
            held: sources.into_iter().collect(),
            grew: true,
        };
        while taint.grew {
            taint.grew = false;
            body.walk(&mut taint);
        }
        taint.held
    }

    /// The `err_internal` calls in `body` whose cause is in `held`.
    fn forwarded(body: Body<'_>, held: &std::collections::HashSet<String>) -> usize {
        struct Calls<'h> {
            held: &'h std::collections::HashSet<String>,
            found: usize,
        }
        impl<'ast> Visit<'ast> for Calls<'_> {
            fn visit_item_fn(&mut self, _: &'ast syn::ItemFn) {}
            fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
                if let syn::Expr::Path(callee) = &*call.func {
                    if names_err_internal(&callee.path)
                        && call.args.iter().nth(1).is_some_and(|cause| {
                            mentions(cause).iter().any(|i| self.held.contains(i))
                        })
                    {
                        self.found += 1;
                    }
                }
                syn::visit::visit_expr_call(self, call);
            }
        }
        let mut calls = Calls { held, found: 0 };
        body.walk(&mut calls);
        calls.found
    }

    struct Finder(Vec<Evasion>);

    impl Finder {
        fn function(&mut self, name: String, sig: &syn::Signature, body: &syn::Block) {
            let generics: Vec<String> = sig
                .generics
                .type_params()
                .map(|param| param.ident.to_string())
                .collect();
            let sources = sig
                .inputs
                .iter()
                .filter_map(|input| match input {
                    syn::FnArg::Typed(arg) if carries_an_error(&arg.ty, &generics) => {
                        Some(binds(&arg.pat))
                    }
                    _ => None,
                })
                .flatten()
                .collect();
            let body = Body::Block(body);
            if forwarded(body, &tainted(body, sources)) > 0 {
                self.0.push(Evasion::Wrapper(name));
            }
        }
    }

    impl<'ast> Visit<'ast> for Finder {
        fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
            if !is_cfg_test(&item.attrs) {
                syn::visit::visit_item_mod(self, item);
            }
        }
        fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
            if !is_cfg_test(&item.attrs) {
                syn::visit::visit_item_impl(self, item);
            }
        }
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            if !is_cfg_test(&item.attrs) {
                syn::visit::visit_item_use(self, item);
            }
        }
        fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
            if !is_cfg_test(&item.attrs) {
                self.function(item.sig.ident.to_string(), &item.sig, &item.block);
                syn::visit::visit_item_fn(self, item);
            }
        }
        fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
            if !is_cfg_test(&item.attrs) {
                self.function(item.sig.ident.to_string(), &item.sig, &item.block);
                syn::visit::visit_impl_item_fn(self, item);
            }
        }
        fn visit_local(&mut self, local: &'ast syn::Local) {
            if let (Some(init), syn::Pat::Ident(name)) = (&local.init, &local.pat) {
                if let syn::Expr::Closure(closure) = &*init.expr {
                    let sources = closure.inputs.iter().flat_map(binds).collect();
                    let body = Body::Expr(&closure.body);
                    if forwarded(body, &tainted(body, sources)) > 0 {
                        self.0.push(Evasion::Wrapper(name.ident.to_string()));
                    }
                }
            }
            syn::visit::visit_local(self, local);
        }
        fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
            if rename.ident == NAME {
                self.0.push(Evasion::Renamed(rename.rename.to_string()));
            }
        }
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            match &*call.func {
                // The callee is the one place the name may appear.
                syn::Expr::Path(callee) if names_err_internal(&callee.path) => {
                    for arg in &call.args {
                        self.visit_expr(arg);
                    }
                }
                _ => syn::visit::visit_expr_call(self, call),
            }
        }
        fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
            if names_err_internal(&expr.path) {
                self.0.push(Evasion::AsValue);
            }
            syn::visit::visit_expr_path(self, expr);
        }
        fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
            if !is_cfg_test(&item.attrs)
                && item.mac.path.is_ident("macro_rules")
                && item.mac.tokens.to_string().contains(NAME)
            {
                self.0.push(Evasion::Macro(
                    item.ident
                        .as_ref()
                        .map_or_else(String::new, ToString::to_string),
                ));
            }
        }
    }

    let file = syn::parse_file(src).unwrap_or_else(|error| panic!("unparseable source: {error}"));
    let mut finder = Finder(Vec::new());
    finder.visit_file(&file);
    finder.0
}

#[test]
fn gated_files_call_err_internal_only_by_name() {
    let mut found = Vec::new();
    let mut listed_and_found = Vec::new();
    for file in gated_scan().collect() {
        if gated_block(&file.rel).is_none() {
            continue;
        }
        for evasion in evasions(&file.text) {
            let listed = LISTED_WRAPPERS.iter().find(|(rel, name, _)| {
                *rel == file.rel && evasion == Evasion::Wrapper(name.to_string())
            });
            match listed {
                Some(entry) => listed_and_found.push(entry),
                None => found.push((file.rel.clone(), evasion)),
            }
        }
    }
    assert!(
        found.is_empty(),
        "these gated files reach `err_internal` in a way the tail inventory \
         cannot count: {found:#?}\n\
         Call `err_internal(label, cause)` by name where the error arises, or \
         route a database error through `crud::db_error_internal`. A wrapper \
         whose callers can only pass a fault of this process may be added to \
         LISTED_WRAPPERS with that reason."
    );
    for entry in LISTED_WRAPPERS {
        assert!(
            entry.2.starts_with("invariant: "),
            "{entry:?} must say why it is an invariant"
        );
        assert!(
            listed_and_found.contains(&entry),
            "{entry:?} is listed but is no longer a wrapper in that file; \
             delete the entry"
        );
    }
}

/// Each evasion is caught, and none of the shapes the gated files really use
/// is mistaken for one.
#[test]
fn the_evasion_gate_catches_each_way_around() {
    let caught = [
        "use crate::http::err_internal as x;\nfn f() {}\n",
        "use crate::http::{err_bad_request, err_internal as fail};\n",
        "fn f() { let fail = crate::http::err_internal; }\n",
        "fn f(r: Result<u8, E>) { r.map_err(err_internal); }\n",
        "macro_rules! fail { ($e:expr) => { err_internal(\"x\", $e) }; }\n",
        "fn fail(error: WaferError) -> OutputStream { err_internal(\"x\", error) }\n",
        "fn fail(e: impl std::fmt::Display) -> OutputStream { err_internal(\"x\", e) }\n",
        "fn fail<E: Display>(e: E) -> OutputStream { err_internal(\"x\", e) }\n",
        "fn respond(result: Result<u8, WaferError>) -> OutputStream {\n\
             match result { Ok(_) => ok(), Err(e) => err_internal(\"x\", e) }\n}\n",
        "fn respond(result: Result<u8, WaferError>) -> Result<u8, OutputStream> {\n\
             result.map_err(|e| err_internal(\"x\", e))\n}\n",
        "impl S { fn fail(&self, error: WaferError) -> OutputStream { err_internal(\"x\", error) } }\n",
        "fn f() { let fail = |e| err_internal(\"x\", e); }\n",
    ];
    for src in caught {
        assert_eq!(evasions(src).len(), 1, "not caught: {src}");
    }
    assert_eq!(
        evasions("use crate::http::err_internal as x;\n"),
        vec![Evasion::Renamed("x".to_string())]
    );

    let allowed = [
        // The shapes the products files use: a call where the error arises,
        // an inline `map_err` closure, a helper that decodes its argument.
        "use crate::http::{err_internal, ok_json};\n\
         fn a(ctx: &dyn Context) -> OutputStream {\n\
             match load(ctx) { Ok(v) => ok_json(&v), Err(e) => err_internal(\"x\", e) }\n}\n",
        "async fn b(ctx: &dyn Context, outcome: &str) -> Result<u16, OutputStream> {\n\
             fee(ctx).await.map_err(|error| err_internal(&format!(\"{outcome}\"), error))\n}\n",
        "fn product_json(record: &db::Record) -> OutputStream {\n\
             match View::from_record(record) { Ok(v) => ok_json(&v), Err(e) => crate::http::err_internal(\"x\", e) }\n}\n",
        // A wrapper in a test is not production code.
        "#[cfg(test)]\nmod tests { fn fail(error: WaferError) -> OutputStream { err_internal(\"x\", error) } }\n",
        // A helper forwarding to the door is the fix, not an evasion.
        "fn fail(error: WaferError) -> OutputStream { crud::db_error_internal(error, \"x\") }\n",
    ];
    for src in allowed {
        assert_eq!(evasions(src), Vec::new(), "wrongly caught: {src}");
    }
}

// ---------------------------------------------------------------------------
// Where the inventory does not run yet
// ---------------------------------------------------------------------------

/// Every block outside [`GATED_BLOCKS`] that still calls
/// `err_internal(label, cause)`, with how many of its files do. Nothing there
/// is read per site: any of those calls may be a database failure answering
/// 500 where a WRAP denial should be a 403 and a quota a 429.
///
/// A block leaves this list by joining `GATED_BLOCKS`, its tails inventoried.
/// The counts are exact both ways, so converting a file lowers its block's
/// count here and a new file with a tail raises it — either way this list is
/// edited, and the backlog it states stays true. A top-level file under
/// `src/blocks/` is its own entry; `crud.rs` is the door and is not listed.
const NOT_YET_GATED: &[(&str, usize)] = &[
    ("admin", 9),
    ("auth", 1),
    ("auth_ui", 13),
    ("dev", 6),
    ("fastembed.rs", 1),
    ("files", 6),
    ("legalpages", 1),
    ("llm", 5),
    ("messages", 1),
    ("signal", 1),
    ("tickets", 1),
    ("userportal", 3),
    ("vector", 1),
];

/// The block a `src/blocks`-relative path belongs to: its first component.
fn block_of(rel: &str) -> &str {
    rel.split('/').next().unwrap_or(rel)
}

#[test]
fn not_yet_gated_is_the_whole_backlog() {
    let mut files_with_tails: std::collections::BTreeMap<String, usize> = Default::default();
    for file in gated_scan().collect() {
        if file.rel == THE_DOOR || gated_block(&file.rel).is_some() {
            continue;
        }
        if !err_internal_labels(&file.text).is_empty() {
            *files_with_tails
                .entry(block_of(&file.rel).to_string())
                .or_default() += 1;
        }
    }
    let listed: std::collections::BTreeMap<String, usize> = NOT_YET_GATED
        .iter()
        .map(|(block, files)| (block.to_string(), *files))
        .collect();
    assert_eq!(
        files_with_tails, listed,
        "NOT_YET_GATED must state exactly the ungated blocks that still call \
         `err_internal(label, cause)`, and in how many files (left: the tree, \
         right: the list)"
    );
}
