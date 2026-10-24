//! A test actor in front of a live mailbox actor: it signals the first Snapshot, answers it via
//! the live actor, then reads nothing more until released, so later reads wait at a known point.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use poise::serenity_prelude::ChannelId;
use tokio::sync::{mpsc, oneshot};

use super::super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxRegistry, GLOBAL_CHANNEL_MAILBOXES,
};

/// How long a test waits for a barrier signal before it fails instead of hanging.
const SIGNAL_WAIT: Duration = Duration::from_secs(6);

/// The installed barrier. Dropping it ends the test actor and puts the live actor back.
pub(crate) struct HealthBarrier {
    handles: Arc<DashMap<ChannelId, ChannelMailboxHandle>>,
    channel: ChannelId,
    actor: ChannelMailboxHandle,
    front: ChannelMailboxHandle,
    received: Option<oneshot::Receiver<()>>,
    answered: Option<oneshot::Receiver<()>>,
    release: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl ChannelMailboxRegistry {
    /// Puts a barrier in front of `channel`'s live actor; it must be called inside a runtime.
    pub(crate) fn install_health_barrier_for_test(&self, channel: ChannelId) -> HealthBarrier {
        let actor = self
            .peek(channel)
            .expect("a live actor to put the barrier in front of");
        let (received_tx, received) = oneshot::channel();
        let (answered_tx, answered) = oneshot::channel();
        let (release, released) = oneshot::channel::<()>();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let live = actor.clone();
        let task = tokio::spawn(async move {
            let (mut received_tx, mut answered_tx) = (Some(received_tx), Some(answered_tx));
            while let Some(msg) = receiver.recv().await {
                let ChannelMailboxMsg::Snapshot { reply } = msg else {
                    let _ = live.sender.send(msg);
                    continue;
                };
                if let Some(received_tx) = received_tx.take() {
                    let _ = received_tx.send(());
                }
                if let Ok(snapshot) = live.try_snapshot().await {
                    let _ = reply.send(snapshot);
                }
                if let Some(answered_tx) = answered_tx.take() {
                    let _ = answered_tx.send(());
                    // Only the permit moves the actor on; a dropped permit also releases it.
                    let _ = released.await;
                    break;
                }
            }
            while let Some(msg) = receiver.recv().await {
                let _ = live.sender.send(msg);
            }
        });
        let front = ChannelMailboxHandle {
            sender,
            recovery_done: actor.recovery_done.clone(),
        };
        self.handles.insert(channel, front.clone());
        GLOBAL_CHANNEL_MAILBOXES.insert(channel, front.clone());
        HealthBarrier {
            handles: self.handles.clone(),
            channel,
            actor,
            front,
            received: Some(received),
            answered: Some(answered),
            release: Some(release),
            task,
        }
    }
}

impl HealthBarrier {
    /// The live actor itself, which the barrier does not hold.
    pub(crate) fn actor(&self) -> &ChannelMailboxHandle {
        &self.actor
    }

    /// Waits until the first Snapshot has reached the test actor, before the live actor sees it.
    pub(crate) async fn first_received(&mut self) {
        wait_for(self.received.take(), "the first Snapshot's arrival").await;
    }

    /// Waits until the test actor has answered its first Snapshot and stopped reading.
    pub(crate) async fn first_answered(&mut self) {
        wait_for(self.answered.take(), "its first answer").await;
    }

    /// Lets the test actor read again; every later message goes straight to the live actor.
    pub(crate) fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

/// Fails the test when `signal` does not come within `SIGNAL_WAIT`.
async fn wait_for(signal: Option<oneshot::Receiver<()>>, what: &str) {
    let signal = signal.unwrap_or_else(|| panic!("{what} awaited twice"));
    match tokio::time::timeout(SIGNAL_WAIT, signal).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => panic!("the test actor ended before {what}"),
        Err(_) => panic!("the test actor gave no signal of {what} within {SIGNAL_WAIT:?}"),
    }
}

impl Drop for HealthBarrier {
    fn drop(&mut self) {
        self.release();
        self.task.abort();
        let restore = |slot: &mut ChannelMailboxHandle| {
            if slot.same_actor(&self.front) {
                *slot = self.actor.clone();
            }
        };
        if let Some(mut slot) = self.handles.get_mut(&self.channel) {
            restore(&mut slot);
        }
        if let Some(mut slot) = GLOBAL_CHANNEL_MAILBOXES.get_mut(&self.channel) {
            restore(&mut slot);
        }
    }
}
