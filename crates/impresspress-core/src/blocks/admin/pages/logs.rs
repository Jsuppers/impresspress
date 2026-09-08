use maud::{html, Markup};
use wafer_block::db::{Filter, FilterOp, SortField};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, Message, OutputStream};

use super::{admin_page, crumb};
use crate::{
    blocks::admin::AUDIT_LOGS_TABLE as AUDIT_LOGS,
    platform_state::request_logs,
    ui::{
        components::{self, badge, pagination, Badge, BadgeVariant},
        icons,
        shell::Topbar,
        templates::{list_page, PageHeader},
    },
    util::RecordExt,
};

pub async fn logs_page(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let tab = msg.query("tab");
    let active_tab = match tab {
        "audit" => "audit",
        _ => "system",
    };

    let refresh_action = html! {
        button .btn .btn--secondary .btn--sm
            hx-get={"/b/admin/logs?tab=" (active_tab)}
            hx-target="#content"
        { (icons::refresh_cw()) " Refresh" }
    };

    let tabs_and_body = html! {
        (components::tab_navigation(vec![
            components::Tab {
                active: active_tab == "system",
                href: "/b/admin/logs",
                label: "System Logs",
                icon: Some(icons::server()),
            },
            components::Tab {
                active: active_tab == "audit",
                href: "/b/admin/logs?tab=audit",
                label: "Audit Logs",
                icon: Some(icons::file_text()),
            },
        ]))

        div #logs-tab-content {
            @if active_tab == "system" {
                (system_logs_tab(ctx, msg).await)
            } @else {
                (audit_logs_tab(ctx, msg).await)
            }
        }
    };

    let body = list_page(
        PageHeader {
            title: "",
            subtitle: None,
            primary_action: None,
        },
        None,
        tabs_and_body,
        None,
    );

    admin_page(
        ctx,
        msg,
        "Logs",
        Topbar {
            crumbs: crumb("Logs"),
            primary_action: Some(refresh_action),
            subtitle: Some("System telemetry and admin audit trail"),
            show_palette: true,
        },
        body,
    )
    .await
}

async fn system_logs_tab(ctx: &dyn Context, msg: &Message) -> Markup {
    let (page, page_size, _) = msg.pagination_params(50);
    let search = msg.query("search").to_string();

    let result = request_logs::paginated(ctx, page as i64, page_size as i64, &search).await;

    html! {
        div .filter-bar {
            (components::search_input_with_value("search", "Search by path...", "/b/admin/logs", "#content", &search))
        }

        @match &result {
            Ok(list) => {
                @let rows: Vec<Vec<Markup>> = list.rows.iter().map(|row| {
                    let status = row.status.as_str();
                    let path = row.path.as_str();
                    let user_id = row.user_id.as_str();
                    let created = row.created_at.as_str();
                    let status_code = row.status_code;
                    let variant = if status == "ERROR" {
                        BadgeVariant::Danger
                    } else if status_code >= 400 {
                        BadgeVariant::Warning
                    } else {
                        BadgeVariant::Success
                    };
                    vec![
                        Badge::new(variant).render(html! { (status_code) }),
                        html! { span .font-medium { (row.method.to_uppercase()) } },
                        html! { (path) },
                        html! { span .text-muted { (row.duration_ms) "ms" } },
                        html! {
                            @if !user_id.is_empty() {
                                span .text-muted { (user_id.get(..8).unwrap_or(user_id)) }
                            }
                        },
                        html! { span .text-muted { (created.get(..19).unwrap_or(created)) } },
                    ]
                }).collect();

                (components::data_table::<fn(usize) -> Option<String>>(
                    &SYSTEM_LOG_COLUMNS,
                    rows,
                    None,
                    html! { p .text-center .text-muted { "No request logs yet" } },
                ))

                (pagination(list.page as u32, list.page_size as u32, list.total_count as u32, "/b/admin/logs"))
            }
            Err(e) => {
                div .login-error { "Failed to load request logs: " (e.message) }
            }
        }
    }
}

async fn audit_logs_tab(ctx: &dyn Context, msg: &Message) -> Markup {
    let (page, page_size, _) = msg.pagination_params(50);
    let search = msg.query("search").to_string();

    let mut filters = Vec::new();
    if !search.is_empty() {
        filters.push(Filter {
            field: "resource".into(),
            operator: FilterOp::Like,
            value: serde_json::Value::String(format!("%{search}%")),
        });
    }

    let sort = vec![SortField {
        field: "created_at".into(),
        desc: true,
    }];
    let result = db::paginated_list(
        ctx,
        AUDIT_LOGS,
        page as i64,
        page_size as i64,
        filters,
        sort,
    )
    .await;

    html! {
        div .filter-bar {
            (components::search_input_with_value("search", "Search by resource...", "/b/admin/logs?tab=audit", "#content", &search))
        }

        @match &result {
            Ok(list) => {
                @let rows: Vec<Vec<Markup>> = list.records.iter().map(|record| {
                    let user_id = record.str_field("user_id");
                    let created = record.str_field("created_at");
                    vec![
                        badge(BadgeVariant::Info, record.str_field("action")),
                        html! { (record.str_field("resource")) },
                        html! { span .text-muted { (user_id.get(..8).unwrap_or(user_id)) } },
                        html! { span .text-muted { (record.str_field("ip_address")) } },
                        html! { span .text-muted { (created.get(..19).unwrap_or(created)) } },
                    ]
                }).collect();

                (components::data_table::<fn(usize) -> Option<String>>(
                    &AUDIT_LOG_COLUMNS,
                    rows,
                    None,
                    html! { p .text-center .text-muted { "No audit logs yet" } },
                ))

                (pagination(list.page as u32, list.page_size as u32, list.total_count as u32, "/b/admin/logs?tab=audit"))
            }
            Err(e) => {
                div .login-error { "Failed to load audit logs: " (e.message) }
            }
        }
    }
}

/// The two log tables' columns. Declared once each so the `<td data-label>`
/// the component stamps on every cell names the same column its header does.
const SYSTEM_LOG_COLUMNS: [components::TableCol<'static>; 6] = [
    components::TableCol {
        label: "Status",
        width: None,
    },
    components::TableCol {
        label: "Method",
        width: None,
    },
    components::TableCol {
        label: "Path",
        width: None,
    },
    components::TableCol {
        label: "Duration",
        width: None,
    },
    components::TableCol {
        label: "User",
        width: None,
    },
    components::TableCol {
        label: "Time",
        width: None,
    },
];

const AUDIT_LOG_COLUMNS: [components::TableCol<'static>; 5] = [
    components::TableCol {
        label: "Action",
        width: None,
    },
    components::TableCol {
        label: "Resource",
        width: None,
    },
    components::TableCol {
        label: "User",
        width: None,
    },
    components::TableCol {
        label: "IP",
        width: None,
    },
    components::TableCol {
        label: "Time",
        width: None,
    },
];
