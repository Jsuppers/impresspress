//! Staged compiler artifacts (`impresspress__dev__builds`).
//!
//! One row per compile that produced bytes, whether or not those bytes turned
//! out to be a usable block. The row keeps the source hash, the artifact hash,
//! the reported `BlockInfo` and the compiler diagnostics, so a refusal can be
//! explained after the fact without re-running the toolchain.

use wafer_block::db::{Filter, FilterOp, ListOptions, SortField};
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

/// Move a build to `status`, optionally replacing its diagnostics and the
/// `BlockInfo` the guest reported.
///
/// Both extras are `Option` writes: `None` leaves the column alone rather
/// than clearing the compiler output that explains the row, or the info a
/// later validation reads back. A row is inserted before the guest has run,
/// so `block_info_json` is `"null"` until this call fills it in.
pub async fn set_status(
    ctx: &dyn Context,
    id: &str,
    status: BuildStatus,
    diagnostics_json: Option<&str>,
    block_info_json: Option<&str>,
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
    if let Some(block_info_json) = block_info_json {
        data.insert("block_info_json".into(), serde_json::json!(block_info_json));
    }
    db::update(ctx, TABLE, id, data).await?;
    Ok(())
}

/// The newest [`BuildStatus::Valid`] build for `artifact_sha256`, if any.
///
/// This is how a block already in the active set gets its `BlockInfo` back:
/// a generation's block manifest carries the artifact hash and the routes but
/// not the endpoints, and the duplicate-agent-tool rule needs the endpoints.
/// Filtered in the query rather than by paging `list_recent` because the
/// answer must not depend on how many builds have happened since.
pub async fn latest_valid_for_artifact(
    ctx: &dyn Context,
    artifact_sha256: &str,
) -> Result<Option<BuildRow>, WaferError> {
    let list = db::list(
        ctx,
        TABLE,
        &ListOptions {
            filters: vec![
                Filter {
                    field: "artifact_sha256".into(),
                    operator: FilterOp::Equal,
                    value: serde_json::json!(artifact_sha256),
                },
                Filter {
                    field: "status".into(),
                    operator: FilterOp::Equal,
                    value: serde_json::json!(BuildStatus::Valid.as_str()),
                },
            ],
            // With `limit: 1` the tiebreaker is not cosmetic: two valid builds
            // of one artifact stamped in the same millisecond would otherwise
            // make "the latest" an arbitrary choice between them, and the
            // duplicate-agent-tool rule reads the `BlockInfo` off whichever
            // row it lands on.
            sort: newest_first(),
            limit: 1,
            skip_count: true,
            ..Default::default()
        },
    )
    .await?;
    list.records.first().map(decode).transpose()
}

/// The `limit` newest builds, newest first.
///
/// `id` breaks a `created_at` tie, for the reason
/// [`super::generations::list_recent`] states: the timestamp is
/// millisecond-resolution on wasm32, and two rows sharing one would otherwise
/// come back in whatever order the backend chose.
pub async fn list_recent(ctx: &dyn Context, limit: i64) -> Result<Vec<BuildRow>, WaferError> {
    let list = db::list(
        ctx,
        TABLE,
        &ListOptions {
            sort: newest_first(),
            limit: limit.clamp(1, MAX_LIST_LIMIT),
            skip_count: true,
            ..Default::default()
        },
    )
    .await?;
    list.records.iter().map(decode).collect()
}

/// Every build stamped at or after `created_at`, newest first.
///
/// The garbage collector's window on a compile that has not reached a
/// generation yet: staging stores the row before the bytes and only asks for
/// an activation once the guest has been accepted, so between those two there
/// is an artifact no manifest names. `None` means "the whole table" — an
/// instance with no generations has no boundary to be younger than, and
/// protecting every artifact is the safe reading of that.
///
/// Capped at [`MAX_LIST_LIMIT`] like every other listing here, and newest
/// first so the page that survives the cap is the one holding the compiles
/// that could still be in flight.
pub async fn list_since(
    ctx: &dyn Context,
    created_at: Option<&str>,
) -> Result<Vec<BuildRow>, WaferError> {
    let list = db::list(
        ctx,
        TABLE,
        &ListOptions {
            filters: created_at
                .map(|created_at| {
                    vec![Filter {
                        field: "created_at".into(),
                        operator: FilterOp::GreaterEqual,
                        value: serde_json::json!(created_at),
                    }]
                })
                .unwrap_or_default(),
            sort: newest_first(),
            limit: MAX_LIST_LIMIT,
            skip_count: true,
            ..Default::default()
        },
    )
    .await?;
    list.records.iter().map(decode).collect()
}

