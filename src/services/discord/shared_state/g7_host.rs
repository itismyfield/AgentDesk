//! `SharedData` as the host the lower layers ask for: the session-transition leases and the intake
//! worker reach this runtime through these two traits only.

use std::sync::atomic::Ordering;

use super::SharedData;
use crate::services::cluster::intake_worker::{IntakeHost, IntakeWorkerLifecycle, ShutdownProbe};
use crate::services::discord::session_transition::{SessionTransitionHost, SessionTransitionLocks};

impl SessionTransitionHost for SharedData {
    fn transition_locks(&self) -> &SessionTransitionLocks {
        &self.session_transition_locks
    }

    fn transition_token_hash(&self) -> &str {
        &self.token_hash
    }
}

impl IntakeHost for SharedData {
    fn intake_lifecycle(&self) -> &IntakeWorkerLifecycle {
        &self.restart.intake_worker_lifecycle
    }

    fn shutdown_probe(&self) -> ShutdownProbe {
        let reader = self.restart.shutdown_reader();
        Box::new(move || reader.load(Ordering::Acquire))
    }
}

#[cfg(test)]
#[path = "g7_host_tests.rs"]
mod tests;
