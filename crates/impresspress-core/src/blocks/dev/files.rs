//! `/b/dev/api/files*` — read, write, list and delete workspace files.
//!
//! # Lost updates, and how far the guard reaches
//!
//! Every mutation states the hash it believes the file currently has
//! (`expected_sha256`, `null` for "no file yet"). A mismatch is a `409`
//! carrying the hash it actually has, so the caller re-reads rather than
//! silently overwriting an edit it never saw. The sandbox's clients are an
//! agent and a human editing the same workspace at the same time; making the
//! check optional would make that race invisible.
//!
//! That guard is **per path**, and it is the only guard these handlers hold.
//! A write is read-modify-write over the whole manifest — `workspace::load`,
//! mutate one entry, `workspace::save` — so two writers interleaving between
//! the load and the save would each save a manifest built from a snapshot
//! that predates the other, and the later save would drop the earlier
//! writer's entry even though both passed their own hash check. Nothing here
//! prevents that.
//!
//! It does not happen on the sandbox's own target: the Service Worker is
//! single-threaded and runs one request to completion, so the load and the
//! save cannot interleave. On native and Cloudflare, where concurrent
//! requests are real, the serialization has to come from above — Task 7's
//! activation queue is what orders workspace mutations there, and any handler
//! added to this module must go through it rather than assume the browser's
//! scheduling.
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
    activation, blobs,
    contracts::{
        FileConflict, FileDeleteRequest, FileDeleteResponse, FileEncoding, FileListQuery,
        FileListResponse, FileReadRequest, FileReadResponse, FileWriteRequest, FileWriteResponse,
        GenerationSummary, SiteManifest,
    },
    generation::{self, GenerationManifest},
    no_store, no_store_error, no_store_error_status,
    paths::{self, WorkspaceArea},
    repo::generations::GenerationCause,
    workspace::{self, FileEntry, Workspace},
    DevShared,
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
pub async fn handle_write(
    ctx: &dyn Context,
    shared: &DevShared,
    input: InputStream,
) -> OutputStream {
    let request: FileWriteRequest = match read_body(input).await {
        Ok(request) => request,
        Err(refusal) => return refusal,
    };
    let area = match paths::validate_path(&request.path) {
        Ok(area) => area,
        Err(e) => return no_store_error(ErrorCode::InvalidArgument, &e.to_string()),
    };
    // Refuse an over-large body BEFORE decoding it. `content` is already in
    // memory as part of the parsed request; decoding would allocate the same
    // payload a second time, so a hostile body must not get that far. The
    // bound is a *lower* bound on the decoded length, so this only ever
    // refuses what the exact check below would refuse anyway.
    if min_decoded_len(request.encoding, &request.content) > paths::MAX_FILE_BYTES {
        return too_large(&format!(
            "the {} body decodes to more than the {}-byte file limit",
            encoding_label(request.encoding),
            paths::MAX_FILE_BYTES
        ));
    }
    let bytes = match decode_content(request.encoding, &request.content) {
        Ok(bytes) => bytes,
        Err(detail) => return no_store_error(ErrorCode::InvalidArgument, &detail),
    };
    if bytes.len() > paths::MAX_FILE_BYTES {
        return too_large(&format!(
            "file is {} bytes; the limit is {} bytes",
            bytes.len(),
            paths::MAX_FILE_BYTES
        ));
    }

    let mut ws = match workspace::load(ctx).await {
        Ok(ws) => ws,
        Err(e) => return err_internal("dev workspace load", e),
    };
    let current = ws.get(&request.path);
    if !hash_matches(current, request.expected_sha256.as_deref()) {
        return conflict(&request.path, current);
    }

    // How much the blob store would grow by. Content some entry already names
    // is certainly stored (the collector only reclaims unreachable blobs), so
    // re-writing it — an undo, or the same asset at a second path — needs no
    // headroom. Content that is stored but unreferenced is treated as if it
    // were new: those bytes are garbage awaiting collection, and refusing
    // rather than assuming they will be there is the safe direction.
    let sha = blobs::sha256_hex(&bytes);
    let new_blob_bytes = if ws.references(&sha) {
        0
    } else {
        bytes.len() as u64
    };
    if let Err(e) = check_quotas(&ws, &request.path, &area, new_blob_bytes) {
        return e.into_response();
    }

    // Store, then save. The other order would let a manifest name a blob that
    // was never written, and every later read of that path would be a 500. In
    // this order a save that fails leaves an uncharged blob behind — the
    // workspace under-counts its own store until the collector reconciles it,
    // which costs headroom rather than correctness.
    match blobs::put_hashed(ctx, &sha, &bytes).await {
        // Charge the workspace only when the store actually grew.
        Ok(blobs::Stored::New) => ws.record_blob_stored(bytes.len() as u64),
        Ok(blobs::Stored::Deduplicated) => {}
        Err(e) => return err_internal("dev workspace blob write", e),
    }
    let entry = ws.insert(&request.path, sha, bytes.len() as u64);
    if let Err(e) = workspace::save(ctx, &ws).await {
        return err_internal("dev workspace save", e);
    }

    // Publish, if the file that changed is one the site serves. The order is
    // load-bearing: the workspace is saved first, so the manifest the
    // activation freezes is one that has already been persisted — an
    // activation that published content the workspace had lost would be
    // unreproducible from the workspace it claims to project.
    let generation =
        match publish_if_site(ctx, shared, &area, &ws, GenerationCause::SiteWrite).await {
            Ok(generation) => generation,
            Err(refusal) => return refusal,
        };
    no_store().json(&FileWriteResponse {
        path: entry.path,
        sha256: entry.sha256,
        size: entry.size,
        generation,
    })
}

