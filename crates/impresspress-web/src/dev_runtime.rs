//! The browser half of the development sandbox: wasmi validation, the runtime
//! rebuild, and what happens on a cold boot.
//!
//! `impresspress-core` decides *what* should be live; this decides *how*. The
//! seam is [`RuntimeControl`], and [`BrowserRuntimeControl`] is its only real
//! implementation: it compiles guest artifacts with wasmi, rebuilds a `Wafer`
//! through the shared [`RuntimeFactory`], and swaps it in.
//!
//! # The cycle, and where it is closed
//!
//! `DevShared` holds the control; the factory holds `DevShared`; the control
//! rebuilds through the factory. Something has to be late-bound, and it is the
//! control's factory handle ([`BrowserRuntimeControl::set_factory`]) — because
//! the *other* order does not work at all. The cold-start runtime has to
//! carry `/b/dev` (otherwise the page 404s until the first rebuild, and a
//! sandbox with no blocks would never rebuild), so `DevShared` must exist
//! before the first `factory.build`. A factory cannot be built before then;
//! a control can, and it needs its factory only at the first `rebuild`, which
//! cannot happen until boot has finished. See [`attach`].
//!
//! # Two contexts that are not the runtime's
//!
//! Both exist because their callers run outside any request:
//!
//! * [`DenyAllContext`] — what a guest is probed under. It grants nothing, on
//!   purpose (see [`BrowserRuntimeControl::probe`]).
//! * [`BootContext`] — what boot convergence and the seed import run under.
//!   It grants the dev block the same reach a request would, and is only ever
//!   constructed by [`install`].
//!
//! # Why no `unsafe impl Send`/`Sync`
//!
//! Every trait this module implements or hands values to (`RuntimeControl`,
//! `Context`, `Block`) is bounded on `wafer_run::MaybeSend + MaybeSync`, which
//! is unbounded on `wasm32`. `Rc`, `Cell` and `RefCell` therefore cross those
//! boundaries without an `unsafe` marker impl; the only cost is
//! `clippy::arc_with_non_send_sync`, allowed at the three sites that build an
//! `Arc` over a single-threaded value with the same justification the block
//! registration path already carries.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use impresspress_core::blocks::dev::{
    activation::{self, ActivationIntent},
    artifacts,
    control::{DynamicBlockSpec, RuntimeControl, ValidationFailure, ValidationStage},
    repo::generations::GenerationCause,
    seed::{self, SeedManifest},
    DevShared, BLOCK_NAME,
};
use wafer_run::{
    context::Context, wasm::WasmiBlock, Block, BlockInfo, BlockRuntime, ErrorCode, FuelLimit,
    InputStream, LifecycleEvent, LifecycleType, Message, OutputStream, ResourceLimits,
    TerminalNotResponse, WaferError,
};
use wasm_bindgen::{prelude::*, JsCast};
use wasm_bindgen_futures::JsFuture;

use crate::runtime_factory::{DynamicBlock, RuntimeFactory};

/// Per-call wasmi fuel budget for a guest block (design §6.6).
///
/// The producer's own default, restated rather than inherited: the sandbox
/// must not silently follow a change to `wafer_run::DEFAULT_FUEL`, because
/// this number is what bounds how long one keystroke's request can run inside
/// a service worker that has no other way to be interrupted.
const GUEST_FUEL: u64 = 100_000_000;

/// Per-call linear-memory cap for a guest block, in 64 KiB wasmi pages
/// (256 pages = 16 MiB).
const GUEST_MEMORY_PAGES: u32 = 256;

/// The limits every guest — probed or live — is loaded under.
///
/// One function, so a guest cannot be probed under bounds it will not run
/// under. `..Default::default()` keeps the producer's SEC-03 caps (host bytes,
/// live streams, table elements), which the sandbox has no reason to move.
fn guest_limits() -> ResourceLimits {
    ResourceLimits {
        fuel: FuelLimit::Metered(GUEST_FUEL),
        memory_pages: GUEST_MEMORY_PAGES,
        ..Default::default()
    }
}

/// Compile `artifact` as the guest `spec` describes.
///
/// The capabilities are `spec.capabilities` — the **accepted** set the dev
/// block's static rules produced, never the guest's own declaration. That is
/// the whole security property of the inspect → rules → probe order, and it is
/// preserved by this function having no other source for them.
pub fn load_guest(
    spec: &DynamicBlockSpec,
    artifact: &[u8],
) -> Result<Arc<dyn Block>, wafer_run::RuntimeError> {
    let block = WasmiBlock::load_with_capabilities_and_limits(
        artifact,
        spec.capabilities.clone(),
        guest_limits(),
    )?;
    Ok(Arc::new(block))
}

