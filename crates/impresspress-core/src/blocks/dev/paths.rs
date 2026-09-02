//! Workspace paths: what the sandbox will accept as a file name, where it
//! lives, how big the whole thing may get, and what a file is served as.
//!
//! The workspace has exactly two areas — `site/…` (published verbatim to
//! `wafer-run/web/site`) and `blocks/<name>/…` (compiled into a guest). Every
//! caller-supplied path is validated here before anything else looks at it, so
//! there is one definition of "a workspace path" rather than one per handler.

/// Largest single file the sandbox stores, in bytes.
///
/// The whole body is held in memory twice on the way in (the JSON envelope and
/// the decoded bytes) and again on the way out, on a runtime that may be a
/// browser tab or a Worker isolate. 512 KiB is comfortably above any hand-
/// written source file or page asset and well below the point where that
/// buffering matters.
pub const MAX_FILE_BYTES: usize = 512 * 1024;

/// Largest number of files the workspace may hold.
pub const MAX_FILES: usize = 2_000;

/// Largest total size of the workspace's files, in bytes.
pub const MAX_WORKSPACE_BYTES: u64 = 64 * 1024 * 1024;

/// Largest number of distinct blocks the workspace may define.
///
/// Every block in the workspace is a guest the runtime rebuild has to load,
/// validate and keep resident, so this bounds activation cost, not disk.
pub const MAX_BLOCKS: usize = 16;

/// Longest a single `/`-separated path segment may be, in bytes.
pub const MAX_SEGMENT_BYTES: usize = 255;

/// Longest a whole workspace path may be, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;

/// The two halves of the workspace a valid path can land in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceArea {
    /// Under `site/` — published as static files by the site publisher.
    Site,
    /// Under `blocks/<name>/` — source for the named block's guest.
    Block(String),
}

/// Why a path was refused. The `Display` text is what the 400 carries, so it
/// has to name the offending part rather than merely restate the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// The path was the empty string.
    Empty,
    /// The path exceeded [`MAX_PATH_BYTES`].
    TooLong,
    /// A `/`-separated segment was empty, relative (`.` / `..`), too long, or
    /// carried a backslash or a control character.
    BadSegment(String),
    /// The path did not start with `site/` or `blocks/<name>/`, or named an
    /// area root with no file under it.
    OutsideWorkspace,
    /// The path was `blocks/<name>/…` but `<name>` is not a legal block name.
    BadBlockName(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "path must not be empty"),
            Self::TooLong => write!(f, "path must be at most {MAX_PATH_BYTES} bytes"),
            Self::BadSegment(segment) => write!(
                f,
                "path segment {segment:?} is not allowed: every segment must be non-empty, \
                 at most {MAX_SEGMENT_BYTES} bytes, and free of `.`, `..`, `\\` and control \
                 characters"
            ),
            Self::OutsideWorkspace => {
                write!(f, "path must name a file under `site/` or `blocks/<name>/`")
            }
            Self::BadBlockName(name) => write!(
                f,
                "block name {name:?} is not allowed: 2-32 characters, starting with a-z, \
                 then a-z, 0-9 or _"
            ),
        }
    }
}

/// Validate a workspace-relative path and report which area it lands in.
///
/// The rules are deliberately narrow *and* deliberately not narrower: a space
/// is a legitimate character inside a segment (`site/my page.html` is a real
/// page a user may create), so only the shapes that break `/`-splitting or
/// escape the workspace are refused — an empty, `.` or `..` segment, a
/// backslash (which some filesystems and every Windows client treat as a
/// separator), and control characters.
///
/// Nothing here normalizes: a path that says `..` is rejected, never rewritten,
/// because rewriting would store the file somewhere other than the caller
/// asked for. The same reasoning governs `wafer_block::wrap`'s
/// `is_traversal_safe_path`, which refuses the same shape one layer down.
pub fn validate_path(path: &str) -> Result<WorkspaceArea, PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(PathError::TooLong);
    }
    let segments: Vec<&str> = path.split('/').collect();
    for segment in &segments {
        if segment.is_empty()
            || *segment == "."
            || *segment == ".."
            || segment.len() > MAX_SEGMENT_BYTES
            || segment.contains('\\')
            || segment.chars().any(char::is_control)
        {
            return Err(PathError::BadSegment((*segment).to_string()));
        }
    }
    match segments.as_slice() {
        ["site", rest @ ..] if !rest.is_empty() => Ok(WorkspaceArea::Site),
        ["blocks", name, rest @ ..] if !rest.is_empty() => {
            if !block_name_is_valid(name) {
                return Err(PathError::BadBlockName((*name).to_string()));
            }
            Ok(WorkspaceArea::Block((*name).to_string()))
        }
        _ => Err(PathError::OutsideWorkspace),
    }
}

