//! Staged compiler artifacts (`impresspress__dev__builds`).
//!
//! One row per compile that produced bytes, whether or not those bytes turned
//! out to be a usable block. The row keeps the source hash, the artifact hash,
//! the reported `BlockInfo` and the compiler diagnostics, so a refusal can be
//! explained after the fact without re-running the toolchain.

use wafer_block::db::{ListOptions, SortField};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, ErrorCode, WaferError};

use crate::util::RecordExt;

pub const TABLE: &str = "impresspress__dev__builds";

/// Upper bound on one `list_recent` page.
const MAX_LIST_LIMIT: i64 = 200;

/// Whether a staged artifact was accepted.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    /// Stored, not yet validated.
    Staged,
    /// Loaded, initialized, started and answered a probe.
    Valid,
    /// Refused; `diagnostics_json` says why.
    Invalid,
}

impl BuildStatus {
    /// Canonical string form (matches the serde representation and the
    /// column's `CHECK` constraint).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Valid => "valid",
            Self::Invalid => "invalid",
        }
    }

    /// Inverse of [`Self::as_str`]; `None` for an unrecognized value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "staged" => Some(Self::Staged),
            "valid" => Some(Self::Valid),
            "invalid" => Some(Self::Invalid),
            _ => None,
        }
    }
}

/// One build row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildRow {
    /// Build id.
    pub id: String,
    /// Registered block name the artifact was compiled for (`site/{name}`).
    pub block_name: String,
    /// SHA-256 of the source manifest the compile ran against, hex-encoded.
    pub source_manifest_sha256: String,
    /// SHA-256 of the produced artifact, hex-encoded.
    pub artifact_sha256: String,
    /// JSON of the `BlockInfo` the guest reported, or `"null"` if it never
    /// got that far.
    pub block_info_json: String,
    /// JSON array of compiler/validator diagnostics.
    pub diagnostics_json: String,
    /// Pinned toolchain revision that produced the artifact.
    pub compiler_version: String,
    /// Whether the artifact was accepted.
    pub status: BuildStatus,
    /// RFC 3339 creation time.
    pub created_at: String,
}

/// The caller-supplied half of a new build row; the repo mints the id,
/// timestamp and initial status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewBuild {
    /// Registered block name the artifact was compiled for.
    pub block_name: String,
    /// SHA-256 of the source manifest the compile ran against.
    pub source_manifest_sha256: String,
    /// SHA-256 of the produced artifact.
    pub artifact_sha256: String,
    /// JSON of the reported `BlockInfo`.
    pub block_info_json: String,
    /// JSON array of compiler diagnostics.
    pub diagnostics_json: String,
    /// Pinned toolchain revision.
    pub compiler_version: String,
}

/// Append a build in [`BuildStatus::Staged`], returning the stored row.
pub async fn insert(ctx: &dyn Context, new: &NewBuild) -> Result<BuildRow, WaferError> {
    let row = BuildRow {
        id: super::new_id(),
        block_name: new.block_name.clone(),
        source_manifest_sha256: new.source_manifest_sha256.clone(),
        artifact_sha256: new.artifact_sha256.clone(),
        block_info_json: new.block_info_json.clone(),
        diagnostics_json: new.diagnostics_json.clone(),
        compiler_version: new.compiler_version.clone(),
        status: BuildStatus::Staged,
        created_at: super::now(),
    };

    db::create(
        ctx,
        TABLE,
        crate::util::json_map(serde_json::json!({
            "id": row.id,
            "block_name": row.block_name,
            "source_manifest_sha256": row.source_manifest_sha256,
            "artifact_sha256": row.artifact_sha256,
            "block_info_json": row.block_info_json,
            "diagnostics_json": row.diagnostics_json,
            "compiler_version": row.compiler_version,
            "status": row.status.as_str(),
            "created_at": row.created_at,
        })),
    )
    .await?;

    Ok(row)
}

/// Read one build by id.
pub async fn get(ctx: &dyn Context, id: &str) -> Result<BuildRow, WaferError> {
    decode(&db::get(ctx, TABLE, id).await?)
}

/// Move a build to `status`, optionally replacing its diagnostics.
///
/// `diagnostics` is an `Option` write: `None` leaves the column alone rather
/// than clearing the compiler output that explains the row.
pub async fn set_status(
    ctx: &dyn Context,
    id: &str,
    status: BuildStatus,
    diagnostics_json: Option<&str>,
) -> Result<(), WaferError> {
    let mut data = crate::util::json_map(serde_json::json!({
        "status": status.as_str(),
    }));
    if let Some(diagnostics_json) = diagnostics_json {
        data.insert(
            "diagnostics_json".into(),
            serde_json::json!(diagnostics_json),
        );
    }
    db::update(ctx, TABLE, id, data).await?;
    Ok(())
}

/// The `limit` newest builds, newest first.
pub async fn list_recent(ctx: &dyn Context, limit: i64) -> Result<Vec<BuildRow>, WaferError> {
    let list = db::list(
        ctx,
        TABLE,
        &ListOptions {
            sort: vec![SortField {
                field: "created_at".into(),
                desc: true,
            }],
            limit: limit.clamp(1, MAX_LIST_LIMIT),
            skip_count: true,
            ..Default::default()
        },
    )
    .await?;
    list.records.iter().map(decode).collect()
}

/// Decode a stored row. An unrecognized `status` is an error, never a default.
fn decode(record: &db::Record) -> Result<BuildRow, WaferError> {
    let status_text = record.str_field("status");
    Ok(BuildRow {
        id: record.str_field("id").to_string(),
        block_name: record.str_field("block_name").to_string(),
        source_manifest_sha256: record.str_field("source_manifest_sha256").to_string(),
        artifact_sha256: record.str_field("artifact_sha256").to_string(),
        block_info_json: super::json_text(record, "block_info_json"),
        diagnostics_json: super::json_text(record, "diagnostics_json"),
        compiler_version: record.str_field("compiler_version").to_string(),
        status: BuildStatus::parse(status_text).ok_or_else(|| {
            WaferError::new(
                ErrorCode::Internal,
                format!("{TABLE}: unknown status `{status_text}`"),
            )
        })?,
        created_at: record.str_field("created_at").to_string(),
    })
}
