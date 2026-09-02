//! The executable half of activation: the seam between the `impresspress/dev`
//! control plane and whatever owns the live runtime.
//!
//! `impresspress-core` can describe a block set ([`DynamicBlockSpec`]) and
//! decide what should be live, but it cannot build a `Wafer` — the runtime is
//! held by the host (`impresspress-web`'s service worker on the browser
//! target, the native/CF builder elsewhere). [`RuntimeControl`] is that
//! boundary: the dev block hands over a validated spec set and the host
//! answers with a rebuilt, swapped-in runtime.
//!
//! The static half of validation (names, route prefixes, capability
//! namespaces, collisions) is the caller's — it is pure data and lives beside
//! the spec. [`RuntimeControl::validate`] is only the part that has to
//! actually execute a guest.

use serde::{Deserialize, Serialize};

/// Access tier a dynamically-registered block asks the router to apply to one
/// of its route prefixes.
///
/// The serialized spelling is the one the generation manifest uses
/// (design §11.3: `"access": "Public"`), and [`Self::as_str`] / [`Self::parse`]
/// speak exactly that spelling — the value has one representation everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum RouteAccessKind {
    /// No auth check; anyone may reach the route.
    Public,
    /// A resolved user identity is required.
    Authenticated,
    /// The `admin` role is required.
    Admin,
}

impl RouteAccessKind {
    /// Canonical string form (matches the serde representation).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "Public",
            Self::Authenticated => "Authenticated",
            Self::Admin => "Admin",
        }
    }

    /// Inverse of [`Self::as_str`]. `None` for an unrecognized value — an
    /// unknown access tier is never silently narrowed or widened.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Public" => Some(Self::Public),
            "Authenticated" => Some(Self::Authenticated),
            "Admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// The router tier this maps onto. The two enums are the same three-tier
    /// ladder; this is the single place a dynamic block's declared tier is
    /// bridged into [`crate::routing::RouteAccess`].
    pub fn to_route_access(self) -> crate::routing::RouteAccess {
        match self {
            Self::Public => crate::routing::RouteAccess::Public,
            Self::Authenticated => crate::routing::RouteAccess::Authenticated,
            Self::Admin => crate::routing::RouteAccess::Admin,
        }
    }
}

/// One route prefix a dynamically-registered block serves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DynamicRoute {
    /// Route prefix, normalized and under `/b/{block}/`.
    pub prefix: String,
    /// Access tier the router enforces for the prefix.
    pub access: RouteAccessKind,
}

/// Everything the host needs to put one compiled guest into a runtime.
///
/// This is also the block entry of the generation manifest (design §11.3), so
/// a stored `block_manifest_json` decodes straight into `Vec<DynamicBlockSpec>`
/// with no intermediate shape.
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DynamicBlockSpec {
    /// Registered block name (`site/{name}`).
    pub name: String,
    /// SHA-256 of the `wasm32-wasip1` artifact, hex-encoded.
    pub artifact_sha256: String,
    /// Route prefixes the block serves.
    pub routes: Vec<DynamicRoute>,
    /// Capabilities the guest is loaded under. Deny-by-default; the caller
    /// validates the declared set against the block's own namespace before
    /// this ever reaches [`RuntimeControl`].
    ///
    /// `BlockCapabilities` is a producer type that derives neither
    /// `JsonSchema` nor `PartialEq`. It is published as a free-form object
    /// here, and compared field-by-field in the `PartialEq` impl below.
    #[schemars(with = "serde_json::Value")]
    pub capabilities: wafer_block::BlockCapabilities,
    /// `wafer_guest.rs` ABI version the artifact was built against.
    pub wafer_guest_version: u32,
}

