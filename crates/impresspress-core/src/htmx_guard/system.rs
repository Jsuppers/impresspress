//! `impresspress/system`: the health probe and the shared static assets.

use super::{Entry, ASSET};

pub(super) fn entry() -> Entry {
    Entry {
        block: "impresspress/system",
        fixture: None,
        exempt: &[
            (
                "/health",
                "liveness probe answered in plain text; renders no page",
            ),
            ("/b/static/{filename}", ASSET),
        ],
        must_fire: &[],
    }
}
