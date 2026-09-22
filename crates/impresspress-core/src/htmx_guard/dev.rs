//! `impresspress/dev`: the dev sandbox page and its JSON workspace API.

use std::sync::Arc;

use wafer_run::{Block, Message};

use super::{Entry, ASSET, JSON_API};
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
            ("/b/dev/static/dev.js", ASSET),
            ("/b/dev/static/dev.css", ASSET),
            ("/b/dev/static/compiler-adapter.js", ASSET),
            ("/b/dev/api/tools.json", JSON_API),
            (
                "/b/dev/api/export",
                "a site-export archive download; renders no page",
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