// ---------------------------------------------------------------------------
// The probe context
// ---------------------------------------------------------------------------

/// The [`Context`] a guest is probed under: every host call is denied.
///
/// A probe is a dry run of untrusted code, and it happens *before* anything
/// has been registered — there is no `wafer-run/database` to route to that
/// would not be acting on the live instance's data. So the answer to every
/// host call is `PermissionDenied`, and the guest's own capability set (which
/// wasmi enforces independently) never gets the chance to matter.
///
/// The denial *count* is the load-bearing part. It is how
/// [`BrowserRuntimeControl::probe`] tells "this guest's `Init` failed because
/// the probe denied it the database" — expected, and not a reason to refuse a
/// block — from "this guest's `Init` failed on its own", which is. Nothing
/// about the guest's error code or message is consulted for that: a template
/// that wraps a host error in its own is judged the same as one that
/// propagates it.
#[derive(Default)]
struct DenyAllContext {
    /// How many host calls this context has refused, over the whole probe.
    denials: Cell<u32>,
}

impl DenyAllContext {
    /// The refusal count, for comparing before and after one probe step.
    fn denials(&self) -> u32 {
        self.denials.get()
    }

    fn deny(&self, what: &str) -> WaferError {
        self.denials.set(self.denials.get().saturating_add(1));
        WaferError::new(
            ErrorCode::PermissionDenied,
            format!("the validation probe grants no host access: {what} is unreachable"),
        )
    }
}

#[wafer_block::wafer_async_trait]
impl Context for DenyAllContext {
    async fn call_block(
        &self,
        block_name: &str,
        _msg: Message,
        _input: InputStream,
    ) -> OutputStream {
        OutputStream::error(self.deny(&format!("block {block_name:?}")))
    }

    fn is_cancelled(&self) -> bool {
        false
    }

    fn config_get(&self, _key: &str) -> Option<&str> {
        None
    }

    fn clone_arc(&self) -> Arc<dyn Context> {
        // A fresh counter rather than a shared one: the clone is handed out
        // for a caller to retain, and a probe step's verdict must be decided
        // by the calls made *during* that step through the context the step
        // was given.
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(Self::default())
    }

    /// Deny, and count.
    ///
    /// The trait's own default already denies (it is fail-closed), so this
    /// override exists for the counter — and to say in one place that this
    /// context refuses on purpose rather than by omission.
    fn check_resource_access(
        &self,
        resource: &str,
        _resource_type: wafer_run::ResourceType,
        _is_write: bool,
    ) -> Result<(), WaferError> {
        Err(self.deny(&format!("resource {resource:?}")))
    }
}

// ---------------------------------------------------------------------------
// The control
// ---------------------------------------------------------------------------

/// The host's half of activation, backed by wasmi and the shared
/// [`RuntimeFactory`].
pub struct BrowserRuntimeControl {
    /// The factory every rebuild goes through, once [`Self::set_factory`] has
    /// closed the cycle described in the module header. `None` only between
    /// construction and that call, which is a window no `rebuild` can reach.
    factory: RefCell<Option<Rc<RuntimeFactory>>>,
    /// Bumped by every successful rebuild; read by `GET /b/dev/api/status`.
    generation: Cell<u64>,
}

impl BrowserRuntimeControl {
    /// A control with no factory yet. `Arc` because that is what `DevShared`
    /// holds; single-threaded contents are fine (see the module header).
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            factory: RefCell::new(None),
            generation: Cell::new(0),
        })
    }

    /// Close the control → factory half of the cycle.
    pub fn set_factory(&self, factory: &Rc<RuntimeFactory>) {
        *self.factory.borrow_mut() = Some(Rc::clone(factory));
    }

    /// The factory, cloned out.
    ///
    /// Cloned rather than borrowed because every caller is about to `await`,
    /// and a `RefCell` borrow held across a suspension point is a panic
    /// waiting for the first concurrent activation.
    fn factory(&self) -> Result<Rc<RuntimeFactory>, String> {
        self.factory
            .borrow()
            .clone()
            .ok_or_else(|| "the sandbox runtime factory was never installed".to_string())
    }
}

