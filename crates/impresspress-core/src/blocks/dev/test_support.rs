//! Test doubles for the [`RuntimeControl`] seam.
//!
//! Exposed under `test-support` (as well as `cfg(test)`) so the `tests/`
//! integration crates and downstream consumers can drive the dev block without
//! a real runtime behind it.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use super::control::{
    DynamicBlockSpec, RuntimeControl, ValidatedGuest, ValidationFailure, ValidationStage,
};

/// A [`RuntimeControl`] that records what it was asked to do instead of
/// building anything.
///
/// `rebuild` appends the requested block set and bumps the generation counter,
/// so a test can assert both *what* was activated and *how many times* — the
/// two things the real control plane is responsible for and the page keys on.
pub struct FakeControl {
    /// Every block set handed to `rebuild`, oldest first.
    pub rebuilt: Mutex<Vec<Vec<DynamicBlockSpec>>>,
    /// What the next `validate` call answers. `Ok(())` becomes a
    /// [`ValidatedGuest`] carrying a `BlockInfo` named after the spec.
    pub validate_result: Mutex<Result<(), ValidationFailure>>,
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
    /// Bumped by every successful `rebuild`.
    generation: AtomicU64,
}

impl FakeControl {
    /// A control that accepts everything and starts at generation 0.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            rebuilt: Mutex::new(Vec::new()),
            validate_result: Mutex::new(Ok(())),
            fail_rebuild: Mutex::new(None),
            gate_rebuild: Mutex::new(None),
            generation: AtomicU64::new(0),
        })
    }

    /// A control whose next `validate` refuses at `stage`.
    pub fn rejecting(stage: ValidationStage, message: &str) -> Arc<Self> {
        let control = Self::new();
        *control
            .validate_result
            .lock()
            .expect("validate_result mutex") = Err(ValidationFailure::new(stage, message));
        control
    }

    /// The block sets handed to `rebuild`, oldest first.
    pub fn rebuilds(&self) -> Vec<Vec<DynamicBlockSpec>> {
        self.rebuilt.lock().expect("rebuilt mutex").clone()
    }

    /// Make the next `rebuild` refuse with `message`, recording nothing.
    ///
    /// The mirror of [`Self::rejecting`] for the other half of the seam: that
    /// one refuses a guest at validation, this one refuses the runtime swap
    /// itself — the failure activation has to survive with the previous
    /// generation still live.
    pub fn fail_next_rebuild(&self, message: &str) {
        *self.fail_rebuild.lock().expect("fail_rebuild mutex") = Some(message.to_string());
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
    async fn validate(
        &self,
        spec: &DynamicBlockSpec,
        _artifact: &[u8],
    ) -> Result<ValidatedGuest, ValidationFailure> {
        self.validate_result
            .lock()
            .expect("validate_result mutex")
            .clone()
            .map(|()| ValidatedGuest {
                info: wafer_run::BlockInfo::new(
                    &spec.name,
                    "0.0.0",
                    "http-handler@v1",
                    "fake validated guest",
                ),
            })
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
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn runtime_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}
