//! `impresspress/dev`: the dev sandbox page and its JSON workspace API.

use std::sync::Arc;

use wafer_run::{Block, Message};

use super::{Entry, Exempt};
use crate::{
    blocks::dev::{test_support::FakeControl, DevBlock},
    test_support::{
        admin_msg,
        htmx::{Fixture, Page, Site},
        TestContext,
    },
};

pub(super) fn entry() -> Entry {
    Entry {
        block: "impresspress/dev",
        fixture: Some(fixture),
        exempt: &[
            ("/b/dev/static/dev.js", Exempt::Asset),
            ("/b/dev/static/dev.css", Exempt::Asset),
            ("/b/dev/static/compiler-adapter.js", Exempt::Asset),
            ("/b/dev/api/tools.json", Exempt::JsonApi),
            (
                "/b/dev/api/export",
                Exempt::NotAPage("a site-export archive download"),
            ),
        ],
        must_fire: &[],
    }
}

fn caller(action: &str, path: &str) -> Message {
    admin_msg(action, path)
}

fn fixture() -> std::pin::Pin<Box<dyn std::future::Future<Output = Fixture>>> {
    Box::pin(async {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let block = Arc::new(DevBlock::with_workspace(ctx.dev_shared()));
        Fixture {
            ctx: Arc::new(ctx),
            site: Site(vec![block as Arc<dyn Block>]),
            caller,
            pages: vec![Page::at("/b/dev")],
            operator_input: &[],
        }
    })
}
