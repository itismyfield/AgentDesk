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

use std::sync::Arc;

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
    /// the instance maps were unlinked. Global mirrors are unlinked only when
    /// they still point at the exact objects this instance verified idle
    /// (#3297 finding 5) — a mismatching mirror is skipped with a WARN.
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
        // Unlink the instance maps only when they still hold the exact
        // entries this purge verified: the handle that was snapshotted and
        // the signal Arcs the instance owns.
        self.handles.remove_if(&channel_id, |_, current| {
            current.sender.same_channel(&handle.sender)
        });
        let removed_recovery_done = self.recovery_done.remove(&channel_id);
        let removed_turn_finished = self.turn_finished.remove(&channel_id);
        // #3297 finding 5: the GLOBAL_* maps are process-wide single slots —
        // another registry instance may have published a DIFFERENT (possibly
        // busy) actor/signal for this channel after ours. The idle check above
        // only proved OUR objects idle, so unlink a global mirror entry only
        // when it still points at the exact object we verified; otherwise
        // skip it and WARN.
        let global_handle_removed = GLOBAL_CHANNEL_MAILBOXES
            .remove_if(&channel_id, |_, mirrored| {
                mirrored.sender.same_channel(&handle.sender)
            })
            .is_some();
        if !global_handle_removed && GLOBAL_CHANNEL_MAILBOXES.contains_key(&channel_id) {
            tracing::warn!(
                channel = channel_id.get(),
                "global mailbox mirror points at a different actor — mirror unlink skipped"
            );
        }
        if let Some((_, signal)) = removed_recovery_done {
            GLOBAL_RECOVERY_DONE_SIGNALS
                .remove_if(&channel_id, |_, mirrored| Arc::ptr_eq(mirrored, &signal));
        }
        if let Some((_, signal)) = removed_turn_finished {
            GLOBAL_TURN_FINISHED_SIGNALS
                .remove_if(&channel_id, |_, mirrored| Arc::ptr_eq(mirrored, &signal));
        }
        tracing::warn!(
            channel = channel_id.get(),
            global_handle_removed,
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

    /// #3297 finding-5 red-green: the global mirrors are process-wide single
    /// slots. When a SECOND registry instance has published a different
    /// (busy) actor for the same channel, purging the FIRST instance's idle
    /// entry must unlink only the instance maps — the global mirror pointing
    /// at the busy foreign actor must survive (pre-fix code removed it
    /// unconditionally on the instance-local idle verdict alone).
    #[tokio::test]
    async fn remove_idle_entry_skips_global_mirrors_owned_by_another_instance() {
        let registry_a = ChannelMailboxRegistry::default();
        let registry_b = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(93_293_005);

        // A registers first (its actor briefly owns the global slot)...
        let handle_a = registry_a.handle(channel);
        let _signal_a = registry_a.recovery_done(channel);
        // ...then B registers the same channel: B's actor + signal now own
        // the global mirrors (last-writer-wins), and B's actor is BUSY.
        let handle_b = registry_b.handle(channel);
        let signal_b = registry_b.recovery_done(channel);
        assert!(
            handle_b
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(7),
                    MessageId::new(11),
                )
                .await
        );
        assert!(
            !handle_a.sender.same_channel(&handle_b.sender),
            "test precondition: two distinct actors for the channel"
        );

        // Purging A's (idle) entry must NOT unlink B's busy global mirror.
        assert_eq!(
            registry_a.remove_idle_entry(channel).await,
            MailboxPurgeOutcome::Removed
        );
        assert!(
            registry_a.peek(channel).is_none(),
            "A's instance entry must be unlinked"
        );
        let surviving = ChannelMailboxRegistry::global_handle(channel)
            .expect("global mirror owned by B must survive A's purge");
        assert!(
            surviving.sender.same_channel(&handle_b.sender),
            "the surviving global mirror must still be B's actor"
        );
        let surviving_signal = ChannelMailboxRegistry::global_recovery_done(channel)
            .expect("global recovery-done signal owned by B must survive A's purge");
        assert!(Arc::ptr_eq(&surviving_signal, &signal_b));

        // Cleanup: direct global-map removal (same convention as the other
        // tests in this module) to keep the process-global maps clean.
        GLOBAL_CHANNEL_MAILBOXES.remove(&channel);
        GLOBAL_RECOVERY_DONE_SIGNALS.remove(&channel);
        GLOBAL_TURN_FINISHED_SIGNALS.remove(&channel);
    }
}
