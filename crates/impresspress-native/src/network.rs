//! Network platform-service factory for native targets.

use std::sync::Arc;

use wafer_core::interfaces::network::service::{NetworkError, NetworkService};

/// Construct an HTTP network service backed by `reqwest`.
///
/// The response-size cap and the connect, read and total-request timeouts
/// (`WAFER_RUN__NETWORK__MAX_RESPONSE_BYTES`, `…__CONNECT_TIMEOUT_SECS`,
/// `…__READ_TIMEOUT_SECS`, `…__REQUEST_TIMEOUT_SECS`) are read from the
/// environment once here; an invalid value is a boot error rather than a
/// silently-applied default.
pub fn make_fetch_network_service() -> Result<Arc<dyn NetworkService>, NetworkError> {
    Ok(Arc::new(
        wafer_block_network::service::HttpNetworkService::from_env()?,
    ))
}
