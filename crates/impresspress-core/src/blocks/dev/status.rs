//! `GET /b/dev/api/status` — what is live, what is in flight.
//!
//! The `/b/dev` page polls this every ~300 ms while a mutating tool call is
//! outstanding (design §7.5), which is why the whole block answers
//! `Cache-Control: no-store`: a cached status is a progress panel that never
//! moves.

use wafer_run::{context::Context, OutputStream, WaferError};

use super::{
    contracts::{ActivationView, ActiveBlockView, GenerationSummary, SiteManifest, StatusResponse},
    control::DynamicBlockSpec,
    no_store,
    repo::{self, generations::GenerationRow},
    DevShared, WAFER_GUEST_VERSION,
};
use crate::http::err_internal;

/// Answer the status endpoint.
pub async fn handle(ctx: &dyn Context, shared: &DevShared) -> OutputStream {
    match build(ctx, shared).await {
        Ok(response) => no_store().json(&response),
        Err(e) => err_internal("dev sandbox status", e),
    }
}

async fn build(ctx: &dyn Context, shared: &DevShared) -> Result<StatusResponse, WaferError> {
    let state = repo::runtime_state::read(ctx).await?;

    let active = match state.active_generation_id.as_deref() {
        Some(id) => Some(repo::generations::get(ctx, id).await?),
        None => None,
    };

    // The block manifest is stored as the same `DynamicBlockSpec` list the
    // runtime is rebuilt from, so the active block set needs no separate
    // record — it is a projection of the generation that is live.
    let specs = match active.as_ref() {
        Some(row) => decode_blocks(row)?,
        None => Vec::new(),
    };

    let activation = state
        .desired_generation_id
        .map(|generation_id| ActivationView {
            generation_id,
            phase: state.activation_phase,
            detail: String::new(),
        });

    Ok(StatusResponse {
        active_generation: active
            .as_ref()
            .map(|row| summarize(row, specs.len()))
            .transpose()?,
        runtime_generation: shared.control.runtime_generation(),
        blocks: specs.iter().map(ActiveBlockView::from_spec).collect(),
        activation,
        wafer_guest_version: WAFER_GUEST_VERSION,
    })
}

/// Decode a generation's block manifest. A manifest that does not parse is an
/// error rather than an empty block list: reporting "no blocks" for a
/// generation that has some would tell the page to drop tool registrations
/// that are still live.
fn decode_blocks(row: &GenerationRow) -> Result<Vec<DynamicBlockSpec>, WaferError> {
    manifest_field(&row.id, "block_manifest_json", &row.block_manifest_json)
}

fn summarize(row: &GenerationRow, blocks: usize) -> Result<GenerationSummary, WaferError> {
    let site: SiteManifest =
        manifest_field(&row.id, "site_manifest_json", &row.site_manifest_json)?;
    Ok(GenerationSummary {
        id: row.id.clone(),
        parent_id: row.parent_id.clone(),
        cause: row.cause,
        status: row.status,
        created_at: row.created_at.clone(),
        activated_at: row.activated_at.clone(),
        site_files: site.files.len() as u32,
        blocks: blocks as u32,
    })
}

fn manifest_field<T: serde::de::DeserializeOwned>(
    generation_id: &str,
    column: &str,
    json: &str,
) -> Result<T, WaferError> {
    serde_json::from_str(json).map_err(|e| {
        WaferError::new(
            wafer_run::ErrorCode::Internal,
            format!("generation {generation_id}: {column} did not parse: {e}"),
        )
    })
}
