//! The tables that hold authentication material, and the one rule the admin
//! SQL explorer applies to them: a query that names one is refused before it
//! runs.
//!
//! # What this is and is not
//!
//! It is **containment, not privilege separation.** Every caller who can reach
//! the explorer is an admin, and an admin can still *change* every one of
//! these values through the surfaces that own them — the Variables page writes
//! the config store, the Users page disables an account. Nothing here is a
//! defence against a hostile administrator, and reading it as one would be
//! reading it wrong.
//!
//! What it stops is one admin session being enough to exfiltrate every
//! credential in the database in a single request: a screen share, a browser
//! history entry, a proxy log, an over-the-shoulder read, a support call where
//! someone is asked to "run this query and paste the output". Those are the
//! ways a deployment's secrets actually escape, and until this existed the
//! shortest path to all of them was one `SELECT`.
//!
//! # Why this is a table-level refusal and not a masked column
//!
//! Every other surface that can publish a stored secret redacts the value it
//! serves, through the single [`crate::util::is_sensitive_key`] predicate. The
//! SQL explorer cannot join that funnel. `wafer_core::clients::database::
//! query_raw` returns records keyed by the column name **the query chose**, so
//! a mask keyed on `(table, column)` is defeated by `SELECT value AS v` — and,
//! since an expression need not preserve the value either, an exact-value
//! comparison is defeated by `SELECT substr(value, 1, 20)`. A mask there would
//! advertise a guarantee it cannot keep, which is worse than no mask at all,
//! because reviewers and operators would rely on it.
//! `tests/admin/sql_explorer_secrets.rs` holds the demonstration.
//!
//! Refusing the query is the only rule the query text cannot be reshaped
//! around: nothing is executed, so aliasing, expressions, subqueries, joins
//! and CTEs have nothing to act on.
//!
//! # What counts as a secret table
//!
//! A table belongs here when it stores material that authenticates someone —
//! whether in the clear (`variables.value`, `provider_links.access_token`,
//! `oauth_pkce_states.code_verifier`, `cloud_shares.token`) or as a digest of
//! it (`local_credentials.password_hash`, and the SHA-256 columns of
//! `tokens`, `personal_access_tokens`, `api_keys`, `bootstrap_tokens`,
//! `users.verification_token` and `purchases.receipt_token_hash`).
//!
//! The digests are included deliberately. A boundary drawn at "plaintext only"
//! would have to re-judge each digest's crackability every time the schema or
//! the hash parameters change — `local_credentials.password_hash` is Argon2id
//! at parameters `NICE_TO_HAVE.md` already records as tuned down for Workers —
//! and a rule that needs re-judging is a rule that rots. "No query may name a
//! table that stores authentication material" is an invariant a reviewer can
//! check by reading the schema.
//!
//! Tables that are *near* credentials but hold none are deliberately left
//! readable, and `tests/admin/sql_explorer_secrets.rs` records why for each:
//! `sessions` (see below), `jwt_blocklist` (revoked `jti`s, which are
//! identifiers of tokens rather than tokens), `block_settings`
//! (`current_hash`/`blessed_hash`/`seed_defaults_hash` are migration-state
//! digests), `orgs`, `rate_limits`.
//!
//! `wafer_run__auth__sessions` is the one worth spelling out, because its name
//! argues for inclusion and its schema does not. Migration
//! `012_sessions_family` DROPPED the table and recreated it keyed on the
//! refresh-rotation `family`: the columns are `family`, `user_id`,
//! `auth_method` and timestamps, and the `token_hash` that made it look like
//! bearer storage is gone. Its own repo module says it outright — "a row here
//! is a *device*, not a credential… Nothing authenticates against this table".
//! Refusing it would cost an operator an ad hoc read of a device list for no
//! security gain. The first draft of this module registered it anyway, on a
//! column that had not existed for a migration; the guard test in
//! `sql_explorer_secrets.rs` now models `DROP TABLE` precisely so that class
//! of claim fails rather than reads plausibly.
//!
//! # Two that were considered and left readable
//!
//! Both hold third-party data rather than this deployment's own credentials,
//! so neither is authentication material under the rule above. Naming them
//! here because "nobody mentioned it" and "somebody decided" look identical
//! six months later.
//!
//! * `impresspress__products__provider_operations.request_json` /
//!   `response_json` sound like raw Stripe bodies and are not: the only
//!   `ensure` call site writes the literal `{"version":1}`, and every
//!   `resolve_*` writes a summary this repo builds itself
//!   (`{id, status, amount_minor, livemode, source}`). No provider payload,
//!   and so no capability, reaches either column.
//! * `impresspress__products__stripe_events.payload_base64` IS the raw webhook
//!   body, base64 of exactly what Stripe posted. It can carry customer PII and
//!   Stripe object ids. It is left readable because the explorer's value for
//!   debugging a payment is real and because none of it authenticates anyone
//!   *to this site* — but that is a narrower claim than "it holds no
//!   capability", which this PR did not establish either way, and
//!   `NICE_TO_HAVE.md` records the open question rather than letting the
//!   omission read as a finding.
//!
//! Each entry names its table through the constant its owning repo module
//! declares, so a table that is ever renamed cannot drift out of this list
//! silently. The two whose owning module is behind a block feature
//! (`cloud_shares`, `purchases`) are the exception and are named as literals —
//! see the comment on [`CLOUD_SHARES_TABLE`] for why a `#[cfg]` would have
//! been the wrong answer, and for the tests that pin the literals to those
//! constants.
//!
//! # The cost, stated
//!
//! `users` and `purchases` are the two entries an operator will feel: they are
//! the tables worth browsing ad hoc, and each is here for exactly one column
//! (`verification_token`, `receipt_token_hash`). Both are already withheld
//! from the typed APIs that publish those rows — see the plain comments above
//! `AdminUserView` in `blocks::admin::contracts` and `PurchaseView` in
//! `blocks::products::contracts`, which call them credential material in those
//! words. The explorer was the surface that still echoed them, and the rows'
//! non-credential content is served by the Users page and the seller orders
//! page. A column-level refusal would cost less, but it would have to be right
//! about `SELECT *`, `count(*)`, and every future way of naming a column
//! indirectly — a rule with carve-outs, where a miss is silent. This one has
//! none.

