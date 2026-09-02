//! Typed request/response contracts for the `/b/dev` JSON API.
//!
//! Every `///` doc comment here is published: the derived schema carries it
//! into `/openapi.json` and, through that, into the agent tool descriptions
//! the `/b/dev` page registers. Write them for the agent, not for the reader
//! of this file.

use serde::{Deserialize, Serialize};

use super::{
    control::{DynamicBlockSpec, DynamicRoute},
    repo::{
        generations::{GenerationCause, GenerationStatus},
        runtime_state::ActivationPhase,
    },
};

/// Response of `GET /b/dev/api/status`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusResponse {
    /// The active generation, or null on a fresh instance.
    pub active_generation: Option<GenerationSummary>,
    /// Bumped on every runtime rebuild; the page refreshes tool registrations
    /// when it changes.
    pub runtime_generation: u64,
    /// Blocks in the active generation.
    pub blocks: Vec<ActiveBlockView>,
    /// The activation in progress, if any.
    pub activation: Option<ActivationView>,
    /// `wafer_guest.rs` version the block scaffolder currently writes.
    pub wafer_guest_version: u32,
}

/// One entry in the publication ledger, as the status and generation views
/// publish it.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerationSummary {
    /// Generation id.
    pub id: String,
    /// The generation this one was derived from, or null for the first.
    pub parent_id: Option<String>,
    /// What created this generation.
    pub cause: GenerationCause,
    /// Where the generation sits in its lifecycle.
    pub status: GenerationStatus,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 time the generation went live, or null if it never did.
    pub activated_at: Option<String>,
    /// Number of files in the generation's site manifest.
    pub site_files: u32,
    /// Number of blocks in the generation's block manifest.
    pub blocks: u32,
}

/// A block serving in the active generation.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActiveBlockView {
    /// Registered block name.
    pub name: String,
    /// SHA-256 of the artifact the block was loaded from, hex-encoded.
    pub artifact_sha256: String,
    /// Route prefixes the block serves, with the access tier the router
    /// enforces for each.
    pub routes: Vec<DynamicRoute>,
}

/// An activation the sandbox is working through right now.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActivationView {
    /// The generation being activated.
    pub generation_id: String,
    /// Which phase it has reached.
    pub phase: ActivationPhase,
    /// Human-readable detail for the progress panel.
    pub detail: String,
}

/// One file in a generation's site manifest (design §11.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SiteFileEntry {
    /// Workspace-relative path under `site/`.
    pub path: String,
    /// SHA-256 of the file's content-addressed blob, hex-encoded.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
    /// Content type the site publisher serves the file with.
    pub content_type: String,
}

/// The `site` half of a generation manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SiteManifest {
    /// Every file the generation publishes.
    #[serde(default)]
    pub files: Vec<SiteFileEntry>,
}

impl ActiveBlockView {
    /// Project a block manifest entry into the view. The manifest entry and
    /// the spec the runtime is built from are the same type
    /// ([`DynamicBlockSpec`]), so this is the only place the wire view drops
    /// fields — capabilities and the guest ABI version are not published.
    pub fn from_spec(spec: &DynamicBlockSpec) -> Self {
        Self {
            name: spec.name.clone(),
            artifact_sha256: spec.artifact_sha256.clone(),
            routes: spec.routes.clone(),
        }
    }
}
