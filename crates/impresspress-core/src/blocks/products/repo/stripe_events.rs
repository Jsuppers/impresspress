//! Table declaration for `impresspress__products__stripe_events`.
//!
//! Row-level access (claim, retry accounting, replay) lives entirely in
//! `stripe.rs` — the webhook-processing pipeline that is this table's only
//! reader/writer, and predates the `repo`-module-owns-its-`TABLE` convention
//! the rest of this directory follows. This module exists solely to own the
//! name per that convention, so a caller outside `stripe.rs` — the data
//! snapshot's export allowlist, specifically — can name the table without
//! retyping the literal string a second time.
pub(crate) const TABLE: &str = "impresspress__products__stripe_events";
