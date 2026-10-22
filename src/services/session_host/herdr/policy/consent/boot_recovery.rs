//! A process boot claim owns interruption authority until metadata recovery has finished.

use std::sync::atomic::{AtomicU8, Ordering};

pub(crate) struct BootState(AtomicU8);
impl BootState {
    pub(crate) const fn new() -> Self {
        Self(AtomicU8::new(0))
    }

    pub(crate) fn try_claim(&self) -> Option<BootRecovery<'_>> {
        self.0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| BootRecovery(self))
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.0.load(Ordering::Acquire) == 2
    }

    pub(crate) fn require_ready(&self) -> Result<(), String> {
        if self.is_ready() {
            Ok(())
        } else {
            Err("consent boot recovery is not complete; preserve held inputs".into())
        }
    }
}

pub(crate) struct BootRecovery<'a>(&'a BootState);
impl BootRecovery<'_> {
    pub(crate) fn check(&self) -> Result<(), String> {
        if self.0.0.load(Ordering::Acquire) == 1 {
            Ok(())
        } else {
            Err("consent boot recovery authority is no longer active".into())
        }
    }
    pub(crate) fn complete(self) {
        self.0.0.store(2, Ordering::Release);
    }

    pub(crate) fn authorizes(&self, state: &BootState) -> bool {
        std::ptr::eq(self.0, state) && self.check().is_ok()
    }
}
impl Drop for BootRecovery<'_> {
    fn drop(&mut self) {
        let _ = self
            .0
            .0
            .compare_exchange(1, 3, Ordering::AcqRel, Ordering::Acquire);
    }
}
