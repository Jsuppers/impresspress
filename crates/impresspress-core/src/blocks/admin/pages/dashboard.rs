use std::collections::HashMap;

use maud::html;
use wafer_run::{context::Context, Message, OutputStream};

use super::{admin_page, crumb};
use crate::{
    blocks::auth::repo::users::{self, DailySignups},
    platform_state::request_logs::{self, DailyCounts, TodayCounts},
    ui::{
        components::{self, Badge, BadgeVariant},
        icons,
        shell::Topbar,
        templates::{dashboard_page, PageHeader, StatTile},
    },
};

/// What a stat tile shows when the aggregate behind it could not be read.
///
/// The dashboard is the page an operator opens *during* an outage, so a
/// failed read marks its own tile rather than failing the whole page (the
/// one exception to the group-1 rule). It must still be a marker and not a
/// number: `0` is a claim, and "Total Users 0" during a database outage is
/// the claim that the deployment has no users.
const TILE_UNAVAILABLE: &str = "Unavailable";

/// The same marker for the card- and chart-shaped panels, which have a body
/// rather than a value.
const CARD_UNAVAILABLE: &str = "Could not load";

/// A tile's pre-formatted figure, or [`TILE_UNAVAILABLE`] when the aggregate
/// behind it could not be read.
fn tile_value(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or(TILE_UNAVAILABLE)
}

/// A chart card whose series could not be read: the same head, and the marker
/// where the plot would be.
///
/// It cannot reuse `components::{line,bar}_chart_card` with an empty series —
/// those render an axis and a flat line, which reads as a real measurement of
/// zero rather than as an absent one.
fn chart_unavailable_card(title: &str, subtitle: &str, view_href: &str) -> maud::Markup {
    html! {
        section .card {
            header .card__head {
                div {
                    h2 .card__title { (title) }
                    p .card__subtitle { (subtitle) }
                }
                a .btn .btn--ghost .btn--sm .card__actions href=(view_href) { "View" }
            }
            div .card__body {
                p .text-muted .text-sm { (CARD_UNAVAILABLE) }
            }
        }
    }
}

/// Trailing 30-day window as `(oldest_day, oldest_day_midnight_iso)`.
/// `oldest_day` anchors the zero-fill; the ISO string is the `created_at >=`
/// lower bound shared by every 30-day query.
fn window_30d() -> (chrono::NaiveDate, String) {
    let today = chrono::Utc::now().date_naive();
    let start = today - chrono::Duration::days(29);
    (start, format!("{start}T00:00:00"))
}

/// Zero-fill `by_day` into a 30-entry series ordered oldest → newest
/// (matching the chart's x-axis). A missing day reads as `0`.
fn zero_filled_30d(by_day: &HashMap<String, i64>, start: chrono::NaiveDate) -> Vec<(String, i64)> {
    (0..30)
        .map(|i| {
            let date = (start + chrono::Duration::days(i))
                .format("%Y-%m-%d")
                .to_string();
            let count = by_day.get(&date).copied().unwrap_or(0);
            (date, count)
        })
        .collect()
}

/// Project the daily signup counts into a zero-filled 30-entry series.
fn series_from_signups(rows: &[DailySignups], start: chrono::NaiveDate) -> Vec<(String, i64)> {
    let by_day: HashMap<String, i64> = rows.iter().map(|r| (r.day.clone(), r.count)).collect();
    zero_filled_30d(&by_day, start)
}

/// Project one metric out of the request log's daily counts into a
/// zero-filled 30-entry series.
fn series_from_daily(
    days: &[DailyCounts],
    pick: fn(&DailyCounts) -> i64,
    start: chrono::NaiveDate,
) -> Vec<(String, i64)> {
    let by_day: HashMap<String, i64> = days.iter().map(|d| (d.day.clone(), pick(d))).collect();
    zero_filled_30d(&by_day, start)
}

