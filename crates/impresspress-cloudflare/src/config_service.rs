use std::collections::HashMap;

use wafer_core::interfaces::config::service::ConfigService;

/// ConfigService backed by a pre-loaded HashMap (from D1 variables table).
/// Read-only in practice — `set()` is a no-op since CF workers are stateless.
pub struct HashMapConfigService {
    vars: HashMap<String, String>,
}

// No `unsafe impl Send/Sync`: a `HashMap<String, String>` is already both, so
// the compiler derives them. See the note in `logger_service`.

impl HashMapConfigService {
    pub fn new(vars: HashMap<String, String>) -> Self {
        Self { vars }
    }
}

impl ConfigService for HashMapConfigService {
    fn get(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }

    /// Deliberately a no-op, and deliberately silent about it.
    ///
    /// A Worker's config is loaded per request from D1; there is no isolate
    /// state a `set` could usefully write to, and the durable copy is written
    /// through the admin block's variables repo, not through this trait.
    ///
    /// It cannot report the refusal either: `ConfigService::set` returns `()`
    /// upstream (`wafer-core`'s `interfaces/config/service.rs`), so making it a
    /// `Result` is a producer change that lands on every other consumer of that
    /// library for one adapter's benefit. Recorded rather than done — see spec
    /// 2.10. Logging on every call was considered and rejected: nothing in the
    /// tree calls `set` on this service, so the line would be noise waiting for
    /// a caller that does not exist.
    fn set(&self, _key: &str, _value: &str) {}
}

/// CFG-01 reproduction for the Cloudflare target.
#[cfg(test)]
mod config_store_reproduction {
    use std::collections::HashMap;

    use wafer_core::interfaces::config::service::ConfigService;
    use wasm_bindgen_test::*;

    /// A config write on Cloudflare must not be silently dropped.
    ///
    /// `ui/settings_form.rs::save_settings` — the write path behind five
    /// admin forms (products, legalpages, userportal, email, auth-ui) —
    /// calls `config::set` and reports `200 Settings saved` whenever that
    /// call returns `Ok`. `ConfigService::set` returns `()` upstream, so the
    /// Ok is unconditional: on Workers the value is dropped and the admin is
    /// told it was saved.
    ///
    /// The `set` no-op below documents its own silence as safe because
    /// "nothing in the tree calls `set` on this service". That premise is
    /// what makes this a defect rather than a design choice — `save_settings`
    /// is exactly such a caller, and has been since those forms shipped.
    ///
    /// Asserted against `make_config_service`, the constructor the runtime
    /// calls, rather than `HashMapConfigService` itself: the config-store
    /// decision replaces that type with a variables-table-backed service, and
    /// this test should follow the replacement instead of pinning a type that
    /// is scheduled for deletion.
    #[wasm_bindgen_test]
    fn a_config_write_is_not_silently_dropped() {
        const KEY: &str = "WAFER_RUN_SHARED__PRIMARY_COLOR";

        let svc = crate::services::make_config_service(HashMap::new());
        svc.set(KEY, "#ff0000");

        assert_eq!(
            svc.get(KEY).as_deref(),
            Some("#ff0000"),
            "a config value written through the service the Worker runtime \
             builds must be readable back; dropping it is what makes the \
             admin settings forms answer 200 while changing nothing"
        );
    }
}
