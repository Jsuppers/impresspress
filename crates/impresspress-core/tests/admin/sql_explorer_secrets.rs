//! The admin SQL explorer must not read a table that stores credential
//! material.
//!
//! Both explorer surfaces — `POST /b/admin/api/database/query` (JSON) and
//! `POST /b/admin/database/query` (the SSR SQL editor) — are driven here
//! through `TestContext::dispatch*`, which is the production request path
//! (`routing::route_to_block`, access gate included). Rows are staged in the
//! real tables through the real migrations, so a passing assertion is about
//! the endpoint an operator reaches, not about a helper it happens to call.
//!
//! # Why the boundary sits at the validator and not at the result set
//!
//! [`column_name_masking_has_nothing_to_key_on`] is the load-bearing test of
//! this file. `db::query_raw` hands back records keyed by the **returned**
//! column name, which the query author chooses. A mask keyed on
//! `(table, column)` therefore dies to `SELECT value AS v`, and to every
//! expression, subquery, join and CTE shape below it — it would advertise a
//! guarantee it cannot keep. Refusing the query before it runs is the only
//! rule the query text cannot be reshaped around.

use std::collections::{BTreeSet, HashMap};

use impresspress_core::{
    blocks::admin::{migrations, AdminBlock},
    platform_state::variables::{self, NewVariable},
    secret_tables::SECRET_TABLES,
    test_support::{admin_msg, output_http_json, output_http_status, TestContext},
};
use serde_json::json;
use wafer_core::clients::database as db;

/// The value staged in `impresspress__admin__variables`. Distinctive enough
/// that a substring check over a whole response body is meaningful.
const SECRET: &str = "s3cr3t-jwt-signing-value-0192837465";

/// Stage one sensitive variables row through the module that owns the table,
/// then prove by direct SQL that the plaintext really is sitting in `value` —
/// otherwise a refusal below would be protecting nothing.
async fn stage_secret_variable(ctx: &TestContext) {
    migrations::apply(ctx)
        .await
        .expect("apply admin migrations");
    variables::insert(
        ctx,
        NewVariable {
            key: "WAFER_RUN__AUTH__JWT_SECRET".to_string(),
            value: SECRET.to_string(),
            name: String::new(),
            description: String::new(),
            warning: String::new(),
            sensitive: true,
            updated_by: "test".to_string(),
            block: None,
        },
    )
    .await
    .expect("insert variables row");

    let rows = db::query_raw(
        ctx,
        "SELECT value FROM impresspress__admin__variables WHERE key = 'WAFER_RUN__AUTH__JWT_SECRET'",
        &[],
    )
    .await
    .expect("read back the staged row");
    assert_eq!(
        rows.first()
            .and_then(|r| r.data.get("value"))
            .and_then(|v| v.as_str()),
        Some(SECRET),
        "the fixture did not put the plaintext in the column under test"
    );
}

/// A `TestContext` with the admin block registered, so `dispatch*` routes to
/// the real handlers rather than 404ing.
async fn explorer_ctx() -> TestContext {
    let mut ctx = TestContext::new().await;
    ctx.register_block("impresspress/admin", std::sync::Arc::new(AdminBlock::new()));
    stage_secret_variable(&ctx).await;
    ctx
}

/// Run `query` through the JSON explorer endpoint and return
/// `(http status, response body as text)`.
async fn run_json(ctx: &TestContext, query: &str) -> (u16, String) {
    let out = ctx
        .dispatch_json(
            admin_msg("create", "/b/admin/api/database/query"),
            &json!({ "query": query }),
        )
        .await;
    let parts = wafer_block::http_codec::collect_http_response(out).await;
    (
        parts.status,
        String::from_utf8_lossy(&parts.body).into_owned(),
    )
}

