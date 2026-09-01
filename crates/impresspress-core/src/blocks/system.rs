use wafer_run::{BlockEndpoint, BlockInfo, InstanceMode};

use crate::{
    http::{err_not_found, ok_json, ResponseBuilder},
    routing,
};

/// Resolve a hashed filename (the part after `/b/static/`) to its bytes and
/// content type. Exact-match against the build-time manifest — no prefix
/// scanning, so no ordering hazard between `itim-latin-` and `itim-latin-ext-`.
#[cfg(feature = "embed-assets")]
pub(crate) fn static_asset(filename: &str) -> Option<(&'static [u8], &'static str)> {
    let e = crate::ui::assets::ASSETS.iter().find(|e| e.filename == filename)?;
    Some((crate::ui::assets::bytes(e.logical)?, e.content_type))
}

crate::impresspress_feature_block! {
    /// System health checks and embedded static assets (`impresspress/system`).
    pub struct SystemBlock;
    name: "impresspress/system",
    info: |_this| {
        // Base set: assets served regardless of feature flags. The LLM
        // (marked/purify/llm-chat) and Files (files-browser) assets are
        // conditionally appended below — they're feature-gated in
        // `ui::assets` behind `block-llm`/`block-files` respectively (those
        // blocks are their only consumers), so a build without the block
        // doesn't advertise an endpoint it can't serve.
        #[allow(unused_mut)]
        let mut endpoints = vec![
            BlockEndpoint::get("/health").summary("Health check"),
            BlockEndpoint::get("/b/static/app-{hash}.css").summary("Embedded CSS"),
            BlockEndpoint::get("/b/static/htmx-{hash}.min.js").summary("Embedded htmx JS"),
            BlockEndpoint::get("/b/static/webmcp-{hash}.js").summary("Embedded WebMCP registration JS"),
            BlockEndpoint::get("/b/static/itim-latin-{hash}.woff2").summary("Embedded Itim font (latin)"),
            BlockEndpoint::get("/b/static/itim-latin-ext-{hash}.woff2").summary("Embedded Itim font (latin-ext)"),
            BlockEndpoint::get("/b/static/impresspress-logo-{hash}.png").summary("Embedded Impresspress square logo"),
            BlockEndpoint::get("/b/static/impresspress-logo-long-{hash}.png").summary("Embedded Impresspress wordmark logo"),
            BlockEndpoint::get("/b/static/favicon-{hash}.ico").summary("Embedded Impresspress favicon"),
        ];
        #[cfg(feature = "block-llm")]
        endpoints.extend([
            BlockEndpoint::get("/b/static/marked-{hash}.min.js").summary("Embedded marked.js"),
            BlockEndpoint::get("/b/static/purify-{hash}.js").summary("Embedded DOMPurify JS"),
            BlockEndpoint::get("/b/static/llm-chat-{hash}.js").summary("Embedded LLM chat JS"),
        ]);
        #[cfg(feature = "block-files")]
        endpoints.push(
            BlockEndpoint::get("/b/static/files-browser-{hash}.js")
                .summary("Embedded files-browser JS"),
        );
        BlockInfo::new("impresspress/system", "0.0.1", "http-handler@v1", "System health and embedded static assets")
            .instance_mode(InstanceMode::Singleton)
            .category(wafer_run::BlockCategory::Infrastructure)
            .description("Core system services including health checks and embedded static assets (CSS, JavaScript).")
            .endpoints(endpoints)
    },
    handle: |_this, _ctx, msg, _input| {
        let path = msg.path();

        if path == "/health" {
            return ok_json(&serde_json::json!({"status": "ok"}));
        }

        // Embedded static assets (CSS, JS, fonts) with content-hash URLs for
        // cache busting. Looked up by exact filename against the build-time
        // manifest (`static_asset`, above) — no prefix/suffix scanning, so no
        // ordering hazard between e.g. `itim-latin-` and `itim-latin-ext-`.
        if let Some(filename) = path.strip_prefix(routing::STATIC_PREFIX) {
            #[cfg(feature = "embed-assets")]
            if let Some((body, content_type)) = static_asset(filename) {
                return ResponseBuilder::new()
                    .set_header("Cache-Control", "public, max-age=31536000, immutable")
                    .body(body.to_vec(), content_type);
            }
            #[cfg(not(feature = "embed-assets"))]
            {
                // Assets were not compiled in. The deployer is responsible for
                // publishing them and pointing IMPRESSPRESS_ASSET_BASE_URL at
                // them (Task 4); reaching this arm means that did not happen.
                let _ = filename;
            }
        }

        err_not_found("not found")
    },
}

