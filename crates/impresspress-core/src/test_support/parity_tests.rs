//! [`TestContext`] dispatches a `call_block` as a sealed runtime does.
//!
//! Each case builds the same blocks twice — in a real, sealed
//! [`wafer_run::Wafer`] with the aliases and admin block
//! `ImpresspressBuilder::build` installs, and in a [`TestContext`] — enters
//! the same block in both with the same message, and asserts both answer
//! identically. The fixture re-states what `RuntimeContext::dispatch_call`
//! does because the runtime keeps that code private; these cases are what
//! hold the two together, and every one of them fails against a fixture
//! that skipped the gate it exercises.

use std::sync::Arc;

use wafer_block::{codec, common::ServiceOp, wire::database::CountRequest};
use wafer_run::{
    context::Context, streams::output::TerminalNotResponse, Block, BlockInfo, InputStream, Message,
    OutputStream, ResourceGrant,
};

use super::TestContext;

/// One call a [`Probe`] makes: `target` receives `op`. A database op counts
/// the rows of `collection`; any other op is forwarded to another probe,
/// carrying the hops left.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Hop {
    target: String,
    op: String,
    collection: Option<String>,
}

impl Hop {
    fn to(target: &str) -> Self {
        Self {
            target: target.to_string(),
            op: "probe.hop".to_string(),
            collection: None,
        }
    }

    fn count(target: &str, collection: &str) -> Self {
        Self {
            target: target.to_string(),
            op: ServiceOp::DATABASE_COUNT.to_string(),
            collection: Some(collection.to_string()),
        }
    }
}

const HOPS_META: &str = "probe.hops";

/// A block that makes the calls its message lists, one hop each, and
/// answers `ok` or the code of the first refusal with the number of hops
/// still unmade — which is what distinguishes the depth a runtime stops at.
struct Probe {
    name: &'static str,
    requires: Vec<String>,
    grants: Vec<ResourceGrant>,
}

impl Probe {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            requires: Vec::new(),
            grants: Vec::new(),
        }
    }

    fn requires(mut self, targets: &[&str]) -> Self {
        self.requires = targets.iter().map(|t| (*t).to_string()).collect();
        self
    }

    fn grants(mut self, grants: Vec<ResourceGrant>) -> Self {
        self.grants = grants;
        self
    }
}

fn message(hops: &[Hop]) -> Message {
    let mut msg = Message::new("probe.hop");
    msg.set_meta(
        HOPS_META,
        serde_json::to_string(hops).expect("hops serialize"),
    );
    msg
}

async fn outcome(out: OutputStream) -> String {
    match out.collect_buffered().await {
        Ok(buf) => String::from_utf8(buf.body).expect("utf-8 outcome"),
        Err(TerminalNotResponse::Error(e)) => format!("{:?}", e.code),
        Err(other) => panic!("a probe answered a non-response terminal: {other:?}"),
    }
}

#[wafer_block::wafer_async_trait]
impl Block for Probe {
    fn info(&self) -> BlockInfo {
        // An interface the runtime has no spec for: every probe op passes
        // the action check, which the cases that are not about it need.
        BlockInfo::new(self.name, "0.0.1", "probe@v1", "parity probe")
            .requires(self.requires.clone())
            .grants(self.grants.clone())
    }

    async fn handle(&self, ctx: &dyn Context, msg: Message, _input: InputStream) -> OutputStream {
        let mut hops: Vec<Hop> =
            serde_json::from_str(msg.get_meta(HOPS_META)).expect("a probe message lists its hops");
        if hops.is_empty() {
            return OutputStream::respond(b"ok".to_vec());
        }
        let hop = hops.remove(0);
        let left = hops.len();
        let answer = match &hop.collection {
            Some(collection) => {
                let body = codec::encode(&CountRequest {
                    collection: collection.clone(),
                    filters: Vec::new(),
                })
                .expect("count request encodes");
                let out = ctx
                    .call_block(
                        &hop.target,
                        Message::new(hop.op.as_str()),
                        InputStream::from_bytes(body),
                    )
                    .await;
                match out.collect_buffered().await {
                    Ok(_) => "ok".to_string(),
                    Err(TerminalNotResponse::Error(e)) => format!("{:?}@{left}", e.code),
                    Err(other) => panic!("the database answered a non-response: {other:?}"),
                }
            }
            None => {
                let mut next = message(&hops);
                next.kind = hop.op.clone();
                let out = ctx
                    .call_block(&hop.target, next, InputStream::empty())
                    .await;
                let answer = outcome(out).await;
                if answer == "ok" || answer.contains('@') {
                    answer
                } else {
                    format!("{answer}@{left}")
                }
            }
        };
        OutputStream::respond(answer.into_bytes())
    }
}

/// What `entry` answers `hops` with in a sealed runtime holding `probes`,
/// and in a [`TestContext`] holding the same probes.
async fn run_both(probes: Vec<Probe>, entry: &str, hops: &[Hop]) -> (String, String) {
    let probes: Vec<Arc<Probe>> = probes.into_iter().map(Arc::new).collect();

    let mut wafer = super::dispatch::empty_wafer();
    wafer_core::service_blocks::database::register_with(
        &mut wafer,
        Arc::new(
            wafer_block_sqlite::service::SQLiteDatabaseService::open_in_memory()
                .expect("open in-memory sqlite"),
        ),
    )
    .expect("the database registers");
    for (alias, target) in crate::builder::SERVICE_ALIASES {
        if *target == "wafer-run/database" {
            wafer.add_alias(*alias, *target).expect("alias");
        }
    }
    for probe in &probes {
        wafer
            .register_block(probe.name, probe.clone())
            .expect("a probe registers");
    }
    wafer.seal().await.expect("the runtime seals");
    let runtime = outcome(
        wafer
            .run_block(entry, message(hops), InputStream::empty())
            .await,
    )
    .await;

    let mut ctx = TestContext::new().await;
    for probe in &probes {
        ctx.register_block(probe.name, probe.clone());
    }
    let entry_block = probes
        .iter()
        .find(|p| p.name == entry)
        .expect("the entry is a probe")
        .clone();
    let fixture = outcome(
        entry_block
            .handle(&ctx.running_as(entry), message(hops), InputStream::empty())
            .await,
    )
    .await;

    (runtime, fixture)
}

