//! The htmx guard, over every block this build registers.
//!
//! [`crate::test_support::htmx`] fires every mutating htmx control a page
//! emits and holds the answer to what htmx does with it. That is only as good
//! as the set of pages it is pointed at, so this module decides the set, and
//! refuses to let it be incomplete:
//!
//! - every block in the build has an [`Entry`] — a block this module does not
//!   know is a failure, not out of scope;
//! - every `GET` row a block declares is one of its pages, publishes a
//!   response schema (so it is JSON, not a page), or is exempt with a stated
//!   reason — a new page or tab cannot escape the guard by not being listed;
//! - every page of every entry is rendered, and every control on it fired.

mod auth_ui;
#[cfg(feature = "block-dev")]
mod dev;
mod email;
#[cfg(feature = "block-files")]
mod files;
#[cfg(feature = "block-legalpages")]
mod legalpages;
#[cfg(feature = "block-llm")]
mod llm;
#[cfg(feature = "block-messages")]
mod messages;
#[cfg(feature = "block-products")]
mod products;
#[cfg(feature = "block-signal")]
mod signal;
mod system;
#[cfg(feature = "block-tickets")]
mod tickets;
#[cfg(feature = "block-userportal")]
mod userportal;
#[cfg(feature = "block-vector")]
mod vector;

use std::collections::BTreeSet;

use wafer_run::{BlockInfo, HttpMethod};

use crate::{
    endpoint_match::match_template,
    test_support::htmx::{fire_every_control, MakeFixture},
};

/// A JSON endpoint that declares no response schema. No htmx control reads it
/// and it renders no page, so there is nothing on it for the guard to fire.
const JSON_API: &str = "JSON API without a declared schema; renders no page";

/// A `GET` answered with a redirect to a page the guard does render.
const REDIRECT: &str = "redirects to a page the guard renders";

/// A static asset (script, stylesheet), not a page.
const ASSET: &str = "static asset, not a page";

/// How the guard covers one block.
struct Entry {
    /// The block's `BlockInfo::name`.
    block: &'static str,
    /// Its pages and the context they render in. `None` for a block that
    /// serves no HTML page at all; then every `GET` row must be exempt or
    /// schema'd, and the completeness check below says so.
    fixture: Option<MakeFixture>,
    /// `GET` rows that are neither a page nor schema'd JSON, each with why.
    exempt: &'static [(&'static str, &'static str)],
    /// `"{action} {template}"` of the controls its pages must keep reaching.
    must_fire: &'static [&'static str],
}

#[expect(
    clippy::vec_init_then_push,
    reason = "each push is `#[cfg]`-gated like the block manifest entry it covers, which a \
              `vec![..]` literal cannot express"
)]
fn entries() -> Vec<Entry> {
    let mut entries = Vec::new();
    entries.push(Entry {
        block: "impresspress/admin",
        fixture: Some(crate::blocks::admin::htmx_contract_tests::fixture),
        exempt: &[
            ("/b/admin/api/users/{id}", JSON_API),
            ("/b/admin/api/database/info", JSON_API),
            ("/b/admin/api/database/tables", JSON_API),
            ("/b/admin/api/database/tables/{name}/columns", JSON_API),
            ("/b/admin/api/iam/permissions", JSON_API),
            ("/b/admin/api/iam/user-roles", JSON_API),
            ("/b/admin/api/settings/all", JSON_API),
            ("/b/admin/api/settings/{key}", JSON_API),
            ("/b/admin/settings/", REDIRECT),
            ("/b/admin/variables", REDIRECT),
            ("/b/admin/network", REDIRECT),
            ("/b/admin/email", REDIRECT),
            ("/b/admin/permissions", REDIRECT),
        ],
        must_fire: crate::blocks::admin::htmx_contract_tests::MUST_FIRE,
    });
    entries.push(auth_ui::entry());
    entries.push(email::entry());
    entries.push(system::entry());
    #[cfg(feature = "block-dev")]
    entries.push(dev::entry());
    #[cfg(feature = "block-files")]
    entries.push(files::entry());
    #[cfg(feature = "block-legalpages")]
    entries.push(legalpages::entry());
    #[cfg(feature = "block-llm")]
    entries.push(llm::entry());
    #[cfg(feature = "block-messages")]
    entries.push(messages::entry());
    #[cfg(feature = "block-products")]
    entries.push(products::entry());
    #[cfg(feature = "block-signal")]
    entries.push(signal::entry());
    #[cfg(feature = "block-tickets")]
    entries.push(tickets::entry());
    #[cfg(feature = "block-userportal")]
    entries.push(userportal::entry());
    #[cfg(feature = "block-vector")]
    entries.push(vector::entry());
    entries
}