#[cfg(test)]
mod tests {
    use wafer_run::{
        context::Context, Block, InputStream, Message, OutputStream, META_RESP_CONTENT_TYPE,
    };

    use super::*;
    use crate::ui::assets;

    #[derive(Clone)]
    struct NopCtx;
    #[async_trait::async_trait]
    impl Context for NopCtx {
        async fn call_block(
            &self,
            _block_name: &str,
            _msg: Message,
            _input: InputStream,
        ) -> OutputStream {
            panic!("call_block not used");
        }
        fn is_cancelled(&self) -> bool {
            false
        }
        fn config_get(&self, _key: &str) -> Option<&str> {
            None
        }
        fn clone_arc(&self) -> std::sync::Arc<dyn Context> {
            std::sync::Arc::new(self.clone())
        }
    }

    #[tokio::test]
    #[cfg(all(feature = "block-llm", feature = "embed-assets"))]
    async fn system_handle_serves_llm_chat_js() {
        let block = SystemBlock::new();
        let url = assets::llm_chat_js_url();
        let mut msg = Message::new(format!("retrieve:{url}"));
        msg.set_meta(wafer_run::META_REQ_ACTION, "retrieve");
        msg.set_meta(wafer_run::META_REQ_RESOURCE, url);

        let out = block.handle(&NopCtx, msg, InputStream::empty()).await;
        let buffered = out.collect_buffered().await.expect("response");
        let content_type = buffered
            .meta
            .iter()
            .find(|m| m.key == META_RESP_CONTENT_TYPE)
            .map(|m| m.value.as_str());
        assert_eq!(
            content_type,
            Some("application/javascript; charset=utf-8"),
            "wrong content type"
        );
        let body = std::str::from_utf8(&buffered.body).unwrap();
        assert!(
            body.contains("impresspressLlmChat"),
            "body should contain the JS module"
        );
    }

    #[tokio::test]
    #[cfg(all(feature = "block-files", feature = "embed-assets"))]
    async fn system_handle_serves_files_browser_js() {
        let block = SystemBlock::new();
        let url = assets::files_browser_js_url();
        let mut msg = Message::new(format!("retrieve:{url}"));
        msg.set_meta(wafer_run::META_REQ_ACTION, "retrieve");
        msg.set_meta(wafer_run::META_REQ_RESOURCE, url);

        let out = block.handle(&NopCtx, msg, InputStream::empty()).await;
        let buffered = out.collect_buffered().await.expect("response");
        let content_type = buffered
            .meta
            .iter()
            .find(|m| m.key == META_RESP_CONTENT_TYPE)
            .map(|m| m.value.as_str());
        assert_eq!(
            content_type,
            Some("application/javascript; charset=utf-8"),
            "wrong content type"
        );
        let body = std::str::from_utf8(&buffered.body).unwrap();
        assert!(
            body.starts_with("// impresspress files-browser"),
            "body should start with the placeholder comment"
        );
    }

    #[tokio::test]
    #[cfg(all(feature = "block-llm", feature = "embed-assets"))]
    async fn system_handle_serves_marked_js() {
        let block = SystemBlock::new();
        let url = assets::marked_js_url();
        let mut msg = Message::new(format!("retrieve:{url}"));
        msg.set_meta(wafer_run::META_REQ_ACTION, "retrieve");
        msg.set_meta(wafer_run::META_REQ_RESOURCE, url);

        let out = block.handle(&NopCtx, msg, InputStream::empty()).await;
        let buffered = out.collect_buffered().await.expect("response");
        let content_type = buffered
            .meta
            .iter()
            .find(|m| m.key == META_RESP_CONTENT_TYPE)
            .map(|m| m.value.as_str());
        assert_eq!(
            content_type,
            Some("application/javascript; charset=utf-8"),
            "wrong content type"
        );
        let body = std::str::from_utf8(&buffered.body).unwrap();
        assert!(
            body.contains("marked"),
            "body should be the vendored marked.js"
        );
    }

