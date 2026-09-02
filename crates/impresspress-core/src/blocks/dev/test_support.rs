//! Test doubles for the [`RuntimeControl`] seam.
//!
//! Exposed under `test-support` (as well as `cfg(test)`) so the `tests/`
//! integration crates and downstream consumers can drive the dev block without
//! a real runtime behind it.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use super::control::{DynamicBlockSpec, RuntimeControl, ValidationFailure, ValidationStage};

/// A [`RuntimeControl`] that records what it was asked to do instead of
/// building anything.
///
/// `rebuild` appends the requested block set and bumps the generation counter,
/// so a test can assert both *what* was activated and *how many times* — the
/// two things the real control plane is responsible for and the page keys on.
/// `inspect` reports whatever `BlockInfo` the test set, and `probe` records
/// the spec it was handed, so a test can assert that the guest was executed
/// under the ACCEPTED capabilities rather than its own declaration.
///
/// It also models the *retained runtime* half of design §7.3: `rebuild` keeps
/// the block set it swapped out and `restore_previous` puts it back without
/// rebuilding. That is what makes "the swap was undone, not redone" an
/// assertion a test can make — [`Self::restores`] against [`Self::rebuilds`],
/// with [`Self::live_blocks`] for what is actually serving.
pub struct FakeControl {
    /// Every block set handed to `rebuild`, oldest first.
    pub rebuilt: Mutex<Vec<Vec<DynamicBlockSpec>>>,
    /// The `BlockInfo` a successful `inspect` reports, set by
    /// [`Self::set_validated_info`].
    ///
    /// This is what makes the static rules testable without wasmi: the
    /// executable half of validation is the seam, so a test that wants to
    /// exercise "a guest that declares X" states X here rather than compiling
    /// a guest that declares it.
    validated_info: Mutex<Option<wafer_run::BlockInfo>>,
    /// Set by [`Self::fail_next_inspect`]: the refusal the next `inspect`
    /// answers with, consumed by that call.
    ///
    /// One-shot for the same reason [`Self::fail_rebuild`] is: the
    /// interesting request is usually the one *after* a refusal.
    fail_inspect: Mutex<Option<ValidationFailure>>,
    /// Set by [`Self::fail_next_probe`]: the refusal the next `probe` answers
    /// with, consumed by that call.
    fail_probe: Mutex<Option<ValidationFailure>>,
    /// Every spec handed to `probe`, oldest first.
    ///
    /// Recorded rather than merely counted because the *capabilities* a probe
    /// runs under are the point of the inspect/probe split: a test asserts
    /// that they are the ACCEPTED set, not the guest's raw declaration.
    probed: Mutex<Vec<DynamicBlockSpec>>,
    /// Bumped by every `inspect` call, refusals included. A test that asserts
    /// a request was refused *before* the module was loaded reads this.
    inspections: AtomicU64,
    /// Set by [`Self::fail_next_rebuild`]: the message the next `rebuild`
    /// refuses with, consumed by that call.
    ///
    /// One-shot rather than sticky because the interesting activation is the
    /// one *after* a failure — the previous generation has to still be live,
    /// and a control that refused forever could not show that.
    fail_rebuild: Mutex<Option<String>>,
    /// Set by [`Self::gate_next_rebuild`]: the next `rebuild` parks on this
    /// until the test's sender fires.
    ///
    /// The activation queue's coalescing only exists while an activation is
    /// *in flight*, and nothing else in the fixture blocks: every database
    /// and storage call completes without yielding, so two `request` futures
    /// polled together would run strictly one after the other and the queue
    /// would never be contended. This is the one place a test can hold an
    /// activation open on purpose.
    gate_rebuild: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
    /// The block set the runtime is serving right now, or `None` before the
    /// first rebuild.
    live: Mutex<Option<Vec<DynamicBlockSpec>>>,
    /// What the last successful `rebuild` swapped out, retained for
    /// `restore_previous` — the fixture's stand-in for the browser control's
    /// retained `Rc<Wafer>`.
    ///
    /// Two layers of `Option` and both are load-bearing: the outer says
    /// whether there is anything to restore *at all* (a `restore_previous`
    /// with nothing retained is an error, exactly as it is in the browser),
    /// the inner is the block set that runtime was serving, which is `None`
    /// before the first rebuild.
    retained: Mutex<Option<Option<Vec<DynamicBlockSpec>>>>,
    /// Bumped by every `restore_previous` that put a runtime back.
    restores: AtomicU64,
    /// Set by [`Self::fail_next_restore`]: the message the next
    /// `restore_previous` refuses with, consumed by that call.
    fail_restore: Mutex<Option<String>>,
    /// Bumped by every successful `rebuild`.
    generation: AtomicU64,
}