/// The blocks this build registers, by `info()`: the feature-block manifest
/// plus the dev block, which the builder registers on its own.
fn registered() -> Vec<BlockInfo> {
    #[cfg_attr(
        not(feature = "block-dev"),
        expect(
            unused_mut,
            reason = "only the block-dev build adds to the manifest's set"
        )
    )]
    let mut infos = crate::blocks::all_block_infos();
    #[cfg(feature = "block-dev")]
    infos.push(wafer_run::Block::info(
        &crate::blocks::dev::DevBlock::with_workspace(crate::blocks::dev::DevShared::new(
            crate::blocks::dev::test_support::FakeControl::new(),
            std::sync::Arc::new(crate::blocks::dev::test_support::FakeShell::new()),
        )),
    ));
    infos
}

#[test]
fn every_registered_block_has_an_entry() {
    let entries = entries();
    let known: BTreeSet<&str> = entries.iter().map(|e| e.block).collect();
    let registered: Vec<String> = registered().into_iter().map(|i| i.name).collect();
    let unknown: Vec<&String> = registered
        .iter()
        .filter(|name| !known.contains(name.as_str()))
        .collect();
    assert!(
        unknown.is_empty(),
        "blocks with no htmx guard entry: {unknown:?} — give each one an `Entry`"
    );
    let dead: Vec<&&str> = known
        .iter()
        .filter(|name| !registered.iter().any(|r| r == **name))
        .collect();
    assert!(
        dead.is_empty(),
        "entries for blocks this build does not register: {dead:?}"
    );
}

#[tokio::test]
async fn every_get_row_is_a_page_or_json_or_exempt() {
    let infos = registered();
    let mut gaps: Vec<String> = Vec::new();
    for entry in entries() {
        let Some(info) = infos.iter().find(|i| i.name == entry.block) else {
            continue;
        };
        let pages: Vec<String> = match entry.fixture {
            Some(make) => make().await.pages.into_iter().map(|p| p.path).collect(),
            None => Vec::new(),
        };
        let gets: Vec<_> = info
            .endpoints
            .iter()
            .filter(|e| e.method == HttpMethod::Get)
            .collect();
        for row in &gets {
            let is_page = pages.iter().any(|path| {
                match_template(&row.path, path).is_some()
                    || (!path.ends_with('/')
                        && match_template(&row.path, &format!("{path}/")).is_some())
            });
            let exempt = entry.exempt.iter().any(|(t, _)| *t == row.path);
            if row.output_schema.is_some() {
                if exempt {
                    gaps.push(format!(
                        "{}: GET {} publishes a schema; its exemption is redundant",
                        entry.block, row.path
                    ));
                }
                continue;
            }
            if is_page && exempt {
                gaps.push(format!(
                    "{}: GET {} is both a page and exempt",
                    entry.block, row.path
                ));
            }
            if !is_page && !exempt {
                gaps.push(format!(
                    "{}: GET {} is neither a page the guard renders, schema'd JSON, nor exempt",
                    entry.block, row.path
                ));
            }
        }
        for (template, _) in entry.exempt {
            if !gets.iter().any(|row| row.path == *template) {
                gaps.push(format!(
                    "{}: exempt {template} is not a GET row of the block",
                    entry.block
                ));
            }
        }
    }
    assert!(gaps.is_empty(), "{}", gaps.join("\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_htmx_control_on_every_page_is_answered_the_way_it_swaps() {
    for entry in entries() {
        let Some(make) = entry.fixture else {
            continue;
        };
        let fired = fire_every_control(make).await;
        for expected in entry.must_fire {
            assert!(
                fired.contains(*expected),
                "{}: no page fired {expected}; fired: {fired:#?}",
                entry.block
            );
        }
    }
}
