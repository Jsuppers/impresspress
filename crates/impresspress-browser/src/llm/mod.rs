//! Browser LLM — `LlmService` impl driving WebLLM's MLCEngine via a
//! SW↔page postMessage bridge. `catalog` carries the model list, `bridge` the
//! postMessage protocol, and `service` wires them together behind
//! `wafer_core::interfaces::llm::service::LlmService`.

#[cfg(target_arch = "wasm32")]
pub mod bridge;
pub mod catalog;
#[cfg(target_arch = "wasm32")]
pub mod service;

pub use catalog::{default_catalog, ModelCatalog};
#[cfg(target_arch = "wasm32")]
pub use service::BrowserLlmService;