/// Every query shape that reaches `impresspress__admin__variables.value`
/// without spelling `value` as the returned column name — the reason a
/// result-set mask cannot work.
fn evasion_shapes() -> Vec<(&'static str, String)> {
    let t = "impresspress__admin__variables";
    vec![
        ("plain", format!("SELECT key, value FROM {t}")),
        ("alias", format!("SELECT value AS v FROM {t}")),
        (
            "expression",
            format!("SELECT substr(value, 1, 20) FROM {t}"),
        ),
        ("hex", format!("SELECT hex(value) FROM {t}")),
        ("concat", format!("SELECT value || '' FROM {t}")),
        ("subquery", format!("SELECT * FROM (SELECT value FROM {t})")),
        (
            "join",
            format!("SELECT v.value FROM {t} v JOIN {t} w ON v.id = w.id"),
        ),
        (
            "cte",
            format!("WITH x AS (SELECT value FROM {t}) SELECT * FROM x"),
        ),
        (
            "group_concat",
            format!("SELECT group_concat(value) FROM {t}"),
        ),
        ("quoted", format!("SELECT value FROM \"{t}\"")),
        ("bracketed", format!("SELECT value FROM [{t}]")),
        ("backticked", format!("SELECT value FROM `{t}`")),
        ("schema_qualified", format!("SELECT value FROM main.{t}")),
        (
            "schema_qualified_quoted",
            format!("SELECT value FROM main.\"{t}\""),
        ),
        (
            "uppercase",
            format!("SELECT VALUE FROM {}", t.to_uppercase()),
        ),
        ("mixed_case", format!("SELECT value FROM {}", mixed_case(t))),
        ("explain", format!("EXPLAIN SELECT value FROM {t}")),
        ("pragma", format!("PRAGMA table_info({t})")),
    ]
}

