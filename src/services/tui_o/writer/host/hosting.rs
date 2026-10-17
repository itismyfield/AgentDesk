//! What a home's watch reads of a hosted writer before it trusts the channel to it: the claim that
//! hosts the channel and whether the host still runs.

use std::sync::atomic::Ordering;

use super::{ManagedWriterHandle, Readiness, locked};

/// Test-only mutant of a writer's liveness.
#[cfg(test)]
fn mutant(name: &str) -> bool {
    std::env::var("ADK_S3B_MUTANT").ok().as_deref() == Some(name)
}

impl Readiness {
    /// The generation whose claim hosts `channel`, read now. A claim is not a live host: one whose
    /// actor ended abnormally keeps it. No claim does mean no host, as each claims before it spawns.
    pub fn hosted_by(&self, channel: u64) -> Option<u64> {
        locked(&self.hosted).get(&channel).copied()
    }
}

impl ManagedWriterHandle {
    /// Whether the host task still runs and was not told to stop.
    pub fn live(&self) -> bool {
        #[cfg(test)]
        if mutant("writer_liveness_ignored") {
            return true;
        }
        !self.host.is_finished() && !*self.stop.signal.borrow()
    }

    /// Leaves the late reservation unsettled when this host ends, for a successor that takes it
    /// over; set before the stop is signalled.
    pub fn keep_reservation(&self) {
        self.keep.store(true, Ordering::SeqCst);
    }
}
