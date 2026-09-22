//! `impresspress/signal`: the WebRTC signalling relay. Every `GET` row
//! publishes a response schema, so none is a page.

use super::Entry;

pub(super) fn entry() -> Entry {
    Entry {
        block: "impresspress/signal",
        fixture: None,
        exempt: &[],
        must_fire: &[],
    }
}
