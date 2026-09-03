//! The service worker's seed bypass prefix and the dev block's seed URL root
//! are one value, spelled in two crates.
//!
//! `impresspress-bundle` is native bundling tooling that deliberately depends
//! on no impresspress crate (the wasm32 runtime never compiles it), so it
//! restates the prefix rather than importing it. This crate depends on both
//! and is therefore the only place the two spellings can be compared — which
//! is what turns "restated" into something other than "free to drift".
//!
//! A drift here is silent and total: the service worker would intercept
//! `/seed/…`, answer it from the published site, and every fresh instance
//! would boot with no seed and no error.

#[test]
fn the_bundler_bypasses_exactly_the_prefix_the_seed_importer_fetches_from() {
    assert_eq!(
        impresspress_bundle::bundle::SEED_BYPASS_PREFIX,
        impresspress_core::blocks::dev::seed::ROOT,
    );
    // And the manifest the service worker probes is under it, so bypassing the
    // prefix is enough to reach the whole bundle.
    assert!(impresspress_core::blocks::dev::seed::MANIFEST_URL
        .starts_with(impresspress_bundle::bundle::SEED_BYPASS_PREFIX));
}
