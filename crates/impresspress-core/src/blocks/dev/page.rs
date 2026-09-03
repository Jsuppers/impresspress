//! `GET /b/dev` — the workspace document, and the two assets it pulls.
//!
//! The page is the human half of the sandbox. The agent works through
//! `/b/dev/api/*` and the tools projected from it; this document is what lets
//! the human see what the agent did, edit a file themselves, watch an
//! activation land, and read the live site — plus the "how this works"
//! section and the suggested prompt that get a first-time visitor started.
//!
//! # Why it builds its own response
//!
//! Every other page in the crate is a [`crate::ui::shell_page`] call. This one
//! needs three headers that page cannot carry:
//!
//! * `Cross-Origin-Opener-Policy: same-origin` and
//!   `Cross-Origin-Embedder-Policy: require-corp` — together they make the
//!   document cross-origin isolated, which is what `SharedArrayBuffer` (and
//!   so the in-browser Rust compiler) requires. They are set here, on this
//!   document only, rather than block- or deployment-wide: isolating the JSON
//!   API buys nothing, and a header on every response would read as a policy
//!   the deployment does not actually have.
//! * `Cache-Control: no-store` — the block-wide rule (design §12).
//!
//! An `OutputStream`'s response meta is fixed when the stream is built, so
//! the headers have to be on the response as it is constructed. Hence
//! [`crate::ui::shell_document`], which renders the shelled markup and leaves
//! the response to the caller.

use maud::{html, Markup};
use wafer_run::{context::Context, Message, OutputStream};

use super::{assets, no_store};
use crate::{http::ResponseBuilder, ui};

/// The prompt the "Suggested prompt" disclosure offers for copying.
///
/// It is one paragraph on purpose: a visitor pastes it verbatim, so it has to
/// name the tools (`shop_create_product`, `shop_create_offer`,
/// `shop_publish_offer`, `shop_update_product`) and the endpoints the agent
/// would otherwise have to discover, and end by asking for the result to be
/// shown — which is what makes the live-site iframe the last thing that moves.
const SUGGESTED_PROMPT: &str = "Build me a small online shop for handmade ceramics. Create a home \
page at site/index.html that lists products from /b/products/catalog and lets a visitor open one, \
using the storefront widget from /b/products/storefront.js. Then create three products with \
shop_create_product, give each a published offer with shop_create_offer and shop_publish_offer, \
and set their status to active with shop_update_product. Show me the live site when you are done.";

/// Serve the workspace document.
pub async fn handle(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let shell = ui::Shell::simple("Workspace", ui::NavKind::Admin, "Workspace");
    let markup = ui::shell_document(ctx, msg, shell, body()).await;
    no_store()
        .set_header("Cross-Origin-Opener-Policy", "same-origin")
        .set_header("Cross-Origin-Embedder-Policy", "require-corp")
        .body(
            markup.into_string().into_bytes(),
            "text/html; charset=utf-8",
        )
}