#[wafer_block::wafer_async_trait]
impl RuntimeControl for BrowserRuntimeControl {
    /// Read the guest's `BlockInfo` without running any of its code beyond
    /// instantiation and `__wafer_info`, under `BlockCapabilities::none()`.
    ///
    /// `Block::info()` is infallible by contract, so a guest that cannot
    /// report its own info does not error — wasmi logs and answers the
    /// placeholder `BlockInfo::new("unknown", "0.0.0", "unknown", …)`. That
    /// placeholder is indistinguishable from a real (if absurd) declaration by
    /// name alone, so the discriminator used here is
    /// [`BlockInfo::runtime`]: a successful read is stamped
    /// [`BlockRuntime::Wasm`] by the loader, and the placeholder keeps the
    /// `Native` default. Refusing on the name instead would also refuse a
    /// guest that legitimately called itself `unknown` — and, worse, would
    /// accept a failed read from a guest whose name happened not to be.
    async fn inspect(&self, artifact: &[u8]) -> Result<BlockInfo, ValidationFailure> {
        let block = WasmiBlock::load_with_capabilities_and_limits(
            artifact,
            wafer_block::BlockCapabilities::none(),
            guest_limits(),
        )
        .map_err(|e| {
            ValidationFailure::new(
                ValidationStage::Load,
                format!("the module did not compile or instantiate: {e}"),
            )
        })?;

        let info = block.info();
        if info.runtime != BlockRuntime::Wasm {
            return Err(ValidationFailure::new(
                ValidationStage::Info,
                "the module did not return a BlockInfo from `__wafer_info` — it is missing the \
                 export, its memory, or the value it wrote could not be decoded",
            ));
        }
        info.validate()
            .map_err(|e| ValidationFailure::new(ValidationStage::Info, format!("{e}")))?;
        Ok(info)
    }

    /// Run `Init`, `Start` and one request under the **accepted** spec's
    /// capabilities, swapping nothing in.
    ///
    /// # What each step proves, and what it does not
    ///
    /// The seam gives the host a `Result`/`OutputStream`, not a trap flag: a
    /// wasmi trap is delivered as an ordinary `ErrorCode::Internal`, which is
    /// also what a guest can return on its own. So neither step can name a
    /// trap exactly, and each is judged on the discriminator that is real:
    ///
    /// * **`Init` / `Start`** fail the probe when they error *without the
    ///   probe context having denied them anything*. A guest whose `Init`
    ///   calls `ensure_table` will be denied by [`DenyAllContext`] and fail;
    ///   that is the probe's own doing, not a defect, so it passes. An `Init`
    ///   that fails having asked the host for nothing failed on its own, and
    ///   would fail identically once live.
    /// * **The request** passes on any terminal except an
    ///   `ErrorCode::Internal` error or a stream that ends with no terminal at
    ///   all. A `GET` of a guest's route root is *expected* to 404 on a block
    ///   that serves only sub-paths, so an application error here proves
    ///   nothing either way — but every host-side failure (a trap, fuel
    ///   exhaustion, a missing `__wafer_handle`, a guest result that does not
    ///   decode) arrives as `Internal`, and a guest that answers `Internal` to
    ///   a bare `GET` of its own prefix is broken whichever produced it.
    ///
    /// A guest with no declared route is not requested at all; there is no URL
    /// to make up, and the static rules have already refused a block whose
    /// endpoints lie outside its prefix.
    async fn probe(
        &self,
        spec: &DynamicBlockSpec,
        artifact: &[u8],
    ) -> Result<(), ValidationFailure> {
        let block = load_guest(spec, artifact).map_err(|e| {
            ValidationFailure::new(
                ValidationStage::Load,
                format!("the module did not compile or instantiate: {e}"),
            )
        })?;
        let ctx = DenyAllContext::default();

        for (event_type, stage) in [
            (LifecycleType::Init, ValidationStage::Init),
            (LifecycleType::Start, ValidationStage::Start),
        ] {
            let before = ctx.denials();
            let outcome = block
                .lifecycle(
                    &ctx,
                    LifecycleEvent {
                        event_type,
                        data: Vec::new(),
                    },
                )
                .await;
            if let Err(e) = outcome {
                if ctx.denials() == before {
                    return Err(ValidationFailure::new(
                        stage,
                        format!(
                            "{:?} failed without asking the host for anything: {}",
                            event_type, e.message
                        ),
                    ));
                }
            }
        }

        let Some(route) = spec.routes.first() else {
            return Ok(());
        };
        let msg = wafer_block::http_codec::build_http_message(
            "GET",
            &route.prefix,
            "",
            "127.0.0.1",
            std::iter::empty::<(&str, &str)>(),
        );
        match block
            .handle(&ctx, msg, InputStream::empty())
            .await
            .collect_buffered()
            .await
        {
            Ok(_) => Ok(()),
            Err(TerminalNotResponse::Error(e)) if e.code == ErrorCode::Internal => {
                Err(ValidationFailure::new(
                    ValidationStage::Probe,
                    format!(
                        "GET {} failed inside the runtime: {}",
                        route.prefix, e.message
                    ),
                ))
            }
            Err(TerminalNotResponse::Malformed) => Err(ValidationFailure::new(
                ValidationStage::Probe,
                format!(
                    "GET {} produced no terminal event — the guest did not complete its ABI \
                     contract",
                    route.prefix
                ),
            )),
            // Error (application), Drop, Halt, Continue: the guest ran and
            // answered. Whether it answered 200 or 404 is not this step's
            // question.
            Err(_) => Ok(()),
        }
    }

