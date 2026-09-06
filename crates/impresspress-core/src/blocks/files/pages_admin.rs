//! SSR admin pages for the files block.

use maud::{html, Markup};
use wafer_run::{context::Context, Message, OutputStream};

use super::repo;
use crate::{
    ui::{self, components, icons, shell::Crumb},
    util::{format_bytes, RecordExt},
};

/// Tabs navigation across the storage-admin sub-pages
/// (Overview / Buckets / Shares / Quotas). `active` matches the
/// crumb label so the active tab can be highlighted.
///
/// Designed to slot into `list_page`'s `filters` arg (the same slot the
/// Users tabs use), so the tab strip lives inside `.page--list` and picks
/// up the page padding consistently.
pub(crate) fn admin_tabs(active: &str) -> Markup {
    let items: &[(&str, &str)] = &[
        ("Overview", "/b/storage/admin/"),
        ("Buckets", "/b/storage/admin/buckets"),
        ("Shares", "/b/storage/admin/shares"),
        ("Quotas", "/b/storage/admin/quotas"),
    ];
    html! {
        div .tabs {
            @for (label, href) in items {
                a class={ "tab" @if *label == active { " active" } } href=(href) { (label) }
            }
        }
    }
}

async fn files_page<'a>(
    ctx: &dyn Context,
    title: &'a str,
    crumb_label: &'a str,
    subtitle: Option<&'a str>,
    content: Markup,
    msg: &Message,
) -> OutputStream {
    files_page_with_action(ctx, title, crumb_label, subtitle, None, content, msg).await
}