/// `impresspress__admin__variables` with alternating capitals — SQLite folds
/// identifier case, so this names the same table.
fn mixed_case(s: &str) -> String {
    s.chars()
        .enumerate()
        .map(|(i, c)| {
            if i % 2 == 0 {
                c.to_ascii_uppercase()
            } else {
                c
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Why the mask has to be at the validator
// ---------------------------------------------------------------------------

/// The evidence for rejecting result-set masking: the records the explorer
/// serialises are keyed by the name the QUERY chose, and the plaintext rides
/// under it.
///
/// This drives `db::query_raw` — the exact call `handle_query` makes — rather
/// than the endpoint, because the claim under test is about what that call
/// returns, which is what a masking layer would have had to work from.
#[tokio::test]
async fn column_name_masking_has_nothing_to_key_on() {
    let ctx = TestContext::new().await;
    stage_secret_variable(&ctx).await;

    let rows = db::query_raw(
        &ctx,
        "SELECT value AS v FROM impresspress__admin__variables",
        &[],
    )
    .await
    .expect("aliased select");
    let row = rows.first().expect("one row");
    assert!(
        !row.data.contains_key("value"),
        "a (table, column) mask would look for `value` and find nothing: {:?}",
        row.data.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        row.data.get("v").and_then(|v| v.as_str()),
        Some(SECRET),
        "the secret arrives under the alias the query picked"
    );

    let rows = db::query_raw(
        &ctx,
        "SELECT substr(value, 1, 12) FROM impresspress__admin__variables",
        &[],
    )
    .await
    .expect("expression select");
    let leaked: Vec<String> = rows
        .first()
        .expect("one row")
        .data
        .values()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert!(
        leaked.iter().any(|v| SECRET.starts_with(v.as_str())),
        "an expression leaks a prefix of the secret under a computed column \
         name, so even an exact-value comparison would not catch it: {leaked:?}"
    );
}

// ---------------------------------------------------------------------------
// The refusal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_evasion_shape_is_refused_and_leaks_nothing() {
    let ctx = explorer_ctx().await;
    for (name, query) in evasion_shapes() {
        let (status, body) = run_json(&ctx, &query).await;
        assert_eq!(status, 403, "{name}: {query}\nbody: {body}");
        assert!(
            !body.contains(SECRET),
            "{name}: the response carried the secret: {body}"
        );
    }
}

#[tokio::test]
async fn the_refusal_names_the_table_and_where_to_go_instead() {
    let ctx = explorer_ctx().await;
    let (_status, body) = run_json(
        &ctx,
        "SELECT key, value FROM impresspress__admin__variables",
    )
    .await;
    assert!(
        body.contains("impresspress__admin__variables"),
        "the refusal must say which table it refused: {body}"
    );
    assert!(
        body.contains("/b/admin/variables"),
        "the refusal must point at the page that serves this need: {body}"
    );
}

#[tokio::test]
async fn the_ssr_sql_editor_refuses_the_same_queries() {
    let ctx = explorer_ctx().await;
    let body = "query=SELECT+value+FROM+impresspress__admin__variables";
    let mut msg = admin_msg("create", "/b/admin/database/query");
    msg.set_meta("req.content_type", "application/x-www-form-urlencoded");
    let out = ctx
        .dispatch_with_input(
            msg,
            wafer_run::InputStream::from_bytes(body.as_bytes().to_vec()),
        )
        .await;
    let parts = wafer_block::http_codec::collect_http_response(out).await;
    let rendered = String::from_utf8_lossy(&parts.body).into_owned();
    assert!(
        !rendered.contains(SECRET),
        "the SSR editor rendered the secret: {rendered}"
    );
    assert!(
        rendered.contains("/b/admin/variables"),
        "the SSR editor must render the same refusal the API returns: {rendered}"
    );
}

#[tokio::test]
async fn every_registered_secret_table_is_refused() {
    let ctx = explorer_ctx().await;
    for entry in SECRET_TABLES {
        let (status, body) = run_json(&ctx, &format!("SELECT * FROM {}", entry.table)).await;
        assert_eq!(status, 403, "{} was queryable\nbody: {body}", entry.table);
        assert!(
            body.contains(entry.table),
            "{}: the refusal did not name it: {body}",
            entry.table
        );
    }
}

/// Postgres can spell an identifier without its own characters appearing in
/// the statement, which is the one hole in a substring test. The explorer
/// refuses the syntax outright rather than leaving it open.
#[tokio::test]
async fn postgres_unicode_escaped_identifiers_are_refused() {
    let ctx = explorer_ctx().await;
    // `impresspress__admin__variable` + `\0073` (`s`) — the config store,
    // spelled so that `impresspress__admin__variables` appears nowhere.
    let query = "SELECT value FROM U&\"impresspress__admin__variable\\0073\"";
    assert!(
        !query.contains("impresspress__admin__variables"),
        "the fixture must not contain the literal name, or it proves nothing"
    );
    let (status, body) = run_json(&ctx, query).await;
    assert_eq!(status, 403, "body: {body}");
    assert!(!body.contains(SECRET), "body: {body}");
}

// ---------------------------------------------------------------------------
// What the refusal must NOT cost
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ordinary_tables_are_still_queryable() {
    let ctx = explorer_ctx().await;
    let mut data: HashMap<String, serde_json::Value> = HashMap::new();
    data.insert("id".into(), json!("role-1"));
    data.insert("name".into(), json!("admin"));
    data.insert("description".into(), json!(""));
    data.insert("permissions".into(), json!("[]"));
    data.insert("is_system".into(), json!(1));
    data.insert("created_at".into(), json!("2026-01-01T00:00:00Z"));
    data.insert("updated_at".into(), json!("2026-01-01T00:00:00Z"));
    db::create(&ctx, "impresspress__admin__roles", data)
        .await
        .expect("stage a roles row");

    let (status, body) = run_json(&ctx, "SELECT name FROM impresspress__admin__roles").await;
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("admin"), "body: {body}");

    let value = output_http_json(
        ctx.dispatch_json(
            admin_msg("create", "/b/admin/api/database/query"),
            &json!({ "query": "SELECT 1 AS one" }),
        )
        .await,
    )
    .await;
    assert_eq!(value["row_count"], json!(1));
    assert_eq!(
        output_http_status(
            ctx.dispatch_json(
                admin_msg("create", "/b/admin/api/database/query"),
                &json!({ "query": "SELECT 1 AS one" }),
            )
            .await,
        )
        .await,
        200
    );
}

// ---------------------------------------------------------------------------
// The boundary is derived, not hand-kept
// ---------------------------------------------------------------------------

/// Columns whose NAME says "credential" but whose content is not one, with the
/// reason each is cleared.
///
/// The scan below is deliberately over-broad: it matches on the naming
/// convention alone, so every digest column in the schema lands in front of it.
/// That is the point — a new `*_hash`, `*token*`, `*secret*`, `*password*` or
/// `*verifier*` column, or a new table carrying a `sensitive` flag, has to be
/// classified by a person. Adding a row here is how you record "looked at it,
/// it is not a credential"; the alternative is
/// `impresspress_core::secret_tables::SECRET_TABLES`.
const CLEARED_COLUMNS: &[(&str, &str, &str)] = &[
    (
        "impresspress__admin__block_settings",
        "current_hash",
        "digest of a block's migration set, for change detection",
    ),
    (
        "impresspress__admin__block_settings",
        "blessed_hash",
        "the migration digest an operator accepted",
    ),
    (
        "impresspress__admin__block_settings",
        "seed_defaults_hash",
        "digest of the seed defaults already applied",
    ),
    (
        "impresspress__products__checkout_presets",
        "configuration_hash",
        "digest of a saved checkout configuration",
    ),
    (
        "impresspress__products__payment_links",
        "configuration_hash",
        "digest of the configuration a payment link was minted for",
    ),
    (
        "impresspress__tickets__tickets",
        "dedupe_hash",
        "digest of an inbound message, for idempotency",
    ),
];

/// `(table, column)` for every column in every block migration whose name
/// matches the credential convention, plus every column of a table that
/// carries a `sensitive` flag.
///
/// The `sensitive` clause is what catches
/// `impresspress__admin__variables.value`, whose name says nothing: a
/// `sensitive` column is the schema's own statement that rows of this table
/// may hold a secret. That is the same column `cache_key::row_is_sensitive`
/// reads (paired with `key` by `sensitive_check_columns`) to keep secrets out
/// of the KV cache.
fn credential_shaped_columns() -> BTreeSet<(String, String)> {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let blocks_dir = manifest_dir.join("src/blocks");
    let mut tables_with_sensitive_flag: BTreeSet<String> = BTreeSet::new();
    let mut all_columns: Vec<(String, String)> = Vec::new();

    let blocks = std::fs::read_dir(&blocks_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", blocks_dir.display()));
    for block in blocks {
        let dir = block.expect("dir entry").path().join("migrations");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("sql") {
                continue;
            }
            let sql = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            scan_sql(&sql, &mut all_columns, &mut tables_with_sensitive_flag);
        }
    }

    assert!(
        all_columns.len() > 200,
        "the migration column scan found {} columns — it lost its way",
        all_columns.len()
    );
    all_columns
        .into_iter()
        .filter(|(table, column)| {
            looks_like_a_credential(column) || tables_with_sensitive_flag.contains(table)
        })
        .collect()
}

/// Collect `(table, column)` for one migration file.
///
/// Handles the two shapes the migrations use: a `CREATE TABLE … ( … )` block
/// whose body lines are `<name> <TYPE> …`, and `ALTER TABLE <t> ADD COLUMN
/// [IF NOT EXISTS] <c> …`, whose clause may sit on the `ALTER TABLE` line or
/// on the line after it (admin migration 003 wraps; the products and auth ones
/// do not).
fn scan_sql(
    sql: &str,
    all_columns: &mut Vec<(String, String)>,
    tables_with_sensitive_flag: &mut BTreeSet<String>,
) {
    let mut current: Option<String> = None;
    let mut altering: Option<String> = None;
    for line in sql.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ALTER TABLE") {
            let table = identifier(rest.trim_start());
            match add_column_name(line) {
                Some(col) => all_columns.push((table, col)),
                // The clause wrapped: remember the table for the next line.
                None => altering = Some(table),
            }
            current = None;
            continue;
        }
        if let Some(table) = altering.take() {
            if let Some(col) = add_column_name(line) {
                all_columns.push((table, col));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("CREATE TABLE") {
            let rest = rest.trim_start();
            let rest = rest
                .strip_prefix("IF NOT EXISTS")
                .map_or(rest, str::trim_start);
            current = Some(identifier(rest));
            continue;
        }
        let Some(table) = current.clone() else {
            continue;
        };
        if line.starts_with(')') {
            current = None;
            continue;
        }
        // A column declaration is `<name> <TYPE> …`. A table constraint
        // (`UNIQUE (a, b)`, `PRIMARY KEY (…)`) fails the type check below, and
        // a comment line fails the identifier check.
        let mut words = line.split_whitespace();
        let (Some(name), Some(ty)) = (words.next(), words.next()) else {
            continue;
        };
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let ty = ty.trim_end_matches(',').to_ascii_uppercase();
        if !matches!(
            ty.as_str(),
            "TEXT" | "BLOB" | "BYTEA" | "INTEGER" | "BIGINT" | "REAL" | "BOOLEAN" | "JSONB"
        ) {
            continue;
        }
        if name.eq_ignore_ascii_case("sensitive") {
            tables_with_sensitive_flag.insert(table.clone());
        }
        all_columns.push((table, name.to_string()));
    }
}

/// The leading identifier of `rest`, stopping at whitespace or `(`.
fn identifier(rest: &str) -> String {
    rest.chars()
        .take_while(|c| !c.is_whitespace() && *c != '(')
        .collect()
}

/// The column name of an `ADD COLUMN [IF NOT EXISTS] <name> …` clause.
fn add_column_name(line: &str) -> Option<String> {
    let at = line.to_ascii_uppercase().find("ADD COLUMN")?;
    let rest = line[at + "ADD COLUMN".len()..].trim_start();
    let rest = match rest.get(.."IF NOT EXISTS".len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case("IF NOT EXISTS") => {
            rest["IF NOT EXISTS".len()..].trim_start()
        }
        _ => rest,
    };
    Some(identifier(rest))
}

/// The naming convention: a column whose name says it carries a secret, a
/// bearer token, or a digest of one.
fn looks_like_a_credential(column: &str) -> bool {
    let c = column.to_ascii_lowercase();
    ["secret", "password", "credential", "token", "verifier"]
        .iter()
        .any(|needle| c.contains(needle))
        || c.ends_with("_hash")
}

#[test]
fn every_credential_shaped_column_is_either_refused_or_cleared() {
    let refused: BTreeSet<&str> = SECRET_TABLES.iter().map(|e| e.table).collect();
    let cleared: BTreeSet<(&str, &str)> = CLEARED_COLUMNS
        .iter()
        .map(|(table, column, _why)| (*table, *column))
        .collect();

    let unclassified: Vec<(String, String)> = credential_shaped_columns()
        .into_iter()
        .filter(|(table, column)| {
            !refused.contains(table.as_str())
                && !cleared.contains(&(table.as_str(), column.as_str()))
        })
        .collect();

    assert!(
        unclassified.is_empty(),
        "credential-shaped columns nobody has classified: {unclassified:?} — either add the \
         table to `secret_tables::SECRET_TABLES` or record in CLEARED_COLUMNS why its contents \
         are not a credential"
    );
}

/// The other half of the closed set: a registered table must actually have the
/// columns its entry claims, and a cleared column must still exist. Either one
/// going stale is how a boundary quietly stops covering what it names.
#[test]
fn the_registry_and_the_clearances_still_describe_the_schema() {
    let shaped = credential_shaped_columns();
    for entry in SECRET_TABLES {
        for column in entry.columns {
            assert!(
                shaped.contains(&(entry.table.to_string(), (*column).to_string())),
                "{}.{column} is registered as credential-bearing but the migrations no longer \
                 declare it",
                entry.table
            );
        }
    }
    for (table, column, _why) in CLEARED_COLUMNS {
        assert!(
            shaped.contains(&((*table).to_string(), (*column).to_string())),
            "{table}.{column} is cleared but the migrations no longer declare it"
        );
    }
}

/// The tables a reviewer would expect to be refused and deliberately are not,
/// stated so the boundary is not read as "everything auth touches".
#[test]
fn deliberately_readable_tables_stay_readable() {
    let refused: BTreeSet<&str> = SECRET_TABLES.iter().map(|e| e.table).collect();
    for (table, why) in [
        (
            "wafer_run__auth__jwt_blocklist",
            "revoked `jti`s: identifiers of tokens, not tokens",
        ),
        ("wafer_run__auth__orgs", "org names and verification refs"),
        ("wafer_run__auth__rate_limits", "counters keyed by bucket"),
        (
            "impresspress__admin__block_settings",
            "per-block enable flag plus migration-state digests",
        ),
    ] {
        assert!(
            !refused.contains(table),
            "{table} became refused; if that is right, drop this row and say why ({why})"
        );
    }
}

/// Selecting a refused table in the SQL editor must not prefill a query whose
/// only outcome is a 403 — the panel says why and where to go instead.
#[tokio::test]
async fn the_sql_editor_does_not_prefill_a_query_it_will_refuse() {
    let ctx = explorer_ctx().await;
    let table = variables::TABLE;

    let mut msg = admin_msg("retrieve", "/b/admin/database");
    msg.set_meta("req.query.table", table);
    msg.set_meta("req.query.tab", "sql");
    let parts = wafer_block::http_codec::collect_http_response(ctx.dispatch(msg).await).await;
    let page = String::from_utf8_lossy(&parts.body).into_owned();

    // The panel really rendered — without this the two assertions below pass
    // on a 404 body, which is how a page test says nothing at all.
    assert!(
        page.contains("db-sql__input"),
        "the SQL panel did not render"
    );
    assert!(
        !page.contains(&format!("SELECT * FROM {table} LIMIT 100;")),
        "the editor prefilled a query the validator refuses"
    );
    assert!(
        page.contains("/b/admin/variables"),
        "the editor must name the surface that serves this table: {page}"
    );

    // An ordinary table still gets its prefill: the note is scoped to the
    // refused set, not to the panel.
    let mut msg = admin_msg("retrieve", "/b/admin/database");
    msg.set_meta("req.query.table", "impresspress__admin__roles");
    msg.set_meta("req.query.tab", "sql");
    let parts = wafer_block::http_codec::collect_http_response(ctx.dispatch(msg).await).await;
    let page = String::from_utf8_lossy(&parts.body).into_owned();
    assert!(
        page.contains("SELECT * FROM impresspress__admin__roles LIMIT 100;"),
        "an ordinary table lost its prefill"
    );
}
