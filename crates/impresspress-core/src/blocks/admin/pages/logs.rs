use maud::{html, Markup};
use wafer_block::db::{Filter, FilterOp, SortField};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, Message, OutputStream};

use super::{admin_page, crumb};
use crate::{
    blocks::admin::AUDIT_LOGS_TABLE as AUDIT_LOGS,
    platform_state::request_logs,
    ui::{
        components::{self, pagination},
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
                div .table-container {
                    table .table {
                        thead {
                            tr {
                                th { "Status" }
                                th { "Method" }
                                th { "Path" }
                                th { "Duration" }
                                th { "User" }
                                th { "Time" }
                            }
                        }
                        tbody {
                            @if list.rows.is_empty() {
                                tr {
                                    td colspan="6" .text-center .text-muted .p-8 { "No request logs yet" }
                                }
                            }
                            @for row in &list.rows {
                                @let status = row.status.as_str();
                                @let method = row.method.as_str();
                                @let path = row.path.as_str();
                                @let duration = row.duration_ms;
                                @let user_id = row.user_id.as_str();
                                @let created = row.created_at.as_str();
                                @let status_code = row.status_code;
                                tr {
                                    td {
                                        span .badge .(if status == "ERROR" { "badge-danger" } else if status_code >= 400 { "badge-warning" } else { "badge-success" }) {
                                            (status_code)
                                        }
                                    }
                                    td .text-sm .font-medium { (method.to_uppercase()) }
                                    td .text-sm { (path) }
                                    td .text-muted .text-sm { (duration) "ms" }
                                    td .text-muted .text-sm {
                                        @if !user_id.is_empty() {
                                            (user_id.get(..8).unwrap_or(user_id))
                                        }
                                    }
                                    td .text-muted .text-sm { (created.get(..19).unwrap_or(created)) }
                                }
                            }
                        }
                    }
                }

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
                div .table-container {
                    table .table {
                        thead {
                            tr {
                                th { "Action" }
                                th { "Resource" }
                                th { "User" }
                                th { "IP" }
                                th { "Time" }
                            }
                        }
                        tbody {
                            @if list.records.is_empty() {
                                tr {
                                    td colspan="5" .text-center .text-muted .p-8 { "No audit logs yet" }
                                }
                            }
                            @for record in &list.records {
                                @let action = record.str_field("action");
                                @let resource = record.str_field("resource");
                                @let user_id = record.str_field("user_id");
                                @let ip = record.str_field("ip_address");
                                @let created = record.str_field("created_at");
                                tr {
                                    td {
                                        span .badge .badge-info { (action) }
                                    }
                                    td .text-sm { (resource) }
                                    td .text-muted .text-sm { (user_id.get(..8).unwrap_or(user_id)) }
                                    td .text-muted .text-sm { (ip) }
                                    td .text-muted .text-sm { (created.get(..19).unwrap_or(created)) }
                                }
                            }
                        }
                    }
                }

                (pagination(list.page as u32, list.page_size as u32, list.total_count as u32, "/b/admin/logs?tab=audit"))
            }
            Err(e) => {
                div .login-error { "Failed to load audit logs: " (e.message) }
            }
        }
    }
}
