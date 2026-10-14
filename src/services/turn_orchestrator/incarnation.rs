//! One registry entry is one mailbox actor incarnation. A purge
//! (`registry_purge.rs`) can replace it, so follow-up to an accepted request is
//! bound to the handle that sent it, never re-resolved by channel. The re-mint
//! fence is the exception: every incarnation shares it (`remint_fence.rs`). The actor task and the
//! request round trip to it live here too.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;
use tokio::sync::{mpsc, oneshot};

use super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxRegistry, ChannelMailboxState,
    GLOBAL_CHANNEL_MAILBOXES, GLOBAL_RECOVERY_DONE_SIGNALS, MailboxUnreachable, RecoveryDoneSignal,
    claim_observation, input_mailbox_step, remint_fence,
};

impl ChannelMailboxHandle {
    pub(crate) fn recovery_done(&self) -> &Arc<RecoveryDoneSignal> {
        &self.recovery_done
    }

    pub(crate) fn same_actor(&self, other: &Self) -> bool {
        self.sender.same_channel(&other.sender)
    }

    pub(super) async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<T>) -> ChannelMailboxMsg,
    ) -> Result<T, MailboxUnreachable> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send((
                build(reply_tx),
                crate::services::discord::input_runtime::fence::effect::current(),
            ))
            .map_err(|_| MailboxUnreachable)?;
        reply_rx.await.map_err(|_| MailboxUnreachable)
    }
}

impl ChannelMailboxRegistry {
    pub(crate) fn handle(&self, channel_id: ChannelId) -> ChannelMailboxHandle {
        if let Some(existing) = self.handles.get(&channel_id) {
            return existing.clone();
        }

        let resolved = match self.handles.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let signal = Arc::new(RecoveryDoneSignal::new());
                let handle =
                    spawn_channel_mailbox(channel_id, self.fence_cell(channel_id), signal.clone());
                // Publish the signal before the handle, so a purge that can see
                // the handle can also unlink its signal. Never displace another
                // actor's global mirror: a recovery kickoff publishes its own.
                self.recovery_done.insert(channel_id, signal.clone());
                GLOBAL_RECOVERY_DONE_SIGNALS
                    .entry(channel_id)
                    .or_insert(signal);
                entry.insert(handle.clone());
                handle
            }
        };
        GLOBAL_CHANNEL_MAILBOXES.insert(channel_id, resolved.clone());
        resolved
    }

    /// #2443 — the recovery-done signal of the channel's live actor
    /// incarnation, creating the actor if none is registered, mirrored into
    /// `GLOBAL_RECOVERY_DONE_SIGNALS` as a recovery kickoff would. Test-only:
    /// production follow-up marks [`ChannelMailboxHandle::recovery_done`] of
    /// the handle that sent the request, never the signal re-resolved by channel.
    #[cfg(test)]
    pub(crate) fn recovery_done(&self, channel_id: ChannelId) -> Arc<RecoveryDoneSignal> {
        let signal = self.handle(channel_id).recovery_done.clone();
        GLOBAL_RECOVERY_DONE_SIGNALS.insert(channel_id, signal.clone());
        signal
    }
}

pub(super) fn spawn_channel_mailbox(
    channel_id: ChannelId,
    fence: remint_fence::FenceCell,
    recovery_done: Arc<RecoveryDoneSignal>,
) -> ChannelMailboxHandle {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let own_recovery_done = recovery_done.clone();
    tokio::spawn(async move {
        let mut state = ChannelMailboxState {
            claim_log: claim_observation::ClaimLog::spawned(channel_id),
            remint_fence: fence,
            ..Default::default()
        };
        while let Some((msg, input_permit)) = rx.recv().await {
            if crate::services::discord::input_runtime::fence::channel_gate(channel_id.get())
                .is_some()
            {
                let signal = own_recovery_done.clone();
                state = tokio::task::spawn_blocking(move || {
                    crate::services::discord::input_runtime::fence::effect::synchronous(
                        input_permit,
                        || {
                            crate::services::discord::input_runtime::fence::blocking(|| {
                                input_mailbox_step(state, channel_id, msg, &signal)
                            })
                        },
                    )
                })
                .await
                .expect("input mailbox step panicked");
            } else {
                state = crate::services::discord::input_runtime::fence::effect::synchronous(
                    input_permit,
                    || input_mailbox_step(state, channel_id, msg, &own_recovery_done),
                );
            }
        }
    });
    ChannelMailboxHandle {
        sender: tx,
        recovery_done,
    }
}