/// Whether `name` is a legal block name: `^[a-z][a-z0-9_]{1,31}$`.
///
/// A block name becomes a directory segment, a crate name and half of a
/// registered block id, so it is restricted to the intersection all three
/// accept.
pub fn block_name_is_valid(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    (1..=31).contains(&rest.len())
        && rest
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
}

/// A lower bound on how many bytes `encoded` decodes to, for padded
/// standard base64.
///
/// The one place the arithmetic lives, because two callers enforce two
/// different limits with it: a workspace write against
/// [`MAX_FILE_BYTES`], and a staged artifact against
/// `validation::MAX_ARTIFACT_BYTES`. Both check it *before* decoding, so an
/// over-large body is refused without a second allocation the size of the
/// first.
///
/// It must never over-estimate, or a legal payload would be refused: base64
/// carries three bytes per four characters, of which at most two are padding.
pub fn min_base64_decoded_len(encoded: &str) -> usize {
    (encoded.len() / 4).saturating_mul(3).saturating_sub(2)
}

/// The content type the site publisher serves `path` with, and the one the
/// read endpoint consults to decide utf8 vs base64.
///
/// Deliberately *not* `wafer_core::mime::mime_for_ext_str`, which this
/// otherwise mirrors. Three entries differ, and each difference is the reason
/// this table exists:
///
/// * `rs` and `toml` are absent upstream and would fall through to
///   `application/octet-stream` — which would make the sandbox hand back a
///   block's own Rust source as base64. They are the workspace's most-edited
///   files.
/// * `md` is `text/plain` here rather than `text/markdown`: the sandbox
///   publishes the file, it does not render it.
/// * `json` carries no charset, matching what the site publisher writes into
///   `wafer-run/web/site` and what a generation manifest records.
pub fn content_type_for(path: &str) -> &'static str {
    match extension_of(path).as_str() {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "txt" | "md" | "rs" | "toml" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        _ => UNKNOWN_CONTENT_TYPE,
    }
}

/// What [`content_type_for`] answers when the extension says nothing.
///
/// A fallback, not a claim: it means "this file's type is unknown", which is
/// exactly why [`may_be_text`] treats it differently from a type that really
/// does describe binary content.
pub const UNKNOWN_CONTENT_TYPE: &str = "application/octet-stream";

/// Whether a content type describes text.
///
/// `image/svg+xml` and the `application/*` textual formats are the reason this
/// is not `starts_with("text/")`: they are source a user edits.
pub fn is_textual(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type.starts_with("application/json")
        || content_type.starts_with("application/javascript")
        || content_type.starts_with("image/svg+xml")
}

/// Whether the read endpoint may offer a file as a JSON string rather than
/// base64 — that is, whether its type is text or simply unknown.
///
/// [`UNKNOWN_CONTENT_TYPE`] counts, and that is the point: `.gitignore`,
/// `README`, `LICENSE` and `Dockerfile` all have no extension the table
/// recognizes, and all of them are text a user edits. Offering them as text —
/// and falling back to base64 the moment the bytes turn out not to be valid
/// UTF-8 — is what keeps them editable, while a type that really does describe
/// binary content (`image/png`, `application/wasm`, `font/woff2`) is never
/// offered as text. Nothing about the *stored* content type changes:
/// [`content_type_for`] still answers `application/octet-stream`, which is
/// what the site publisher serves.
pub fn may_be_text(content_type: &str) -> bool {
    is_textual(content_type) || content_type == UNKNOWN_CONTENT_TYPE
}

