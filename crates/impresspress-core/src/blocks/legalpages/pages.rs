//! SSR admin pages for the legal pages block.
//!
//! Provides a tabbed admin UI with:
//! - Privacy Policy editor (Quill rich text editor)
//! - Terms of Service editor (Quill rich text editor)
//! - API endpoints reference

use maud::{html, Markup, PreEscaped};
use wafer_run::{context::Context, ErrorCode, InputStream, Message, OutputStream, WaferError};

use super::{
    repo::documents::{self, DocumentRow, NewDraft},
    service,
};
use crate::{
    http::{err_bad_request, err_internal, err_not_found, ok_json, ResponseBuilder},
    ui::{self, components, icons, settings_form},
};

// ---------------------------------------------------------------------------
// Document lookup
// ---------------------------------------------------------------------------

/// Find the current document for a given type.
/// Prefers the latest draft (so admin sees their in-progress edits),
/// then falls back to the published version.
///
/// A read failure is an `Err`, not an empty editor: rendering "no document"
/// over a database error invites the admin to type a replacement into a form
/// whose save then forks the document they could not see.
async fn find_current_doc(
    ctx: &dyn Context,
    doc_type: &str,
) -> Result<Option<DocumentRow>, WaferError> {
    if let Some(draft) = documents::find_latest_draft(ctx, doc_type).await? {
        return Ok(Some(draft));
    }
    documents::find_published(ctx, doc_type).await
}

// ---------------------------------------------------------------------------
// Editor page (Privacy / Terms)
// ---------------------------------------------------------------------------

pub async fn editor_page(ctx: &dyn Context, msg: &Message, doc_type: &str) -> OutputStream {
    let doc = match find_current_doc(ctx, doc_type).await {
        Ok(doc) => doc,
        Err(e) => return err_internal("Failed to load the legal document", e),
    };
    let default_title = if doc_type == "privacy" {
        "Privacy Policy"
    } else {
        "Terms of Service"
    };

    let (doc_id, title, content, status, updated_at, version) = match &doc {
        Some(d) => (
            d.id.as_str(),
            if d.title.is_empty() {
                default_title
            } else {
                d.title.as_str()
            },
            d.content.as_str(),
            d.status.as_str(),
            d.updated_at.as_str(),
            d.version,
        ),
        None => ("", default_title, "", "none", "", 1),
    };

    let page_content = editor_markup_for_test(
        doc_type, doc_id, title, content, status, updated_at, version,
    );

    ui::shell_page(
        ctx,
        msg,
        ui::Shell::simple(default_title, ui::NavKind::Portal, default_title),
        page_content,
    )
    .await
}

/// Build the editor markup. Split out from `editor_page` so it can be
/// unit-tested without a `Context`.
pub(super) fn editor_markup_for_test(
    doc_type: &str,
    doc_id: &str,
    title: &str,
    content: &str,
    status: &str,
    updated_at: &str,
    version: i64,
) -> Markup {
    let default_title = if doc_type == "privacy" {
        "Privacy Policy"
    } else {
        "Terms of Service"
    };
    let badge_class = match status {
        "published" => "badge-success",
        "draft" => "badge-warning",
        _ => "badge-info",
    };
    let badge_text = match status {
        "published" => "Published",
        "draft" => "Draft",
        "archived" => "Archived",
        _ => "No document",
    };

    html! {
        // Status bar (compact, top of page)
        div .flex .items-center .justify-between .mb-3 {
            div .flex .items-center .gap-2 {
                h2 .editor-status__title { (default_title) }
                span #status-badge .badge .(badge_class) { (badge_text) }
                span .badge .editor-status__version .text-xs .cursor-pointer
                    title="Click to change version"
                    onclick="promptVersion()"
                { "v" span #version-display { (version) } }
                @if !updated_at.is_empty() {
                    span .text-muted .text-xs {
                        " \u{00b7} " (updated_at.get(..10).unwrap_or(updated_at))
                    }
                }
            }
            div .flex .gap-2 {
                a .btn .btn--sm .btn--ghost
                    href={"/b/legalpages/" (doc_type)}
                    target="_blank"
                {
                    "Open public page"
                }
                button #btn-save .btn .btn--sm .btn--secondary onclick="saveDocument(false)" {
                    "Save Draft"
                }
                button #btn-publish .btn .btn--sm .btn--primary onclick="saveDocument(true)" {
                    "Publish"
                }
            }
        }

        // Title input
        input #title-input .form-input .text-lg .font-semibold .mb-2
            type="text"
            name="title"
            value=(title)
            placeholder="Document title";

        // Hidden fields used by save handler JS
        input #doc-type type="hidden" value=(doc_type);
        input #doc-id type="hidden" value=(doc_id);
        input #doc-version type="hidden" value=(version);

        // Tab strip
        div .editor-tabs {
            button .editor-tab .editor-tab--active type="button"
                data-tab="edit"
                onclick="setEditorTab('edit')"
            { "Edit" }
            button .editor-tab type="button"
                data-tab="preview"
                onclick="setEditorTab('preview')"
            { "Preview" }
        }

        // Edit pane (textarea)
        div #editor-edit-pane .editor-pane {
            textarea #editor .form-input .editor-textarea
                name="content"
                placeholder="Write your legal document in Markdown..."
            { (content) }
        }

        // Preview pane (vanilla JS fetch target)
        div #editor-preview-pane .editor-pane .hidden {
            div #editor-preview .preview-content {
                p .text-muted { "Click Preview above to render." }
            }
        }

        script { (PreEscaped(EDITOR_JS)) }
    }
}