pub async fn dashboard(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let today_start = format!("{today}T00:00:00");

    // Every read below is independent, so we issue them concurrently with
    // `futures::join!`. Concurrency alone doesn't cut the D1 *statement* count,
    // though — each round-trip is a billed statement on Cloudflare — so the
    // header tiles now fold their several per-filter counts into ONE aggregate
    // per table (conditional `CaseWhenSum` columns), and the two REQUEST_LOGS
    // chart series come from ONE grouped-by-day statement. That is six D1
    // statements to render the whole page (was ten): two consolidated header
    // aggregates, two recent-row lists, and two daily grouped aggregates.
    let (start_30d, start_iso) = window_30d();

    let user_counts_fut = users::active_count_and_created_since(ctx, &today_start);
    let request_counts_fut = request_logs::today_counts(ctx, &today_start);

    let users_daily_fut = users::daily_signups(ctx, &start_iso);
    let requests_daily_fut = request_logs::daily_counts(ctx, &start_iso);

    let recent_users_fut = users::list_recent_active(ctx, 5);

    let recent_errors_fut = request_logs::list_recent_errors(ctx, 5);

    let (
        user_counts_r,
        request_counts_r,
        recent_users_r,
        recent_errors_r,
        users_daily_r,
        request_days_r,
    ) = futures::join!(
        user_counts_fut,
        request_counts_fut,
        recent_users_fut,
        recent_errors_fut,
        users_daily_fut,
        requests_daily_fut,
    );

    // Each read is logged and marked where it renders, rather than being
    // published as `0` and the empty state. Everything below reads `Option`:
    // `None` is "could not be read", and it never reaches a figure, a table
    // row, a chart or a sparkline.
    let request_counts = match request_counts_r {
        Ok(counts) => Some(counts),
        Err(e) => {
            tracing::error!(error = %e, "admin dashboard: today's request counts failed");
            None
        }
    };
    let user_counts = match user_counts_r {
        Ok(counts) => Some(counts),
        Err(e) => {
            tracing::error!(error = %e, "admin dashboard: user counts failed");
            None
        }
    };
    let recent_users = match recent_users_r {
        Ok(rows) => Some(rows),
        Err(e) => {
            tracing::error!(error = %e, "admin dashboard: recent users failed");
            None
        }
    };
    let recent_errors = match recent_errors_r {
        Ok(rows) => Some(rows),
        Err(e) => {
            tracing::error!(error = %e, "admin dashboard: recent errors failed");
            None
        }
    };
    let users_daily_rows = match users_daily_r {
        Ok(rows) => Some(rows),
        Err(e) => {
            tracing::error!(error = %e, "admin dashboard: daily signups failed");
            None
        }
    };
    let request_days = match request_days_r {
        Ok(rows) => Some(rows),
        Err(e) => {
            tracing::error!(error = %e, "admin dashboard: daily request counts failed");
            None
        }
    };

    // Two grouped statements back all three charts: the USERS series comes from
    // its own daily aggregate; the request-log "requests" and "errors" series
    // are two metrics projected out of the *same* per-day counts.
    //
    // An unreadable aggregate must NOT be zero-filled: the 30-day fill turns
    // "we could not read this" into a flat line along the axis, which is a
    // picture of "nothing happened".
    let new_users_daily = users_daily_rows
        .as_ref()
        .map(|rows| series_from_signups(rows, start_30d));
    let requests_daily = request_days
        .as_ref()
        .map(|days| series_from_daily(days, |d| d.requests, start_30d));
    let errors_daily = request_days
        .as_ref()
        .map(|days| series_from_daily(days, |d| d.errors, start_30d));

    let user_count_str = user_counts.map(|(total, _)| total.to_string());
    let new_users_str = user_counts.map(|(_, today)| today.to_string());
    let requests_str = request_counts
        .as_ref()
        .map(|c: &TodayCounts| c.requests.to_string());
    let errors_str = request_counts
        .as_ref()
        .map(|c: &TodayCounts| c.errors.to_string());
    let avg_ms_str = request_counts
        .as_ref()
        .map(|c: &TodayCounts| format!("{:.0}ms", c.avg_ms));

    // Sparklines reuse the daily series already fetched for the charts below —
    // no extra D1 statements. "Avg response" has no per-day series fetched, so
    // its sparkline is `None` rather than reusing an unrelated metric — and so
    // is any sparkline whose series could not be read.
    let spark = |series: &Option<Vec<(String, i64)>>, color: &str| {
        series.as_ref().map(|series| {
            components::sparkline(&series.iter().map(|(_, v)| *v).collect::<Vec<_>>(), color)
        })
    };
    let new_users_spark = spark(&new_users_daily, "var(--primary-color)");
    let requests_spark = spark(&requests_daily, "var(--accent-warning)");
    let errors_spark = spark(&errors_daily, "var(--accent-danger)");

    let stats = vec![
        StatTile {
            label: "Total Users",
            value: tile_value(&user_count_str),
            icon: icons::users(),
            spark: new_users_spark.clone(),
        },
        StatTile {
            label: "New Today",
            value: tile_value(&new_users_str),
            icon: icons::user_plus(),
            spark: new_users_spark,
        },
        StatTile {
            label: "Requests Today",
            value: tile_value(&requests_str),
            icon: icons::file_text(),
            spark: requests_spark,
        },
        StatTile {
            label: "Errors Today",
            value: tile_value(&errors_str),
            icon: icons::triangle_alert(),
            spark: errors_spark,
        },
        StatTile {
            label: "Avg Response",
            value: tile_value(&avg_ms_str),
            icon: icons::activity(),
            spark: None,
        },
    ];

    let recent_users_card = html! {
        section .card {
            header .card__head {
                h2 .card__title { "Recent Users" }
                a .btn .btn--ghost .btn--sm href="/b/admin/users" { "View all" }
            }
            div .card__body {
                @if let Some(recent_users) = &recent_users {
                @if recent_users.is_empty() {
                    p .text-muted .text-sm { "No users yet" }
                } @else {
                    div .table-container {
                        table .table {
                            tbody {
                                @for record in recent_users {
                                    @let email = record.email.as_str();
                                    @let created = record.created_at.as_str();
                                    tr {
                                        td .text-sm { (email) }
                                        td .text-muted .text-sm .text-right { (created.get(..10).unwrap_or(created)) }
                                    }
                                }
                            }
                        }
                    }
                }
                } @else {
                    p .text-muted .text-sm { (CARD_UNAVAILABLE) }
                }
            }
        }
    };

    let recent_errors_card = html! {
        section .card {
            header .card__head {
                h2 .card__title { "Recent Errors" }
                a .btn .btn--ghost .btn--sm .card__actions href="/b/admin/logs?status=ERROR" { "View all" }
            }
            div .card__body {
                @if let Some(recent_errors) = &recent_errors {
                @if recent_errors.is_empty() {
                    p .text-muted .text-sm { "No errors recently" }
                } @else {
                    div .table-container {
                        table .table {
                            thead {
                                tr {
                                    th { "Status" }
                                    th { "Method" }
                                    th { "Path" }
                                    th { "Time" }
                                }
                            }
                            tbody {
                                @for row in recent_errors {
                                    @let code = row.status_code;
                                    @let method = row.method.as_str();
                                    @let path = row.path.as_str();
                                    @let created = row.created_at.as_str();
                                    tr {
                                        td {
                                            @let variant = if code >= 500 { BadgeVariant::Danger } else { BadgeVariant::Warning };
                                            (Badge::new(variant).render(html! { (code) }))
                                        }
                                        td .text-sm .font-medium { (method.to_uppercase()) }
                                        td .text-sm { (path) }
                                        td .text-muted .text-sm { (created.get(..19).unwrap_or(created)) }
                                    }
                                }
                            }
                        }
                    }
                }
                } @else {
                    p .text-muted .text-sm { (CARD_UNAVAILABLE) }
                }
            }
        }
    };

    let new_users_chart = match &new_users_daily {
        Some(series) => components::line_chart_card(
            "New users",
            "Last 30 days",
            series,
            "var(--primary-color)",
            "/b/admin/users",
        ),
        None => chart_unavailable_card("New users", "Last 30 days", "/b/admin/users"),
    };
    let requests_chart = match &requests_daily {
        Some(series) => components::bar_chart_card(
            "Requests",
            "Last 30 days",
            series,
            "var(--accent-warning)",
            "/b/admin/logs",
        ),
        None => chart_unavailable_card("Requests", "Last 30 days", "/b/admin/logs"),
    };
    let errors_chart = match &errors_daily {
        Some(series) => components::line_chart_card(
            "Errors",
            "Last 30 days",
            series,
            "var(--accent-danger)",
            "/b/admin/logs?status=ERROR",
        ),
        None => chart_unavailable_card("Errors", "Last 30 days", "/b/admin/logs?status=ERROR"),
    };

    let charts_section = html! {
        div .dashboard-charts {
            (new_users_chart)
            (requests_chart)
            (errors_chart)
        }
    };

    let body = dashboard_page(
        PageHeader {
            title: "",
            subtitle: None,
            primary_action: None,
        },
        stats,
        recent_users_card,
        recent_errors_card,
        None,
        Some(charts_section),
    );

    admin_page(
        ctx,
        msg,
        "Dashboard",
        Topbar {
            crumbs: crumb("Dashboard"),
            primary_action: None,
            subtitle: Some("System overview"),
            show_palette: true,
        },
        body,
    )
    .await
}