/// The lowercase extension of `path`'s last segment, or `""` when it has none.
///
/// A leading dot does not start an extension (`.gitignore` has none), matching
/// `std::path::Path::extension`.
fn extension_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(0) | None => String::new(),
        Some(dot) => name[dot + 1..].to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_and_block_files_are_accepted() {
        assert_eq!(validate_path("site/index.html"), Ok(WorkspaceArea::Site));
        assert_eq!(
            validate_path("site/assets/img/logo.png"),
            Ok(WorkspaceArea::Site)
        );
        assert_eq!(
            validate_path("blocks/hello/src/lib.rs"),
            Ok(WorkspaceArea::Block("hello".to_string()))
        );
    }

    /// A space is a normal character in a file name and must survive: the
    /// rules exist to stop `/`-splitting and workspace escape, not to impose
    /// a naming style on the user's pages.
    #[test]
    fn spaces_and_unicode_inside_a_segment_are_fine() {
        assert_eq!(validate_path("site/my page.html"), Ok(WorkspaceArea::Site));
        assert_eq!(
            validate_path("site/héllo wörld.md"),
            Ok(WorkspaceArea::Site)
        );
    }

    #[test]
    fn area_roots_without_a_file_under_them_are_refused() {
        for path in ["site", "site/", "blocks", "blocks/hello", "blocks/hello/"] {
            assert!(
                matches!(
                    validate_path(path),
                    Err(PathError::OutsideWorkspace) | Err(PathError::BadSegment(_))
                ),
                "{path} must not name a file"
            );
        }
    }

    #[test]
    fn traversal_and_separator_tricks_are_refused() {
        assert_eq!(validate_path(""), Err(PathError::Empty));
        for path in [
            "../x",
            "site/../../etc",
            "site//a",
            "/site/a.css",
            "site/./a.css",
            "site/a\\b",
            "site/a\u{0}b",
            "site/a\nb",
        ] {
            assert!(
                matches!(validate_path(path), Err(PathError::BadSegment(_))),
                "{path} must be a bad segment"
            );
        }
        assert_eq!(validate_path("sw.js"), Err(PathError::OutsideWorkspace));
        assert_eq!(
            validate_path("public/index.html"),
            Err(PathError::OutsideWorkspace)
        );
    }

    #[test]
    fn oversized_paths_and_segments_are_refused() {
        let long_segment = "a".repeat(MAX_SEGMENT_BYTES + 1);
        assert_eq!(
            validate_path(&format!("site/{long_segment}")),
            Err(PathError::BadSegment(long_segment))
        );
        // At the limit is fine.
        assert_eq!(
            validate_path(&format!("site/{}", "a".repeat(MAX_SEGMENT_BYTES))),
            Ok(WorkspaceArea::Site)
        );
        // Total length is checked before segments, so a path built of legal
        // segments still trips it.
        let deep = format!("site/{}", vec!["ab"; 400].join("/"));
        assert!(deep.len() > MAX_PATH_BYTES);
        assert_eq!(validate_path(&deep), Err(PathError::TooLong));
    }

    #[test]
    fn the_base64_bound_never_over_estimates() {
        use base64ct::{Base64, Encoding};

        // The bound backs two size limits, and over-estimating would refuse a
        // legal payload — so check it against real encodings across every
        // padding case.
        for len in [0usize, 1, 2, 3, 4, 5, 6, 100, 4 * 1024 * 1024 + 1] {
            let encoded = Base64::encode_string(&vec![0u8; len]);
            let bound = min_base64_decoded_len(&encoded);
            assert!(
                bound <= len,
                "bound {bound} over-estimates {len} bytes ({} chars)",
                encoded.len(),
            );
            // And it is tight enough to be useful: never more than the two
            // padding bytes short.
            assert!(len - bound <= 2, "bound {bound} is loose for {len} bytes");
        }
    }

    #[test]
    fn block_names_follow_the_declared_pattern() {
        for good in [
            "ab",
            "hello",
            "hello_world",
            "a1",
            &format!("a{}", "b".repeat(31)),
        ] {
            assert!(block_name_is_valid(good), "{good} must be valid");
        }
        for bad in [
            "",
            "a",
            "1abc",
            "_abc",
            "Abc",
            "ab-cd",
            "ab cd",
            "abc.def",
            &format!("a{}", "b".repeat(32)),
        ] {
            assert!(!block_name_is_valid(bad), "{bad} must be invalid");
        }
    }

    #[test]
    fn a_bad_block_name_is_reported_as_such_not_as_a_bad_segment() {
        assert_eq!(
            validate_path("blocks/Bad Name/src/lib.rs"),
            Err(PathError::BadBlockName("Bad Name".to_string()))
        );
    }

    #[test]
    fn content_types_cover_the_workspace_file_kinds() {
        for (path, expected) in [
            ("site/index.html", "text/html; charset=utf-8"),
            ("site/a.css", "text/css; charset=utf-8"),
            ("site/app.js", "application/javascript; charset=utf-8"),
            ("site/app.mjs", "application/javascript; charset=utf-8"),
            ("site/data.json", "application/json"),
            ("site/logo.svg", "image/svg+xml"),
            ("site/dot.png", "image/png"),
            ("site/photo.JPG", "image/jpeg"),
            ("site/photo.jpeg", "image/jpeg"),
            ("site/anim.gif", "image/gif"),
            ("site/pic.webp", "image/webp"),
            ("site/favicon.ico", "image/x-icon"),
            ("site/notes.txt", "text/plain; charset=utf-8"),
            ("site/readme.md", "text/plain; charset=utf-8"),
            ("blocks/hello/src/lib.rs", "text/plain; charset=utf-8"),
            ("blocks/hello/Cargo.toml", "text/plain; charset=utf-8"),
            ("site/mod.wasm", "application/wasm"),
            ("site/font.woff2", "font/woff2"),
            ("site/unknown.xyz", "application/octet-stream"),
            ("site/noext", "application/octet-stream"),
            ("site/.gitignore", "application/octet-stream"),
        ] {
            assert_eq!(content_type_for(path), expected, "{path}");
        }
    }

    /// The read endpoint answers `utf8` exactly when this says so, so a
    /// wrong answer here is a `.rs` file handed back base64-encoded.
    #[test]
    fn textual_types_are_the_ones_read_hands_back_as_utf8() {
        for path in [
            "site/index.html",
            "site/a.css",
            "site/app.js",
            "site/data.json",
            "site/logo.svg",
            "site/notes.txt",
            "blocks/hello/src/lib.rs",
            "blocks/hello/Cargo.toml",
        ] {
            assert!(is_textual(content_type_for(path)), "{path} must be textual");
            assert!(may_be_text(content_type_for(path)));
        }
        for path in ["site/dot.png", "site/font.woff2", "site/mod.wasm"] {
            assert!(
                !is_textual(content_type_for(path)),
                "{path} must not be textual"
            );
            assert!(
                !may_be_text(content_type_for(path)),
                "{path} is known-binary and must never be offered as text"
            );
        }
    }

    /// A file the table cannot classify is not thereby binary. `.gitignore`,
    /// `README` and `LICENSE` are the files a user is most likely to add
    /// without an extension, and all of them are text.
    #[test]
    fn an_unknown_type_may_still_be_text_without_becoming_one() {
        for path in [
            "blocks/hello/.gitignore",
            "blocks/hello/README",
            "site/LICENSE",
            "site/unknown.xyz",
        ] {
            assert_eq!(
                content_type_for(path),
                UNKNOWN_CONTENT_TYPE,
                "{path} stores as octet-stream"
            );
            assert!(
                !is_textual(content_type_for(path)),
                "{path} is not a text media type"
            );
            assert!(
                may_be_text(content_type_for(path)),
                "{path} must still be offered as text when its bytes are UTF-8"
            );
        }
    }
}
