//! One registry entry is one mailbox actor incarnation. A purge
//! (`registry_purge.rs`) can replace it, so follow-up to an accepted request is
//! bound to the handle that sent it, never re-resolved by channel. The re-mint
//! fence is the exception: every incarnation shares it (`remint_fence.rs`).

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::{
    ChannelMailboxHandle, ChannelMailboxRegistry, GLOBAL_CHANNEL_MAILBOXES,
    GLOBAL_RECOVERY_DONE_SIGNALS, RecoveryDoneSignal, spawn_channel_mailbox,
};

impl ChannelMailboxHandle {
    pub(crate) fn recovery_done(&self) -> &Arc<RecoveryDoneSignal> {
        &self.recovery_done
    }

    pub(crate) fn same_actor(&self, other: &Self) -> bool {
        self.sender.same_channel(&other.sender)
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
                // the handle can also unlink its signal.
                self.recovery_done.insert(channel_id, signal.clone());
                GLOBAL_RECOVERY_DONE_SIGNALS.insert(channel_id, signal);
                entry.insert(handle.clone());
                handle
            }
        };
        GLOBAL_CHANNEL_MAILBOXES.insert(channel_id, resolved.clone());
        resolved
    }

    /// #2443 — the recovery-done signal of the channel's live actor
    /// incarnation, creating the actor if none is registered. Each incarnation
    /// owns its signal, mirrored into `GLOBAL_RECOVERY_DONE_SIGNALS` when it is
    /// spawned so callers that only have a `ChannelId` can resolve it via
    /// `global_recovery_done`. Test-only: production follow-up marks
    /// [`ChannelMailboxHandle::recovery_done`] of the handle that sent the
    /// request, never the signal re-resolved by channel.
    #[cfg(test)]
    pub(crate) fn recovery_done(&self, channel_id: ChannelId) -> Arc<RecoveryDoneSignal> {
        self.handle(channel_id).recovery_done.clone()
    }
}