const EDITOR_JS: &str = r#"
(function() {
    // Preview wiring: vanilla JS fetch (no json-enc htmx extension loaded)
    window.setEditorTab = function(name) {
        document.querySelectorAll('.editor-tab').forEach(function(t) {
            t.classList.toggle('editor-tab--active', t.dataset.tab === name);
        });
        document.getElementById('editor-edit-pane').classList.toggle('hidden', name !== 'edit');
        document.getElementById('editor-preview-pane').classList.toggle('hidden', name !== 'preview');
        if (name === 'preview') {
            var content = document.getElementById('editor').value;
            fetch('/b/legalpages/admin/render-preview', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ content: content })
            })
            .then(function(r) {
                if (!r.ok) { throw new Error('HTTP ' + r.status); }
                return r.text();
            })
            .then(function(html) { document.getElementById('editor-preview').innerHTML = html; })
            .catch(function(err) {
                document.getElementById('editor-preview').innerHTML =
                    '<p class="text-danger">Preview failed: ' + err.message + '</p>';
            });
        }
    };

    window.promptVersion = function() {
        var current = document.getElementById('doc-version').value;
        var v = prompt('Set version number:', current);
        if (v !== null && v.trim() !== '') {
            var num = parseInt(v, 10);
            if (num > 0) {
                document.getElementById('doc-version').value = num;
                document.getElementById('version-display').textContent = num;
            }
        }
    };

    // Ctrl+S / Cmd+S → save draft
    document.addEventListener('keydown', function(e) {
        if ((e.ctrlKey || e.metaKey) && e.key === 's') {
            e.preventDefault();
            saveDocument(false);
        }
    });

    // Save handler (reads textarea .value)
    window.saveDocument = function(publish) {
        var title = document.getElementById('title-input').value;
        var content = document.getElementById('editor').value;
        var docType = document.getElementById('doc-type').value;
        var docId = document.getElementById('doc-id').value;
        var version = parseInt(document.getElementById('doc-version').value, 10) || 0;
        var url = publish ? '/b/legalpages/admin/publish' : '/b/legalpages/admin/save';

        var btn = document.getElementById(publish ? 'btn-publish' : 'btn-save');
        var origText = btn.textContent;
        btn.disabled = true;
        btn.textContent = publish ? 'Publishing...' : 'Saving...';

        fetch(url, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ doc_type: docType, title: title, content: content, doc_id: docId, version: version })
        })
        .then(function(r) { return r.json(); })
        .then(function(data) {
            document.body.dispatchEvent(new CustomEvent('showToast', {
                detail: { message: data.message || (data.error || 'Done'), type: data.error ? 'error' : 'success' }
            }));
            if (data.doc_id) document.getElementById('doc-id').value = data.doc_id;
            if (data.version) {
                document.getElementById('doc-version').value = data.version;
                document.getElementById('version-display').textContent = data.version;
            }
            if (data.status) {
                var badge = document.getElementById('status-badge');
                if (badge) {
                    badge.className = 'badge ' + (data.status === 'published' ? 'badge-success' : 'badge-warning');
                    badge.textContent = data.status.charAt(0).toUpperCase() + data.status.slice(1);
                }
            }
        })
        .catch(function(err) {
            document.body.dispatchEvent(new CustomEvent('showToast', {
                detail: { message: 'Error: ' + err.message, type: 'error' }
            }));
        })
        .finally(function() {
            btn.disabled = false;
            btn.textContent = origText;
        });
    };
})();
"#;

// ---------------------------------------------------------------------------
// Endpoints page
// ---------------------------------------------------------------------------

