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
//! - every `GET` row that is not a page is dispatched once, and must not
//!   answer a page: a schema or an exemption is a claim about the answer, and
//!   the answer is what is checked, not the claim;
//! - every page of every entry, and every tab and view those pages link to,
//!   is rendered, and every control on it fired.

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
    test_support::htmx::{crawl, fire_every_control, send, MakeFixture},
};

/// Why a `GET` row that is not a page publishes no response schema. Each is
/// checked against the row's answer, not taken on trust.
#[derive(Debug, Clone, Copy)]
enum Exempt {
    /// A JSON endpoint that declares no response schema. No htmx control
    /// reads it and it renders no page, so there is nothing on it for the
    /// guard to fire. Its answer must not be HTML.
    JsonApi,
    /// Answered with a redirect to a page the guard renders.
    Redirect,
    /// A static asset (script, stylesheet), not a page. Its answer must not
    /// be HTML.
    Asset,
    /// Some other answer that is not a page — a download, a plain-text probe —
    /// with what it is. Its answer must not be HTML.
    NotAPage(&'static str),
}

impl Exempt {
    /// What the exemption says the row answers.
    fn reason(self) -> &'static str {
        match self {
            Exempt::JsonApi => "JSON API without a declared schema",
            Exempt::Redirect => "redirect to a page the guard renders",
            Exempt::Asset => "static asset",
            Exempt::NotAPage(what) => what,
        }
    }
}

/// What every path parameter of a `GET` row is filled with to dispatch it:
/// a value no fixture seeds, so a row with parameters answers its not-found
/// path. The row's answer is checked for what it is (HTML or not), which a
/// not-found answers the same way as a found one.
const PROBE_PARAM: &str = "htmx-guard-probe";

/// How the guard covers one block.
struct Entry {
    /// The block's `BlockInfo::name`.
    block: &'static str,
    /// Its pages and the context they render in, and the context its `GET`
    /// rows are dispatched in. `None` only for a block that declares no `GET`
    /// row at all, and the completeness check below says so.
    fixture: Option<MakeFixture>,
    /// `GET` rows that are neither a page nor schema'd JSON, each with why.
    exempt: &'static [(&'static str, Exempt)],
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
            ("/b/admin/api/users/{id}", Exempt::JsonApi),
            ("/b/admin/api/database/info", Exempt::JsonApi),
            ("/b/admin/api/database/tables", Exempt::JsonApi),
            (
                "/b/admin/api/database/tables/{name}/columns",
                Exempt::JsonApi,
            ),
            ("/b/admin/api/iam/permissions", Exempt::JsonApi),
            ("/b/admin/api/iam/user-roles", Exempt::JsonApi),
            ("/b/admin/api/settings/all", Exempt::JsonApi),
            ("/b/admin/api/settings/{key}", Exempt::JsonApi),
            ("/b/admin/settings/", Exempt::Redirect),
            ("/b/admin/variables", Exempt::Redirect),
            ("/b/admin/network", Exempt::Redirect),
            ("/b/admin/email", Exempt::Redirect),
            ("/b/admin/permissions", Exempt::Redirect),
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
        if entry.fixture.is_none() && !gets.is_empty() {
            gaps.push(format!(
                "{}: declares GET rows but has no fixture to dispatch them in",
                entry.block
            ));
        }
        for row in &gets {
            let is_page = pages.iter().any(|path| is_row_of(&row.path, path));
            let exempt = entry.exempt.iter().any(|(t, _)| *t == row.path);
            if row.output_schema.is_some() {
                if exempt {
                    gaps.push(format!(
                        "{}: GET {} publishes a schema; its exemption is redundant",
                        entry.block, row.path
                    ));
                }
                if is_page {
                    gaps.push(format!(
                        "{}: GET {} publishes a schema and is a page the guard renders",
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

/// Whether the page at `path` is served by the row `template`.
fn is_row_of(template: &str, path: &str) -> bool {
    match_template(template, path).is_some()
        || (!path.ends_with('/') && match_template(template, &format!("{path}/")).is_some())
}

/// `template` with every `{param}` / `{param...}` filled with [`PROBE_PARAM`].
fn probe_path(template: &str) -> String {
    let mut path = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let close = open
            + rest[open..]
                .find('}')
                .expect("a template parameter is closed");
        path.push_str(&rest[..open]);
        path.push_str(PROBE_PARAM);
        rest = &rest[close + 1..];
    }
    path.push_str(rest);
    path
}

/// Every `GET` row that is not a page — schema'd or exempt — is dispatched
/// through its block, as the fixture's visitor, and must not answer a page.
/// A route whose handler renders HTML under a response schema (or under a
/// JSON / asset / download exemption) would otherwise be accepted on its
/// label and never looked at, and a page among those rows has no controls
/// the guard would ever fire. A redirect must land on a page the crawl
/// renders.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_get_row_that_is_not_a_page_answers_something_other_than_a_page() {
    let infos = registered();
    let mut wrong: Vec<String> = Vec::new();
    for entry in entries() {
        let Some(info) = infos.iter().find(|i| i.name == entry.block) else {
            continue;
        };
        let Some(make) = entry.fixture else {
            continue;
        };
        let fixture = make().await;
        let rendered: Vec<String> = crawl(&fixture)
            .await
            .iter()
            .map(|view| view.page(&fixture).url())
            .collect();
        for row in info
            .endpoints
            .iter()
            .filter(|e| e.method == HttpMethod::Get)
        {
            let exempt = entry
                .exempt
                .iter()
                .find(|(t, _)| *t == row.path)
                .map(|(_, why)| *why);
            if row.output_schema.is_none() && exempt.is_none() {
                continue;
            }
            let path = probe_path(&row.path);
            let (_, answer) = send(&fixture, "retrieve", &path, "", false).await;
            if answer.content_type.starts_with("text/html") {
                wrong.push(format!(
                    "{}: GET {} ({}) answers {} {}, a page: {}",
                    entry.block,
                    row.path,
                    match exempt {
                        Some(why) => format!("exempt as a {}", why.reason()),
                        None => "publishes a response schema".to_string(),
                    },
                    answer.status,
                    answer.content_type,
                    answer.body.chars().take(200).collect::<String>()
                ));
                continue;
            }
            if matches!(exempt, Some(Exempt::Redirect)) {
                let lands = (300..400).contains(&answer.status)
                    && answer
                        .location
                        .as_deref()
                        .is_some_and(|to| rendered.iter().any(|page| page == to));
                if !lands {
                    wrong.push(format!(
                        "{}: GET {} is exempt as a redirect to a rendered page, and answers \
                         {} to {:?}; the crawl renders {rendered:?}",
                        entry.block, row.path, answer.status, answer.location
                    ));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
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