use crate::{
    blocks::auth::repo::{
        api_keys, bootstrap_tokens, local_credentials, oauth_pkce, pats, provider_links, tokens,
        users,
    },
    platform_state::variables,
};

// The two tables whose owning module lives behind a block feature, named as
// literals rather than through their constants.
//
// A `#[cfg]` on the entries themselves would make the refusal a property of
// the BUILD, and the table is a property of the DATABASE. A deployment that
// once ran with `block-files` keeps `impresspress__files__cloud_shares`, live
// share tokens included, when it is next built without the block:
// `introspect_table_summaries` still lists it, the SQL explorer still reads
// it, and a `#[cfg]`-ed registry would have had nothing to say about it. The
// same holds for a build that never had the block but inherited someone
// else's D1.
//
// So the literal is the price of naming a table a build may not compile. It
// is pinned to the door's own constant by the two tests at the bottom of this
// file, which run in every build that HAS the module — so the anti-drift
// property the constant gives every other entry is kept, enforced by a test
// instead of by the type system. `tests/repo_door.rs` carries the matching
// `LITERAL_ALLOWED` entries with the same reason.
const CLOUD_SHARES_TABLE: &str = "impresspress__files__cloud_shares";
const PRODUCTS_PURCHASES_TABLE: &str = "impresspress__products__purchases";

/// One table the SQL explorer refuses, with the columns that put it here and
/// the admin surface that serves the same need safely.
pub struct SecretTable {
    /// The table name, taken from its owning repo module's `TABLE` constant
    /// (or, for a feature-gated module, from the pinned literal beside it).
    pub table: &'static str,
    /// The columns that hold authentication material. Recorded for the
    /// completeness test in `tests/admin/sql_explorer_secrets.rs`, which
    /// re-derives the set from the migration files and fails when a new
    /// credential-shaped column appears in a table nothing has classified.
    pub columns: &'static [&'static str],
    /// Where an admin should go instead, as one sentence appended to the
    /// refusal. The repo does not present dead ends (see `key_is_deletable`
    /// in `blocks::admin::pages::variables`, which hides a control rather than
    /// offering an inert one); a refusal with no alternative is the same
    /// failure in a different shape.
    pub instead: &'static str,
}

/// The admin page that serves configured values with secrets masked.
const VARIABLES_PAGE: &str =
    "Use the Variables page (/b/admin/variables), which serves these rows with their \
     secrets masked.";