/// Admin storage shell. Thin wrapper over [`ui::shell_page`] that fixes the
/// nav to Admin and keeps the storage pages' single-crumb shape; tabs ride in
/// each caller's `list_page` `filters` slot (matching `/b/admin/users`).
async fn files_page_with_action<'a>(
    ctx: &dyn Context,
    title: &'a str,
    crumb_label: &'a str,
    subtitle: Option<&'a str>,
    primary_action: Option<Markup>,
    content: Markup,
    msg: &Message,
) -> OutputStream {
    ui::shell_page(
        ctx,
        msg,
        ui::Shell {
            title,
            nav: ui::NavKind::Admin,
            crumbs: vec![Crumb {
                label: crumb_label,
                href: None,
            }],
            subtitle,
            primary_action,
        },
        content,
    )
    .await
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct AdminStats {
    pub buckets: i64,
    pub files: i64,
    pub total_size_bytes: i64,
    pub shares: i64,
    pub quotas_count: i64,
}

/// Render the 4 stat cards (Buckets, Files, Total Size, Active Shares)
/// for the admin storage overview. Designed for the `list_page` template's
/// `filters` slot. Pure helper — no `Context` access.
pub fn render_admin_overview_stats(stats: &AdminStats) -> Markup {
    html! {
        div .stats-grid {
            (components::stat_card("Buckets", &stats.buckets.to_string(), icons::folder(), None))
            (components::stat_card("Files", &stats.files.to_string(), icons::file_text(), None))
            (components::stat_card("Total Size", &format_bytes(stats.total_size_bytes), icons::hard_drive(), None))
            (components::stat_card("Active Shares", &stats.shares.to_string(), icons::globe(), None))
        }
    }
}

/// Render the "create your first bucket" CTA for the empty admin storage
/// overview. Renders empty markup once at least one bucket exists — this is
/// a first-run nudge, not a permanent overview fixture. Links to the
/// Buckets tab, which owns the actual "+ New bucket" trigger (the modal +
/// `files-browser.js` bootstrap script) — no duplicate modal wiring here.
pub fn render_admin_overview_empty_cta(bucket_count: i64) -> Markup {
    if bucket_count > 0 {
        return html! {};
    }
    components::empty_state(
        icons::folder(),
        "Create your first bucket",
        "Buckets hold the files uploaded through Storage. Create one to get started.",
        Some(html! {
            a .btn .btn--primary .btn--md href="/b/storage/admin/buckets" { "+ New bucket" }
        }),
    )
}

/// Render the optional "X user(s) with custom quotas" hint card.
/// Returns an empty markup when `quotas_count == 0`. Pure helper.
pub fn render_admin_overview_quotas_hint(quotas_count: i64) -> Markup {
    if quotas_count <= 0 {
        return html! {};
    }
    html! {
        div .card .p-4 {
            p .text-muted .text-sm {
                (quotas_count) " user(s) with custom quotas configured."
            }
        }
    }
}

pub async fn overview(ctx: &dyn Context, msg: &Message) -> OutputStream {
    use crate::ui::templates::{list_page, PageHeader};

    let stats = load_admin_stats(ctx).await;

    // Tabs go in the `filters` slot (their padding gutter matches
    // /b/admin/users); stats live in the body. Keeping them in separate
    // slots prevents `.page-filters` (display:flex) from putting tabs
    // and the stats-grid side-by-side at wide viewports.
    let body = list_page(
        PageHeader {
            title: "",
            subtitle: None,
            primary_action: None,
        },
        Some(admin_tabs("Overview")),
        html! {
            (render_admin_overview_stats(&stats))
            (render_admin_overview_empty_cta(stats.buckets))
            (render_admin_overview_quotas_hint(stats.quotas_count))
        },
        None,
    );

    files_page(
        ctx,
        "Storage",
        "Overview",
        Some("File storage statistics"),
        body,
        msg,
    )
    .await
}

async fn load_admin_stats(ctx: &dyn Context) -> AdminStats {
    let buckets = repo::buckets::count_all(ctx).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e.message, "admin overview: bucket count failed");
        0
    });

    let files = repo::objects::count_completed(ctx)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e.message, "admin overview: files count failed");
            0
        });

    let shares = repo::shares::count_all(ctx).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e.message, "admin overview: shares count failed");
        0
    });

    let quotas_count = repo::quota::count_all(ctx).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e.message, "admin overview: quotas count failed");
        0
    });

    let total_size_bytes = repo::objects::sum_size_completed(ctx)
        .await
        .map(|s| s as i64)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e.message, "admin overview: total size sum failed");
            0
        });

    AdminStats {
        buckets,
        files,
        total_size_bytes,
        shares,
        quotas_count,
    }
}

// ---------------------------------------------------------------------------
// Column shaping
//
// The admin tables are narrow, so ids and timestamps are cut to a prefix.
// These are the exact `get(..n).unwrap_or(..)` calls the row decoders used
// to make inline, kept bit-for-bit: a value SHORTER than the cut renders the
// fallback rather than the value, because `str::get` returns `None` for an
// out-of-range (or non-char-boundary) index.
// ---------------------------------------------------------------------------

/// A user id cut to its first 8 bytes; an em dash when it is shorter.
fn short_id(id: &str) -> String {
    id.get(..8).unwrap_or("—").to_string()
}

/// An RFC 3339 timestamp cut to its `YYYY-MM-DD` prefix; empty when shorter.
fn short_date(ts: &str) -> String {
    ts.get(..10).unwrap_or("").to_string()
}

// ---------------------------------------------------------------------------
// Buckets
// ---------------------------------------------------------------------------

/// A render-side projection of [`repo::buckets::BucketRow`]: the owner id
/// and the timestamp truncated for the table's narrow columns. It holds no
/// decoding — `public` is the row's `bool`, decoded once in the repo (B13).
#[derive(Clone, Debug)]
pub struct AdminBucketRow {
    pub name: String,
    pub owner_short: String,
    pub public: bool,
    pub created_at_short: String,
}

impl From<&repo::buckets::BucketRow> for AdminBucketRow {
    fn from(row: &repo::buckets::BucketRow) -> Self {
        Self {
            name: row.name.clone(),
            owner_short: short_id(&row.created_by),
            public: row.public,
            created_at_short: short_date(&row.created_at),
        }
    }
}

