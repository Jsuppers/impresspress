//! Request bodies for the `/b/messages/api/*` JSON surface.
//!
//! [`CreateContextRequest`] / [`AddEntryRequest`] promote the private,
//! in-function `Body` structs `rest::create_context` / `rest::add_entry`
//! already deserialized into — they were already real deserialize targets,
//! just not named or reachable from `mod.rs`, so the schema next to the
//! endpoint declaration had to be hand-written independently. Same field
//! list, same `#[serde(...)]` attributes, so `.input::<T>()` describes
//! exactly what these handlers already accepted.
//!
//! [`UpdateContextRequest`] is the one genuine "write the struct" fix:
//! `rest::update_context` used to deserialize the body into
//! `HashMap<String, serde_json::Value>` and `service::update_context` then
//! walked a `let allowed = ["status", "title", "metadata"]` array to pick
//! out the only three keys it would ever apply. The struct below replaces
//! both the ad hoc map and the string-keyed whitelist — there is no fourth
//! field left to whitelist against, the type says what used to be an
//! enforced-at-runtime convention. Any other key in the request body is
//! still silently ignored (serde's default behaviour for an unrecognized
//! field, same as the whitelist loop's `.get(*key)` was), so this is
//! behaviour-preserving.
//!
//! `list_contexts` / `list_entries` response schemas and the `GetContext`
//! path params stay hand-written in `mod.rs` — see the comments at those
//! call sites for why.

use serde::{Deserialize, Serialize};

/// `POST /b/messages/api/contexts` request body.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateContextRequest {
    /// Context type: conversation, task, notification, etc.
    #[serde(rename = "type")]
    pub context_type: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub sender_id: String,
    #[serde(default)]
    pub recipient_id: String,
    /// Parent context ID for sub-tasks/threads.
    pub parent_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// `PATCH /b/messages/api/contexts/{id}` request body. Every field is
/// optional and only the ones present are applied — see the module doc for
/// why this replaced a `HashMap<String, Value>` plus a runtime whitelist.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UpdateContextRequest {
    pub status: Option<String>,
    pub title: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// `POST /b/messages/api/contexts/{id}/entries` request body.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AddEntryRequest {
    /// Entry kind: message, artifact, notification, status.
    #[serde(default = "default_entry_kind")]
    pub kind: String,
    /// Sender role: user, agent, system.
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub sender_id: String,
    #[serde(default)]
    pub content: String,
    /// MIME type of `content`. Defaults to `text/plain` when omitted or
    /// explicitly `null`.
    // The explicit `#[serde(default = ..)]` (rather than relying on
    // `Option<T>`'s automatic missing-key handling, as `metadata` below
    // does) exists purely so schemars can surface a `default` in the schema
    // — matching `service::add_entry`'s real `content_type.unwrap_or(..)`
    // fallback, which runs identically whether this field ends up `None`
    // (key missing) or `Some("text/plain")` (key missing, default applied).
    #[serde(default = "default_content_type")]
    pub content_type: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

fn default_entry_kind() -> String {
    "message".to_string()
}

fn default_content_type() -> Option<String> {
    Some("text/plain".to_string())
}
