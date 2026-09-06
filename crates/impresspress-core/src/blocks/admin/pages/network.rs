use maud::{html, Markup};
use wafer_run::{context::Context, Message, OutputStream};

use crate::{
    platform_state::request_logs,
    ui::{components, icons},
};

/// Render JUST the network monitoring body. The parent `settings_page`
/// handler wraps this in the form-less `tabbed_page` shell. This tab is
/// read-only monitoring — it renders no `<form>` and has nothing to save.
pub async fn settings_body(ctx: &dyn Context, msg: &Message) -> Markup {
    html! {
        div .filter-bar .mb-2 {
            button .btn .btn--secondary .btn--sm
                hx-get="/b/admin/settings/network"
                hx-target="#content"
            { (icons::refresh_cw()) " Refresh" }
        }

        (components::tab_navigation(vec![components::Tab {
            active: true,
            href: "/b/admin/settings/network",
            label: "Inbound",
            icon: Some(icons::arrow_down_left()),
        }]))

        div #network-tab-content {
            (network_inbound_tab(ctx, msg).await)
        }
    }
}

async fn network_inbound_tab(ctx: &dyn Context, msg: &Message) -> Markup {
    let search = msg.query("search").to_string();

    let summary = request_logs::summarise_by_path(ctx, &search, 50)
        .await
        .unwrap_or_default();

    html! {
        div .filter-bar {
            (components::search_input_with_value("search", "Search by path...", "/b/admin/settings/network", "#content", &search))
        }

        style { (maud::PreEscaped("
            .expand-row { cursor: pointer; }
            .expand-row:hover { background: var(--bg-secondary, #f8fafc); }
            .detail-rows td { background: var(--bg-secondary, #f8fafc); font-size: 12px; }
            .detail-rows[hidden] { display: none; }
        ")) }
        // Delegated click handler — the row carries `data-detail-*` attributes
        // (maud-escaped) instead of an `onclick` JS-string literal, which maud
        // does NOT escape and so let an attacker-controlled request path break
        // out and run script in an admin's session. Bound once per document.
        script { (maud::PreEscaped("
            if (!window.__networkDetailBound) {
                window.__networkDetailBound = true;
                document.addEventListener('click', function (e) {
                    var row = e.target.closest('.expand-row[data-detail-target]');
                    if (!row) return;
                    var detail = document.getElementById(row.dataset.detailTarget);
                    if (!detail) return;
                    var dr = detail.closest('tr');
                    if (!dr.hidden) { dr.hidden = true; return; }
                    dr.hidden = false;
                    if (!detail.innerHTML) {
                        htmx.ajax('GET', row.dataset.detailUrl, {target: '#' + row.dataset.detailTarget, swap: 'innerHTML'});
                    }
                });
            }
        ")) }

        div .table-container {
            table .table {
                thead {
                    tr {
                        th .w-30 { "" }
                        th { "Method" }
                        th { "Path" }
                        th { "Requests" }
                        th { "Avg Duration" }
                        th { "Errors" }
                        th { "Last Seen" }
                    }
                }
                tbody {
                    @if summary.is_empty() {
                        tr {
                            td colspan="7" .text-center .text-muted .p-8 { "No inbound requests yet" }
                        }
                    }
                    @for row in &summary {
                        (inbound_row(&row.method, &row.path, row.count, row.avg_ms, row.errors, &row.last_seen))
                    }
                }
            }
        }
    }
}

/// Render one inbound-summary row: the clickable row plus its lazily-loaded
/// detail row. `method`/`path` come from the request log and are
/// attacker-controlled (any HTTP request with a crafted path is logged), so
/// they appear only in maud-escaped attribute/text contexts. The row carries
/// `data-detail-target`/`data-detail-url` that the delegated click handler
/// reads — never an `onclick` JS-string literal (maud doesn't escape JS-string
/// context, which was a stored-XSS sink).
fn inbound_row(
    method: &str,
    path: &str,
    cnt: i64,
    avg_ms: i64,
    errors: i64,
    last_seen: &str,
) -> Markup {
    let row_id = format!("inbound-{}-{}", method, path.replace('/', "_"));
    let detail_url = format!("/b/admin/network/detail/inbound?method={method}&path={path}");
    html! {
        tr .expand-row data-detail-target=(row_id) data-detail-url=(detail_url) {
            td .text-muted { (icons::chevron_right()) }
            td .text-sm .font-medium { (method.to_uppercase()) }
            td .text-sm { (path) }
            td .text-sm {
                span .badge .badge-info { (cnt) }
            }
            td .text-muted .text-sm { (avg_ms) "ms" }
            td .text-sm {
                @if errors > 0 {
                    span .badge .badge-danger { (errors) }
                } @else {
                    span .text-muted { "0" }
                }
            }
            td .text-muted .text-sm { (last_seen.get(..19).unwrap_or(last_seen)) }
        }
        tr .detail-rows hidden {
            td colspan="7" .p-0 {
                div id=(row_id) {}
            }
        }
    }
}

/// Htmx fragment: individual requests for a given inbound path.
pub async fn network_inbound_detail(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let method = msg.query("method").to_string();
    let path = msg.query("path").to_string();
    let offset: i64 = msg.query("offset").parse().unwrap_or(0);
    let limit: i64 = 20;

    // One more than the page shows, to learn whether a next page exists.
    let rows = request_logs::list_for_path(ctx, &method, &path, offset, limit + 1)
        .await
        .unwrap_or_default();

    let has_more = rows.len() as i64 > limit;
    let display_rows = if has_more {
        &rows[..limit as usize]
    } else {
        &rows
    };

    let markup = html! {
        table .table .m-0 {
            thead {
                tr {
                    th { "Status" }
                    th { "Duration" }
                    th { "IP" }
                    th { "User" }
                    th { "Time" }
                }
            }
            tbody {
                @for row in display_rows {
                    @let status_code = row.status_code;
                    @let duration = row.duration_ms;
                    @let client_ip = row.client_ip.as_str();
                    @let user_id = row.user_id.as_str();
                    @let created = row.created_at.as_str();
                    tr {
                        td {
                            span .badge .(if status_code >= 500 { "badge-danger" } else if status_code >= 400 { "badge-warning" } else { "badge-success" }) {
                                (status_code)
                            }
                        }
                        td .text-muted { (duration) "ms" }
                        td .text-muted { (client_ip) }
                        td .text-muted {
                            @if !user_id.is_empty() {
                                (user_id.get(..8).unwrap_or(user_id))
                            }
                        }
                        td .text-muted { (created.get(..19).unwrap_or(created)) }
                    }
                }
            }
        }
        @if has_more {
            @let next_offset = offset + limit;
            div .text-center .p-2 {
                button .btn .btn--secondary .btn--sm
                    hx-get={"/b/admin/network/detail/inbound?method=" (method) "&path=" (path) "&offset=" (next_offset)}
                    hx-target="closest div"
                    hx-swap="outerHTML"
                { "Load more" }
            }
        }
    };
    crate::ui::html_response(markup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_row_has_no_js_string_xss_sink() {
        // Attacker-controlled request path crafted to break out of the old
        // `onclick="toggleDetail('…')"` JS-string literal.
        let html = inbound_row(
            "GET",
            "'); alert(document.cookie); //",
            1,
            2,
            0,
            "2026-01-01T00:00:00Z",
        )
        .into_string();

        // The JS-string sink is gone entirely.
        assert!(
            !html.contains("onclick"),
            "must not emit an onclick JS-string sink: {html}"
        );
        // Replaced by maud-escaped data-* attributes the delegated handler reads.
        assert!(
            html.contains("data-detail-target="),
            "row must carry data-detail-target: {html}"
        );
        assert!(
            html.contains("data-detail-url="),
            "row must carry data-detail-url: {html}"
        );
        // maud escapes the attribute value (e.g. the URL's `&`), proving the
        // path lands in escaped attribute context, not a raw/JS sink.
        assert!(
            html.contains("method=GET&amp;path="),
            "detail URL must be HTML-escaped in the attribute: {html}"
        );
        assert!(
            !html.contains("method=GET&path="),
            "a raw unescaped query would mean an injection sink survived: {html}"
        );
    }
}