    /// Build a runtime with exactly `blocks` and swap it in.
    ///
    /// Artifacts are read through the platform `StorageService` directly:
    /// there is no request in flight here (an activation can be driven by boot
    /// convergence, where no `Context` exists at all), and
    /// [`artifacts::get_direct`] is where the key layout is stated so the two
    /// readers cannot drift.
    ///
    /// Nothing is swapped until every guest has loaded and the whole runtime
    /// has booted, so a failure anywhere leaves the live runtime exactly as it
    /// was. The generation counter is bumped last, after the swap, because it
    /// is what the `/b/dev` page keys its tool re-registration on.
    async fn rebuild(&self, blocks: &[DynamicBlockSpec]) -> Result<(), String> {
        let factory = self.factory()?;
        let storage = impresspress_browser::make_storage_service();

        let mut dynamic: Vec<DynamicBlock> = Vec::with_capacity(blocks.len());
        for spec in blocks {
            let artifact = artifacts::get_direct(&storage, &spec.artifact_sha256)
                .await
                .map_err(|e| format!("block {}: {}", spec.name, e.message))?;
            let block =
                load_guest(spec, &artifact).map_err(|e| format!("block {}: {e}", spec.name))?;
            dynamic.push((spec.clone(), block));
        }

        let (wafer, _storage_block) = factory
            .build(&dynamic)
            .await
            .map_err(|e| describe_js(&e, "building the runtime"))?;
        impresspress_browser::replace_wafer(wafer).map_err(|e| e.to_string())?;
        self.generation.set(self.generation.get().saturating_add(1));
        Ok(())
    }

    fn runtime_generation(&self) -> u64 {
        self.generation.get()
    }
}

// ---------------------------------------------------------------------------
// The boot context
// ---------------------------------------------------------------------------

/// The [`Context`] the boot-time half of the sandbox runs under.
///
/// Boot convergence and the seed import are the dev block's own work, but they
/// happen with no request in flight, so there is no `RuntimeContext` for them
/// — `Wafer::make_context` is `pub(crate)` and `Wafer::run_block` builds a
/// context for a *block it dispatches to*, which is not what a host-side
/// caller of `impresspress_core::blocks::dev::activation` needs.
///
/// So this is that context, and it is deliberately thin:
///
/// * `call_block` looks the block up in the live runtime and calls it. The
///   runtime has already been sealed and `init_all_blocks` has run (see
///   `builder::boot`), so there is no lazy init left to drive.
/// * `caller_id` is the dev block. This is the load-bearing field:
///   `ImpresspressStorageBlock` namespaces every folder by it, so a boot that
///   reported anything else would read and write `unknown/…` instead of
///   `impresspress/dev/…`. The storage block's own cross-block WRAP check —
///   the one that admits the reach into `wafer-run/web/site` — still runs
///   against the real grant list.
/// * `check_resource_access` allows. This is the host's own trusted entry, in
///   the same sense `Wafer::run_block` is documented to be: it runs before any
///   request has been served, on behalf of the block that owns the data it
///   touches.
///
/// The `Rc` is pinned to the runtime that was live when [`install`] started,
/// and a rebuild during boot swaps a *different* one in behind it. That is
/// deliberate and harmless: every platform service is shared by construction
/// (`RuntimeFactory` builds them once), so the database and object store this
/// keeps reaching are the same ones the new runtime holds. The only thing that
/// goes stale is the block set, which nothing on the boot path reads.
#[derive(Clone)]
struct BootContext {
    wafer: Rc<wafer_run::Wafer>,
}

