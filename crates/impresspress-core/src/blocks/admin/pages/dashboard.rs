use std::collections::HashMap;

use maud::html;
use wafer_block::{
    db::{Filter, FilterOp, ListOptions, SortField},
    wire::database as wire,
};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, Message, OutputStream};

use super::{admin_page, crumb};
use crate::{
    blocks::auth::USERS_TABLE as USERS,
    platform_state::request_logs::{self, DailyCounts, TodayCounts},
    ui::{
        components, icons,
        shell::Topbar,
        templates::{dashboard_page, PageHeader, StatTile},
    },
    util::{daily_grouped, to_wire_filters, RecordExt},
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

/// Project one aggregate `alias` out of grouped daily `rows` (from
/// [`daily_grouped`]) into a zero-filled 30-entry series. A group whose
/// conditional sum was `NULL` reads as `0`.
fn series_from_rows(
    rows: &[wire::Record],
    alias: &str,
    start: chrono::NaiveDate,
) -> Vec<(String, i64)> {
    let by_day: HashMap<String, i64> = rows
        .iter()
        .filter_map(|r| {
            let day = r.data.get("created_at").and_then(|v| v.as_str())?;
            let cnt = r.data.get(alias).and_then(|v| v.as_i64()).unwrap_or(0);
            Some((day.to_string(), cnt))
        })
        .collect();
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

/// Header-tile USER counts in ONE statement: `(total_active, active_today)`.
///
/// Both counts share the `deleted_at IS NULL` predicate, so it becomes the
/// query's `WHERE` and the "created today" restriction rides along as a
/// conditional `CaseWhenSum` — replacing the two separate `db::count`
/// round-trips with a single aggregate that returns the same two numbers.
async fn user_counts(ctx: &dyn Context, today_start: &str) -> (i64, i64) {
    let active = [Filter {
        field: "deleted_at".into(),
        operator: FilterOp::IsNull,
        value: serde_json::Value::Null,
    }];
    let created_today = [Filter {
        field: "created_at".into(),
        operator: FilterOp::GreaterEqual,
        value: serde_json::json!(today_start),
    }];
    let req = wire::AggregateRequest {
        collection: USERS.to_string(),
        select_columns: vec![],
        aggregates: vec![
            wire::AggregateColumnDef::Count {
                alias: "total".into(),
            },
            wire::AggregateColumnDef::CaseWhenSum {
                when: to_wire_filters(&created_today),
                alias: "today".into(),
            },
        ],
        filters: to_wire_filters(&active),
        group_by: vec![],
        sort: vec![],
        limit: 0,
    };
    let rows = db::aggregate(ctx, req).await.unwrap_or_default();
    let row = rows.first();
    let read = |k: &str| {
        row.and_then(|r| r.data.get(k))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    };
    (read("total"), read("today"))
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

    let user_counts_fut = user_counts(ctx, &today_start);
    let request_counts_fut = request_logs::today_counts(ctx, &today_start);

    let users_daily_fut = daily_grouped(
        ctx,
        USERS,
        &start_iso,
        vec![Filter {
            field: "deleted_at".into(),
            operator: FilterOp::IsNull,
            value: serde_json::Value::Null,
        }],
        vec![wire::AggregateColumnDef::Count {
            alias: "cnt".into(),
        }],
    );
    let requests_daily_fut = request_logs::daily_counts(ctx, &start_iso);

    let recent_users_opts = ListOptions {
        columns: Some(vec!["id".into(), "email".into(), "created_at".into()]),
        filters: vec![Filter {
            field: "deleted_at".into(),
            operator: FilterOp::IsNull,
            value: serde_json::Value::Null,
        }],
        sort: vec![SortField {
            field: "created_at".into(),
            desc: true,
        }],
        limit: 5,
        skip_count: true,
        ..Default::default()
    };
    let recent_users_fut = db::list(ctx, USERS, &recent_users_opts);

    let recent_errors_fut = request_logs::list_recent_errors(ctx, 5);

    let (
        (user_count, new_users_today),
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
    let recent_users = recent_users_r.map(|rl| rl.records).unwrap_or_default();
    let recent_errors = recent_errors_r.unwrap_or_default();
    let users_daily_rows = users_daily_r.unwrap_or_default();
    let request_days = request_days_r.unwrap_or_default();

    // Two grouped statements back all three charts: the USERS series comes from
    // its own daily aggregate; the request-log "requests" and "errors" series
    // are two metrics projected out of the *same* per-day counts.
    let new_users_daily = series_from_rows(&users_daily_rows, "cnt", start_30d);
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
                                    @let email = record.str_field("email");
                                    @let created = record.str_field("created_at");
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
    //! Correctness: the consolidated user aggregates return byte-for-byte the
    //! same numbers the previous per-filter `db::count` / per-metric grouped
    //! queries produced. Each helper is checked against the equivalent
    //! separate `db::count` calls over the same seeded in-memory database, and
    //! against hand-computed expectations for the fixed seed. The request-log
    //! aggregates have the same check beside their owner,
    //! `platform_state::request_logs`.

    use std::collections::HashMap;

    use serde_json::json;
    use wafer_block::db::{Filter, FilterOp};
    use wafer_core::clients::database as db;

    use super::{series_from_rows, user_counts, window_30d, wire, USERS};
    use crate::{test_support::TestContext, util::daily_grouped};

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

    async fn seed_user(ctx: &TestContext, id: &str, created_at: &str, deleted_at: Option<&str>) {
        let mut data: HashMap<String, serde_json::Value> = HashMap::new();
        data.insert("id".into(), json!(id));
        data.insert("email".into(), json!(format!("{id}@example.test")));
        data.insert("display_name".into(), json!(id));
        data.insert("created_at".into(), json!(created_at));
        if let Some(ts) = deleted_at {
            data.insert("deleted_at".into(), json!(ts));
        }
        db::create(ctx, USERS, data)
            .await
            .unwrap_or_else(|e| panic!("seed user {id}: {e}"));
    }

    /// Value for `date` in a `(date, count)` series, or `-1` if the day is absent.
    fn day_value(series: &[(String, i64)], date: &str) -> i64 {
        series
            .iter()
            .find(|(d, _)| d == date)
            .map(|(_, c)| *c)
            .unwrap_or(-1)
    }

    fn sum(series: &[(String, i64)]) -> i64 {
        series.iter().map(|(_, c)| c).sum()
    }

    #[tokio::test]
    async fn consolidated_aggregates_match_per_filter_queries() {
        let ctx = TestContext::with_auth().await;

        let today = chrono::Utc::now().date_naive();
        // Noon timestamps so a stored `...T12:00:00` sorts after `today_start`
        // (`...T00:00:00`) yet buckets to the same day under SQLite's `date()`.
        let at = |ago: i64| {
            (today - chrono::Duration::days(ago))
                .format("%Y-%m-%dT12:00:00")
                .to_string()
        };
        let day = |ago: i64| {
            (today - chrono::Duration::days(ago))
                .format("%Y-%m-%d")
                .to_string()
        };
        let today_start = format!("{}T00:00:00", today.format("%Y-%m-%d"));

        // Users: 3 active today, 2 active 5d ago, 1 active 40d ago (outside the
        // 30-day window), 2 deleted today (excluded by `deleted_at IS NULL`).
        for i in 0..3 {
            seed_user(&ctx, &format!("u_today_{i}"), &at(0), None).await;
        }
        for i in 0..2 {
            seed_user(&ctx, &format!("u_5d_{i}"), &at(5), None).await;
        }
        seed_user(&ctx, "u_40d", &at(40), None).await;
        for i in 0..2 {
            seed_user(&ctx, &format!("u_del_{i}"), &at(0), Some(&at(0))).await;
        }

        // --- header tile counts: consolidated vs. separate per-filter counts ---
        let active = [Filter {
            field: "deleted_at".into(),
            operator: FilterOp::IsNull,
            value: serde_json::Value::Null,
        }];
        let active_today = [
            Filter {
                field: "deleted_at".into(),
                operator: FilterOp::IsNull,
                value: serde_json::Value::Null,
            },
            Filter {
                field: "created_at".into(),
                operator: FilterOp::GreaterEqual,
                value: json!(&today_start),
            },
        ];
        let total_expected = db::count(&ctx, USERS, &active).await.unwrap();
        let new_expected = db::count(&ctx, USERS, &active_today).await.unwrap();
        let (total, new_today) = user_counts(&ctx, &today_start).await;
        assert_eq!(
            (total, new_today),
            (total_expected, new_expected),
            "user_counts must match separate db::count calls"
        );
        assert_eq!((total, new_today), (6, 3), "hand-computed user counts");

        // --- daily chart series ---
        let (start_30d, start_iso) = window_30d();

        let users_rows = daily_grouped(
            &ctx,
            USERS,
            &start_iso,
            vec![Filter {
                field: "deleted_at".into(),
                operator: FilterOp::IsNull,
                value: serde_json::Value::Null,
            }],
            vec![wire::AggregateColumnDef::Count {
                alias: "cnt".into(),
            }],
        )
        .await
        .expect("daily users aggregate");
        let new_users_daily = series_from_rows(&users_rows, "cnt", start_30d);
        assert_eq!(new_users_daily.len(), 30, "30-entry zero-filled series");
        assert_eq!(day_value(&new_users_daily, &day(0)), 3, "3 users today");
        assert_eq!(day_value(&new_users_daily, &day(5)), 2, "2 users 5d ago");
        assert_eq!(sum(&new_users_daily), 5, "40d-ago user + deleted excluded");
    }
}