#[tokio::test]
async fn the_call_depth_ceiling_is_the_runtimes() {
    let hops: Vec<Hop> = (0..40).map(|_| Hop::to("test/deep")).collect();
    let (runtime, fixture) = run_both(vec![Probe::new("test/deep")], "test/deep", &hops).await;
    assert!(
        runtime.starts_with("ResourceExhausted@"),
        "the runtime stops a deep recursion: {runtime}"
    );
    assert_eq!(fixture, runtime, "the fixture stops at the same depth");
}

#[tokio::test]
async fn a_target_missing_from_requires_is_refused() {
    let (runtime, fixture) = run_both(
        vec![
            Probe::new("test/a").requires(&["test/c"]),
            Probe::new("test/b"),
            Probe::new("test/c"),
        ],
        "test/a",
        &[Hop::to("test/b")],
    )
    .await;
    assert_eq!(runtime, "PermissionDenied@0");
    assert_eq!(fixture, runtime);
}

#[tokio::test]
async fn an_alias_resolves_before_the_requires_check() {
    let (runtime, fixture) = run_both(
        vec![Probe::new("test/a").requires(&["wafer-run/database"])],
        "test/a",
        &[Hop::count("db", "test__a__rows")],
    )
    .await;
    assert!(
        !runtime.starts_with("PermissionDenied") && !runtime.starts_with("Unimplemented"),
        "the runtime reaches the database through its alias: {runtime}"
    );
    assert_eq!(fixture, runtime);
}

#[tokio::test]
async fn an_action_outside_the_targets_interface_is_unimplemented() {
    let (runtime, fixture) = run_both(
        vec![Probe::new("test/a")],
        "test/a",
        &[Hop {
            target: "wafer-run/database".to_string(),
            op: "probe.hop".to_string(),
            collection: None,
        }],
    )
    .await;
    assert_eq!(runtime, "Unimplemented@0");
    assert_eq!(fixture, runtime);
}

#[tokio::test]
async fn an_unregistered_target_is_unimplemented() {
    let (runtime, fixture) = run_both(
        vec![Probe::new("test/a")],
        "test/a",
        &[Hop::to("test/nowhere")],
    )
    .await;
    assert_eq!(runtime, "Unimplemented@0");
    assert_eq!(fixture, runtime);
}

/// WRAP is on in every frame: a block reading another block's table with no
/// grant is refused — however the test entered it.
#[tokio::test]
async fn an_ungranted_read_of_another_blocks_table_is_refused() {
    let (runtime, fixture) = run_both(
        vec![Probe::new("test/a"), Probe::new("test/b")],
        "test/a",
        &[Hop::count("wafer-run/database", "test__b__rows")],
    )
    .await;
    assert_eq!(runtime, "PermissionDenied@0");
    assert_eq!(fixture, runtime);
}

/// The grant the owning block declares is the one that admits the read.
#[tokio::test]
async fn the_owners_grant_admits_the_read() {
    let (runtime, fixture) = run_both(
        vec![
            Probe::new("test/a"),
            Probe::new("test/b").grants(vec![ResourceGrant::read("test/a", "test__b__rows")]),
        ],
        "test/a",
        &[Hop::count("wafer-run/database", "test__b__rows")],
    )
    .await;
    assert!(
        !runtime.starts_with("PermissionDenied"),
        "the grant admits the read: {runtime}"
    );
    assert_eq!(fixture, runtime);
}

/// A nested block's service calls are authorized as THAT block, not as the
/// block the test entered: `test/a` may read its own table, and `test/b`,
/// which `test/a` calls, may not.
#[tokio::test]
async fn a_nested_call_is_authorized_as_the_block_that_makes_it() {
    let (runtime, fixture) = run_both(
        vec![Probe::new("test/a"), Probe::new("test/b")],
        "test/a",
        &[
            Hop::to("test/b"),
            Hop::count("wafer-run/database", "test__a__rows"),
        ],
    )
    .await;
    assert_eq!(runtime, "PermissionDenied@0");
    assert_eq!(fixture, runtime);
}

/// A cancelled dispatch refuses every further call. The runtime cancels on
/// a deadline or an abort, neither of which a top-level `run_block` exposes,
/// so this pins the fixture against the runtime's code for it.
#[tokio::test]
async fn a_cancelled_fixture_refuses_further_calls() {
    let mut ctx = TestContext::new().await;
    ctx.register_block("test/a", Arc::new(Probe::new("test/a")));
    let frame = ctx.running_as("test/a");
    frame.cancel();
    assert!(frame.is_cancelled());
    let answer = outcome(
        frame
            .call_block("test/a", message(&[]), InputStream::empty())
            .await,
    )
    .await;
    assert_eq!(answer, "Cancelled");
}