/// The admin page for accounts and their sign-in state.
const USERS_PAGE: &str = "Use the Users page (/b/admin/users) for account state.";

/// Every table the explorer refuses.
pub const SECRET_TABLES: &[SecretTable] = &[
    // The config store. `value` holds whatever this deployment configured —
    // the JWT signing secret, OAuth client secrets, Stripe keys, SMTP
    // credentials, an unredeemed bootstrap admin token.
    SecretTable {
        table: variables::TABLE,
        columns: &["value"],
        instead: VARIABLES_PAGE,
    },
    // Argon2 verifier for a human-chosen password: the one credential here
    // whose digest is worth attacking offline.
    SecretTable {
        table: local_credentials::TABLE,
        columns: &["password_hash"],
        instead: USERS_PAGE,
    },
    // `verification_token` is `sha256_hex` of the user's email-verification
    // token — the capability that marks an address as proven. The typed
    // `GET /b/admin/api/users` view already withholds it by name; this is the
    // sibling surface that did not.
    SecretTable {
        table: users::TABLE,
        columns: &["verification_token"],
        instead: USERS_PAGE,
    },
    // A third-party OAuth access token in the clear — replayable against the
    // provider by anyone who reads it, with no involvement of this site.
    SecretTable {
        table: provider_links::TABLE,
        columns: &["access_token"],
        instead: USERS_PAGE,
    },
    // The PKCE verifier for a sign-in that is mid-flight. Short-lived and
    // single-use, which bounds the window but does not close it.
    SecretTable {
        table: oauth_pkce::TABLE,
        columns: &["code_verifier"],
        instead: USERS_PAGE,
    },
    // The bearer-material tables. Each stores SHA-256 of a token the client
    // holds, so a reader learns a digest rather than a credential — included
    // for the reason in the module docs, not because a digest is itself
    // replayable.
    SecretTable {
        table: tokens::TABLE,
        columns: &["token_hash"],
        instead: USERS_PAGE,
    },
    SecretTable {
        table: pats::TABLE,
        columns: &["token_hash"],
        instead: USERS_PAGE,
    },
    SecretTable {
        table: api_keys::TABLE,
        columns: &["key_hash"],
        instead: USERS_PAGE,
    },
    SecretTable {
        table: bootstrap_tokens::TABLE,
        columns: &["token_hash"],
        instead: USERS_PAGE,
    },
    // The capability in a public `/b/storage/direct/{token}` URL, stored in
    // the clear because the share handler looks it up by equality.
    SecretTable {
        table: CLOUD_SHARES_TABLE,
        columns: &["token"],
        instead: "Use the Storage page (/b/admin/storage) to manage shares.",
    },
    // `receipt_token_hash` is the digest of the guest receipt capability
    // issued at checkout: whoever holds the raw token reads the order's
    // status with no session. `PurchaseView` withholds it on every tier.
    SecretTable {
        table: PRODUCTS_PURCHASES_TABLE,
        columns: &["receipt_token_hash"],
        instead: "Use the seller orders page (/b/products/selling/orders) for order state.",
    },
];

impl SecretTable {
    /// The refusal an admin reads.
    pub fn refusal(&self) -> String {
        format!(
            "{} stores credential material and cannot be read through the SQL explorer. {}",
            self.table, self.instead
        )
    }
}

/// The first [`SECRET_TABLES`] entry `query` names, or `None`.
///
/// Matching is a case-insensitive substring test on the whole statement. Every
/// way of dressing an identifier up leaves the name itself intact —
/// `"quoted"`, `[bracketed]`, `` `backticked` ``, `main.qualified`, and any
/// case, since SQLite folds identifier case and these names are already
/// lowercase in Postgres. Over-matching is the only error it can make (a query
/// that merely mentions the name in a string literal is refused too), and
/// over-matching is the safe direction.
///
/// # The two assumptions it rests on
///
/// Neither is a law of SQL, and if either stops holding this function stops
/// covering the table it names.
///
/// 1. **No view, and no other indirection, stands over a refused table.** A
///    view is precisely a way to read a table without naming it, so one over
///    `…__variables` would read straight through this check. As of this
///    writing no migration in the tree contains `CREATE VIEW` (nor `CREATE
///    TRIGGER`, nor `ATTACH`), and the explorer cannot mint one because
///    `CREATE` and `ATTACH` are both on the validator's forbidden-keyword
///    list. Nothing enforces it beyond that: a migration that adds a view over
///    a refused table re-opens the gap silently, and whoever writes it has to
///    account for it here.
/// 2. **Identifiers appear in the statement as themselves.** The one construct
///    that breaks this is Postgres's `U&"…"` unicode-escaped identifier, which
///    can spell a name the bytes of the statement never contain;
///    [`rejects_unicode_escape`] is why it cannot reach here.
pub fn secret_table_named_in(query: &str) -> Option<&'static SecretTable> {
    let lowered = query.to_ascii_lowercase();
    SECRET_TABLES
        .iter()
        .find(|entry| lowered.contains(entry.table))
}

