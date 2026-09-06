use std::collections::HashMap;

use maud::html;
use wafer_run::{context::Context, Message, OutputStream};

use super::{admin_page, crumb};
use crate::{
    blocks::auth::repo::users::{self, DailySignups},
    platform_state::request_logs::{self, DailyCounts, TodayCounts},
    ui::{
        components, icons,
        shell::Topbar,
        templates::{dashboard_page, PageHeader, StatTile},
    },
};

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

    let TodayCounts {
        requests: requests_today,
        errors: errors_today,
        avg_ms,
    } = request_counts_r.unwrap_or_default();
    let (user_count, new_users_today) = user_counts_r.unwrap_or((0, 0));
    let recent_users = recent_users_r.unwrap_or_default();
    let recent_errors = recent_errors_r.unwrap_or_default();
    let users_daily_rows = users_daily_r.unwrap_or_default();
    let request_days = request_days_r.unwrap_or_default();

    // Two grouped statements back all three charts: the USERS series comes from
    // its own daily aggregate; the request-log "requests" and "errors" series
    // are two metrics projected out of the *same* per-day counts.
    let new_users_daily = series_from_signups(&users_daily_rows, start_30d);
    let requests_daily = series_from_daily(&request_days, |d| d.requests, start_30d);
    let errors_daily = series_from_daily(&request_days, |d| d.errors, start_30d);

    let user_count_str = user_count.to_string();
    let new_users_str = new_users_today.to_string();
    let requests_str = requests_today.to_string();
    let errors_str = errors_today.to_string();
    let avg_ms_str = format!("{avg_ms:.0}ms");

    // Sparklines reuse the daily series already fetched for the charts below —
    // no extra D1 statements. "Avg response" has no per-day series fetched, so
    // its sparkline is `None` rather than reusing an unrelated metric.
    let new_users_series: Vec<i64> = new_users_daily.iter().map(|(_, v)| *v).collect();
    let requests_series: Vec<i64> = requests_daily.iter().map(|(_, v)| *v).collect();
    let errors_series: Vec<i64> = errors_daily.iter().map(|(_, v)| *v).collect();

    let stats = vec![
        StatTile {
            label: "Total Users",
            value: &user_count_str,
            icon: icons::users(),
            spark: Some(components::sparkline(
                &new_users_series,
                "var(--primary-color)",
            )),
        },
        StatTile {
            label: "New Today",
            value: &new_users_str,
            icon: icons::user_plus(),
            spark: Some(components::sparkline(
                &new_users_series,
                "var(--primary-color)",
            )),
        },
        StatTile {
            label: "Requests Today",
            value: &requests_str,
            icon: icons::file_text(),
            spark: Some(components::sparkline(
                &requests_series,
                "var(--accent-warning)",
            )),
        },
        StatTile {
            label: "Errors Today",
            value: &errors_str,
            icon: icons::triangle_alert(),
            spark: Some(components::sparkline(
                &errors_series,
                "var(--accent-danger)",
            )),
        },
        StatTile {
            label: "Avg Response",
            value: &avg_ms_str,
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
                @if recent_users.is_empty() {
                    p .text-muted .text-sm { "No users yet" }
                } @else {
                    div .table-container {
                        table .table {
                            tbody {
                                @for record in &recent_users {
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
                                @for row in &recent_errors {
                                    @let code = row.status_code;
                                    @let method = row.method.as_str();
                                    @let path = row.path.as_str();
                                    @let created = row.created_at.as_str();
                                    tr {
                                        td {
                                            span .badge .(if code >= 500 { "badge-danger" } else { "badge-warning" }) { (code) }
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
            }
        }
    };

    let charts_section = html! {
        div .dashboard-charts {
            (components::line_chart_card("New users", "Last 30 days", &new_users_daily, "var(--primary-color)", "/b/admin/users"))
            (components::bar_chart_card("Requests", "Last 30 days", &requests_daily, "var(--accent-warning)", "/b/admin/logs"))
            (components::line_chart_card("Errors", "Last 30 days", &errors_daily, "var(--accent-danger)", "/b/admin/logs?status=ERROR"))
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