pub async fn endpoints_page(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let content = html! {
        (components::page_header("API Endpoints", Some("Available endpoints for legal pages"), None))

        // Public endpoints
        h3 .card-title .mb-2 { "Public Endpoints" }
        p .text-muted .text-sm .mb-3 {
            "These endpoints are publicly accessible and return formatted HTML pages."
        }
        div .table-container .mb-8 {
            table .table {
                thead {
                    tr {
                        th .w-80 { "Method" }
                        th { "Endpoint" }
                        th { "Description" }
                    }
                }
                tbody {
                    tr {
                        td { span .badge .badge-success { "GET" } }
                        td { code { "/b/legalpages/terms" } }
                        td { "View published Terms of Service page" }
                    }
                    tr {
                        td { span .badge .badge-success { "GET" } }
                        td { code { "/b/legalpages/privacy" } }
                        td { "View published Privacy Policy page" }
                    }
                }
            }
        }

        // Admin API
        h3 .card-title .mb-2 { "Admin API Endpoints" }
        p .text-muted .text-sm .mb-3 {
            "These endpoints require admin authentication and return JSON responses."
        }
        div .table-container {
            table .table {
                thead {
                    tr {
                        th .w-80 { "Method" }
                        th { "Endpoint" }
                        th { "Description" }
                    }
                }
                tbody {
                    tr {
                        td { span .badge .badge-success { "GET" } }
                        td { code { "/b/legalpages/api/documents" } }
                        td { "List all documents (supports " code { "?type=terms|privacy" } " filter)" }
                    }
                    tr {
                        td { span .badge .badge-info { "POST" } }
                        td { code { "/b/legalpages/api/documents" } }
                        td { "Create a new document " span .text-muted { "(body: doc_type, title, content)" } }
                    }
                    tr {
                        td { span .badge .badge-warning { "PATCH" } }
                        td { code { "/b/legalpages/api/documents/:id" } }
                        td { "Update a document" }
                    }
                    tr {
                        td { span .badge .badge-info { "POST" } }
                        td { code { "/b/legalpages/api/documents/:id/publish" } }
                        td { "Publish a document (archives previous published version)" }
                    }
                    tr {
                        td { span .badge .badge-danger { "DELETE" } }
                        td { code { "/b/legalpages/api/documents/:id" } }
                        td { "Delete a document" }
                    }
                }
            }
        }

        // Document schema
        h3 .card-title .mt-8 .mb-2 { "Document Schema" }
        p .text-muted .text-sm .mb-3 {
            "Each legal document has the following fields."
        }
        div .table-container {
            table .table {
                thead {
                    tr {
                        th { "Field" }
                        th { "Type" }
                        th { "Description" }
                    }
                }
                tbody {
                    tr {
                        td { code { "doc_type" } }
                        td { "string" }
                        td { "Document type: " code { "terms" } " or " code { "privacy" } }
                    }
                    tr {
                        td { code { "title" } }
                        td { "string" }
                        td { "Document title" }
                    }
                    tr {
                        td { code { "content" } }
                        td { "text" }
                        td { "HTML content of the document" }
                    }
                    tr {
                        td { code { "status" } }
                        td { "string" }
                        td {
                            "Document status: "
                            span .badge .badge-warning { "draft" }
                            " "
                            span .badge .badge-success { "published" }
                            " "
                            span .badge { "archived" }
                        }
                    }
                    tr {
                        td { code { "version" } }
                        td { "int" }
                        td { "Version number" }
                    }
                    tr {
                        td { code { "published_at" } }
                        td { "datetime" }
                        td { "When the document was last published" }
                    }
                }
            }
        }
    };

    ui::shell_page(
        ctx,
        msg,
        ui::Shell::simple("Endpoints", ui::NavKind::Portal, "Endpoints"),
        content,
    )
    .await
}

// ---------------------------------------------------------------------------
// Save / Publish handlers
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct SaveRequest {
    doc_type: String,
    title: String,
    content: String,
    #[serde(default)]
    doc_id: String,
    #[serde(default)]
    version: i64,
}

/// Save a draft document. If the current doc is published, creates a new draft
/// so the live version stays untouched until the admin explicitly publishes.
pub async fn handle_save(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let raw = input.collect_to_bytes().await;
    let body: SaveRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        // Previously returned 200 OK with an `error` key — htmx clients
        // would still treat that as success. Use the proper 4xx so the
        // caller can branch on status alone.
        Err(e) => return err_bad_request(&format!("Invalid request: {e}")),
    };

    // Three outcomes, not two. The lookup used to fold its `Err` into "no
    // such document, create a draft", so a transient read failure forked the
    // document the admin was editing into a second row and answered 200
    // (B10). An error is now reported; only a genuinely absent row, or a
    // *published* one, creates a draft.
    let existing = if body.doc_id.is_empty() {
        None
    } else {
        match documents::get(ctx, &body.doc_id).await {
            Ok(found) => found,
            Err(e) => return err_internal("Failed to load the legal-page document", e),
        }
    };

    // Editing a published document creates a new draft instead of modifying
    // the live version, so the published text stays untouched until the admin
    // explicitly publishes again.
    let saved = match existing {
        Some(doc) if doc.status != "published" => {
            documents::update_content(ctx, &doc.id, Some(&body.title), Some(&body.content))
                .await
                .map(|row| row.id)
        }
        _ => documents::insert_draft(
            ctx,
            NewDraft {
                doc_type: &body.doc_type,
                title: &body.title,
                content: &body.content,
                created_by: msg.user_id(),
            },
        )
        .await
        .map(|row| row.id),
    };

    match saved {
        Ok(doc_id) => ok_json(&serde_json::json!({
            "doc_id": doc_id,
            "status": "draft",
            "message": "Draft saved"
        })),
        Err(e) => err_internal("Failed to save legal-page draft", e),
    }
}

