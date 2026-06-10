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
//!
//! #3297 round 2 (codex): the idle check is actor-mediated. The original
//! `snapshot().await`-then-unlink sequence left a TOCTOU window — a
//! `TryStartTurn` processed by the SAME actor between the idle snapshot and
//! the unlink activated a turn, and the unlink then severed that LIVE actor
//! from the registry/global mirrors. `CloseIfIdle` verifies idleness and sets
//! the `closed` tombstone in one serialized actor step; because the actor
//! processes its mailbox FIFO, every racing `TryStartTurn` lands either
//! BEFORE the verdict (live token ⇒ purge refused) or AFTER it (tombstone ⇒
//! start refused; the caller re-resolves a fresh actor via the registry).

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxRegistry, ChannelMailboxState,
    GLOBAL_CHANNEL_MAILBOXES, GLOBAL_RECOVERY_DONE_SIGNALS, GLOBAL_TURN_FINISHED_SIGNALS,
};

/// Actor-side verdict for [`ChannelMailboxMsg::CloseIfIdle`], invoked from the
/// mailbox actor loop while it exclusively owns `state` — the idle decision
/// and the tombstone write are therefore one atomic (actor-serialized) step.
/// Kept here, out of the ratchet-frozen module root, with the rest of the
/// purge logic. Gate order mirrors the original snapshot recheck.
pub(super) fn close_if_idle_verdict(state: &mut ChannelMailboxState) -> Result<(), &'static str> {
    if state.cancel_token.is_some() {
        return Err("live_cancel_token");
    }
    if !state.intervention_queue.is_empty() {
        return Err("queue_not_empty");
    }
    if state.recovery_started_at.is_some() {
        return Err("recovery_in_progress");
    }
    state.closed = true;
    Ok(())
}

impl ChannelMailboxHandle {
    /// Ask the actor to verify it is idle and, if so, tombstone itself
    /// (`Ok(())` ⇒ purgeable; `Err(reason)` ⇒ live work, purge refused).
    /// A dead actor (mailbox closed / reply dropped) can never start work
    /// again, so the request fallback treats it as trivially purgeable.
    async fn close_if_idle(&self) -> Result<(), &'static str> {
        self.request(|reply| ChannelMailboxMsg::CloseIfIdle { reply }, Ok(()))
            .await
    }
}

/// Outcome of [`ChannelMailboxRegistry::remove_idle_entry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MailboxPurgeOutcome {
    /// No registry entry existed for the channel — nothing to unlink.
    NoEntry,
    /// Entry existed, the actor-serialized `CloseIfIdle` verdict confirmed
    /// idle (tombstoning the actor against post-verdict starts), and the
    /// instance maps were unlinked. Global mirrors are unlinked only when
    /// they still point at the exact objects this instance verified idle
    /// (#3297 finding 5) — a mismatching mirror is skipped with a WARN.
    Removed,
    /// Live-work evidence appeared on the actor's `CloseIfIdle` verdict —
    /// refused, and the actor is left un-tombstoned.
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
    /// `no_live_work_evidence` gate chain.
    ///
    /// #3297 round 2 (codex): the final idle recheck is performed by the
    /// actor itself (`CloseIfIdle`), which tombstones the actor in the same
    /// serialized step — a `TryStartTurn` racing the unlink can therefore
    /// never activate the to-be-unlinked actor (see the module docs). Removal
    /// is an in-memory unlink only — the worst-case race outcome is a
    /// short-lived second actor for a channel, never data loss.
    pub(crate) async fn remove_idle_entry(&self, channel_id: ChannelId) -> MailboxPurgeOutcome {
        let Some(handle) = self.peek(channel_id) else {
            return MailboxPurgeOutcome::NoEntry;
        };
        if let Err(refusal) = handle.close_if_idle().await {
            return MailboxPurgeOutcome::RefusedLiveWork(refusal);
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

    /// #3297 round-2 red-green (codex TOCTOU finding): a `TryStartTurn`
    /// processed by the actor AFTER the purge's idle verdict — i.e. the
    /// interleaving where, pre-fix, the start landed between the idle
    /// `snapshot()` and the registry unlink — must be REFUSED. To the actor,
    /// "between verdict and unlink" and "after unlink" are indistinguishable
    /// (the unlink never touches the actor), so driving the start through a
    /// retained handle clone after `remove_idle_entry` pins exactly the
    /// post-snapshot interleaving deterministically. Pre-fix this test fails:
    /// the start returned `true`, activating a turn on an actor that the
    /// purge had just severed from the registry/global mirrors.
    #[tokio::test]
    async fn purged_actor_refuses_a_racing_try_start_turn() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(93_293_006);
        // The racing starter's retained handle clone (in production: a task
        // that resolved the handle before the purge, or a start already
        // queued in the actor mailbox behind the idle verdict).
        let stale_handle = registry.handle(channel);

        assert_eq!(
            registry.remove_idle_entry(channel).await,
            MailboxPurgeOutcome::Removed
        );

        let started = stale_handle
            .try_start_turn(
                Arc::new(CancelToken::new()),
                UserId::new(7),
                MessageId::new(11),
            )
            .await;
        assert!(
            !started,
            "a start racing the purge must be refused by the closed tombstone \
             (pre-fix it activated a turn on the unlinked actor)"
        );
        assert!(
            !stale_handle.has_active_turn().await,
            "the tombstoned actor must remain idle"
        );

        // The channel itself stays serviceable: a fresh registry resolution
        // mints a NEW actor that accepts work normally.
        let fresh_handle = registry.handle(channel);
        assert!(
            fresh_handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(7),
                    MessageId::new(12),
                )
                .await,
            "a freshly minted actor must accept work after the purge"
        );
        let _ = fresh_handle.hard_stop().await;
        GLOBAL_CHANNEL_MAILBOXES.remove(&channel);
    }

    /// Companion exhaustiveness check for the round-2 fix: the actor mailbox
    /// is FIFO, so EVERY racing `TryStartTurn` is processed strictly before
    /// or strictly after the `CloseIfIdle` verdict. Before ⇒ the verdict sees
    /// the live token and the purge is refused (actor untouched, no
    /// tombstone); after ⇒ the tombstone refuses the start (previous test).
    /// Together the two orderings leave no interleaving in which an ACTIVE
    /// actor is unlinked.
    #[tokio::test]
    async fn close_verdict_refuses_when_start_won_the_race() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(93_293_007);
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

        assert_eq!(handle.close_if_idle().await, Err("live_cancel_token"));
        assert_eq!(
            registry.remove_idle_entry(channel).await,
            MailboxPurgeOutcome::RefusedLiveWork("live_cancel_token")
        );

        // The refused verdict must NOT have tombstoned the actor: after the
        // live turn finishes, the same actor keeps serving starts.
        let _ = handle.hard_stop().await;
        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(7),
                    MessageId::new(12),
                )
                .await,
            "a refused purge must leave the actor fully operational"
        );
        let _ = handle.hard_stop().await;
        GLOBAL_CHANNEL_MAILBOXES.remove(&channel);
    }
}
