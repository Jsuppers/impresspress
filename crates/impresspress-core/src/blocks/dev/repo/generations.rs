//! The publication ledger (`impresspress__dev__generations`).
//!
//! Append-only: a generation is never edited back into a previous shape.
//! Rolling back publishes a *new* generation that copies an old one's
//! manifests (design §7.2), so the history stays a straight line.

use wafer_block::db::{ListOptions, SortField};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, ErrorCode, WaferError};

use crate::util::RecordExt;

pub const TABLE: &str = "impresspress__dev__generations";

/// Where a generation sits in its lifecycle.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GenerationStatus {
    /// Recorded, not yet examined.
    Staged,
    /// Blobs, artifacts and guests are being checked.
    Validating,
    /// The runtime swap and site publish are in progress.
    Activating,
    /// Currently serving.
    Active,
    /// Abandoned; `failure_message` says why.
    Failed,
    /// Aged out of the retention window.
    Superseded,
}

impl GenerationStatus {
    /// Canonical string form (matches the serde representation and the
    /// column's `CHECK` constraint).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Validating => "validating",
            Self::Activating => "activating",
            Self::Active => "active",
            Self::Failed => "failed",
            Self::Superseded => "superseded",
        }
    }

    /// Inverse of [`Self::as_str`]; `None` for an unrecognized value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "staged" => Some(Self::Staged),
            "validating" => Some(Self::Validating),
            "activating" => Some(Self::Activating),
            "active" => Some(Self::Active),
            "failed" => Some(Self::Failed),
            "superseded" => Some(Self::Superseded),
            _ => None,
        }
    }
}

/// Why a generation was created.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GenerationCause {
    /// A `site/**` write. Site-only — no runtime rebuild.
    SiteWrite,
    /// A `site/**` delete. Site-only — no runtime rebuild.
    SiteDelete,
    /// A successful compile stage; the block set changed.
    BlockCompile,
    /// A block was removed; the block set changed.
    BlockRemove,
    /// A republish of an earlier generation's manifests.
    Rollback,
    /// Generation 0, imported from the seed bundle on cold boot.
    Seed,
}

impl GenerationCause {
    /// Canonical string form (matches the serde representation and the
    /// column's `CHECK` constraint).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SiteWrite => "site_write",
            Self::SiteDelete => "site_delete",
            Self::BlockCompile => "block_compile",
            Self::BlockRemove => "block_remove",
            Self::Rollback => "rollback",
            Self::Seed => "seed",
        }
    }

    /// Inverse of [`Self::as_str`]; `None` for an unrecognized value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "site_write" => Some(Self::SiteWrite),
            "site_delete" => Some(Self::SiteDelete),
            "block_compile" => Some(Self::BlockCompile),
            "block_remove" => Some(Self::BlockRemove),
            "rollback" => Some(Self::Rollback),
            "seed" => Some(Self::Seed),
            _ => None,
        }
    }

    /// Whether a generation with this cause changes the block set, and so
    /// needs a runtime rebuild rather than a site-only republish (§7.2).
    pub fn rebuilds_runtime(self) -> bool {
        match self {
            Self::SiteWrite | Self::SiteDelete => false,
            Self::BlockCompile | Self::BlockRemove | Self::Rollback | Self::Seed => true,
        }
    }
}

/// One ledger row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationRow {
    /// Generation id.
    pub id: String,
    /// The generation this one was derived from, or `None` for generation 0.
    pub parent_id: Option<String>,
    /// Lifecycle status.
    pub status: GenerationStatus,
    /// What created it.
    pub cause: GenerationCause,
    /// Canonical JSON of the `site` half of the manifest (§11.3).
    pub site_manifest_json: String,
    /// Canonical JSON of the `blocks` half of the manifest (§11.3).
    pub block_manifest_json: String,
    /// SHA-256 of the canonical manifest, hex-encoded.
    pub manifest_sha256: String,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 time the generation went live, if it ever did.
    pub activated_at: Option<String>,
    /// Why it failed, if it did.
    pub failure_message: Option<String>,
}

/// The caller-supplied half of a new ledger row; the repo mints the id,
/// timestamp and initial status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewGeneration {
    /// Parent generation, or `None` for generation 0.
    pub parent_id: Option<String>,
    /// What created it.
    pub cause: GenerationCause,
    /// Canonical JSON of the `site` half of the manifest.
    pub site_manifest_json: String,
    /// Canonical JSON of the `blocks` half of the manifest.
    pub block_manifest_json: String,
    /// SHA-256 of the canonical manifest, hex-encoded.
    pub manifest_sha256: String,
}