/// Save and publish a document. Archives any previously published document
/// of the same type (publish-then-archive ordering lives in
/// `service::publish_document`).
pub async fn handle_publish(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let raw = input.collect_to_bytes().await;
    let body: SaveRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        // Previously returned 200 OK with an `error` key — clients would
        // still treat that as success. Use the proper 4xx so the caller
        // can branch on status alone (matches `handle_save`).
        Err(e) => return err_bad_request(&format!("Invalid request: {e}")),
    };

    let published = match service::publish_document(
        ctx,
        service::PublishRequest {
            doc_type: &body.doc_type,
            doc_id: &body.doc_id,
            title: Some(&body.title),
            content: Some(&body.content),
            version: body.version,
            created_by: msg.user_id(),
        },
    )
    .await
    {
        Ok(p) => p,
        Err(e) if e.code == ErrorCode::NotFound => return err_not_found("Document not found"),
        Err(e) => return err_internal("Failed to publish legal page", e),
    };

    ok_json(&serde_json::json!({
        "doc_id": published.row.id,
        "status": "published",
        "version": published.version,
        "message": format!("Published as v{}", published.version)
    }))
}

// ---------------------------------------------------------------------------
// Settings page
// ---------------------------------------------------------------------------

pub async fn settings_page(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let vars = super::config_vars();
    let sections = [settings_form::SettingsSection::new(
        "Appearance",
        icons::settings(),
        &vars,
    )];

    // The live-preview links ride in the form's `extra` slot so they stay
    // inside the settings form (above the Save button), as before.
    let preview = html! {
        div .card .mb-5 .p-4 {
            h4 .text-sm .font-semibold .mb-2 { "Preview" }
            p .text-muted .text-xs .mb-3 {
                "See how your changes look on the public pages."
            }
            div .flex .gap-2 {
                a .btn .btn--sm .btn--ghost href="/b/legalpages/privacy" target="_blank" {
                    (icons::eye()) " Privacy Policy"
                }
                a .btn .btn--sm .btn--ghost href="/b/legalpages/terms" target="_blank" {
                    (icons::eye()) " Terms of Service"
                }
            }
        }
    };

    let saved = msg.query("saved") == "1";

    let content = html! {
        (components::page_header("Settings", Some("Customize the public legal pages appearance"), None))

        @if saved {
            div .alert .alert--success .mb-4 {
                span aria-hidden="true" { (icons::check()) }
                "Settings saved successfully."
            }
        }

        (settings_form::settings_form(ctx, "/b/legalpages/admin/settings", &sections, preview).await)
    };

    ui::shell_page(
        ctx,
        msg,
        ui::Shell::simple("Settings", ui::NavKind::Portal, "Settings"),
        content,
    )
    .await
}

pub async fn handle_save_settings(ctx: &dyn Context, input: InputStream) -> OutputStream {
    settings_form::save_settings(ctx, input, &super::config_vars(), "legalpages").await
}

// ---------------------------------------------------------------------------
// Preview rendering (used by editor's Preview tab via htmx)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct PreviewRequest {
    content: String,
}

/// Render Markdown into the same `<div class="public-page__content">`
/// wrapper used by the live `/b/legalpages/{terms,privacy}` pages, so
/// the Preview tab in the editor matches production typography exactly.
pub(super) fn render_preview_fragment(markdown: &str) -> String {
    let rendered = super::markdown_to_html(markdown);
    format!(r#"<div class="public-page__content">{rendered}</div>"#)
}

/// `POST /b/legalpages/admin/render-preview` — body: `{"content": "<markdown>"}`.
/// Returns the rendered HTML fragment for direct htmx swap into the
/// preview pane.
pub async fn handle_render_preview(_ctx: &dyn Context, input: InputStream) -> OutputStream {
    let raw = input.collect_to_bytes().await;
    let body: PreviewRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid request: {e}")),
    };
    let fragment = render_preview_fragment(&body.content);
    ResponseBuilder::new().body(fragment.into_bytes(), "text/html; charset=utf-8")
}