/// Whether `query` uses Postgres's unicode-escape syntax for an identifier or
/// a string constant (`U&"d\0061ta"` / `U&'d\0061ta'`).
///
/// It is refused outright, because it is the one spelling of a table name that
/// [`secret_table_named_in`]'s substring test cannot see: `U&"impresspress__
/// admin__variable\0073"` names the config store without the bytes
/// `impresspress__admin__variables` appearing anywhere in the statement.
/// SQLite has no such syntax, so this costs a SQLite deployment nothing, and
/// nothing an admin would type into a read-only explorer needs it on Postgres
/// either.
///
/// The quote character must follow `u&` immediately — that is what the syntax
/// requires — so an ordinary bitwise expression like `u & 3`, or even `u&3`,
/// is untouched.
pub fn rejects_unicode_escape(query: &str) -> bool {
    let lowered = query.to_ascii_lowercase();
    lowered.contains("u&\"") || lowered.contains("u&'")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config store, spelled through its door's constant rather than as a
    /// literal — `tests/repo_door.rs` is what makes that mandatory, and it is
    /// the same reason the registry above does it.
    fn store() -> &'static str {
        variables::TABLE
    }

    /// The `block-files` build pins the literal to the door's own constant.
    /// Without this the `#[cfg]`-free entry would be a second spelling of the
    /// table that could drift from the door.
    #[cfg(feature = "block-files")]
    #[test]
    fn the_cloud_shares_literal_matches_its_door_constant() {
        assert_eq!(
            CLOUD_SHARES_TABLE,
            crate::blocks::files::repo::shares::TABLE
        );
    }

    /// The same pin for the products door.
    #[cfg(feature = "block-products")]
    #[test]
    fn the_purchases_literal_matches_its_door_constant() {
        assert_eq!(
            PRODUCTS_PURCHASES_TABLE,
            crate::blocks::products::PURCHASES_TABLE
        );
    }

    #[test]
    fn the_config_store_is_registered_with_its_value_column() {
        let entry = secret_table_named_in(&format!("SELECT value FROM {}", store()))
            .expect("the variables table is registered");
        assert_eq!(entry.table, variables::TABLE);
        assert_eq!(entry.columns, &["value"]);
    }

    #[test]
    fn matching_ignores_case_and_identifier_quoting() {
        let t = store();
        for query in [
            format!("select * from {}", t.to_uppercase()),
            format!("select * from \"{t}\""),
            format!("select * from [{t}]"),
            format!("select * from `{t}`"),
            format!("select * from main.{t}"),
            format!("select * from public.\"{t}\""),
        ] {
            assert!(
                secret_table_named_in(&query).is_some(),
                "not matched: {query}"
            );
        }
    }

    #[test]
    fn an_unrelated_query_matches_nothing() {
        assert!(secret_table_named_in("SELECT * FROM impresspress__admin__roles").is_none());
        assert!(secret_table_named_in("SELECT 1").is_none());
    }

    #[test]
    fn unicode_escapes_are_detected_but_bitwise_and_is_not() {
        assert!(rejects_unicode_escape("SELECT * FROM U&\"foo\""));
        assert!(rejects_unicode_escape("SELECT u&'\\0061'"));
        assert!(!rejects_unicode_escape("SELECT u & 3 FROM t"));
        assert!(!rejects_unicode_escape("SELECT u&3 FROM t"));
    }

    #[test]
    fn the_refusal_names_the_table_and_an_alternative() {
        let entry = secret_table_named_in(&format!("SELECT * FROM {}", store())).unwrap();
        let message = entry.refusal();
        assert!(message.contains(variables::TABLE), "{message}");
        assert!(message.contains("/b/admin/variables"), "{message}");
    }
}