/// Append a generation in [`GenerationStatus::Staged`], returning the stored
/// row (so the caller has the minted id without re-reading).
pub async fn insert(ctx: &dyn Context, new: &NewGeneration) -> Result<GenerationRow, WaferError> {
    let row = GenerationRow {
        id: super::new_id(),
        parent_id: new.parent_id.clone(),
        status: GenerationStatus::Staged,
        cause: new.cause,
        site_manifest_json: new.site_manifest_json.clone(),
        block_manifest_json: new.block_manifest_json.clone(),
        manifest_sha256: new.manifest_sha256.clone(),
        created_at: super::now(),
        activated_at: None,
        failure_message: None,
    };

    db::create(
        ctx,
        TABLE,
        crate::util::json_map(serde_json::json!({
            "id": row.id,
            "parent_id": row.parent_id,
            "status": row.status.as_str(),
            "cause": row.cause.as_str(),
            "site_manifest_json": row.site_manifest_json,
            "block_manifest_json": row.block_manifest_json,
            "manifest_sha256": row.manifest_sha256,
            "created_at": row.created_at,
            "activated_at": row.activated_at,
            "failure_message": row.failure_message,
        })),
    )
    .await?;

    Ok(row)
}

/// Read one generation by id.
pub async fn get(ctx: &dyn Context, id: &str) -> Result<GenerationRow, WaferError> {
    decode(&db::get(ctx, TABLE, id).await?)
}

/// Move a generation to `status`, optionally recording why it failed and when
/// it went live.
///
/// `failure` and `activated_at` are `Option` writes: `None` leaves the column
/// alone rather than clearing it, so a status change never silently erases the
/// message that explains an earlier one.
pub async fn set_status(
    ctx: &dyn Context,
    id: &str,
    status: GenerationStatus,
    failure: Option<&str>,
    activated_at: Option<&str>,
) -> Result<(), WaferError> {
    let mut data = crate::util::json_map(serde_json::json!({
        "status": status.as_str(),
    }));
    if let Some(failure) = failure {
        data.insert("failure_message".into(), serde_json::json!(failure));
    }
    if let Some(activated_at) = activated_at {
        data.insert("activated_at".into(), serde_json::json!(activated_at));
    }
    db::update(ctx, TABLE, id, data).await?;
    Ok(())
}

/// The `limit` newest generations, newest first.
pub async fn list_recent(ctx: &dyn Context, limit: i64) -> Result<Vec<GenerationRow>, WaferError> {
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

/// Upper bound on one `list_recent` page. The retention window is 20
/// generations (§7.3); a page an order of magnitude larger is already a
/// generous ceiling for the ledger view.
const MAX_LIST_LIMIT: i64 = 200;

/// Mark every generation outside the newest `keep` as
/// [`GenerationStatus::Superseded`], returning how many rows changed.
///
/// Retention only *labels* rows here — blob collection reads the labels and is
/// a separate step, so a crash between the two loses no data.
pub async fn mark_superseded_before(ctx: &dyn Context, keep: usize) -> Result<usize, WaferError> {
    let retained = list_recent(ctx, MAX_LIST_LIMIT).await?;
    let mut marked = 0usize;
    for row in retained.into_iter().skip(keep) {
        if row.status == GenerationStatus::Superseded {
            continue;
        }
        set_status(ctx, &row.id, GenerationStatus::Superseded, None, None).await?;
        marked += 1;
    }
    Ok(marked)
}

/// Decode a stored row. An unrecognized `status` or `cause` is an error, never
/// a default: the ledger drives what gets served, so an unreadable row must
/// stop the caller rather than be quietly reinterpreted.
fn decode(record: &db::Record) -> Result<GenerationRow, WaferError> {
    let status_text = record.str_field("status");
    let cause_text = record.str_field("cause");
    Ok(GenerationRow {
        id: record.str_field("id").to_string(),
        parent_id: record.opt_str_field("parent_id"),
        status: GenerationStatus::parse(status_text)
            .ok_or_else(|| unknown_value("status", status_text))?,
        cause: GenerationCause::parse(cause_text)
            .ok_or_else(|| unknown_value("cause", cause_text))?,
        site_manifest_json: super::json_text(record, "site_manifest_json"),
        block_manifest_json: super::json_text(record, "block_manifest_json"),
        manifest_sha256: record.str_field("manifest_sha256").to_string(),
        created_at: record.str_field("created_at").to_string(),
        activated_at: record.opt_str_field("activated_at"),
        failure_message: record.opt_str_field("failure_message"),
    })
}

fn unknown_value(column: &str, value: &str) -> WaferError {
    WaferError::new(
        ErrorCode::Internal,
        format!("{TABLE}: unknown {column} `{value}`"),
    )
}
