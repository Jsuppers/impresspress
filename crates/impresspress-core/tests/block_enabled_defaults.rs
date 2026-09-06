//! The guard for spec 2.1.4: switching the block-enablement seed from the
//! hand-maintained `features::ENABLED_DEFAULTS` table to the blocks' own
//! `BlockInfo` declarations must not change what any existing deployment ends
//! up with.
//!
//! Why this needs a test at all: `plan_seed_decisions` re-seeds every row whose
//! `seed_defaults_hash` is a *stale* `seed:<hex>` (the #222 hash gate). A row
//! written at `seed_hash_for(true)` becomes stale the moment the derived
//! default for that block is `false`, and the next cold start rewrites it to
//! `false`. So a divergence between the constant and a `.default_enabled(..)`
//! declaration is not a cosmetic inconsistency — it is a silent, deployment-wide
//! flip of a feature flag.
//!
//! The three tests below cover the three ways the two sets can differ:
//! same name in both (values must agree), constant-only, derived-only.

use std::collections::BTreeMap;

use impresspress_core::blocks::all_block_infos;

/// The derived set, read from the function the seed callers use.
fn derived_defaults() -> BTreeMap<String, bool> {
    impresspress_core::blocks::block_enabled_defaults()
        .into_iter()
        .collect()
}

/// Every block name the registry knows about, whether or not it is disableable.
fn registered_names() -> BTreeMap<String, bool> {
    all_block_infos()
        .iter()
        .map(|i| (i.name.clone(), i.can_disable))
        .collect()
}

/// A block present in both sets must be seeded to the same value by both, or
/// the switch flips it on every deployment that has not been admin-edited.
#[test]
fn derived_defaults_match_todays_constant() {
    let derived = derived_defaults();
    let mut mismatches = Vec::new();

    for (name, constant_value) in impresspress_core::features::ENABLED_DEFAULTS {
        if let Some(derived_value) = derived.get(*name) {
            if derived_value != constant_value {
                mismatches.push(format!(
                    "  {name}: ENABLED_DEFAULTS says {constant_value}, \
                     BlockInfo::default_enabled says {derived_value}"
                ));
            }
        }
    }

    assert!(
        mismatches.is_empty(),
        "every block in both the constant and the derived set must agree, \
         otherwise switching the seed source re-seeds live rows:\n{}",
        mismatches.join("\n"),
    );
}

/// A name the constant seeds but the derivation drops must be safe to drop:
/// it is either not a registered block at all (`wafer-run/auth`, registered
/// through `register_auth` and absent from `all_block_infos()`) or it declares
/// `can_disable == false` (`impresspress/system`, `impresspress/admin`), and in
/// either case the constant seeded it `true` — which is exactly what
/// `BlockSettings::is_block_enabled` reports for an absent row.
#[test]
fn constant_only_entries_are_not_can_disable_and_default_true() {
    let derived = derived_defaults();
    let registered = registered_names();

    for (name, constant_value) in impresspress_core::features::ENABLED_DEFAULTS {
        if derived.contains_key(*name) {
            continue;
        }
        match registered.get(*name) {
            None => { /* not a registered block; the seed row was dead weight */ }
            Some(can_disable) => assert!(
                !can_disable,
                "{name} is registered and disableable but the derivation dropped it — \
                 that would be a real behaviour change, not a fallback"
            ),
        }
        assert!(
            *constant_value,
            "{name} loses its seed row, so it falls back to enabled; the constant \
             seeded it {constant_value}, so dropping the row changes behaviour"
        );
    }
}

/// A name the derivation adds that the constant never seeded must be adding a
/// row at `true` — the same value `is_block_enabled` already reported for its
/// absent row. `impresspress/llm` and `impresspress/vector` are the two: the
/// constant excluded them with a comment about a trait refactor that has since
/// landed (`blocks/mod.rs`).
#[test]
fn derived_only_entries_default_true() {
    let constant: BTreeMap<&str, bool> = impresspress_core::features::ENABLED_DEFAULTS
        .iter()
        .map(|(name, value)| (*name, *value))
        .collect();

    for (name, derived_value) in derived_defaults() {
        if constant.contains_key(name.as_str()) {
            continue;
        }
        assert!(
            derived_value,
            "{name} gains a seed row it never had; only `true` matches the \
             absent-row fallback it has been running on"
        );
    }
}