/// Field-by-field capability comparison.
///
/// `wafer_block::BlockCapabilities` derives `Debug, Clone, Default, Serialize,
/// Deserialize` and nothing else, so [`DynamicBlockSpec`] cannot derive
/// `PartialEq`. Destructuring (rather than reading fields through `.`) is what
/// keeps this honest: when the producer adds a capability — the `schema`
/// capability landing beside `ddl`, for one — this stops compiling instead of
/// silently declaring two different capability sets equal.
fn capabilities_eq(
    left: &wafer_block::BlockCapabilities,
    right: &wafer_block::BlockCapabilities,
) -> bool {
    let wafer_block::BlockCapabilities {
        collections,
        raw_sql,
        ddl,
        schema,
        storage_folders,
        crypto,
        network,
        config,
        vector_indexes,
        callable_blocks,
        headers,
    } = left;
    let wafer_block::capabilities::HeaderPolicy {
        readable,
        writable,
        masked,
    } = headers;

    *collections == right.collections
        && *raw_sql == right.raw_sql
        && *ddl == right.ddl
        && *schema == right.schema
        && *storage_folders == right.storage_folders
        && *crypto == right.crypto
        && *network == right.network
        && *config == right.config
        && *vector_indexes == right.vector_indexes
        && *callable_blocks == right.callable_blocks
        && *readable == right.headers.readable
        && *writable == right.headers.writable
        && *masked == right.headers.masked
}

impl PartialEq for DynamicBlockSpec {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.artifact_sha256 == other.artifact_sha256
            && self.routes == other.routes
            && self.wafer_guest_version == other.wafer_guest_version
            && capabilities_eq(&self.capabilities, &other.capabilities)
    }
}

impl Eq for DynamicBlockSpec {}

/// Where a guest failed [`RuntimeControl::validate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStage {
    /// wasmi could not compile or instantiate the artifact.
    Load,
    /// `BlockInfo` did not parse, or failed WAFER validation.
    Info,
    /// The `Init` lifecycle event trapped or errored.
    Init,
    /// The `Start` lifecycle event trapped or errored.
    Start,
    /// The single probe request trapped or errored.
    Probe,
}

impl ValidationStage {
    /// Canonical string form (matches the serde representation).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Info => "info",
            Self::Init => "init",
            Self::Start => "start",
            Self::Probe => "probe",
        }
    }

    /// Inverse of [`Self::as_str`]; `None` for an unrecognized value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "load" => Some(Self::Load),
            "info" => Some(Self::Info),
            "init" => Some(Self::Init),
            "start" => Some(Self::Start),
            "probe" => Some(Self::Probe),
            _ => None,
        }
    }
}

/// A guest that loaded, initialized, started and answered a probe under its
/// declared capabilities.
#[derive(Clone, Debug)]
pub struct ValidatedGuest {
    /// The `BlockInfo` the guest reported. The caller checks it against the
    /// spec (name, routes, endpoints) before activating the generation.
    pub info: wafer_block::BlockInfo,
}

/// A structured refusal. Surfaced to the agent as a diagnostic in the tool
/// result, never as a transport error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidationFailure {
    /// The stage that failed.
    pub stage: ValidationStage,
    /// Operator/agent-facing explanation.
    pub message: String,
}

impl ValidationFailure {
    /// Build a failure for `stage` with `message`.
    pub fn new(stage: ValidationStage, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValidationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.stage.as_str(), self.message)
    }
}

/// The host's half of activation.
///
/// `MaybeSend + MaybeSync` (rather than a hard `Send + Sync`) is what lets the
/// browser implementation hold the live `Rc<Wafer>` it is required to hold:
/// the bound is `Send + Sync` on native and unbounded on wasm32, exactly like
/// [`crate::blocks::llm::provider_admin::ProviderAdmin`] and
/// [`crate::FeatureConfig`].
#[wafer_block::wafer_async_trait]
pub trait RuntimeControl: wafer_run::MaybeSend + wafer_run::MaybeSync {
    /// Load the artifact under its declared capabilities and limits, parse
    /// `BlockInfo`, run Init/Start and one probe request. Static rules are
    /// checked by the caller first; this is the executable half.
    async fn validate(
        &self,
        spec: &DynamicBlockSpec,
        artifact: &[u8],
    ) -> Result<ValidatedGuest, ValidationFailure>;

    /// Rebuild the runtime with exactly this block set and swap it in.
    async fn rebuild(&self, blocks: &[DynamicBlockSpec]) -> Result<(), String>;

    /// Monotonic runtime generation counter (bumped by every successful
    /// rebuild). The `/b/dev` page re-registers its agent tools whenever this
    /// changes.
    fn runtime_generation(&self) -> u64;
}
