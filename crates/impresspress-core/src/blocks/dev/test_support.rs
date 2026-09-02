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
    /// Bumped by every successful `rebuild`.
    generation: AtomicU64,
}

impl FakeControl {
    /// A control that accepts everything and starts at generation 0.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            rebuilt: Mutex::new(Vec::new()),
            validate_result: Mutex::new(Ok(())),
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
