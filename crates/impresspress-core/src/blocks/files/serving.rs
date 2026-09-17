//! How a user-uploaded object is handed to a browser.
//!
//! Both download paths — the authenticated
//! `GET /b/storage/api/buckets/{bucket}/objects/{key}` and the public share
//! link `GET /b/storage/direct/{token}` — serve bytes a *user* uploaded, under
//! a content type that same user chose, from the application's own origin.
//! That is the classic stored-XSS shape: upload `payload.html` (or an SVG with
//! a `<script>` in it), send someone the link, and the script runs on the
//! app's origin with the victim's session.
//!
//! [`user_object_leading_meta`] is the single answer to that, used by both
//! paths so neither can serve an object on weaker terms than the other:
//!
//! - **An allowlist decides what may render inline.** Only types that cannot
//!   carry script at all — raster images, audio, video, PDF, plain text — get
//!   `Content-Disposition: inline`. Everything else, `text/html` and
//!   `image/svg+xml` included, is an `attachment`: the browser saves it
//!   instead of rendering it, so nothing executes. The allowlist is the fix;
//!   the two headers below are what make it airtight and what cover the
//!   attachment path.
//! - **`X-Content-Type-Options: nosniff`** pins the declared type. Without it
//!   a browser is free to sniff HTML out of a body served as `image/png` and
//!   render it, which would route around the allowlist entirely.
//! - **A sandbox CSP on the attachment path.** If a browser ignored the
//!   disposition and rendered an attachment anyway, `sandbox` puts it in an
//!   opaque origin with scripting off, so it still cannot reach the session
//!   it was aimed at.
//!
//! The inline allowlist deliberately carries no CSP: `sandbox` blocks plugins,
//! and Chrome's PDF viewer is one, so applying it there would trade a working
//! PDF preview for defence-in-depth on a set of types that cannot execute in
//! the first place.

use wafer_run::MetaEntry;

/// Response header that stops content-type sniffing. The declared type is the
/// only type the browser may treat the body as.
const NOSNIFF_HEADER: (&str, &str) = ("X-Content-Type-Options", "nosniff");

/// Response header served with every object that is NOT on the inline
/// allowlist: an opaque origin, no scripts, no plugins, no subresources — so a
/// browser that renders an `attachment` anyway renders something inert.
const SANDBOX_CSP_HEADER: (&str, &str) = (
    "Content-Security-Policy",
    "default-src 'none'; sandbox; frame-ancestors 'none'",
);

/// The `Content-Disposition` filename for an object with no usable key.
const FALLBACK_FILENAME: &str = "download";

/// Whether an object of `content_type` may be rendered **inline** by the
/// browser.
///
/// The list is exactly the types that cannot carry executable script:
/// - raster images, but **not** `image/svg+xml` (or any other `+xml` image) —
///   an SVG is a document, it can hold `<script>`, and it is the type this
///   allowlist exists to keep out of an inline response;
/// - audio and video, which are decoded, never parsed as a document;
/// - `application/pdf`, so previews keep working — the viewer runs a PDF's own
///   scripting in its own sandbox, never on this origin;
/// - `text/plain`, which paired with `X-Content-Type-Options: nosniff` is
///   displayed as text and can never be sniffed into markup.
///
/// Anything else — `text/html`, `application/xhtml+xml`, `image/svg+xml`,
/// `application/javascript`, an unknown type, or nothing at all — is served as
/// an attachment.
fn renders_inline_safely(content_type: &str) -> bool {
    // A content type may carry parameters (`text/plain; charset=utf-8`); only
    // the type/subtype decides.
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    if essence == "application/pdf" || essence == "text/plain" {
        return true;
    }
    let Some((top, sub)) = essence.split_once('/') else {
        return false;
    };
    match top {
        // `+xml` is the marker of an XML-based image — SVG today, and whatever
        // else adopts the convention — every one of which is a document that
        // can hold script.
        "image" => !sub.ends_with("+xml") && sub != "xml",
        "audio" | "video" => true,
        _ => false,
    }
}

/// The filename to advertise for `key`: its last path segment, with the
/// characters that would break out of the quoted `filename="…"` form removed.
///
/// A key is validated at upload (`storage::validation::is_valid_storage_key`)
/// and can still contain `/` — it is a path within the bucket — so the last
/// segment is the file, and a key that ends in `/` or is otherwise unusable
/// falls back to [`FALLBACK_FILENAME`] rather than producing an empty
/// `filename=""`.
fn disposition_filename(key: &str) -> String {
    let name: String = key
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| *c != '"' && *c != '\\' && !c.is_control())
        .collect();
    if name.trim().is_empty() {
        FALLBACK_FILENAME.to_string()
    } else {
        name
    }
}

