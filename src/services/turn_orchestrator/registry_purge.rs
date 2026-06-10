//! #3293 (c): channel-mailbox registry hygiene.
//!
//! Two additions, kept out of the (ratchet-frozen) `turn_orchestrator.rs`
//! module root:
//!
//! * [`ChannelMailboxRegistry::peek`] — a NON-creating lookup. Health/repair
//!   probes previously used `handle()`, which mints a permanent mailbox actor
//!   + registry entry for every probed channel id, so a probe against a
//!   non-existent (bogus) channel polluted the registry forever.
//! * [`ChannelMailboxRegistry::remove_idle_entry`] — operator-gated in-memory
//!   unlink of an idle entry across all six maps (instance `handles` /
//!   `recovery_done` / `turn_finished` + the three process-global mirrors).
//!   No disk or DB state is touched; the actor task ends naturally once the
//!   last `ChannelMailboxHandle` sender is dropped.

use poise::serenity_prelude::ChannelId;

use super::{
    ChannelMailboxHandle, ChannelMailboxRegistry, GLOBAL_CHANNEL_MAILBOXES,
    GLOBAL_RECOVERY_DONE_SIGNALS, GLOBAL_TURN_FINISHED_SIGNALS,
};

/// Outcome of [`ChannelMailboxRegistry::remove_idle_entry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MailboxPurgeOutcome {
    /// No registry entry existed for the channel — nothing to unlink.
    NoEntry,
    /// Entry existed, the final actor-snapshot recheck confirmed idle, and
    /// every map was unlinked.
    Removed,
    /// Live-work evidence appeared on the final snapshot recheck — refused.
    RefusedLiveWork(&'static str),
}

impl ChannelMailboxRegistry {
    /// Non-creating lookup: returns the existing handle for `channel_id`, or
    /// `None`. Unlike [`ChannelMailboxRegistry::handle`] this NEVER spawns a
    /// mailbox actor or inserts registry/global entries — safe for probes.
    pub(crate) fn peek(&self, channel_id: ChannelId) -> Option<ChannelMailboxHandle> {
        self.handles
            .get(&channel_id)
            .map(|entry| entry.value().clone())
    }

    /// Remove the channel's registry entry IF (and only if) the mailbox actor
    /// is verifiably idle at the moment of removal: no cancel token, empty
    /// intervention queue, no recovery in progress. Callers (the repair API)
    /// have already passed the CAS `expected_has_cancel_token` +
    /// `no_live_work_evidence` gate chain; this final snapshot recheck closes
    /// the remaining race window. Removal is an in-memory unlink only — the
    /// worst-case race outcome is a short-lived second actor for a channel,
    /// never data loss.
    pub(crate) async fn remove_idle_entry(&self, channel_id: ChannelId) -> MailboxPurgeOutcome {
        let Some(handle) = self.peek(channel_id) else {
            return MailboxPurgeOutcome::NoEntry;
        };
        let snapshot = handle.snapshot().await;
        if snapshot.cancel_token.is_some() {
            return MailboxPurgeOutcome::RefusedLiveWork("live_cancel_token");
        }
        if !snapshot.intervention_queue.is_empty() {
            return MailboxPurgeOutcome::RefusedLiveWork("queue_not_empty");
        }
        if snapshot.recovery_started_at.is_some() {
            return MailboxPurgeOutcome::RefusedLiveWork("recovery_in_progress");
        }
        self.handles.remove(&channel_id);
        self.recovery_done.remove(&channel_id);
        self.turn_finished.remove(&channel_id);
        GLOBAL_CHANNEL_MAILBOXES.remove(&channel_id);
        GLOBAL_RECOVERY_DONE_SIGNALS.remove(&channel_id);
        GLOBAL_TURN_FINISHED_SIGNALS.remove(&channel_id);
        tracing::warn!(
            channel = channel_id.get(),
            "mailbox registry entry purged (operator repair; in-memory unlink only)"
        );
        MailboxPurgeOutcome::Removed
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    use super::super::{
        ChannelMailboxRegistry, GLOBAL_CHANNEL_MAILBOXES, GLOBAL_RECOVERY_DONE_SIGNALS,
        GLOBAL_TURN_FINISHED_SIGNALS,
    };
    use super::MailboxPurgeOutcome;
    use crate::services::provider::CancelToken;

    // The GLOBAL_* maps are process-wide; every test here uses a unique
    // channel id (93293xxx block) so parallel tests cannot collide.

    #[tokio::test]
    async fn peek_never_creates_an_entry() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(93_293_001);

        assert!(registry.peek(channel).is_none());
        assert!(
            registry.handles.is_empty(),
            "peek must not insert into the instance handle map"
        );
        assert!(
            !GLOBAL_CHANNEL_MAILBOXES.contains_key(&channel),
            "peek must not insert into the global handle map"
        );

        // And it returns the existing handle once one exists.
        let _ = registry.handle(channel);
        assert!(registry.peek(channel).is_some());
        GLOBAL_CHANNEL_MAILBOXES.remove(&channel);
    }

    #[tokio::test]
    async fn remove_idle_entry_noops_when_no_entry_exists() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(93_293_002);
        assert_eq!(
            registry.remove_idle_entry(channel).await,
            MailboxPurgeOutcome::NoEntry
        );
    }

    #[tokio::test]
    async fn remove_idle_entry_refuses_live_cancel_token() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(93_293_003);
        let handle = registry.handle(channel);
        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(7),
                    MessageId::new(11),
                )
                .await
        );

        let outcome = registry.remove_idle_entry(channel).await;
        assert_eq!(
            outcome,
            MailboxPurgeOutcome::RefusedLiveWork("live_cancel_token")
        );
        assert!(
            registry.peek(channel).is_some(),
            "refused purge must leave the entry in place"
        );
        GLOBAL_CHANNEL_MAILBOXES.remove(&channel);
    }

    #[tokio::test]
    async fn remove_idle_entry_unlinks_all_six_maps_when_idle() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(93_293_004);
        let _ = registry.handle(channel);
        let _ = registry.recovery_done(channel);
        let _ = registry.turn_finished(channel);
        assert!(GLOBAL_CHANNEL_MAILBOXES.contains_key(&channel));
        assert!(GLOBAL_RECOVERY_DONE_SIGNALS.contains_key(&channel));
        assert!(GLOBAL_TURN_FINISHED_SIGNALS.contains_key(&channel));

        assert_eq!(
            registry.remove_idle_entry(channel).await,
            MailboxPurgeOutcome::Removed
        );

        assert!(registry.handles.get(&channel).is_none());
        assert!(registry.recovery_done.get(&channel).is_none());
        assert!(registry.turn_finished.get(&channel).is_none());
        assert!(!GLOBAL_CHANNEL_MAILBOXES.contains_key(&channel));
        assert!(!GLOBAL_RECOVERY_DONE_SIGNALS.contains_key(&channel));
        assert!(!GLOBAL_TURN_FINISHED_SIGNALS.contains_key(&channel));
    }
}