#[wafer_block::wafer_async_trait]
impl Context for BootContext {
    async fn call_block(&self, block_name: &str, msg: Message, input: InputStream) -> OutputStream {
        let Some(block) = self.wafer.lookup_block(block_name).map(|(_, block)| block) else {
            return OutputStream::error(WaferError::new(
                ErrorCode::NotFound,
                format!("block not found: {block_name}"),
            ));
        };
        block.handle(self, msg, input).await
    }

    fn is_cancelled(&self) -> bool {
        false
    }

    fn config_get(&self, key: &str) -> Option<&str> {
        self.wafer.config_snapshot().get(key).map(String::as_str)
    }

    fn caller_id(&self) -> Option<&str> {
        Some(BLOCK_NAME)
    }

    fn clone_arc(&self) -> Arc<dyn Context> {
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(self.clone())
    }

    fn check_resource_access(
        &self,
        _resource: &str,
        _resource_type: wafer_run::ResourceType,
        _is_write: bool,
    ) -> Result<(), WaferError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fetching the seed bundle
// ---------------------------------------------------------------------------

/// [`seed::SeedFetch`] over the service worker's own `fetch`.
///
/// `web_sys::window()` is `None` in a service worker, so the global is reached
/// as a [`web_sys::ServiceWorkerGlobalScope`]. These requests are *not* served
/// by this worker's own fetch handler (a worker does not intercept its own
/// outgoing requests), and the bundle additionally adds [`seed::ROOT`] to the
/// service worker's bypass list so a page asking for the same files reaches
/// the static host too.
struct SwFetch;

impl SwFetch {
    /// `Ok(None)` for a 404.
    ///
    /// Only [`seed::MANIFEST_URL`] is allowed to be absent — that is how a
    /// bundle says "no seed" — so the [`seed::SeedFetch`] impl below turns the
    /// same answer into an error for every other file.
    async fn try_get(&self, url: &str) -> Result<Option<Vec<u8>>, String> {
        let global: web_sys::ServiceWorkerGlobalScope = js_sys::global()
            .dyn_into()
            .map_err(|_| "the seed can only be fetched from a service worker global".to_string())?;
        let response: web_sys::Response = JsFuture::from(global.fetch_with_str(url))
            .await
            .map_err(|e| describe_js(&e, &format!("fetching {url}")))?
            .dyn_into()
            .map_err(|_| format!("fetching {url}: the response was not a Response"))?;

        if response.status() == 404 {
            return Ok(None);
        }
        if !response.ok() {
            return Err(format!("fetching {url}: HTTP {}", response.status()));
        }
        let buffer = JsFuture::from(
            response
                .array_buffer()
                .map_err(|e| describe_js(&e, &format!("reading {url}")))?,
        )
        .await
        .map_err(|e| describe_js(&e, &format!("reading {url}")))?;
        Ok(Some(
            js_sys::Uint8Array::new(&buffer.unchecked_into::<js_sys::ArrayBuffer>()).to_vec(),
        ))
    }
}

impl seed::SeedFetch for SwFetch {
    fn get<'a>(&'a self, url: &'a str) -> seed::FetchFuture<'a> {
        Box::pin(async move {
            self.try_get(url)
                .await?
                .ok_or_else(|| format!("{url}: the seed manifest names it, but it is not served"))
        })
    }
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// Attach the sandbox control plane to `factory`, before the first runtime is
/// built.
///
/// Returns the shared factory handle and — when the sandbox is actually active
/// — the `DevShared` [`install`] needs. `None` is the whole "feature off (or
/// flag off) = nothing" rule from this crate's `resolve_dev_active`: no
/// control, no `DevShared`, so `RuntimeFactory::with_dev` is never called and
/// the runtime that gets built is byte-identical to one that never asked for a
/// sandbox.
pub fn attach(factory: RuntimeFactory) -> (Rc<RuntimeFactory>, Option<Arc<DevShared>>) {
    if !factory.dev_active {
        return (Rc::new(factory), None);
    }
    let control = BrowserRuntimeControl::new();
    let shared = DevShared::new(control.clone());
    let factory = Rc::new(factory.with_dev(shared.clone()));
    control.set_factory(&factory);
    (factory, Some(shared))
}

/// Bring the sandbox up on the runtime that was just stored.
///
/// In order:
///
/// 1. **Seed**, when this instance has never published anything. A bundle with
///    no `/seed/manifest.json` is the ordinary case and not an error.
/// 2. **Converge** on whatever the activation journal says was in flight, and
///    learn the block set the active generation declares.
/// 3. **Rebuild**, when that set is not empty and steps 1–2 have not already
///    rebuilt, *before returning*. Requests only start arriving once
///    `initialize()` has resolved, so this is what keeps a request from being
///    served by the base runtime while its blocks are still pending.
///
/// The "have not already rebuilt" guard is the runtime generation counter.
/// Activating a seed that carries blocks, or converging on an interrupted
/// activation, rebuilds on its own — and in every path through
/// `activation.rs` the *last* rebuild is with the set that ends up active,
/// which is exactly what `converge_on_boot` then returns. Rebuilding again
/// would boot a second identical runtime (migrations, block init and all) on
/// the one boot that can least afford it: the first.
///
/// Every step logs its own failure and continues rather than failing
/// `initialize()`. A sandbox that refuses to boot is a sandbox whose `/b/dev`
/// page — the only thing that could fix it — never comes up; a sandbox that
/// boots with nothing dynamic still serves the page, the ledger and the
/// diagnostics that say why.
pub async fn install(shared: &Arc<DevShared>) {
    let Some(wafer) = impresspress_browser::current_wafer() else {
        web_sys::console::error_1(
            &"impresspress: the dev sandbox cannot install before a runtime is stored".into(),
        );
        return;
    };
    let ctx = BootContext { wafer };
    let generation_before = shared.control.runtime_generation();

    if let Err(e) = seed_on_boot(&ctx, shared).await {
        web_sys::console::error_1(&format!("impresspress: dev sandbox seed import: {e}").into());
    }

    let blocks = match activation::converge_on_boot(&ctx, shared).await {
        Ok(blocks) => blocks,
        Err(e) => {
            web_sys::console::error_1(
                &format!("impresspress: dev sandbox boot convergence: {e}").into(),
            );
            return;
        }
    };
    if blocks.is_empty() || shared.control.runtime_generation() != generation_before {
        return;
    }
    if let Err(e) = shared.control.rebuild(&blocks).await {
        web_sys::console::error_1(
            &format!(
                "impresspress: dev sandbox could not load its {} active block(s): {e}",
                blocks.len()
            )
            .into(),
        );
        return;
    }
    web_sys::console::log_1(
        &format!(
            "impresspress: dev sandbox runtime carries {} block(s)",
            blocks.len()
        )
        .into(),
    );
}

/// Import generation 0 from the origin's seed bundle, when there is one and
/// this instance has never published anything.
async fn seed_on_boot(ctx: &dyn Context, shared: &Arc<DevShared>) -> Result<(), String> {
    // Freshness first: this runs on every boot, and an instance that has been
    // used has no business fetching a bundle it will refuse to import.
    if !seed::is_fresh(ctx).await? {
        return Ok(());
    }
    let fetch = SwFetch;
    let Some(bytes) = fetch.try_get(seed::MANIFEST_URL).await? else {
        return Ok(());
    };
    let manifest: SeedManifest =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", seed::MANIFEST_URL))?;
    let Some(generation) = seed::import(ctx, &manifest, &fetch).await? else {
        return Ok(());
    };
    let outcome = activation::request(
        ctx,
        shared,
        GenerationCause::Seed,
        ActivationIntent::Seed {
            manifest: generation,
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    web_sys::console::log_1(
        &format!(
            "impresspress: dev sandbox imported the seed as generation {} ({} site files, {} \
             blocks)",
            outcome.generation.id, outcome.generation.site_files, outcome.generation.blocks
        )
        .into(),
    );
    Ok(())
}

/// A `JsValue` rejection as a message, prefixed with what was being attempted.
///
/// `JsValue`'s own `Debug` renders an `Error` as `JsValue(Error: …)`, which is
/// what would otherwise end up in a `ValidationFailure` an agent reads.
fn describe_js(value: &JsValue, doing: &str) -> String {
    let detail = value
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(value, &JsValue::from_str("message"))
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| format!("{value:?}"));
    format!("{doing}: {detail}")
}
