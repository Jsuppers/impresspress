//! Row-level data access for the `impresspress/files` block.
//!
//! Mirrors `blocks/auth/repo/`: each submodule owns its table — the
//! `TABLE` const, the row type, its one `from_record`, and every `db::*`
//! statement that touches it live here, so the handler/page modules above
//! never issue database calls and never decode a column. Functions are thin
//! typed wrappers around the pre-existing queries (same filters, same
//! values) and surface the db client's `WaferError` unchanged, so call-site
//! error handling (NotFound matching, warn-and-default, `err_internal`)
//! keeps its exact previous behavior.
//!
//! One decode per column is the point, not a tidiness preference. `public`
//! on the buckets table is `INTEGER` on SQLite and `BOOLEAN` on Postgres and
//! is written as a JSON bool by every writer, so it reads back as three
//! different JSON shapes across the backends. When two pages each decoded it
//! their own way, the same bucket was public on one and private on the other
//! (B13). [`buckets::BucketRow::from_record`] decodes it once, through
//! `RecordExt::bool_field`, which is the accessor that accepts all three.
//!
//! Submodule → table map:
//! - [`buckets`] — `impresspress__files__buckets`
//! - [`objects`] — `impresspress__files__objects`
//! - [`views`] — `impresspress__files__views`
//! - [`shares`] — `impresspress__files__cloud_shares` +
//!   `impresspress__files__cloud_access_logs` (the access log is a child
//!   audit table of shares; one submodule owns both)
//! - [`quota`] — `impresspress__files__cloud_quotas`

use wafer_core::clients::database::{Record, RecordList};

pub mod buckets;
pub mod objects;
pub mod quota;
pub mod shares;
pub mod views;

/// One page of typed rows plus the total number of rows matching the query
/// across all pages — the block's single return shape for a listing.
///
/// It replaces `RecordList` at the repo boundary. `RecordList` also carries
/// `page`/`page_size`, which every caller here already knows because it
/// computed the offset it passed in, so the page keeps only what a caller
/// cannot derive: the rows and the total.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Page<T> {
    /// The rows on this page, in the query's sort order.
    pub rows: Vec<T>,
    /// Total rows matching the query across all pages.
    pub total: i64,
}

impl<T> Page<T> {
    /// Decode a `RecordList` into a page of rows with the table's own
    /// `from_record`. The only place a listing crosses from records to rows.
    pub(super) fn decode(list: RecordList, from_record: fn(&Record) -> T) -> Self {
        Self {
            rows: list.records.iter().map(from_record).collect(),
            total: list.total_count,
        }
    }
}
