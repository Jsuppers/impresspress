//! The release-manifest wire types `/_deploy/verify` reads out of R2.
//!
//! `impresspress deploy --target cloudflare` writes this document beside the
//! immutable asset objects it uploads; the Worker re-reads it during deep
//! verification and re-derives from it the asset-set digest bound into the
//! prepared plan. `deny_unknown_fields` on both row types is load-bearing: a
//! manifest carrying a field this Worker version does not know about was
//! written under a different release contract, and verification must fail
//! rather than silently ignore it.

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeReleaseManifest {
    pub(crate) schema_version: u32,
    pub(crate) asset_set_sha256: String,
    pub(crate) immutable_prefix: String,
    pub(crate) files: Vec<RuntimeReleaseAssetEntry>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeReleaseAssetEntry {
    pub(crate) logical_key: String,
    pub(crate) size: u64,
    pub(crate) sha256: String,
    pub(crate) content_type: String,
}

#[derive(serde::Serialize)]
pub(crate) struct RuntimeReleaseManifestIdentity<'a> {
    pub(crate) schema_version: u32,
    pub(crate) files: &'a [RuntimeReleaseAssetEntry],
}
