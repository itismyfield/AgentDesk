//! Shared input policy and generic supervisor task lifecycle.

use super::reconcile::HoldCause;
use super::{Ports, REGISTRY, Refused, Registry, Supervisor, reconcile};

#[path = "runtime/drive.rs"]
pub(crate) mod drive;
#[path = "runtime/notice.rs"]
pub(crate) mod notice;
#[path = "runtime/ports.rs"]
pub(crate) mod ports;
pub(in crate::services::discord) use ports::source_policy_facts;

pub(crate) fn registry() -> &'static Registry {
    &REGISTRY
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "runtime/dormant_tests.rs"]
mod dormant_tests;

impl<P: Ports> Supervisor<P> {
    pub(crate) fn prepare_handoff(&mut self) {
        self.registration
            .report(&HoldCause::RuntimeViewPending, true);
    }

    pub(super) fn adopt_task(&mut self, channel: u64) -> Result<(), Refused> {
        if channel != self.config.channel
            || self.registration.key != (self.config.provider.as_str().to_owned(), channel)
        {
            return Err(Refused::Mismatch);
        }
        if self.adopted {
            return Err(Refused::Duplicate);
        }
        let entries = self
            .registration
            .registry
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if entries
            .get(&self.registration.key)
            .is_none_or(|entry| entry.poisoned)
        {
            return Err(Refused::Poisoned);
        }
        self.adopted = true;
        Ok(())
    }

    /// One Notice per cause and key in a supervisor's life; a failed send stays unsent.
    pub(super) async fn notify(&mut self, key: Option<u64>, reason: &'static str) {
        let episode = (key, reconcile::topic(reason));
        if self.sent.contains(&episode) {
            return;
        }
        let reason = *self.pending_notices.entry(episode).or_insert(reason);
        if self.ports.notice(reconcile::notice(key, reason)).await {
            self.sent.insert(episode);
            self.pending_notices.remove(&episode);
        }
    }

    pub(super) async fn retry_notices(&mut self) {
        let pending: Vec<_> = self
            .pending_notices
            .iter()
            .map(|(&(key, _), &reason)| (key, reason))
            .collect();
        for (key, reason) in pending {
            self.notify(key, reason).await;
        }
    }
}