/// The leading `Meta` frame for streaming a user-uploaded object: the
/// streaming opt-in marker and content type from
/// [`crate::streaming::download_leading_meta`], plus the disposition and
/// security headers this module's doc explains, plus any `extra_headers` the
/// caller adds (the share path's `Cache-Control`).
///
/// Every response this builds carries `X-Content-Type-Options: nosniff`. The
/// disposition is `inline` only for the types
/// [`renders_inline_safely`] admits; everything else is an `attachment` and
/// additionally carries the sandbox CSP.
pub(in crate::blocks::files) fn user_object_leading_meta(
    content_type: &str,
    key: &str,
    extra_headers: &[(&str, &str)],
) -> Vec<MetaEntry> {
    let inline = renders_inline_safely(content_type);
    let disposition = format!(
        "{}; filename=\"{}\"",
        if inline { "inline" } else { "attachment" },
        disposition_filename(key)
    );

    let mut headers: Vec<(&str, &str)> = vec![
        ("Content-Disposition", disposition.as_str()),
        NOSNIFF_HEADER,
    ];
    if !inline {
        headers.push(SANDBOX_CSP_HEADER);
    }
    headers.extend_from_slice(extra_headers);
    crate::streaming::download_leading_meta(content_type, &headers)
}

#[cfg(test)]
mod tests {
    use wafer_run::MetaGet;

    use super::*;

    fn header<'m>(meta: &'m [MetaEntry], name: &str) -> Option<&'m str> {
        MetaGet::get(meta, &format!("resp.header.{name}"))
    }

    /// The types an uploader can use to get script onto this origin are the
    /// ones that must never come back `inline`.
    #[test]
    fn active_content_types_are_never_inline() {
        for active in [
            "text/html",
            "text/html; charset=utf-8",
            "TEXT/HTML",
            "application/xhtml+xml",
            "image/svg+xml",
            "image/svg+xml; charset=utf-8",
            "application/javascript",
            "text/xml",
            "application/octet-stream",
            "",
            "nonsense",
        ] {
            assert!(
                !renders_inline_safely(active),
                "{active} must not render inline"
            );
        }
    }

    /// Previews keep working for the types that cannot execute.
    #[test]
    fn inert_content_types_still_preview_inline() {
        for inert in [
            "image/png",
            "image/jpeg",
            "IMAGE/PNG",
            "image/webp",
            "application/pdf",
            "text/plain; charset=utf-8",
            "audio/mpeg",
            "video/mp4",
        ] {
            assert!(renders_inline_safely(inert), "{inert} must render inline");
        }
    }

    #[test]
    fn an_active_type_is_an_attachment_with_nosniff_and_a_sandbox_csp() {
        let meta = user_object_leading_meta("text/html", "nested/payload.html", &[]);

        assert_eq!(
            header(&meta, "Content-Disposition"),
            Some("attachment; filename=\"payload.html\"")
        );
        assert_eq!(header(&meta, "X-Content-Type-Options"), Some("nosniff"));
        assert_eq!(
            header(&meta, "Content-Security-Policy"),
            Some(SANDBOX_CSP_HEADER.1)
        );
    }

    #[test]
    fn an_inert_type_is_inline_and_still_carries_nosniff() {
        let meta = user_object_leading_meta("image/png", "pic.png", &[]);

        assert_eq!(
            header(&meta, "Content-Disposition"),
            Some("inline; filename=\"pic.png\"")
        );
        assert_eq!(header(&meta, "X-Content-Type-Options"), Some("nosniff"));
        assert_eq!(
            header(&meta, "Content-Security-Policy"),
            None,
            "the sandbox CSP would cost the PDF/image preview the allowlist exists to keep"
        );
    }

    /// The filename is a header value, so a key cannot close the quoted form
    /// and add parameters of its own, and a key with no usable last segment
    /// still produces a filename.
    #[test]
    fn the_filename_cannot_break_out_of_the_header() {
        assert_eq!(disposition_filename("a/b/c.png"), "c.png");
        assert_eq!(
            disposition_filename("evil\".png"),
            "evil.png",
            "a quote would end the filename parameter"
        );
        assert_eq!(disposition_filename("line\r\nbreak.png"), "linebreak.png");
        assert_eq!(disposition_filename("dir/"), FALLBACK_FILENAME);
        assert_eq!(disposition_filename(""), FALLBACK_FILENAME);
    }

    /// Caller-supplied headers ride along with the security ones rather than
    /// replacing them.
    #[test]
    fn extra_headers_are_appended() {
        let meta = user_object_leading_meta(
            "image/png",
            "pic.png",
            &[("Cache-Control", "private, max-age=3600")],
        );
        assert_eq!(
            header(&meta, "Cache-Control"),
            Some("private, max-age=3600")
        );
        assert_eq!(header(&meta, "X-Content-Type-Options"), Some("nosniff"));
    }
}
