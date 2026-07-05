use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};

use poise::serenity_prelude::ChannelId;
use tokio::sync::Notify;

/// #2424 — generic "active turn finished" signal per channel.
///
/// Same latch shape as `RecoveryDoneSignal`: a terminal mailbox transition
/// can happen before a deferred monitor auto-turn subscribes, so late
/// subscribers must observe the already-finished state without falling back
/// to mailbox-state polling.
pub(crate) struct TurnFinishedSignal {
    notify: Notify,
    latched: std::sync::atomic::AtomicBool,
}

impl TurnFinishedSignal {
    fn new() -> Self {
        Self {
            notify: Notify::new(),
            latched: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn mark_done(&self) {
        self.latched.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub(crate) fn reset(&self) {
        self.latched.store(false, Ordering::Release);
    }

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

pub(super) static GLOBAL_TURN_FINISHED_SIGNALS: LazyLock<
    dashmap::DashMap<ChannelId, Arc<TurnFinishedSignal>>,
> = LazyLock::new(dashmap::DashMap::new);

pub(super) fn turn_finished_signal(channel_id: ChannelId) -> Arc<TurnFinishedSignal> {
    if let Some(existing) = GLOBAL_TURN_FINISHED_SIGNALS.get(&channel_id) {
        return existing.value().clone();
    }
    let signal = Arc::new(TurnFinishedSignal::new());
    match GLOBAL_TURN_FINISHED_SIGNALS.entry(channel_id) {
        dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            entry.insert(signal.clone());
            signal
        }
    }
}

pub(super) fn reset_turn_finished_signal(channel_id: ChannelId) {
    turn_finished_signal(channel_id).reset();
}

pub(super) fn mark_turn_finished_signal_done(channel_id: ChannelId) {
    turn_finished_signal(channel_id).mark_done();
}
