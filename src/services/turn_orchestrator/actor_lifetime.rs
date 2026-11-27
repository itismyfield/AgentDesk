use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{Notify, watch};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ActorEpoch(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Retired(pub(super) ActorEpoch);

pub(super) struct Retirement {
    epoch: ActorEpoch,
    retired: watch::Sender<Option<Retired>>,
}

impl Retirement {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            epoch: ActorEpoch(NEXT.fetch_add(1, Ordering::Relaxed)),
            retired: watch::channel(None).0,
        }
    }

    pub(super) fn retire(&self) {
        self.retired.send_if_modified(|status| {
            if status.is_some() {
                return false;
            }
            *status = Some(Retired(self.epoch));
            true
        });
    }

    #[cfg(test)]
    pub(super) fn epoch(&self) -> ActorEpoch {
        self.epoch
    }

    #[cfg(test)]
    fn subscribe(&self) -> watch::Receiver<Option<Retired>> {
        self.retired.subscribe()
    }
}

pub(super) struct RetireOnDrop(pub(super) Arc<RecoveryDoneSignal>);

impl Drop for RetireOnDrop {
    fn drop(&mut self) {
        self.0.retirement.retire();
    }
}

/// Incarnation-local latch for recovery completion and the separate retirement signal.
pub(crate) struct RecoveryDoneSignal {
    notify: Notify,
    latched: std::sync::atomic::AtomicBool,
    pub(super) retirement: Retirement,
}

impl RecoveryDoneSignal {
    pub(super) fn new() -> Self {
        Self {
            notify: Notify::new(),
            latched: std::sync::atomic::AtomicBool::new(false),
            retirement: Retirement::new(),
        }
    }

    /// Mark recovery as finished. Wakes all current waiters and latches the
    /// signal so subsequent `wait()` calls return immediately until `reset()`.
    pub(crate) fn mark_done(&self) {
        self.latched.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Reset the latch so the next recovery cycle starts clean. Should be
    /// called at recovery kickoff so an old "done" flag does not satisfy a
    /// watcher waiting for the new run.
    pub(crate) fn reset(&self) {
        self.latched.store(false, Ordering::Release);
    }

    /// Wait for completion, including a mark that arrived before subscription.
    pub(crate) async fn wait(&self) {
        if self.latched.load(Ordering::Acquire) {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        if self.latched.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::turn_orchestrator::ChannelMailboxRegistry;
    use poise::serenity_prelude::ChannelId;

    #[tokio::test]
    async fn drive_foundation_retirement_is_latched_on_the_exact_actor() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(7_272_845);
        let old = registry.handle(channel);
        let signal = &old.recovery_done().retirement;
        let epoch = signal.epoch();
        assert_eq!(old.clone().recovery_done().retirement.epoch(), epoch);
        let mut subscribed = signal.subscribe();
        assert!(subscribed.borrow().is_none());

        registry.remove_idle_entry(channel).await;

        assert!(!old.is_closed(), "the old sender is still held");
        assert!(subscribed.has_changed().unwrap());
        assert_eq!(*subscribed.borrow_and_update(), Some(Retired(epoch)));
        assert_eq!(*signal.subscribe().borrow(), Some(Retired(epoch)));

        let successor = registry.handle(channel);
        let next = &successor.recovery_done().retirement;
        assert!(!old.same_actor(&successor));
        assert_ne!(next.epoch(), epoch);
        assert!(next.subscribe().borrow().is_none());
        signal.retire();
        assert!(!subscribed.has_changed().unwrap());
        assert!(next.subscribe().borrow().is_none());
    }
}