impl FakeControl {
    /// A control that accepts everything and starts at generation 0.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            rebuilt: Mutex::new(Vec::new()),
            validated_info: Mutex::new(None),
            fail_inspect: Mutex::new(None),
            fail_probe: Mutex::new(None),
            probed: Mutex::new(Vec::new()),
            inspections: AtomicU64::new(0),
            fail_rebuild: Mutex::new(None),
            gate_rebuild: Mutex::new(None),
            live: Mutex::new(None),
            retained: Mutex::new(None),
            restores: AtomicU64::new(0),
            fail_restore: Mutex::new(None),
            generation: AtomicU64::new(0),
        })
    }

    /// A control whose next `probe` refuses at `stage`.
    pub fn rejecting(stage: ValidationStage, message: &str) -> Arc<Self> {
        let control = Self::new();
        control.fail_next_probe(stage, message);
        control
    }

    /// Make every successful `inspect` report `info`.
    pub fn set_validated_info(&self, info: wafer_run::BlockInfo) {
        *self.validated_info.lock().expect("validated_info mutex") = Some(info);
    }

    /// Make the next `inspect` refuse at `stage` with `message`.
    pub fn fail_next_inspect(&self, stage: ValidationStage, message: &str) {
        *self.fail_inspect.lock().expect("fail_inspect mutex") =
            Some(ValidationFailure::new(stage, message));
    }

    /// Make the next `probe` refuse at `stage` with `message`.
    pub fn fail_next_probe(&self, stage: ValidationStage, message: &str) {
        *self.fail_probe.lock().expect("fail_probe mutex") =
            Some(ValidationFailure::new(stage, message));
    }

    /// How many times `inspect` has been called.
    pub fn inspections(&self) -> u64 {
        self.inspections.load(Ordering::SeqCst)
    }

    /// The specs handed to `probe`, oldest first.
    pub fn probes(&self) -> Vec<DynamicBlockSpec> {
        self.probed.lock().expect("probed mutex").clone()
    }

    /// The block sets handed to `rebuild`, oldest first.
    pub fn rebuilds(&self) -> Vec<Vec<DynamicBlockSpec>> {
        self.rebuilt.lock().expect("rebuilt mutex").clone()
    }

    /// Make the next `rebuild` refuse with `message`, recording nothing.
    ///
    /// The mirror of [`Self::rejecting`] for the other half of the seam: that
    /// one refuses a guest at validation, this one refuses the runtime swap
    /// itself — the failed activation has to survive with the previous
    /// generation still live.
    pub fn fail_next_rebuild(&self, message: &str) {
        *self.fail_rebuild.lock().expect("fail_rebuild mutex") = Some(message.to_string());
    }

    /// Make the next `restore_previous` refuse with `message`, restoring
    /// nothing.
    ///
    /// The retained runtime is *kept*, so the refusal is the swap failing
    /// rather than there being nothing to swap back.
    pub fn fail_next_restore(&self, message: &str) {
        *self.fail_restore.lock().expect("fail_restore mutex") = Some(message.to_string());
    }

    /// How many times `restore_previous` has put a runtime back.
    ///
    /// The counter that separates §7.3's restore from a second `rebuild`: a
    /// test asserting the swap was *undone* rather than *redone* reads this
    /// against [`Self::rebuilds`].
    pub fn restores(&self) -> u64 {
        self.restores.load(Ordering::SeqCst)
    }

    /// The block set the runtime is serving now — what a `rebuild` last
    /// installed, or what a `restore_previous` put back.
    pub fn live_blocks(&self) -> Option<Vec<DynamicBlockSpec>> {
        self.live.lock().expect("live mutex").clone()
    }

    /// Hold the next `rebuild` open until the returned sender fires (or is
    /// dropped), so a test can observe what happens *during* an activation.
    pub fn gate_next_rebuild(&self) -> futures::channel::oneshot::Sender<()> {
        let (tx, rx) = futures::channel::oneshot::channel();
        *self.gate_rebuild.lock().expect("gate_rebuild mutex") = Some(rx);
        tx
    }
}

#[wafer_block::wafer_async_trait]
impl RuntimeControl for FakeControl {
    async fn inspect(&self, _artifact: &[u8]) -> Result<wafer_run::BlockInfo, ValidationFailure> {
        self.inspections.fetch_add(1, Ordering::SeqCst);
        if let Some(failure) = self.fail_inspect.lock().expect("fail_inspect mutex").take() {
            return Err(failure);
        }
        Ok(self
            .validated_info
            .lock()
            .expect("validated_info mutex")
            .clone()
            .unwrap_or_else(|| {
                wafer_run::BlockInfo::new(
                    "site/fake",
                    "0.0.0",
                    "http-handler@v1",
                    "fake inspected guest",
                )
            }))
    }

    async fn probe(
        &self,
        spec: &DynamicBlockSpec,
        _artifact: &[u8],
    ) -> Result<(), ValidationFailure> {
        if let Some(failure) = self.fail_probe.lock().expect("fail_probe mutex").take() {
            return Err(failure);
        }
        self.probed.lock().expect("probed mutex").push(spec.clone());
        Ok(())
    }

    async fn rebuild(&self, blocks: &[DynamicBlockSpec]) -> Result<(), String> {
        // Taken out of the lock before the await: a `MutexGuard` is not held
        // across a suspension point anywhere in this fixture, for the same
        // reason the activation queue never holds one.
        let gate = self.gate_rebuild.lock().expect("gate_rebuild mutex").take();
        if let Some(gate) = gate {
            // A dropped sender releases the gate too: a test that forgets to
            // fire it fails on its assertions, not on a hang.
            let _ = gate.await;
        }
        if let Some(message) = self.fail_rebuild.lock().expect("fail_rebuild mutex").take() {
            return Err(message);
        }
        self.rebuilt
            .lock()
            .expect("rebuilt mutex")
            .push(blocks.to_vec());
        // Retain what was live, then install the new set — the fixture's
        // model of `replace_wafer` handing back the runtime it swapped out.
        let previous = self
            .live
            .lock()
            .expect("live mutex")
            .replace(blocks.to_vec());
        *self.retained.lock().expect("retained mutex") = Some(previous);
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn restore_previous(&self) -> Result<(), String> {
        if let Some(message) = self.fail_restore.lock().expect("fail_restore mutex").take() {
            return Err(message);
        }
        let Some(previous) = self.retained.lock().expect("retained mutex").take() else {
            return Err("no retained runtime to restore".to_string());
        };
        *self.live.lock().expect("live mutex") = previous;
        self.restores.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn runtime_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}
