//! HTTP response construction for the streaming block protocol.
//!
//! The response sugar — `ResponseBuilder`, `ok_json`/`ok_empty`, and the
//! `err_*` constructors — is the canonical implementation in
//! [`wafer_block::response`] (the producer half of the cross-repo
//! response-sugar finding). This module re-exports it so impresspress keeps a
//! single import path (`crate::http::*`) without carrying a behaviourally
//! identical local copy (a local copy of an upstream surface is a shim).
//!
//! The only impresspress-specific addition is [`redirect`], a thin convenience over
//! [`ResponseBuilder`] for the redirect response shape (status + `Location` +
//! empty `text/plain` body) used by page handlers.

pub use wafer_block::{
    err_bad_request, err_conflict, err_forbidden, err_internal, err_internal_no_cause,
    err_not_found, err_unauthorized, ok_empty, ok_json, ResponseBuilder,
};
use wafer_run::OutputStream;

/// Build a redirect `OutputStream` with the given status (302, 303, …) and
/// `Location` header. Single source of truth for the redirect response shape
/// (status + `Location` + empty `text/plain` body) used by page handlers.
pub fn redirect(status: u16, location: &str) -> OutputStream {
    ResponseBuilder::new()
        .status(status)
        .set_header("Location", location)
        .body(Vec::new(), "text/plain")
}

/// Conditional GET (`If-None-Match` → `304 Not Modified`).
///
/// Two routes serve fixed content at a STABLE url — no query string or path
/// segment carries a version, unlike the content-hashed `/b/static/*`
/// bundle — so revalidation is the only way a repeat visitor avoids
/// re-downloading bytes that have not changed: `/b/webmcp/webmcp.js`
/// (`pipeline.rs`) and `/b/dev/static/{dev.js,dev.css}`
/// (`blocks::dev::page`). Both already sent a quoted `ETag`, but nothing
/// compared it against a repeat visitor's `If-None-Match`, so the `304`
/// their `Cache-Control: no-cache` implied never actually fired — this
/// module is that comparison.
pub mod conditional {
    use wafer_run::{Message, OutputStream};

    use super::ResponseBuilder;

    /// If the request's `If-None-Match` header (RFC 9110 §13.1.2) names
    /// `etag` — already the quoted form, e.g. `"\"a1b2c3d4\""` — or is `*`,
    /// return the `304` this GET should answer with instead: no body, the
    /// same `ETag` and `Cache-Control` the `200` would have carried. `None`
    /// means the caller has no cached copy (or a stale one) and should send
    /// the full response.
    ///
    /// Per spec, `If-None-Match` uses the WEAK comparison function: a weak
    /// validator (`W/"..."`) and a strong one with the same opaque tag
    /// match — this is a cache-validation header, not an edit-conflict
    /// check (contrast `expected_sha256` in `blocks::dev::files`, which
    /// needs the STRONG comparison a byte-identity guarantee requires). A
    /// missing header, or one that names something else, is a normal `200`.
    pub fn not_modified(msg: &Message, etag: &str, cache_control: &str) -> Option<OutputStream> {
        let header = msg.get_meta("http.header.if-none-match");
        if header.is_empty() || !matches(header, etag) {
            return None;
        }
        Some(
            ResponseBuilder::new()
                .status(304)
                .set_header("Cache-Control", cache_control)
                .set_header("ETag", etag)
                .empty(),
        )
    }

    /// `*` matches unconditionally — RFC 9110 §13.1.2 makes it false only
    /// when the origin server has no current representation, and every
    /// caller of [`not_modified`] is serving one. Otherwise a
    /// comma-separated list of entity-tags, each optionally weak
    /// (`W/"..."`), any of which may match `etag` (the `W/` prefix ignored
    /// on both sides — the weak comparison function).
    fn matches(header: &str, etag: &str) -> bool {
        let header = header.trim();
        if header == "*" {
            return true;
        }
        let target = strip_weak(etag.trim());
        header
            .split(',')
            .map(str::trim)
            .any(|candidate| strip_weak(candidate) == target)
    }

    fn strip_weak(tag: &str) -> &str {
        tag.strip_prefix("W/").unwrap_or(tag)
    }

    #[cfg(test)]
    mod tests {
        use wafer_run::MetaGet;

        use super::*;

        fn msg_with_if_none_match(value: &str) -> Message {
            let mut m = Message::new("http.request");
            m.set_meta("http.header.if-none-match", value);
            m
        }

        #[test]
        fn no_header_means_no_304() {
            let m = Message::new("http.request");
            assert!(not_modified(&m, "\"abcd1234\"", "no-cache").is_none());
        }

        #[test]
        fn exact_strong_match_is_304() {
            let m = msg_with_if_none_match("\"abcd1234\"");
            assert!(not_modified(&m, "\"abcd1234\"", "no-cache").is_some());
        }

        #[test]
        fn mismatch_is_not_304() {
            let m = msg_with_if_none_match("\"deadbeef\"");
            assert!(not_modified(&m, "\"abcd1234\"", "no-cache").is_none());
        }

        #[test]
        fn weak_validator_on_the_request_still_matches_a_strong_etag() {
            let m = msg_with_if_none_match("W/\"abcd1234\"");
            assert!(not_modified(&m, "\"abcd1234\"", "no-cache").is_some());
        }

        #[test]
        fn comma_separated_list_matches_any_entry() {
            let m = msg_with_if_none_match("\"11111111\", \"abcd1234\", W/\"22222222\"");
            assert!(not_modified(&m, "\"abcd1234\"", "no-cache").is_some());
        }

        #[test]
        fn star_always_matches() {
            let m = msg_with_if_none_match("*");
            assert!(not_modified(&m, "\"anything\"", "no-cache").is_some());
        }

        #[tokio::test]
        async fn a_304_carries_no_body_and_the_same_etag_and_cache_control() {
            let m = msg_with_if_none_match("\"abcd1234\"");
            let out = not_modified(&m, "\"abcd1234\"", "no-cache").expect("should match");
            let buf = out.collect_buffered().await.expect("respond");
            assert!(buf.body.is_empty());
            assert_eq!(
                MetaGet::get(&buf.meta, wafer_run::META_RESP_STATUS),
                Some("304")
            );
            assert_eq!(
                MetaGet::get(&buf.meta, "resp.header.ETag"),
                Some("\"abcd1234\"")
            );
            assert_eq!(
                MetaGet::get(&buf.meta, "resp.header.Cache-Control"),
                Some("no-cache")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use wafer_run::{MetaGet, META_RESP_STATUS};

    use super::*;

    #[tokio::test]
    async fn redirect_sets_status_and_location() {
        let buf = redirect(303, "/login")
            .collect_buffered()
            .await
            .expect("respond");
        assert!(buf.body.is_empty());
        assert_eq!(MetaGet::get(&buf.meta, META_RESP_STATUS), Some("303"));
        assert_eq!(
            MetaGet::get(&buf.meta, "resp.header.Location"),
            Some("/login")
        );
    }
}