/// Render the admin Buckets table (or empty state).
pub fn render_admin_buckets_table(rows: &[AdminBucketRow]) -> Markup {
    if rows.is_empty() {
        return html! {
            div .empty-state { p { "No buckets" } }
        };
    }
    html! {
        table .data-table {
            thead { tr {
                th { "Name" }
                th { "Owner" }
                th { "Public" }
                th { "Created" }
            } }
            tbody {
                @for r in rows {
                    tr data-bucket=(r.name) {
                        td data-label="Name" .font-medium { (r.name) }
                        td data-label="Owner" .text-muted .text-sm { (r.owner_short) }
                        td data-label="Public" {
                            (components::status_badge(if r.public { "public" } else { "private" }))
                        }
                        td data-label="Created" .text-muted .text-sm { (r.created_at_short) }
                    }
                }
            }
        }
    }
}

pub async fn buckets(ctx: &dyn Context, msg: &Message) -> OutputStream {
    use crate::ui::templates::{list_page, PageHeader};

    let rows: Vec<AdminBucketRow> = match repo::buckets::list_recent(ctx, 100).await {
        Ok(page) => page.rows.iter().map(AdminBucketRow::from).collect(),
        Err(e) => {
            tracing::warn!(error = %e.message, "admin bucket list failed");
            Vec::new()
        }
    };

    // Admin can create buckets the same way users do — re-use the
    // native <dialog> modal + JS from `pages_user`. The bootstrap
    // script with empty bucket/prefix is needed for the JS to wire
    // the "+ New bucket" trigger; without it the JS bails on init.
    let js_url = crate::ui::assets::files_browser_js_url();
    let body = list_page(
        PageHeader {
            title: "",
            subtitle: None,
            primary_action: None,
        },
        Some(admin_tabs("Buckets")),
        html! {
            (render_admin_buckets_table(&rows))
            (super::pages_user::render_new_bucket_modal())
            script type="application/json" id="files-browser-bootstrap" {
                "{}"
            }
            script src=(js_url) defer {}
        },
        None,
    );

    files_page_with_action(
        ctx,
        "Buckets",
        "Buckets",
        Some("All storage buckets"),
        Some(crate::ui::components::button(
            crate::ui::components::BtnVariant::Primary,
            crate::ui::components::CtrlSize::Sm,
            "+ New bucket",
            maud::PreEscaped(r#"type="button" data-action="open-new-bucket""#.to_string()),
        )),
        body,
        msg,
    )
    .await
}

// ---------------------------------------------------------------------------
// Shares
// ---------------------------------------------------------------------------

/// A render-side projection of [`repo::shares::ShareRow`]: the token and the
/// timestamps cut for the admin table's narrow columns. It holds no
/// decoding — `max_access_count` is the row's already-normalised `Option`.
#[derive(Clone, Debug)]
pub struct AdminShareRow {
    pub token_short: String,
    pub bucket: String,
    pub key: String,
    pub access_count: i64,
    pub max_access_count: Option<i64>,
    pub expires_short: Option<String>,
    pub owner_short: String,
}

impl From<&repo::shares::ShareRow> for AdminShareRow {
    fn from(row: &repo::shares::ShareRow) -> Self {
        Self {
            token_short: row.token.get(..12).unwrap_or("—").to_string(),
            bucket: row.bucket.clone(),
            key: row.key.clone(),
            access_count: row.access_count,
            max_access_count: row.max_access_count,
            expires_short: row
                .expires_at
                .as_deref()
                // The expiry column keeps the value when it is shorter than
                // the cut, unlike the id and date columns above — the shape
                // the inline decoder had.
                .map(|exp| exp.get(..10).unwrap_or(exp).to_string()),
            owner_short: short_id(&row.created_by),
        }
    }
}

/// Render the admin Shares table (or empty state). Token displayed as
/// short prefix in a `<code>` block; access count includes optional
/// "/ N" divisor when a max is set; "Never" renders for unset expires.
pub fn render_admin_shares_table(rows: &[AdminShareRow]) -> Markup {
    if rows.is_empty() {
        return html! {
            div .empty-state { p { "No active shares" } }
        };
    }
    html! {
        table .data-table {
            thead { tr {
                th { "Token" }
                th { "Bucket" }
                th { "File" }
                th { "Access Count" }
                th { "Expires" }
                th { "Created By" }
            } }
            tbody {
                @for r in rows {
                    tr data-share-token=(r.token_short) {
                        td data-label="Token" .text-sm { code { (r.token_short) "..." } }
                        td data-label="Bucket" .font-medium { (r.bucket) }
                        td data-label="File" .text-sm { (r.key) }
                        td data-label="Access Count" .text-sm {
                            (r.access_count)
                            @if let Some(max) = r.max_access_count {
                                @if max > 0 { " / " (max) }
                            }
                        }
                        td data-label="Expires" .text-muted .text-sm {
                            @if let Some(exp) = &r.expires_short { (exp) } @else { "Never" }
                        }
                        td data-label="Created By" .text-muted .text-sm { (r.owner_short) }
                    }
                }
            }
        }
    }
}

pub async fn shares(ctx: &dyn Context, msg: &Message) -> OutputStream {
    use crate::ui::templates::{list_page, PageHeader};

    let rows: Vec<AdminShareRow> = match repo::shares::list_recent(ctx, 100, 0).await {
        Ok(page) => page.rows.iter().map(AdminShareRow::from).collect(),
        Err(e) => {
            tracing::warn!(error = %e.message, "admin shares list failed");
            Vec::new()
        }
    };

    let body = list_page(
        PageHeader {
            title: "",
            subtitle: None,
            primary_action: None,
        },
        Some(admin_tabs("Shares")),
        render_admin_shares_table(&rows),
        None,
    );

    files_page(
        ctx,
        "Shares",
        "Shares",
        Some("Public file share links"),
        body,
        msg,
    )
    .await
}

// ---------------------------------------------------------------------------
// Quotas
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct AdminQuotaRow {
    pub user_short: String,
    pub max_storage_bytes: i64,
    pub max_file_size_bytes: i64,
    pub max_files_per_bucket: i64,
}

/// Render the admin Storage Quotas table (or empty state). Bytes
/// columns humanize via `format_bytes`; user_id is truncated to the
/// first 8 chars in the loader. Pure helper.
pub fn render_admin_quotas_table(rows: &[AdminQuotaRow]) -> Markup {
    if rows.is_empty() {
        return html! {
            div .empty-state {
                p { "No custom quotas. Default: 1 GB storage, 100 MB file size, 10,000 files per bucket." }
            }
        };
    }
    html! {
        table .data-table {
            thead { tr {
                th { "User" }
                th { "Max Storage" }
                th { "Max File Size" }
                th { "Max Files/Bucket" }
            } }
            tbody {
                @for r in rows {
                    tr {
                        td data-label="User" .text-sm { (r.user_short) }
                        td data-label="Max Storage" .text-sm { (format_bytes(r.max_storage_bytes)) }
                        td data-label="Max File Size" .text-sm { (format_bytes(r.max_file_size_bytes)) }
                        td data-label="Max Files/Bucket" .text-sm { (r.max_files_per_bucket) }
                    }
                }
            }
        }
    }
}

pub async fn quotas(ctx: &dyn Context, msg: &Message) -> OutputStream {
    use crate::ui::templates::{list_page, PageHeader};

    let rows: Vec<AdminQuotaRow> = match repo::quota::list_recent(ctx, 100).await {
        Ok(list) => list
            .records
            .into_iter()
            .map(|r| AdminQuotaRow {
                user_short: r.str_field("user_id").get(..8).unwrap_or("—").to_string(),
                max_storage_bytes: r.i64_field("max_storage_bytes"),
                max_file_size_bytes: r.i64_field("max_file_size_bytes"),
                max_files_per_bucket: r.i64_field("max_files_per_bucket"),
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e.message, "admin quotas list failed");
            Vec::new()
        }
    };

    let body = list_page(
        PageHeader {
            title: "",
            subtitle: None,
            primary_action: None,
        },
        Some(admin_tabs("Quotas")),
        render_admin_quotas_table(&rows),
        None,
    );

    files_page(
        ctx,
        "Quotas",
        "Quotas",
        Some("Per-user storage limits"),
        body,
        msg,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_admin_overview_stats_renders_four_stat_cards() {
        let stats = AdminStats {
            buckets: 3,
            files: 42,
            total_size_bytes: 2_500_000_000,
            shares: 5,
            quotas_count: 1,
        };
        let html = render_admin_overview_stats(&stats).into_string();
        assert!(html.contains(">3<"), "buckets count missing: {html}");
        assert!(html.contains(">42<"), "files count missing: {html}");
        assert!(html.contains(">5<"), "shares count missing: {html}");
        // total_size_bytes 2.5 GB → "2.3 GB" via format_bytes (or close).
        assert!(html.contains("GB"), "size humanization missing: {html}");
    }

    #[test]
    fn render_admin_overview_empty_cta_shown_when_zero_buckets() {
        let html = render_admin_overview_empty_cta(0).into_string();
        assert!(
            html.contains("Create your first bucket"),
            "cta title missing: {html}"
        );
        assert!(
            html.contains(r#"href="/b/storage/admin/buckets""#),
            "cta should link to the Buckets tab (the real create trigger): {html}"
        );
        assert!(html.contains("New bucket"), "cta label missing: {html}");
    }

    #[test]
    fn render_admin_overview_empty_cta_hidden_when_buckets_exist() {
        let html = render_admin_overview_empty_cta(3).into_string();
        assert!(
            html.trim().is_empty(),
            "cta should be hidden once a bucket exists: {html}"
        );
    }

    #[tokio::test]
    async fn overview_page_shows_create_bucket_cta_when_empty() {
        use crate::test_support::{admin_msg, output_html, TestContext};

        let ctx = TestContext::with_files().await;
        let msg = admin_msg("retrieve", "/b/storage/admin/");
        let html = output_html(overview(&ctx, &msg).await).await;

        assert!(
            html.contains("Create your first bucket"),
            "empty-state CTA missing from the live overview render: {html}"
        );
        assert!(
            html.contains(r#"href="/b/storage/admin/buckets""#),
            "CTA should link to the Buckets tab: {html}"
        );
    }

    #[tokio::test]
    async fn overview_page_hides_create_bucket_cta_once_a_bucket_exists() {
        use crate::test_support::{admin_msg, output_html, TestContext};

        let ctx = TestContext::with_files().await;
        let mut row: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        row.insert("name".into(), serde_json::json!("photos"));
        row.insert("created_by".into(), serde_json::json!("admin_1"));
        repo::buckets::seed(&ctx, row).await.expect("seed bucket");

        let msg = admin_msg("retrieve", "/b/storage/admin/");
        let html = output_html(overview(&ctx, &msg).await).await;

        assert!(
            !html.contains("Create your first bucket"),
            "CTA should be gone once a bucket exists: {html}"
        );
    }

    #[test]
    fn render_admin_overview_quotas_hint_when_present() {
        let html = render_admin_overview_quotas_hint(3).into_string();
        assert!(
            html.contains("3 user(s) with custom quotas"),
            "quotas hint missing: {html}"
        );
    }

    #[test]
    fn render_admin_overview_quotas_hint_empty_when_zero() {
        let html = render_admin_overview_quotas_hint(0).into_string();
        // Empty markup or no visible "with custom quotas" copy.
        assert!(
            !html.contains("with custom quotas"),
            "should be empty when zero: {html}"
        );
    }

    #[test]
    fn render_admin_buckets_table_empty_state() {
        let html = render_admin_buckets_table(&[]).into_string();
        assert!(html.contains("No buckets"), "missing empty hint: {html}");
    }

    #[test]
    fn render_admin_buckets_table_renders_rows() {
        let rows = vec![
            AdminBucketRow {
                name: "photos".into(),
                owner_short: "admin_1".into(),
                public: true,
                created_at_short: "2026-05-06".into(),
            },
            AdminBucketRow {
                name: "docs".into(),
                owner_short: "user_42".into(),
                public: false,
                created_at_short: "2026-05-05".into(),
            },
        ];
        let html = render_admin_buckets_table(&rows).into_string();
        assert!(html.contains(">photos<"), "name missing: {html}");
        assert!(html.contains(">docs<"));
        assert!(html.contains("admin_1"));
        // status_badge renders class names containing "public" / "private".
        assert!(html.contains("public"));
        assert!(html.contains("private"));
        assert!(html.contains("2026-05-06"));
    }

    /// One capped, expiring share row, as the repo decodes it.
    fn sample_share_row() -> repo::shares::ShareRow {
        repo::shares::ShareRow::from_record(&wafer_core::clients::database::Record {
            id: "s1".to_string(),
            data: [
                ("token", serde_json::json!("tok12345abcdef-more")),
                ("bucket", serde_json::json!("photos")),
                ("key", serde_json::json!("a.png")),
                ("created_by", serde_json::json!("alice-1234-5678")),
                ("created_at", serde_json::json!("2026-05-06T10:00:00Z")),
                ("expires_at", serde_json::json!("2026-06-06T10:00:00Z")),
                ("access_count", serde_json::json!(4)),
                ("max_access_count", serde_json::json!(10)),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        })
    }

    /// The admin share projection cuts the token to 12 and the expiry to 10
    /// and reads nothing else. `max_access_count` comes straight off the row:
    /// the inline decoder it replaces read it with `str_field(..).parse()`,
    /// which is empty for the JSON number SQLite's `INTEGER` column returns,
    /// so a capped share rendered as uncapped.
    #[test]
    fn admin_share_projection_shapes_only_what_the_table_renders() {
        let row = sample_share_row();
        let projected = AdminShareRow::from(&row);
        assert_eq!(projected.token_short, "tok12345abcd");
        assert_eq!(projected.bucket, "photos");
        assert_eq!(projected.key, "a.png");
        assert_eq!(projected.access_count, 4);
        assert_eq!(projected.max_access_count, Some(10));
        assert_eq!(projected.expires_short.as_deref(), Some("2026-06-06"));
        assert_eq!(projected.owner_short, "alice-12");
    }

    #[test]
    fn render_admin_shares_table_empty_state() {
        let html = render_admin_shares_table(&[]).into_string();
        assert!(html.contains("No active shares"), "missing empty: {html}");
    }

    #[test]
    fn render_admin_shares_table_renders_rows() {
        let rows = vec![AdminShareRow {
            token_short: "tok12345abc1".into(),
            bucket: "photos".into(),
            key: "a.png".into(),
            access_count: 4,
            max_access_count: Some(10),
            expires_short: Some("2026-06-06".into()),
            owner_short: "admin_1".into(),
        }];
        let html = render_admin_shares_table(&rows).into_string();
        assert!(html.contains("tok12345abc1"));
        assert!(html.contains(">photos<"));
        assert!(html.contains(">a.png<"));
        // access_count and max rendered together as "4 / 10"
        assert!(
            html.contains("4 / 10"),
            "access count + max missing: {html}"
        );
        // max_access_count rendered as "/ 10"
        assert!(html.contains("/ 10"));
        assert!(html.contains("2026-06-06"));
        assert!(html.contains("admin_1"));
    }

    #[test]
    fn render_admin_shares_table_no_expires_renders_never() {
        let rows = vec![AdminShareRow {
            token_short: "abc".into(),
            bucket: "b".into(),
            key: "k".into(),
            access_count: 0,
            max_access_count: None,
            expires_short: None,
            owner_short: "u".into(),
        }];
        let html = render_admin_shares_table(&rows).into_string();
        assert!(
            html.contains("Never"),
            "missing 'Never' for null expires: {html}"
        );
        // No "/ N" segment when max_access_count is None.
        assert!(
            !html.contains("/ "),
            "should not show max divisor when None: {html}"
        );
    }

    #[test]
    fn render_admin_quotas_table_empty_state() {
        let html = render_admin_quotas_table(&[]).into_string();
        assert!(
            html.contains("No custom quotas"),
            "missing empty hint: {html}"
        );
        // Default values surfaced in the empty state copy.
        assert!(html.contains("1 GB"), "missing 1 GB default copy: {html}");
    }

    #[test]
    fn render_admin_quotas_table_renders_rows() {
        let rows = vec![AdminQuotaRow {
            user_short: "user_1".into(),
            max_storage_bytes: 5_000_000_000,
            max_file_size_bytes: 100_000_000,
            max_files_per_bucket: 1000,
        }];
        let html = render_admin_quotas_table(&rows).into_string();
        assert!(html.contains("user_1"), "user missing: {html}");
        // 5 GB ≈ "4.7 GB" via format_bytes humanization.
        assert!(html.contains("GB"), "GB unit missing: {html}");
        // 100 MB → "95.4 MB".
        assert!(html.contains("MB"), "MB unit missing: {html}");
        // max_files_per_bucket as integer in its own cell.
        assert!(
            html.contains(">1000<"),
            "files-per-bucket count missing: {html}"
        );
    }
}

#[cfg(test)]
mod b13_visibility_tests {
    use serde_json::json;
    use wafer_core::clients::database::Record;

    use super::*;
    use crate::test_support::{admin_msg, output_html, TestContext};

    /// One bucket row whose `public` column arrived in `shape`.
    fn row_with_public(shape: serde_json::Value) -> repo::buckets::BucketRow {
        repo::buckets::BucketRow::from_record(&Record {
            id: "b1".to_string(),
            data: [
                ("name".to_string(), json!("photos")),
                ("public".to_string(), shape),
                ("created_by".to_string(), json!("alice-1234-5678")),
                ("created_at".to_string(), json!("2026-05-06T10:00:00Z")),
            ]
            .into_iter()
            .collect(),
        })
    }

    /// The two projections, off one repo row, for every shape `public` can
    /// arrive in — including the Postgres `Bool` and TEXT `String` shapes the
    /// SQLite-backed page tests below cannot produce. Both projections read
    /// the row's already-decoded `bool`, so there is no second place for the
    /// two pages to drift apart again.
    #[test]
    fn both_bucket_projections_agree_for_every_shape_public_arrives_in() {
        for (shape, expected) in [
            (json!(1), true),
            (json!(true), true),
            (json!("true"), true),
            (json!(0), false),
            (json!(false), false),
            (json!("false"), false),
        ] {
            let row = row_with_public(shape.clone());
            let user = super::super::pages_user::BucketRow::from((&row, 3));
            let admin = AdminBucketRow::from(&row);
            assert_eq!(
                (user.public, admin.public),
                (expected, expected),
                "`public` as {shape} projected as user={} admin={}",
                user.public,
                admin.public
            );
        }
    }

    /// The projections shape their columns and read nothing else: the user
    /// table keeps the full timestamp and carries the object count from the
    /// second query, the admin table cuts the owner to 8 and the date to 10.
    #[test]
    fn the_bucket_projections_shape_only_what_the_table_renders() {
        let row = row_with_public(json!(1));
        let user = super::super::pages_user::BucketRow::from((&row, 7));
        assert_eq!(user.name, "photos");
        assert_eq!(user.created_at, "2026-05-06T10:00:00Z");
        assert_eq!(user.object_count, 7);

        let admin = AdminBucketRow::from(&row);
        assert_eq!(admin.name, "photos");
        assert_eq!(admin.owner_short, "alice-12");
        assert_eq!(admin.created_at_short, "2026-05-06");
    }

    /// Does the user-facing bucket table say this bucket is public?
    /// `pages_user::render_buckets_table` renders `badge-success`/"Public"
    /// for a public bucket and a bare `badge`/"Private" otherwise.
    fn user_page_says_public(html: &str) -> bool {
        assert!(
            html.contains(">photos<"),
            "the bucket is missing from the user page entirely: {html}"
        );
        html.contains("badge-success")
    }

    /// Does the admin bucket table say this bucket is public?
    /// `render_admin_buckets_table` renders `status_badge("public")` /
    /// `status_badge("private")`, i.e. the literal word as the badge label.
    fn admin_page_says_public(html: &str) -> bool {
        assert!(
            html.contains(">photos<"),
            "the bucket is missing from the admin page entirely: {html}"
        );
        html.contains(">public</span>")
    }

    /// B13. `public` is written as a JSON bool by every writer
    /// ([`repo::buckets::insert`] among them) into a column that is
    /// `INTEGER` on SQLite and `BOOLEAN` on Postgres. The user bucket page
    /// decoded it with `as_bool()` and the admin bucket page with
    /// `str_field("public") == "true"`, so between them they accepted a JSON
    /// bool and a JSON string and neither accepted the integer SQLite hands
    /// back — one bucket, one row, two pages, and up to three different
    /// answers depending on the backend.
    ///
    /// The fix is to decode it exactly once, in
    /// [`repo::buckets::BucketRow::from_record`], through
    /// `RecordExt::bool_field` (which accepts all three shapes). This test
    /// therefore asserts the *correct* answer on both pages, not merely that
    /// the two agree: on SQLite today they already agree — on "Private", for
    /// a bucket that was created public.
    #[tokio::test]
    async fn both_bucket_pages_report_a_public_bucket_as_public() {
        let ctx = TestContext::with_files().await;
        // `admin_msg`'s user id, so the owner-scoped user page shows it too.
        repo::buckets::insert(&ctx, "photos", true, "admin_1")
            .await
            .expect("seed public bucket");

        let user_html = output_html(
            super::super::pages_user::bucket_list_page(&ctx, &admin_msg("retrieve", "/b/storage/"))
                .await,
        )
        .await;
        let admin_html =
            output_html(buckets(&ctx, &admin_msg("retrieve", "/b/storage/admin/buckets")).await)
                .await;

        let user = user_page_says_public(&user_html);
        let admin = admin_page_says_public(&admin_html);
        assert!(
            user && admin,
            "a bucket created public must read public on both pages; \
             user page said public={user}, admin page said public={admin}"
        );
    }

    /// The other half of the same door: a private bucket must read private on
    /// both pages. Guards the fix from over-correcting into "everything is
    /// public".
    #[tokio::test]
    async fn both_bucket_pages_report_a_private_bucket_as_private() {
        let ctx = TestContext::with_files().await;
        repo::buckets::insert(&ctx, "photos", false, "admin_1")
            .await
            .expect("seed private bucket");

        let user_html = output_html(
            super::super::pages_user::bucket_list_page(&ctx, &admin_msg("retrieve", "/b/storage/"))
                .await,
        )
        .await;
        let admin_html =
            output_html(buckets(&ctx, &admin_msg("retrieve", "/b/storage/admin/buckets")).await)
                .await;

        let user = user_page_says_public(&user_html);
        let admin = admin_page_says_public(&admin_html);
        assert!(
            !user && !admin,
            "a bucket created private must read private on both pages; \
             user page said public={user}, admin page said public={admin}"
        );
    }
}