#[cfg(test)]
mod tests {
    //! The dashboard's own arithmetic: the 30-day zero-fill that turns a
    //! sparse per-day aggregate into the chart's x-axis. The aggregates
    //! themselves are `auth::repo::users` and `platform_state::request_logs`
    //! functions now, and are checked against separate `db::count` calls
    //! beside their owners.

    use std::collections::HashMap;

    use super::{series_from_signups, window_30d, zero_filled_30d};
    use crate::blocks::auth::repo::users::DailySignups;

    #[test]
    fn dashboard_renders_stats_before_charts() {
        let m = crate::ui::templates::dashboard_page(
            crate::ui::templates::PageHeader {
                title: "Dashboard",
                subtitle: None,
                primary_action: None,
            },
            vec![crate::ui::templates::StatTile {
                label: "TOTAL USERS",
                value: "1",
                icon: maud::html! { span .probe-icon {} },
                spark: None,
            }],
            maud::html! { div .probe-primary {} },
            maud::html! {},
            None,
            None,
        )
        .into_string();
        let stats = m.find("stats-grid").expect("stats grid missing");
        let charts = m.find("dashboard-grid").expect("charts grid missing");
        assert!(stats < charts, "stat tiles must precede the charts row");
    }

    /// Value for `date` in a `(date, count)` series, or `-1` if absent.
    fn day_value(series: &[(String, i64)], date: &str) -> i64 {
        series
            .iter()
            .find(|(d, _)| d == date)
            .map(|(_, c)| *c)
            .unwrap_or(-1)
    }