/// Delete every build row naming `artifact_sha256`.
///
/// For the garbage collector, and only after the artifact itself is gone: a
/// row that says an artifact was accepted, for bytes the store no longer
/// holds, is a claim [`latest_valid_for_artifact`] would hand back to the
/// duplicate-tool check as if the block were still loadable.
pub async fn delete_for_artifact(
    ctx: &dyn Context,
    artifact_sha256: &str,
) -> Result<(), WaferError> {
    db::delete_by_filters(
        ctx,
        TABLE,
        vec![Filter {
            field: "artifact_sha256".into(),
            operator: FilterOp::Equal,
            value: serde_json::json!(artifact_sha256),
        }],
    )
    .await
}

/// Newest first, with `id` breaking a `created_at` tie — the ordering every
/// listing here uses, for the reason [`list_recent`] states.
fn newest_first() -> Vec<SortField> {
    vec![
        SortField {
            field: "created_at".into(),
            desc: true,
        },
        SortField {
            field: "id".into(),
            desc: true,
        },
    ]
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{blocks::dev::test_support::FakeControl, test_support::TestContext};

    /// JSON-shaped columns in canonical form — same round-trip hazard as the
    /// generation manifests (see `repo::json_text`).
    const BLOCK_INFO: &str = r#"{"name":"site/newsletter","version":"0.1.0"}"#;
    const DIAGNOSTICS: &str = r#"[{"level":"warning","message":"unused import"}]"#;

    fn new_build(block_name: &str) -> NewBuild {
        NewBuild {
            block_name: block_name.to_string(),
            source_manifest_sha256: "src".to_string(),
            artifact_sha256: "art".to_string(),
            block_info_json: BLOCK_INFO.to_string(),
            diagnostics_json: DIAGNOSTICS.to_string(),
            compiler_version: "rubrc@pinned".to_string(),
        }
    }

    #[tokio::test]
    async fn insert_then_get_round_trips_every_column() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let inserted = insert(&ctx, &new_build("site/newsletter"))
            .await
            .expect("insert");
        assert_eq!(inserted.status, BuildStatus::Staged);

        let read_back = get(&ctx, &inserted.id).await.expect("get");
        assert_eq!(read_back, inserted);
        assert_eq!(read_back.block_info_json, BLOCK_INFO);
        assert_eq!(read_back.diagnostics_json, DIAGNOSTICS);
        assert_eq!(read_back.compiler_version, "rubrc@pinned");
    }

    #[tokio::test]
    async fn set_status_replaces_diagnostics_only_when_given_them() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let row = insert(&ctx, &new_build("site/newsletter"))
            .await
            .expect("insert");

        // Accepting a build must not wipe the warnings the compile produced.
        set_status(&ctx, &row.id, BuildStatus::Valid, None, None)
            .await
            .expect("accept");
        let valid = get(&ctx, &row.id).await.expect("get");
        assert_eq!(valid.status, BuildStatus::Valid);
        assert_eq!(valid.diagnostics_json, DIAGNOSTICS);

        let refusal = r#"[{"level":"error","message":"probe trapped"}]"#;
        set_status(&ctx, &row.id, BuildStatus::Invalid, Some(refusal), None)
            .await
            .expect("refuse");
        let invalid = get(&ctx, &row.id).await.expect("get");
        assert_eq!(invalid.status, BuildStatus::Invalid);
        assert_eq!(invalid.diagnostics_json, refusal);
    }

    #[tokio::test]
    async fn list_recent_is_newest_first_and_honours_the_limit() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let mut rows = Vec::new();
        for name in ["site/a", "site/b", "site/c"] {
            rows.push(insert(&ctx, &new_build(name)).await.expect("insert"));
        }
        assert!(
            rows[0].created_at < rows[1].created_at && rows[1].created_at < rows[2].created_at,
            "insert must produce strictly increasing created_at: {:?}",
            rows.iter().map(|r| &r.created_at).collect::<Vec<_>>(),
        );

        let listed = list_recent(&ctx, 10).await.expect("list");
        assert_eq!(
            listed
                .iter()
                .map(|r| r.block_name.as_str())
                .collect::<Vec<_>>(),
            vec!["site/c", "site/b", "site/a"],
        );
        assert_eq!(list_recent(&ctx, 1).await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn the_reported_block_info_is_written_by_the_call_that_accepts_the_build() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let row = insert(&ctx, &new_build("site/newsletter"))
            .await
            .expect("insert");
        // Nothing is filed under the artifact until a build is accepted.
        assert_eq!(
            latest_valid_for_artifact(&ctx, "art")
                .await
                .expect("lookup"),
            None,
        );

        let reported = r#"{"name":"site/newsletter","version":"0.2.0"}"#;
        set_status(&ctx, &row.id, BuildStatus::Valid, None, Some(reported))
            .await
            .expect("accept");
        let found = latest_valid_for_artifact(&ctx, "art")
            .await
            .expect("lookup")
            .expect("a valid build");
        assert_eq!(found.id, row.id);
        assert_eq!(found.block_info_json, reported);
        // A different artifact is a different question.
        assert_eq!(
            latest_valid_for_artifact(&ctx, "other")
                .await
                .expect("lookup"),
            None,
        );
    }

    /// The collector reads this to decide which artifacts a compile may still
    /// be on its way to a generation with, so the boundary has to be
    /// inclusive and `None` has to mean "everything".
    #[tokio::test]
    async fn list_since_is_bounded_below_by_the_timestamp_it_is_given() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let mut rows = Vec::new();
        for name in ["site/a", "site/b", "site/c"] {
            rows.push(insert(&ctx, &new_build(name)).await.expect("insert"));
        }

        let all = list_since(&ctx, None).await.expect("list");
        assert_eq!(all.len(), 3, "no boundary is the whole table");

        let from_second = list_since(&ctx, Some(&rows[1].created_at))
            .await
            .expect("list");
        assert_eq!(
            from_second
                .iter()
                .map(|r| r.block_name.as_str())
                .collect::<Vec<_>>(),
            vec!["site/c", "site/b"],
            "at or after the boundary, newest first",
        );
    }

    #[tokio::test]
    async fn delete_for_artifact_removes_every_row_naming_it() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        // Two rows for one artifact — a re-stage of identical bytes — plus one
        // for different bytes, which must survive.
        let first = insert(&ctx, &new_build("site/a")).await.expect("insert");
        let second = insert(&ctx, &new_build("site/b")).await.expect("insert");
        let mut other = new_build("site/c");
        other.artifact_sha256 = "kept".to_string();
        let other = insert(&ctx, &other).await.expect("insert");

        delete_for_artifact(&ctx, "art").await.expect("delete");
        for gone in [&first, &second] {
            assert_eq!(
                get(&ctx, &gone.id).await.expect_err("gone").code,
                ErrorCode::NotFound
            );
        }
        assert_eq!(get(&ctx, &other.id).await.expect("kept").id, other.id);

        // Idempotent: collecting an artifact whose rows are already gone is
        // not an error.
        delete_for_artifact(&ctx, "art")
            .await
            .expect("delete again");
    }

    #[test]
    fn decode_refuses_an_unknown_status() {
        let mut data = HashMap::new();
        data.insert("id".to_string(), serde_json::json!("b1"));
        data.insert("block_name".to_string(), serde_json::json!("site/x"));
        data.insert(
            "source_manifest_sha256".to_string(),
            serde_json::json!("src"),
        );
        data.insert("artifact_sha256".to_string(), serde_json::json!("art"));
        data.insert("block_info_json".to_string(), serde_json::json!("null"));
        data.insert("diagnostics_json".to_string(), serde_json::json!("[]"));
        data.insert("compiler_version".to_string(), serde_json::json!("v"));
        data.insert("status".to_string(), serde_json::json!("probably_fine"));
        data.insert("created_at".to_string(), serde_json::json!("1970-01-01"));
        let record = db::Record {
            id: "b1".to_string(),
            data,
        };

        let err = decode(&record).expect_err("unknown status must fail");
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(err.message.contains("probably_fine"), "{err}");
    }
}