/// `POST /b/dev/api/files/delete` — drop one file from the manifest.
pub async fn handle_delete(
    ctx: &dyn Context,
    shared: &DevShared,
    input: InputStream,
) -> OutputStream {
    let request: FileDeleteRequest = match read_body(input).await {
        Ok(request) => request,
        Err(refusal) => return refusal,
    };
    let area = match paths::validate_path(&request.path) {
        Ok(area) => area,
        Err(e) => return no_store_error(ErrorCode::InvalidArgument, &e.to_string()),
    };
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
    let generation =
        match publish_if_site(ctx, shared, &area, &ws, GenerationCause::SiteDelete).await {
            Ok(generation) => generation,
            Err(refusal) => return refusal,
        };
    no_store().json(&FileDeleteResponse {
        path: request.path,
        generation,
    })
}

/// Publish the workspace's `site/` half as a new generation, when the file
/// that changed was one the site serves.
///
/// A `blocks/` edit publishes nothing (design §7.2): block source only reaches
/// the runtime through a compile, and republishing on every keystroke in a
/// `.rs` file would rebuild the runtime from an artifact that has not been
/// rebuilt.
///
/// The block half of the manifest is the *active* generation's, unchanged —
/// this is a site republish, so `block_set_changed` is false and the runtime
/// is left alone.
async fn publish_if_site(
    ctx: &dyn Context,
    shared: &DevShared,
    area: &WorkspaceArea,
    ws: &Workspace,
    cause: GenerationCause,
) -> Result<Option<GenerationSummary>, OutputStream> {
    if !matches!(area, WorkspaceArea::Site) {
        return Ok(None);
    }
    let blocks = match generation::active(ctx).await {
        Ok(active) => active
            .map(|(_row, manifest)| manifest.blocks)
            .unwrap_or_default(),
        Err(e) => return Err(err_internal("dev active generation", e)),
    };
    let next = GenerationManifest::staged(
        SiteManifest {
            files: workspace::site_manifest(ws),
        },
        blocks,
    );
    match activation::request(ctx, shared, cause, next).await {
        Ok(outcome) => Ok(Some(outcome.generation)),
        Err(e) => Err(e.into_response()),
    }
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

/// A lower bound on how many bytes `content` decodes to.
///
/// Used to refuse an over-large body before it is decoded into a second
/// allocation. It must never over-estimate, or a legal write would be refused:
/// utf8 is exact (a JSON string's UTF-8 bytes are the file's bytes), and
/// padded standard base64 carries three bytes per four characters of which at
/// most two are padding.
fn min_decoded_len(encoding: FileEncoding, content: &str) -> usize {
    match encoding {
        FileEncoding::Utf8 => content.len(),
        FileEncoding::Base64 => (content.len() / 4).saturating_mul(3).saturating_sub(2),
    }
}

/// How an encoding is named in a refusal.
fn encoding_label(encoding: FileEncoding) -> &'static str {
    match encoding {
        FileEncoding::Utf8 => "utf8",
        FileEncoding::Base64 => "base64",
    }
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
/// `utf8` only when the type *could* be text ([`paths::may_be_text`], which
/// includes the unknown-extension fallback so a `.gitignore` or a `README`
/// comes back editable) **and** the bytes really are UTF-8: a `.txt` holding
/// latin-1 would otherwise be lossily re-encoded by the JSON serializer, and
/// the caller would write back something other than what it read. The stored
/// content type is untouched either way — this decides the envelope, not the
/// file's type.
fn encode_content(content_type: &str, bytes: Vec<u8>) -> (FileEncoding, String) {
    if !paths::may_be_text(content_type) {
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

/// The `413` every size refusal sends.
fn too_large(message: &str) -> OutputStream {
    no_store_error_status(ErrorCode::ResourceExhausted, 413, message)
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
    /// Storing the new blob would take the workspace's blob store past
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
            Self::TooManyFiles => too_large(&format!(
                "the workspace already holds {} files, which is the limit",
                paths::MAX_FILES
            )),
            Self::WorkspaceFull { would_be } => too_large(&format!(
                "the write would take the workspace's stored blobs to {would_be} bytes; the \
                 limit is {} bytes. Superseded and deleted content still counts until it is \
                 collected.",
                paths::MAX_WORKSPACE_BYTES
            )),
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

/// Whether a write that adds `new_blob_bytes` to the blob store at `path`
/// keeps the workspace inside every limit.
///
/// `new_blob_bytes` is what the *store* would grow by, not the file's size:
/// re-writing content that is already stored grows nothing, and the 64 MiB
/// limit is on stored blobs — including the ones no entry names any more —
/// because that is what actually occupies the user's storage. A limit
/// computed from the live manifest would let two hundred overwrites of one
/// 512 KiB page consume 100 MB while reporting 512 KiB.
///
/// Pure, and separate from the handler, because the interesting cases are the
/// boundaries — a workspace full of unreachable blobs, a block that already
/// exists — and driving hundreds of HTTP writes to reach one of them would
/// test the loop rather than the rule.
fn check_quotas(
    ws: &Workspace,
    path: &str,
    area: &WorkspaceArea,
    new_blob_bytes: u64,
) -> Result<(), QuotaError> {
    if ws.get(path).is_none() && ws.files.len() >= paths::MAX_FILES {
        return Err(QuotaError::TooManyFiles);
    }
    let would_be = ws.blob_bytes.saturating_add(new_blob_bytes);
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
        // Unknown type, valid UTF-8: still handed back as text. This is the
        // `.gitignore` / `README` case.
        assert_eq!(
            encode_content(paths::UNKNOWN_CONTENT_TYPE, b"target/\n".to_vec()),
            (FileEncoding::Utf8, "target/\n".to_string())
        );
        // Unknown type, not UTF-8: base64.
        assert_eq!(
            encode_content(paths::UNKNOWN_CONTENT_TYPE, latin1.clone()),
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

    /// Build a workspace whose entries are `files`, charged as if each had
    /// been stored once — the state a run of successful writes leaves behind.
    fn workspace_with(files: &[(&str, u64)]) -> Workspace {
        let mut ws = Workspace::default();
        for (path, size) in files {
            ws.insert(path, format!("sha-{path}"), *size);
            ws.record_blob_stored(*size);
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

    /// A body far too large must be refused from its encoded length alone,
    /// before it is decoded into a second copy. That is what the guard is
    /// for — a hostile body, not a body one byte over.
    #[test]
    fn an_over_large_body_is_refused_from_its_encoded_length() {
        let hostile = paths::MAX_FILE_BYTES * 2;
        assert!(min_decoded_len(FileEncoding::Utf8, &"x".repeat(hostile)) > paths::MAX_FILE_BYTES);
        // 4 base64 characters per 3 bytes.
        let encoded = "A".repeat(hostile.div_ceil(3) * 4);
        assert!(min_decoded_len(FileEncoding::Base64, &encoded) > paths::MAX_FILE_BYTES);

        // utf8 is exact, so even one byte over is caught before decoding.
        assert!(
            min_decoded_len(FileEncoding::Utf8, &"x".repeat(paths::MAX_FILE_BYTES + 1))
                > paths::MAX_FILE_BYTES
        );
        // base64's bound is short by up to two bytes (it cannot know how much
        // of the tail is padding), so a body a byte or two over slips past
        // this guard and is caught by the exact check after decoding. Both
        // refuse; only the first avoids the allocation.
        let just_over = vec![b'z'; paths::MAX_FILE_BYTES + 1];
        assert!(
            min_decoded_len(FileEncoding::Base64, &Base64::encode_string(&just_over))
                <= paths::MAX_FILE_BYTES
        );
        assert_eq!(
            decode_content(FileEncoding::Base64, &Base64::encode_string(&just_over))
                .expect("decodes")
                .len(),
            paths::MAX_FILE_BYTES + 1
        );
    }

    /// The bound must never over-estimate, or a legal write is refused before
    /// anything looks at the real bytes.
    #[test]
    fn the_pre_decode_bound_never_exceeds_the_real_decoded_length() {
        for len in 0..64usize {
            let bytes = vec![b'z'; len];
            let encoded = Base64::encode_string(&bytes);
            assert!(
                min_decoded_len(FileEncoding::Base64, &encoded) <= len,
                "base64 bound over-estimated at len {len}"
            );
            let text = "z".repeat(len);
            assert_eq!(min_decoded_len(FileEncoding::Utf8, &text), len);
        }
        // A file exactly at the limit must survive the pre-check in both
        // encodings.
        let at_limit = vec![b'z'; paths::MAX_FILE_BYTES];
        assert!(
            min_decoded_len(FileEncoding::Base64, &Base64::encode_string(&at_limit))
                <= paths::MAX_FILE_BYTES
        );
        assert!(
            min_decoded_len(FileEncoding::Utf8, &"z".repeat(paths::MAX_FILE_BYTES))
                <= paths::MAX_FILE_BYTES
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
    fn the_byte_limit_bounds_stored_blobs_not_the_live_manifest() {
        let ws = workspace_with(&[("site/a.css", paths::MAX_WORKSPACE_BYTES)]);
        assert_eq!(
            check_quotas(&ws, "site/b.css", &WorkspaceArea::Site, 1),
            Err(QuotaError::WorkspaceFull {
                would_be: paths::MAX_WORKSPACE_BYTES + 1
            })
        );
        // A write that stores nothing new — content some entry already names
        // — needs no headroom even at the limit.
        assert_eq!(
            check_quotas(&ws, "site/a.css", &WorkspaceArea::Site, 0),
            Ok(())
        );
        // Exactly at the limit is allowed.
        let ws = workspace_with(&[("site/a.css", paths::MAX_WORKSPACE_BYTES - 1)]);
        assert_eq!(
            check_quotas(&ws, "site/b.css", &WorkspaceArea::Site, 1),
            Ok(())
        );
    }

    /// The regression this quota exists for: content that is stored but no
    /// longer reachable still costs. A limit read off the live manifest would
    /// see an empty workspace here and wave the write through.
    #[test]
    fn unreferenced_blobs_still_count_against_the_limit() {
        let mut ws = Workspace::default();
        // 128 overwrites of a 512 KiB page, then a delete: nothing reachable,
        // 64 MiB stored.
        for _ in 0..(paths::MAX_WORKSPACE_BYTES / paths::MAX_FILE_BYTES as u64) {
            ws.record_blob_stored(paths::MAX_FILE_BYTES as u64);
        }
        assert!(ws.files.is_empty());
        assert_eq!(ws.total_bytes(), 0);
        assert_eq!(ws.blob_bytes, paths::MAX_WORKSPACE_BYTES);

        assert_eq!(
            check_quotas(&ws, "site/a.css", &WorkspaceArea::Site, 1),
            Err(QuotaError::WorkspaceFull {
                would_be: paths::MAX_WORKSPACE_BYTES + 1
            })
        );
        // Collecting one blob makes room again.
        ws.record_blob_freed(paths::MAX_FILE_BYTES as u64);
        assert_eq!(
            check_quotas(&ws, "site/a.css", &WorkspaceArea::Site, 1),
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