    #[test]
    fn signup_series_is_zero_filled_over_the_thirty_day_window() {
        let (start_30d, _) = window_30d();
        let day = |ago: i64| {
            (chrono::Utc::now().date_naive() - chrono::Duration::days(ago))
                .format("%Y-%m-%d")
                .to_string()
        };
        let rows = vec![
            DailySignups {
                day: day(0),
                count: 3,
            },
            DailySignups {
                day: day(5),
                count: 2,
            },
            // Outside the window: dropped rather than folded into an edge day.
            DailySignups {
                day: day(40),
                count: 9,
            },
        ];
        let series = series_from_signups(&rows, start_30d);
        assert_eq!(series.len(), 30, "30-entry zero-filled series");
        assert_eq!(day_value(&series, &day(0)), 3);
        assert_eq!(day_value(&series, &day(5)), 2);
        assert_eq!(day_value(&series, &day(1)), 0, "a quiet day reads as zero");
        assert_eq!(
            series.iter().map(|(_, c)| c).sum::<i64>(),
            5,
            "the 40-days-ago row is outside the window"
        );
    }

    #[test]
    fn zero_fill_starts_at_the_window_start_and_runs_forward() {
        let start = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let series = zero_filled_30d(&HashMap::from([("2026-01-03".to_string(), 7)]), start);
        assert_eq!(series[0].0, "2026-01-01");
        assert_eq!(series[29].0, "2026-01-30");
        assert_eq!(series[2], ("2026-01-03".to_string(), 7));
    }
}

#[cfg(test)]
mod outage_tests {
    //! The dashboard is the one page in group 1 that must NOT fail whole.
    //!
    //! It aggregates six independent reads and it is the page an operator
    //! opens *during* an outage, so a failed read marks its own tile and the
    //! rest of the page still renders. What it must never do is what it did:
    //! publish the failure as the number `0` and the empty state, so a
    //! deployment in trouble rendered as a healthy, unused one — "Total Users
    //! 0", "Errors Today 0", "No errors recently", and three flat charts
    //! along the axis.