/// The page body: six panes with stable ids, then the assets that drive them.
///
/// The ids are the contract between this markup, `dev.js` and the end-to-end
/// test — `dev.js` looks every element up by id and does nothing else with
/// the document's shape, so the layout can change without touching it.
fn body() -> Markup {
    html! {
        div .dev-workspace {
            section #dev-guide .dev-pane {
                h2 { "How this workspace works" }
                p {
                    "This page is a WebMCP workspace. An agent in your browser sees the tools \
                     registered here and can edit the site under " code { "site/" } ", write Rust \
                     backend blocks under " code { "blocks/<name>/" } ", stock the shop with the "
                    code { "shop_*" } " tools, and export the result. Every successful change is \
                     live at " a href="/" target="_blank" { "/" } " immediately; "
                    code { "dev_rollback" } " undoes a generation."
                }
                p {
                    "Start with " code { "dev_status" } ". Credentials for this browser-local \
                     instance: " code { "admin@example.com" } " / " code { "admin123" } "."
                }
                details {
                    summary { "Suggested prompt" }
                    pre #dev-suggested-prompt { (SUGGESTED_PROMPT) }
                }
            }
            section #dev-files .dev-pane {
                h2 { "Files" }
                ul #dev-file-list {}
                button #dev-new-file .btn .btn-secondary type="button" { "New file" }
            }
            section #dev-editor .dev-pane {
                h2 #dev-editor-title { "Editor" }
                textarea #dev-editor-text spellcheck="false" {}
                div .dev-editor-actions {
                    button #dev-save .btn .btn-primary type="button" { "Save" }
                    button #dev-delete .btn .btn-danger type="button" { "Delete" }
                }
            }
            section #dev-preview .dev-pane {
                h2 { "Live site" }
                // `sandbox` is the prompt-injection boundary (design §4.2):
                // whatever the agent — or a shopper's content — puts on the
                // published site renders here, and must not be able to reach
                // this document's tools. `allow-same-origin` is what lets the
                // parent reload the frame after a generation lands; the site
                // it frames is same-origin anyway, so it grants nothing the
                // frame could not otherwise obtain.
                iframe #dev-preview-frame
                    src="/"
                    sandbox="allow-scripts allow-same-origin allow-forms allow-popups"
                    title="Live site" {}
            }
            section #dev-progress .dev-pane {
                h2 { "Progress" }
                ol #dev-progress-steps {}
                pre #dev-log {}
            }
            section #dev-actions .dev-pane {
                // Both stay `disabled` until the compiler and the exporter
                // exist. The buttons ship anyway so the page's shape is the
                // final one and the human can see what is coming; the agent
                // sees the same two names as tools that refuse honestly.
                button #dev-compile .btn .btn-secondary type="button" disabled { "Compile block" }
                button #dev-export .btn .btn-secondary type="button" disabled { "Export" }
                button #dev-refresh-tools .btn .btn-secondary type="button" { "Refresh tools" }
            }
        }
        link rel="stylesheet" href="/b/dev/static/dev.css";
        script src="/b/dev/static/dev.js" defer {}
    }
}

/// Serve `/b/dev/static/dev.js`.
pub fn handle_script() -> OutputStream {
    asset(
        assets::dev_js().as_bytes(),
        "application/javascript; charset=utf-8",
    )
}

/// Serve `/b/dev/static/dev.css`.
pub fn handle_stylesheet() -> OutputStream {
    asset(assets::dev_css().as_bytes(), "text/css; charset=utf-8")
}

/// The two page assets are the block's only responses that are not
/// `no-store`.
///
/// `no-cache` still means "revalidate before every use", so a rebuilt bundle
/// is never served from a stale cache — these URLs carry no content hash, so
/// the immutable, year-long `Cache-Control` the hashed `/b/static/*` bundle
/// uses would be wrong. What it buys over `no-store` is a 304 instead of a
/// re-download of the whole script on every navigation to the page, which is
/// the difference §12's rule was never about: the rule exists so a *stale
/// answer about the workspace* cannot outlive the generation that changed it,
/// and these bytes describe no generation at all.
fn asset(bytes: &'static [u8], content_type: &str) -> OutputStream {
    ResponseBuilder::new()
        .set_header("Cache-Control", "no-cache")
        .set_header("X-Content-Type-Options", "nosniff")
        .body(bytes.to_vec(), content_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ids `dev.js` looks up must exist in the markup it is served with.
    /// The two halves ship in the same crate and are only ever deployed
    /// together, so a rename that hits one and not the other is a defect this
    /// test can see without a browser.
    #[test]
    fn every_id_dev_js_looks_up_is_in_the_document() {
        let html = body().into_string();
        for id in [
            "dev-log",
            "dev-progress-steps",
            "dev-preview-frame",
            "dev-file-list",
            "dev-editor-text",
            "dev-editor-title",
            "dev-save",
            "dev-delete",
            "dev-new-file",
            "dev-refresh-tools",
        ] {
            assert!(
                assets::dev_js().contains(&format!("'{id}'")),
                "{id} is not looked up by dev.js — drop it or wire it"
            );
            assert!(
                html.contains(&format!("id=\"{id}\"")),
                "{id} missing from the document"
            );
        }
    }

    /// The prompt names tools, not endpoints the agent would have to guess
    /// at, and every tool it names is one `/b/dev/api/tools.json` publishes.
    #[test]
    fn the_suggested_prompt_only_names_tools_that_exist() {
        for tool in [
            "shop_create_product",
            "shop_create_offer",
            "shop_publish_offer",
            "shop_update_product",
        ] {
            assert!(SUGGESTED_PROMPT.contains(tool), "{tool}");
            assert!(
                super::super::tools::SELECTIONS
                    .iter()
                    .any(|(_, _, _, name, _)| *name == tool),
                "{tool} is not in the page's tool manifest"
            );
        }
    }
}