    #[tokio::test]
    #[cfg(all(feature = "block-llm", feature = "embed-assets"))]
    async fn system_handle_serves_purify_js() {
        let block = SystemBlock::new();
        let url = assets::purify_js_url();
        let mut msg = Message::new(format!("retrieve:{url}"));
        msg.set_meta(wafer_run::META_REQ_ACTION, "retrieve");
        msg.set_meta(wafer_run::META_REQ_RESOURCE, url);

        let out = block.handle(&NopCtx, msg, InputStream::empty()).await;
        let buffered = out.collect_buffered().await.expect("response");
        let content_type = buffered
            .meta
            .iter()
            .find(|m| m.key == META_RESP_CONTENT_TYPE)
            .map(|m| m.value.as_str());
        assert_eq!(
            content_type,
            Some("application/javascript; charset=utf-8"),
            "wrong content type"
        );
        let body = std::str::from_utf8(&buffered.body).unwrap();
        assert!(
            body.contains("DOMPurify"),
            "body should be the vendored DOMPurify build"
        );
    }

    #[tokio::test]
    #[cfg(feature = "embed-assets")]
    async fn system_handle_serves_webmcp_js() {
        let block = SystemBlock::new();
        let url = assets::webmcp_js_url();
        let mut msg = Message::new(format!("retrieve:{url}"));
        msg.set_meta(wafer_run::META_REQ_ACTION, "retrieve");
        msg.set_meta(wafer_run::META_REQ_RESOURCE, url);

        let out = block.handle(&NopCtx, msg, InputStream::empty()).await;
        let buffered = out.collect_buffered().await.expect("response");
        let content_type = buffered
            .meta
            .iter()
            .find(|m| m.key == META_RESP_CONTENT_TYPE)
            .map(|m| m.value.as_str());
        assert_eq!(
            content_type,
            Some("application/javascript; charset=utf-8"),
            "wrong content type"
        );
        let body = std::str::from_utf8(&buffered.body).unwrap();
        assert!(
            body.contains("registerTool"),
            "body should be the WebMCP registration script"
        );
    }

    /// `declared_access` (`routing.rs`) fails closed to `AuthLevel::
    /// Authenticated` for any path a block does not declare as a
    /// `BlockEndpoint` — this is the endpoint declaration this block adds
    /// alongside htmx/CSS/fonts (`system.rs`'s `endpoints` list). Checked
    /// via `effective_access`, not `declared_access` directly: `/b/static/`
    /// is mounted as `Route::router_declared_public` (`router_final`), so
    /// the router's own `Public` declaration is what actually admits an
    /// anonymous request regardless of this endpoint entry (proved
    /// end-to-end by `routing::tests::webmcp_script_asset_is_publicly_reachable`,
    /// which registers no `BlockInfo` at all and still dispatches) —
    /// `effective_access` is the resolver that mirrors exactly what the
    /// router enforces (it's the same one `pipeline.rs` plugs into the
    /// WebMCP manifest generator), so it's the correct thing to assert
    /// against here.
    #[test]
    fn webmcp_script_asset_is_publicly_reachable() {
        let info = SystemBlock::new().info();
        let ep = info
            .endpoints
            .iter()
            .find(|e| e.path == "/b/static/webmcp-{hash}.js")
            .expect("webmcp asset endpoint not declared in SystemBlock::info()");
        assert_eq!(
            crate::routing::effective_access(&info, ep, &[]),
            wafer_run::AuthLevel::Public,
            "the WebMCP script must load for anonymous visitors — an \
             effective auth level above Public would silently disable tools \
             on the public storefront"
        );
    }

    #[cfg(feature = "embed-assets")]
    #[test]
    fn static_lookup_matches_exact_hashed_filename_without_ordering_hazard() {
        // The prefix-table version needed `itim-latin-ext-` to be scanned before
        // `itim-latin-`. Exact-filename matching makes that class of bug impossible.
        let ext = crate::ui::assets::entry("itim-latin-ext.woff2");
        let base = crate::ui::assets::entry("itim-latin.woff2");
        assert_ne!(ext.filename, base.filename);
        assert_eq!(super::static_asset(ext.filename).unwrap().1, "font/woff2");
        assert_eq!(super::static_asset(base.filename).unwrap().1, "font/woff2");
    }

    #[cfg(feature = "embed-assets")]
    #[test]
    fn static_lookup_rejects_unknown_and_traversal_paths() {
        assert!(super::static_asset("nope.css").is_none());
        assert!(super::static_asset("../../etc/passwd").is_none());
    }
}
