//! `/b/dev/api/files*` — read, write, list and delete workspace files.
//!
//! # Lost updates
//!
//! Every mutation states the hash it believes the file currently has
//! (`expected_sha256`, `null` for "no file yet"). A mismatch is a `409`
//! carrying the hash it actually has, so the caller re-reads rather than
//! silently overwriting an edit it never saw. The sandbox's clients are an
//! agent and a human editing the same workspace at the same time; making the
//! check optional would make the race invisible instead of impossible.
//!
//! # Refusal shapes
//!
//! * A hash mismatch is a real `409` *response* whose body is
//!   [`FileConflict`] — the caller needs the current state, and an error
//!   terminal renders `{error, message}` instead.
//! * Everything else is an error terminal built by the block's own
//!   `no_store_error` / `no_store_error_status`: a bad path or body is `400`,
//!   a size or count over quota is `413`, a missing file is `404`, and the
//!   block-count quota is `409` (the workspace conflicts with a limit — the
//!   payload is not too large). All of them carry `Cache-Control: no-store`,
//!   as design §12 requires of every `/b/dev` response.

use base64ct::{Base64, Encoding};
use wafer_run::{context::Context, ErrorCode, InputStream, Message, OutputStream};

use super::{
    blobs,
    contracts::{
        FileConflict, FileDeleteRequest, FileDeleteResponse, FileEncoding, FileListQuery,
        FileListResponse, FileReadRequest, FileReadResponse, FileWriteRequest, FileWriteResponse,
    },
    no_store, no_store_error, no_store_error_status,
    paths::{self, WorkspaceArea},
    workspace::{self, FileEntry, Workspace},
};
use crate::{blocks::crud, http::err_internal};

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /b/dev/api/files` — the workspace manifest, optionally prefix-filtered.
pub async fn handle_list(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let query = FileListQuery::from_message(msg);
    let ws = match workspace::load(ctx).await {
        Ok(ws) => ws,
        Err(e) => return err_internal("dev workspace load", e),
    };
    let prefix = query.prefix.unwrap_or_default();
    // `Workspace::files` is a `BTreeMap`, so this is already path-ordered.
    let files: Vec<FileEntry> = ws
        .files
        .values()
        .filter(|entry| entry.path.starts_with(&prefix))
        .cloned()
        .collect();
    no_store().json(&FileListResponse { files })
}

/// `POST /b/dev/api/files/read` — one file's content.
pub async fn handle_read(ctx: &dyn Context, input: InputStream) -> OutputStream {
    let request: FileReadRequest = match read_body(input).await {
        Ok(request) => request,
        Err(refusal) => return refusal,
    };
    if let Err(e) = paths::validate_path(&request.path) {
        return no_store_error(ErrorCode::InvalidArgument, &e.to_string());
    }
    let ws = match workspace::load(ctx).await {
        Ok(ws) => ws,
        Err(e) => return err_internal("dev workspace load", e),
    };
    let Some(entry) = ws.get(&request.path) else {
        return no_store_error(
            ErrorCode::NotFound,
            &format!("no file at {:?}", request.path),
        );
    };
    let bytes = match blobs::get(ctx, &entry.sha256).await {
        Ok(bytes) => bytes,
        // A manifest entry naming a blob that is not there is corruption, not
        // a missing file: the path exists, its content has gone.
        Err(e) => return err_internal("dev workspace blob read", e),
    };
    let (encoding, content) = encode_content(&entry.content_type, bytes);
    no_store().json(&FileReadResponse {
        path: entry.path.clone(),
        sha256: entry.sha256.clone(),
        size: entry.size,
        encoding,
        content,
    })
}

/// `POST /b/dev/api/files/write` — create or replace one file.
pub async fn handle_write(ctx: &dyn Context, input: InputStream) -> OutputStream {
    let request: FileWriteRequest = match read_body(input).await {
        Ok(request) => request,
        Err(refusal) => return refusal,
    };
    let area = match paths::validate_path(&request.path) {
        Ok(area) => area,
        Err(e) => return no_store_error(ErrorCode::InvalidArgument, &e.to_string()),
    };
    let bytes = match decode_content(request.encoding, &request.content) {
        Ok(bytes) => bytes,
        Err(detail) => return no_store_error(ErrorCode::InvalidArgument, &detail),
    };
    if bytes.len() > paths::MAX_FILE_BYTES {
        return no_store_error_status(
            ErrorCode::ResourceExhausted,
            413,
            &format!(
                "file is {} bytes; the limit is {} bytes",
                bytes.len(),
                paths::MAX_FILE_BYTES
            ),
        );
    }

    let mut ws = match workspace::load(ctx).await {
        Ok(ws) => ws,
        Err(e) => return err_internal("dev workspace load", e),
    };
    let current = ws.get(&request.path);
    if !hash_matches(current, request.expected_sha256.as_deref()) {
        return conflict(&request.path, current);
    }
    if let Err(e) = check_quotas(&ws, &request.path, &area, bytes.len() as u64) {
        return e.into_response();
    }

    let sha = match blobs::put(ctx, &bytes).await {
        Ok(sha) => sha,
        Err(e) => return err_internal("dev workspace blob write", e),
    };
    let entry = ws.insert(&request.path, sha, bytes.len() as u64);
    if let Err(e) = workspace::save(ctx, &ws).await {
        return err_internal("dev workspace save", e);
    }
    no_store().json(&FileWriteResponse {
        path: entry.path,
        sha256: entry.sha256,
        size: entry.size,
        // Task 7 wires a `site/` write to an activation; until then a write
        // stages nothing.
        generation: None,
    })
}

/// `POST /b/dev/api/files/delete` — drop one file from the manifest.
pub async fn handle_delete(ctx: &dyn Context, input: InputStream) -> OutputStream {
    let request: FileDeleteRequest = match read_body(input).await {
        Ok(request) => request,
        Err(refusal) => return refusal,
    };
    if let Err(e) = paths::validate_path(&request.path) {
        return no_store_error(ErrorCode::InvalidArgument, &e.to_string());
    }
    let mut ws = match workspace::load(ctx).await {
        Ok(ws) => ws,
        Err(e) => return err_internal("dev workspace load", e),
    };
    let current = ws.get(&request.path);
    if !hash_matches(current, Some(&request.expected_sha256)) {
        return conflict(&request.path, current);
    }
    // The blob stays: a generation that can still be rolled back to names it.
    ws.remove(&request.path);
    if let Err(e) = workspace::save(ctx, &ws).await {
        return err_internal("dev workspace save", e);
    }
    no_store().json(&FileDeleteResponse {
        path: request.path,
        generation: None,
    })
}

// ---------------------------------------------------------------------------
// Bodies and encodings
// ---------------------------------------------------------------------------

/// Deserialize a request body, refusing a malformed one with a `no-store` 400.
async fn read_body<T: serde::de::DeserializeOwned>(input: InputStream) -> Result<T, OutputStream> {
    crud::read_json_body_or(input, |detail| {
        no_store_error(
            ErrorCode::InvalidArgument,
            &format!("invalid request body: {detail}"),
        )
    })
    .await
}

/// Decode a request's `content` field per its declared encoding.
fn decode_content(encoding: FileEncoding, content: &str) -> Result<Vec<u8>, String> {
    match encoding {
        FileEncoding::Utf8 => Ok(content.as_bytes().to_vec()),
        FileEncoding::Base64 => {
            Base64::decode_vec(content).map_err(|e| format!("`content` is not valid base64: {e}"))
        }
    }
}

/// Choose how to hand `bytes` back to the caller.
///
/// `utf8` only when the file is textual *and* the bytes really are UTF-8: a
/// `.txt` holding latin-1 would otherwise be lossily re-encoded by the JSON
/// serializer, and the caller would write back something other than what it
/// read.
fn encode_content(content_type: &str, bytes: Vec<u8>) -> (FileEncoding, String) {
    if !paths::is_textual(content_type) {
        return (FileEncoding::Base64, Base64::encode_string(&bytes));
    }
    // `into_bytes` hands the buffer back on failure, so the UTF-8 check costs
    // no copy of a body that can be half a megabyte.
    match String::from_utf8(bytes) {
        Ok(text) => (FileEncoding::Utf8, text),
        Err(e) => (FileEncoding::Base64, Base64::encode_string(&e.into_bytes())),
    }
}

// ---------------------------------------------------------------------------
// Conflicts and quotas
// ---------------------------------------------------------------------------

/// Whether `expected` describes `current`: `None` means "no file here".
fn hash_matches(current: Option<&FileEntry>, expected: Option<&str>) -> bool {
    match (current, expected) {
        (None, None) => true,
        (Some(entry), Some(expected)) => entry.sha256 == expected,
        _ => false,
    }
}

/// The `409` for a hash that does not describe the file as it stands.
fn conflict(path: &str, current: Option<&FileEntry>) -> OutputStream {
    no_store()
        .status(409)
        .json(&FileConflict::new(path, current))
}

/// A workspace-wide limit a write would have crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuotaError {
    /// The workspace already holds [`paths::MAX_FILES`] files.
    TooManyFiles,
    /// The write would take the workspace past
    /// [`paths::MAX_WORKSPACE_BYTES`].
    WorkspaceFull { would_be: u64 },
    /// The write would define a [`paths::MAX_BLOCKS`]-plus-first block.
    TooManyBlocks,
}

impl QuotaError {
    /// The refusal to send.
    ///
    /// Size limits are `413`; the block count is `409`, because nothing about
    /// the request is too large — the workspace's shape conflicts with a limit
    /// on how much the runtime can be asked to rebuild.
    fn into_response(self) -> OutputStream {
        match self {
            Self::TooManyFiles => no_store_error_status(
                ErrorCode::ResourceExhausted,
                413,
                &format!(
                    "the workspace already holds {} files, which is the limit",
                    paths::MAX_FILES
                ),
            ),
            Self::WorkspaceFull { would_be } => no_store_error_status(
                ErrorCode::ResourceExhausted,
                413,
                &format!(
                    "the write would take the workspace to {would_be} bytes; the limit is {} bytes",
                    paths::MAX_WORKSPACE_BYTES
                ),
            ),
            Self::TooManyBlocks => no_store_error(
                ErrorCode::AlreadyExists,
                &format!(
                    "the workspace already defines {} blocks, which is the limit",
                    paths::MAX_BLOCKS
                ),
            ),
        }
    }
}

/// Whether writing `size` bytes at `path` keeps the workspace inside every
/// limit.
///
/// Pure, and separate from the handler, because the interesting cases are the
/// boundaries — a replacement that frees as much as it adds, a block that
/// already exists — and driving 2 000 HTTP writes to reach one of them would
/// test the loop rather than the rule.
fn check_quotas(
    ws: &Workspace,
    path: &str,
    area: &WorkspaceArea,
    size: u64,
) -> Result<(), QuotaError> {
    let existing = ws.get(path);
    if existing.is_none() && ws.files.len() >= paths::MAX_FILES {
        return Err(QuotaError::TooManyFiles);
    }
    let would_be = ws.total_bytes() - existing.map_or(0, |entry| entry.size) + size;
    if would_be > paths::MAX_WORKSPACE_BYTES {
        return Err(QuotaError::WorkspaceFull { would_be });
    }
    if let WorkspaceArea::Block(name) = area {
        let names = ws.block_names();
        if !names.iter().any(|known| known == name) && names.len() >= paths::MAX_BLOCKS {
            return Err(QuotaError::TooManyBlocks);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sha: &str, size: u64) -> FileEntry {
        FileEntry {
            path: "site/a.css".to_string(),
            sha256: sha.to_string(),
            size,
            content_type: "text/css; charset=utf-8".to_string(),
        }
    }

    #[test]
    fn a_hash_describes_a_file_only_when_both_agree_it_exists() {
        assert!(hash_matches(None, None));
        assert!(hash_matches(Some(&entry("abc", 1)), Some("abc")));
        // A new-file write over a file that exists.
        assert!(!hash_matches(Some(&entry("abc", 1)), None));
        // A replacement of a file that is not there.
        assert!(!hash_matches(None, Some("abc")));
        assert!(!hash_matches(Some(&entry("abc", 1)), Some("def")));
        // Hex is compared verbatim — no case folding, no prefix match.
        assert!(!hash_matches(Some(&entry("abc", 1)), Some("ABC")));
        assert!(!hash_matches(Some(&entry("abc", 1)), Some("ab")));
    }

    #[test]
    fn utf8_text_comes_back_as_text_and_everything_else_as_base64() {
        assert_eq!(
            encode_content("text/plain; charset=utf-8", b"hi".to_vec()),
            (FileEncoding::Utf8, "hi".to_string())
        );
        assert_eq!(
            encode_content("image/png", vec![0x89, 0x50]),
            (FileEncoding::Base64, Base64::encode_string(&[0x89, 0x50]))
        );
        // Textual by extension, but not actually UTF-8: base64 rather than a
        // lossy re-encode.
        let latin1 = vec![0xffu8, 0xfe];
        assert_eq!(
            encode_content("text/plain; charset=utf-8", latin1.clone()),
            (FileEncoding::Base64, Base64::encode_string(&latin1))
        );
    }

    #[test]
    fn content_decodes_per_the_declared_encoding() {
        assert_eq!(decode_content(FileEncoding::Utf8, "hi"), Ok(b"hi".to_vec()));
        assert_eq!(
            decode_content(FileEncoding::Base64, &Base64::encode_string(&[1, 2, 3])),
            Ok(vec![1, 2, 3])
        );
        assert!(decode_content(FileEncoding::Base64, "not base64!!").is_err());
        // A base64 body is never silently taken as text.
        assert_eq!(
            decode_content(FileEncoding::Utf8, "aGk="),
            Ok(b"aGk=".to_vec())
        );
    }

    fn workspace_with(files: &[(&str, u64)]) -> Workspace {
        let mut ws = Workspace::default();
        for (path, size) in files {
            ws.insert(path, format!("sha-{path}"), *size);
        }
        ws
    }

    #[test]
    fn a_write_inside_every_limit_is_allowed() {
        let ws = workspace_with(&[("site/a.css", 10)]);
        assert_eq!(
            check_quotas(&ws, "site/b.css", &WorkspaceArea::Site, 10),
            Ok(())
        );
    }

    #[test]
    fn the_file_count_limit_counts_new_paths_only() {
        let files: Vec<(String, u64)> = (0..paths::MAX_FILES)
            .map(|i| (format!("site/f{i}.txt"), 1))
            .collect();
        let ws = workspace_with(
            &files
                .iter()
                .map(|(p, s)| (p.as_str(), *s))
                .collect::<Vec<_>>(),
        );
        assert_eq!(ws.files.len(), paths::MAX_FILES);
        assert_eq!(
            check_quotas(&ws, "site/one-more.txt", &WorkspaceArea::Site, 1),
            Err(QuotaError::TooManyFiles)
        );
        // Replacing a file the workspace already holds adds no path, so a
        // full workspace stays editable.
        assert_eq!(
            check_quotas(&ws, "site/f0.txt", &WorkspaceArea::Site, 1),
            Ok(())
        );
    }

    #[test]
    fn the_byte_limit_accounts_for_what_a_replacement_frees() {
        let ws = workspace_with(&[("site/a.css", paths::MAX_WORKSPACE_BYTES)]);
        assert_eq!(
            check_quotas(&ws, "site/b.css", &WorkspaceArea::Site, 1),
            Err(QuotaError::WorkspaceFull {
                would_be: paths::MAX_WORKSPACE_BYTES + 1
            })
        );
        // Overwriting the offending file with the same size is not a growth.
        assert_eq!(
            check_quotas(
                &ws,
                "site/a.css",
                &WorkspaceArea::Site,
                paths::MAX_WORKSPACE_BYTES
            ),
            Ok(())
        );
        // Exactly at the limit is allowed.
        let ws = workspace_with(&[("site/a.css", paths::MAX_WORKSPACE_BYTES - 1)]);
        assert_eq!(
            check_quotas(&ws, "site/b.css", &WorkspaceArea::Site, 1),
            Ok(())
        );
    }

    #[test]
    fn the_block_limit_counts_distinct_blocks_not_files() {
        let files: Vec<(String, u64)> = (0..paths::MAX_BLOCKS)
            .map(|i| (format!("blocks/b{i}/src/lib.rs"), 1))
            .collect();
        let ws = workspace_with(
            &files
                .iter()
                .map(|(p, s)| (p.as_str(), *s))
                .collect::<Vec<_>>(),
        );
        assert_eq!(ws.block_names().len(), paths::MAX_BLOCKS);
        assert_eq!(
            check_quotas(
                &ws,
                "blocks/extra/src/lib.rs",
                &WorkspaceArea::Block("extra".to_string()),
                1
            ),
            Err(QuotaError::TooManyBlocks)
        );
        // A second file in a block that already exists is not a new block.
        assert_eq!(
            check_quotas(
                &ws,
                "blocks/b0/Cargo.toml",
                &WorkspaceArea::Block("b0".to_string()),
                1
            ),
            Ok(())
        );
        // Nor is a site file affected by the block count.
        assert_eq!(
            check_quotas(&ws, "site/a.css", &WorkspaceArea::Site, 1),
            Ok(())
        );
    }
}
