//! App-specific config loading (schema-aware, depends on impresspress-core).
//!
//! `filter_to_declared_keys` sits between the library's raw-env-var
//! collection and the SQLite-backed variable seeding, preserving the
//! prior behavior of only persisting env vars that match a declared
//! block/shared config var key.

use std::collections::HashMap;

pub fn filter_to_declared_keys(env_vars: HashMap<String, String>) -> Vec<(String, String)> {
    let block_infos = impresspress_core::blocks::all_block_infos();
    let all_vars = impresspress_core::config_vars::collect_all_config_vars(&block_infos);
    // Borrow the declared keys directly — the HashSet only lives for the
    // duration of the filter, so there's no need to allocate owned Strings.
    let known: std::collections::HashSet<&str> = all_vars.iter().map(|v| v.key.as_str()).collect();
    env_vars
        .into_iter()
        .filter(|(k, _)| known.contains(k.as_str()))
        .collect()
}

// Block-settings loading + the #222 hash-gate seed are handled by the shared
// `impresspress_core::platform_state::block_settings::load_and_seed`, and admin-created
// WRAP grants by `impresspress_core::boot::load_wrap_grants_from_db`, both over
// the platform `DatabaseService` (see `server.rs::build_native_runtime`). The
// previous native-only readers opened `IMPRESSPRESS_DB_PATH` as a SQLite file
// directly, which no other target does and which finds nothing on Postgres.
