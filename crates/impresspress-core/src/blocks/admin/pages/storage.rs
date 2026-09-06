use maud::{html, Markup};
use wafer_block::db::{ListOptions, SortField};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, Message, OutputStream};

use super::{admin_page, crumb};
use crate::{
    blocks::admin::STORAGE_ACCESS_LOGS_TABLE as STORAGE_ACCESS_LOGS,
    ui::{
        components, icons,
        shell::Topbar,
        templates::{list_page, PageHeader},
    },
};

pub async fn storage_page(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let refresh_action = html! {
        button .btn .btn--secondary .btn--sm
            hx-get="/b/admin/storage"
            hx-target="#content"
        { (icons::refresh_cw()) " Refresh" }
    };

    // "No storage access logs yet." is what a deployment whose blocks have
    // never touched storage renders; an unreadable log must not borrow it.
    let logs_tab = match storage_logs_tab(ctx, msg).await {
        Ok(markup) => markup,
        Err(e) => {
            tracing::error!(error = %e, "admin storage page: access-log read failed");
            return crate::ui::server_error_response(msg);
        }
    };

    let tabs_and_body = html! {
        (components::tab_navigation(vec![components::Tab {
            active: true,
            href: "/b/admin/storage",
            label: "Access Logs",
            icon: Some(icons::eye()),
        }]))

        div #storage-tab-content {
            (logs_tab)
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
        "Storage",
        Topbar {
            crumbs: crumb("Storage"),
            primary_action: Some(refresh_action),
            subtitle: Some("Per-block storage isolation and access logs"),
            show_palette: true,
        },
        body,
    )
    .await
}

async fn storage_logs_tab(
    ctx: &dyn Context,
    _msg: &Message,
) -> Result<Markup, wafer_run::WaferError> {
    let logs = db::list(
        ctx,
        STORAGE_ACCESS_LOGS,
        &ListOptions {
            columns: Some(vec![
                "source_block".into(),
                "operation".into(),
                "path".into(),
                "status".into(),
                "created_at".into(),
            ]),
            sort: vec![SortField {
                field: "created_at".into(),
                desc: true,
            }],
            limit: 100,
            skip_count: true,
            ..Default::default()
        },
    )
    .await?
    .records;

    Ok(html! {
        p .text-muted .mb-4 {
            "Recent storage access by blocks. Each block is isolated to "
            code { "/storage/{block-name}/" }
            "."
        }

        div .table-container {
            table .table {
                thead {
                    tr {
                        th { "Block" }
                        th { "Operation" }
                        th { "Path" }
                        th { "Status" }
                        th { "Time" }
                    }
                }
                tbody {
                    @if logs.is_empty() {
                        tr {
                            td colspan="5" .text-center .text-muted .p-8 {
                                "No storage access logs yet."
                            }
                        }
                    }
                    @for log in &logs {
                        @let source = log.data.get("source_block").and_then(|v| v.as_str()).unwrap_or("");
                        @let op = log.data.get("operation").and_then(|v| v.as_str()).unwrap_or("");
                        @let path = log.data.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        @let status = log.data.get("status").and_then(|v| v.as_str()).unwrap_or("");
                        @let created = log.data.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
                        tr {
                            td {
                                @if !source.is_empty() {
                                    span .badge .badge-info { (source) }
                                }
                            }
                            td .text-sm .font-mono { (op) }
                            td .text-sm .font-mono { (path) }
                            td .text-sm {
                                @if status.starts_with("BLOCKED") {
                                    span .badge .badge-danger { (status) }
                                } @else if status.starts_with("ERROR") {
                                    span .badge .badge-warning { (status) }
                                } @else {
                                    span .text-muted { (status) }
                                }
                            }
                            td .text-muted .text-sm { (created.get(..19).unwrap_or(created)) }
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod outage_tests {
    //! "No storage access logs yet." is what a deployment whose blocks have
    //! never touched storage renders. An outage rendered the same sentence.

    use super::*;
    use crate::test_support::{admin_msg, output_http_status, TestContext};

    #[tokio::test]
    async fn a_failing_access_log_read_renders_the_error_page_not_an_empty_log() {
        let ctx = TestContext::with_admin().await.break_reads();
        let msg = admin_msg("retrieve", "/b/admin/storage");
        assert_eq!(
            output_http_status(storage_page(&ctx, &msg).await).await,
            500
        );
    }
}