    use super::*;
    use crate::test_support::{admin_msg, output_html, output_http_status, TestContext};

    #[tokio::test]
    async fn every_failed_read_marks_its_own_tile_and_the_page_still_renders() {
        let ctx = TestContext::with_auth().await.break_reads();
        let msg = admin_msg("retrieve", "/b/admin/");

        assert_eq!(
            output_http_status(dashboard(&ctx, &msg).await).await,
            200,
            "the dashboard must survive an outage — it is what an operator opens during one"
        );

        let html = output_html(dashboard(&ctx, &admin_msg("retrieve", "/b/admin/")).await).await;

        assert!(
            !html.contains(r#"<div class="stat-value">0</div>"#),
            "a failed read must not be published as the figure 0: {html}"
        );
        assert_eq!(
            html.matches(TILE_UNAVAILABLE).count(),
            5,
            "all five stat tiles are fed by the two failed aggregates"
        );
        assert!(
            !html.contains("No errors recently"),
            "an unreadable error log must not render as 'no errors recently': {html}"
        );
        assert!(
            !html.contains("No users yet"),
            "an unreadable user list must not render as 'no users yet': {html}"
        );
        assert_eq!(
            html.matches(CARD_UNAVAILABLE).count(),
            5,
            "two recent-row cards and three chart cards each carry a marker"
        );
        assert!(
            !html.contains("chart__plot") && !html.contains("charts-css"),
            "a chart whose series could not be read must not be plotted at all — a \
             zero-filled 30-day series draws a flat line along the axis, which is a \
             picture of 'nothing happened': {html}"
        );
        assert!(
            !html.contains("stat-spark"),
            "a sparkline drawn from an unreadable series is the same lie in miniature: {html}"
        );
    }

    /// The healthy render is untouched: real figures, both empty states, and
    /// three drawn charts, with no marker anywhere.
    #[tokio::test]
    async fn a_healthy_dashboard_carries_no_marker() {
        let ctx = TestContext::with_auth().await;
        let html = output_html(dashboard(&ctx, &admin_msg("retrieve", "/b/admin/")).await).await;

        assert!(
            !html.contains(TILE_UNAVAILABLE) && !html.contains(CARD_UNAVAILABLE),
            "a healthy dashboard carries no unavailable marker: {html}"
        );
        assert!(
            html.contains("No users yet") && html.contains("No errors recently"),
            "the genuine empty states still render on a healthy, unused deployment: {html}"
        );
        assert_eq!(
            html.matches(r#"class="card__subtitle">Last 30 days<"#)
                .count(),
            3,
            "all three chart cards render"
        );
        assert!(
            html.contains("chart__plot") && html.contains("charts-css"),
            "both the line charts and the bar chart are plotted: {html}"
        );
    }

    /// "Avg Response" is a latency measured during the run that screenshots
    /// this page, so the visual-baseline suite masks it. It has to be masked
    /// by its label text (`.stat-card:has-text("Avg Response") .stat-value`)
    /// rather than by an attribute, because `StatTile::value` is a plain
    /// `&str` and `components::stat_card` offers no markup slot to hang one
    /// on. That makes the label part of the mask's contract: rename it and
    /// the mask silently stops matching, and the tile is compared pixel by
    /// pixel again with nothing announcing the change.
    #[tokio::test]
    async fn the_avg_response_tile_keeps_the_label_the_visual_mask_keys_on() {
        let ctx = TestContext::with_auth().await;
        let html = output_html(dashboard(&ctx, &admin_msg("retrieve", "/b/admin/")).await).await;

        let label = r#"<div class="stat-label">Avg Response</div>"#;
        let at = html
            .find(label)
            .unwrap_or_else(|| panic!("visual-baseline.spec.ts masks this tile by this exact label, which is gone: {html}"));
        let rest = &html[at + label.len()..];
        assert!(
            rest.starts_with(r#"<div class="stat-value">"#),
            "the masked element is the `.stat-value` that follows the label: {rest:.120}"
        );
    }
}
