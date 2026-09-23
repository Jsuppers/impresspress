//! `impresspress/signal`: the WebRTC signalling relay. Every `GET` row
//! publishes a response schema, so none is a page; the fixture is the context
//! those rows are dispatched in, and has no page to render.

use std::sync::Arc;

use wafer_run::{Block, Message};

use super::Entry;
use crate::{
    blocks::signal::SignalBlock,
    test_support::{
        anon_msg,
        htmx::{Fixture, Site},
        TestContext,
    },
};

pub(super) fn entry() -> Entry {
    Entry {
        block: "impresspress/signal",
        fixture: Some(fixture),
        exempt: &[],
        must_fire: &[],
    }
}

/// Every row is public: a visitor with no session.
fn caller(action: &str, path: &str) -> Message {
    anon_msg(action, path)
}

fn fixture() -> std::pin::Pin<Box<dyn std::future::Future<Output = Fixture>>> {
    Box::pin(async {
        Fixture {
            ctx: Arc::new(TestContext::with_signal().await),
            site: Site(vec![Arc::new(SignalBlock::new()) as Arc<dyn Block>]),
            caller,
            pages: Vec::new(),
            operator_input: &[],
        }
    })
}
