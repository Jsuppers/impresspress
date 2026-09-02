//! Isolate-local "config tables were written" counter.
//!
//! Exists because a runtime build both READS config and WRITES it, in that
//! order, within one pass. `builder::boot` and `strict_init_all_blocks`
//! initialize the admin block first; admin's `Init` runs its migrations and
//! then `settings::seed_defaults`, and the Cloudflare boot hook follows with
//! `boot::seed_auto_generated`. Every one of those inserts rows into the
//! variables table AFTER some other block has already resolved its config —
//! the database service block, for one, is lazily initialized by admin's own
//! seeding query and declares a config key of its own.
//!
//! A config reader that caches what it read therefore cannot cache it for its
//! own lifetime: a value seeded halfway through the pass would stay invisible
//! to every block initialized after the cache was filled. For a key that is
//! required and has no default that is not a stale read, it is a permanent
//! `InitError` cached for the block slot's lifetime — the impresspress #209
//! regression class the `BootHooks` ordering exists to prevent.
//!
//! So writers bump this counter and readers compare it against the value they
//! captured. A build that seeds nothing (the overwhelmingly common case on an
//! established database) never bumps it and never re-reads.
//!
//! `Cell<u64>`, never a `RefCell`: Cloudflare can hard-stop a request without
//! running destructors, and a stranded borrow flag wedges the isolate for the
//! rest of its life (see `impresspress_core::isolate_cell`).

use std::cell::Cell;

thread_local! {
    /// Bumped on every local write to a table whose contents a runtime bakes
    /// in at build/init time. Monotonic within an isolate; the absolute value
    /// is meaningless, only changes are.
    static CONFIG_WRITE_GENERATION: Cell<u64> = const { Cell::new(0) };
}

/// Record that this isolate just wrote to a config table.
///
/// Deliberately NOT gated on whether the KV config-version stamp was also
/// bumped: the deploy-init funnel suppresses that stamp (one explicit bump
/// after ~19 sequential same-key puts) and the seeding it performs is exactly
/// the case this counter has to catch.
pub fn note_config_write() {
    CONFIG_WRITE_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
}

/// The current generation. A reader that cached config alongside a previous
/// value must re-read when this differs.
pub fn config_write_generation() -> u64 {
    CONFIG_WRITE_GENERATION.with(Cell::get)
}

/// Whether a write to `table` can change what the config snapshot holds.
///
/// Narrower than [`crate::cache_key::bumps_config_version`] on purpose. That
/// predicate covers every table a runtime bakes in — variables,
/// block_settings and wrap_grants — because all three must move the KV
/// version stamp. The snapshot is built from the VARIABLES table alone, so
/// bumping it for the other two would discard a perfectly good snapshot:
/// every block writes its migration state to `block_settings` during its own
/// `Init` (`migration_helper::write_state`), so on a first boot after a code
/// change that is one discarded snapshot and one re-query per block — the
/// per-block read amplification this whole mechanism exists to remove,
/// reintroduced in the pass that can least afford it.
pub fn writes_invalidate_config_snapshot(table: &str) -> bool {
    table == crate::blocks::admin::VARIABLES_TABLE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_changes_the_generation_a_read_captured() {
        let before = config_write_generation();
        assert_eq!(
            config_write_generation(),
            before,
            "reading must not itself advance the generation"
        );

        note_config_write();
        assert_ne!(
            config_write_generation(),
            before,
            "a config write must invalidate what a reader captured earlier"
        );

        let after_one = config_write_generation();
        note_config_write();
        assert_ne!(config_write_generation(), after_one);
    }

    /// Only the table the snapshot is BUILT from may invalidate it.
    ///
    /// `block_settings` is the one that matters: every block writes its
    /// migration state there during `Init`, so treating those writes as
    /// snapshot-invalidating costs one full re-query per block on a first
    /// boot — exactly the amplification the snapshot removes.
    #[test]
    fn only_variables_writes_invalidate_the_snapshot() {
        assert!(writes_invalidate_config_snapshot(
            crate::blocks::admin::VARIABLES_TABLE
        ));
        assert!(!writes_invalidate_config_snapshot(
            crate::blocks::admin::BLOCK_SETTINGS_TABLE
        ));
        assert!(!writes_invalidate_config_snapshot("unrelated_table"));

        // Every table this returns true for must also bump the KV version
        // stamp, or a runtime could rebuild from config no isolate is told
        // to re-read.
        assert!(crate::cache_key::bumps_config_version(
            crate::blocks::admin::VARIABLES_TABLE
        ));
    }
}
