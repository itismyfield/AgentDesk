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
//!
//! #3297 round 3 (codex): the tombstone gate covers EVERY start-like arm, not
//! just `TryStartTurn` — [`gate_closed_arm`] intercepts `RecoveryKickoff`,
//! `Enqueue`, and `RestoreActiveTurn` too, so no arm in the verdict→unlink
//! FIFO window can mint live work (or queue content) on an actor that is
//! about to be severed from the registry. Refused callers recover through the
//! `*_with_closed_retry` registry helpers below, which re-resolve a FRESH
//! actor (the unlink runs right after the verdict) and replay the request.

use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxRegistry, ChannelMailboxState,
    EnqueueInterventionResult, EnqueueRefusalReason, GLOBAL_CHANNEL_MAILBOXES,
    GLOBAL_RECOVERY_DONE_SIGNALS, GLOBAL_TURN_FINISHED_SIGNALS, Intervention,
    QueuePersistenceContext, RecoveryKickoffResult, TryStartTurnResult,
};
use crate::services::provider::CancelToken;

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
    // #3297 r3 — a `TakeNextSoft` head handed out for dispatch but not yet
    // claimed (`pending_user_dispatch`) IS live work: tombstoning during that
    // window would force the in-flight user turn onto its requeue/retry
    // fallbacks. Refuse, like any other live-work evidence.
    if state.pending_user_dispatch.is_some() {
        return Err("pending_user_dispatch");
    }
    state.closed = true;
    Ok(())
}

/// #3297 r3 (codex) — single tombstone gate run by the actor loop BEFORE the
/// arm match. When `state.closed` is set, every START-LIKE arm (class (a) in
/// the `ChannelMailboxMsg` classification docs) is answered here with its
/// arm's existing "cannot start" reply and `None` is returned so the loop
/// skips the match; all other arms pass through untouched. Keeping the
/// classification in ONE place (instead of per-arm `state.closed` checks)
/// makes "new arm ⇒ classify it" reviewable at a glance.
pub(super) fn gate_closed_arm(
    state: &ChannelMailboxState,
    msg: ChannelMailboxMsg,
) -> Option<ChannelMailboxMsg> {
    if !state.closed {
        return Some(msg);
    }
    match msg {
        // Mirrors the lost-race reply of a slot already held (#3297 r2).
        ChannelMailboxMsg::TryStartTurn { reply, .. } => {
            let _ = reply.send(TryStartTurnResult::default());
            None
        }
        // Fire-and-forget restore: the only refusal shape is a no-op ack.
        // (Dormant/test-only wrapper, but it binds a token — class (a).)
        ChannelMailboxMsg::RestoreActiveTurn { reply, .. } => {
            let _ = reply.send(());
            None
        }
        // Pre-fix this arm unconditionally bound the cancel token, marked
        // `recovery_started_at`, and (via the wrapper's `activated_turn`)
        // incremented `global_active` — live work on a severed actor.
        ChannelMailboxMsg::RecoveryKickoff { reply, .. } => {
            let _ = reply.send(RecoveryKickoffResult {
                activated_turn: false,
                refused_closed: true,
            });
            None
        }
        // Pre-fix this arm accepted (and disk-persisted) queue content that
        // the unlink then orphaned out of every registered-mailbox scan.
        ChannelMailboxMsg::Enqueue { reply, .. } => {
            let _ = reply.send(EnqueueInterventionResult {
                enqueued: false,
                merged: false,
                refusal_reason: Some(EnqueueRefusalReason::MailboxClosed),
                queue_exit_events: Vec::new(),
                persistence_error: None,
            });
            None
        }
        other => Some(other),
    }
}

/// Bounded attempts for the `*_with_closed_retry` helpers. A `MailboxClosed`
/// refusal means the registry resolved a tombstoned actor inside the tiny
/// verdict→unlink window of [`ChannelMailboxRegistry::remove_idle_entry`];
/// the unlink lands without further awaits, so one yield is normally enough
/// for `handle()` to mint a fresh actor. The bound only matters if a purge
/// future is dropped mid-removal (tombstoned entry never unlinked).
const CLOSED_RETRY_ATTEMPTS: usize = 3;

impl ChannelMailboxRegistry {
    /// #3297 r3 — enqueue that survives a purge-tombstone race: on a
    /// [`EnqueueRefusalReason::MailboxClosed`] refusal, re-resolve the channel
    /// through the registry (minting a fresh actor once the purge unlink
    /// lands) and replay. Any other outcome — success, dedup refusal,
    /// persistence error, actor-unreachable — is returned unchanged, so
    /// callers keep their existing semantics.
    pub(crate) async fn enqueue_with_closed_retry(
        &self,
        channel_id: ChannelId,
        intervention: Intervention,
        persistence: QueuePersistenceContext,
    ) -> EnqueueInterventionResult {
        for attempt in 1..=CLOSED_RETRY_ATTEMPTS {
            let result = self
                .handle(channel_id)
                .enqueue(intervention.clone(), persistence.clone())
                .await;
            if result.refusal_reason != Some(EnqueueRefusalReason::MailboxClosed) {
                return result;
            }
            if attempt == CLOSED_RETRY_ATTEMPTS {
                tracing::error!(
                    channel = channel_id.get(),
                    "enqueue still refused by a purge-tombstoned mailbox after retries"
                );
                return result;
            }
            tokio::task::yield_now().await;
        }
        unreachable!("loop always returns by the final attempt");
    }

    /// #3297 r3 — recovery kickoff with the same tombstone-refusal retry as
    /// [`Self::enqueue_with_closed_retry`], keyed on
    /// `RecoveryKickoffResult::refused_closed`.
    pub(crate) async fn recovery_kickoff_with_closed_retry(
        &self,
        channel_id: ChannelId,
        cancel_token: Arc<CancelToken>,
        request_owner: UserId,
        user_message_id: Option<MessageId>,
    ) -> RecoveryKickoffResult {
        for attempt in 1..=CLOSED_RETRY_ATTEMPTS {
            let result = self
                .handle(channel_id)
                .recovery_kickoff(cancel_token.clone(), request_owner, user_message_id)
                .await;
            if !result.refused_closed {
                return result;
            }
            if attempt == CLOSED_RETRY_ATTEMPTS {
                tracing::error!(
                    channel = channel_id.get(),
                    "recovery kickoff still refused by a purge-tombstoned mailbox after retries"
                );
                return result;
            }
            tokio::task::yield_now().await;
        }
        unreachable!("loop always returns by the final attempt");
    }
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
    use std::time::Instant;

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    use super::super::test_support::{AGENTDESK_ROOT_DIR_ENV, lock_test_env};
    use super::super::{
        ChannelMailboxRegistry, ChannelMailboxState, EnqueueRefusalReason,
        GLOBAL_CHANNEL_MAILBOXES, GLOBAL_RECOVERY_DONE_SIGNALS, GLOBAL_TURN_FINISHED_SIGNALS,
        Intervention, InterventionMode, QueuePersistenceContext,
    };
    use super::MailboxPurgeOutcome;
    use crate::services::provider::{CancelToken, ProviderKind};

    // The GLOBAL_* maps are process-wide; every test here uses a unique
    // channel id (93293xxx block) so parallel tests cannot collide.

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
        }
    }

    fn make_intervention(message_id: u64, text: &str) -> Intervention {
        Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    fn test_persistence(token_hash: &str) -> QueuePersistenceContext {
        QueuePersistenceContext::new(&ProviderKind::Claude, token_hash, None)
    }

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

    /// #3297 round-3 red-green (codex finding 1): a `RecoveryKickoff` that
    /// lands in the actor FIFO AFTER the purge verdict must be refused by the
    /// tombstone. Pre-fix the arm unconditionally bound the cancel token, set
    /// `recovery_started_at`, and replied `activated_turn = true` (which made
    /// the wrapper increment `global_active`) — live work on an actor the
    /// purge had just severed from the registry/global mirrors.
    #[tokio::test]
    async fn purged_actor_refuses_a_racing_recovery_kickoff() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(93_293_101);
        let stale_handle = registry.handle(channel);

        assert_eq!(
            registry.remove_idle_entry(channel).await,
            MailboxPurgeOutcome::Removed
        );

        let result = stale_handle
            .recovery_kickoff(
                Arc::new(CancelToken::new()),
                UserId::new(7),
                Some(MessageId::new(11)),
            )
            .await;
        assert!(
            result.refused_closed,
            "a kickoff racing the purge must be refused by the closed tombstone"
        );
        assert!(
            !result.activated_turn,
            "a refused kickoff must not report an activated turn \
             (pre-fix this incremented global_active for an unreachable actor)"
        );
        assert!(
            !stale_handle.has_active_turn().await,
            "the tombstoned actor must remain idle"
        );
        let snapshot = stale_handle.snapshot().await;
        assert!(
            snapshot.recovery_started_at.is_none(),
            "the tombstoned actor must not carry a recovery marker"
        );
        GLOBAL_CHANNEL_MAILBOXES.remove(&channel);
    }

    /// #3297 round-3 red-green (codex finding 2): an `Enqueue` that lands in
    /// the actor FIFO AFTER the purge verdict must be refused by the
    /// tombstone. Pre-fix the arm accepted the intervention into memory AND
    /// persisted it to the disk queue; the unlink then dropped that queue out
    /// of every registered-mailbox scan — orphaned until some later hydrate
    /// path happened to touch the channel.
    #[tokio::test]
    async fn purged_actor_refuses_a_racing_enqueue() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(93_293_102);
        let stale_handle = registry.handle(channel);

        assert_eq!(
            registry.remove_idle_entry(channel).await,
            MailboxPurgeOutcome::Removed
        );

        let result = stale_handle
            .enqueue(
                make_intervention(11, "post-verdict enqueue"),
                test_persistence("registry-purge-r3-refusal"),
            )
            .await;
        assert!(
            !result.enqueued,
            "an enqueue racing the purge must be refused by the closed tombstone \
             (pre-fix it was accepted and orphaned on the unlinked actor)"
        );
        assert_eq!(
            result.refusal_reason,
            Some(EnqueueRefusalReason::MailboxClosed)
        );
        assert!(
            result.persistence_error.is_none(),
            "the refusal happens before any disk write"
        );
        let snapshot = stale_handle.snapshot().await;
        assert!(
            snapshot.intervention_queue.is_empty(),
            "the tombstoned actor must not hold queue content"
        );
        GLOBAL_CHANNEL_MAILBOXES.remove(&channel);
    }

    /// #3297 r3 — the registry-level retry helper turns a tombstone refusal
    /// into a successful enqueue on a FRESHLY minted actor (the production
    /// path for the verdict→unlink FIFO window). A sync `#[test]` holds the
    /// shared env lock with no await in scope (the awaits run inside
    /// `block_on`), so no `await_holding_lock` allow is needed.
    #[test]
    fn enqueue_closed_retry_lands_on_a_fresh_actor_after_purge() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let registry = ChannelMailboxRegistry::default();
            let channel = ChannelId::new(93_293_103);
            let stale_handle = registry.handle(channel);
            assert_eq!(
                registry.remove_idle_entry(channel).await,
                MailboxPurgeOutcome::Removed
            );

            let result = registry
                .enqueue_with_closed_retry(
                    channel,
                    make_intervention(12, "retry onto fresh actor"),
                    test_persistence("registry-purge-r3-retry"),
                )
                .await;
            assert!(
                result.enqueued,
                "the retry helper must land the enqueue on a fresh registry actor"
            );
            assert!(result.persistence_error.is_none());

            // The intervention lives on the REGISTERED (fresh) actor — not on
            // the tombstone the stale handle still points at.
            let fresh_handle = registry.handle(channel);
            assert_eq!(fresh_handle.snapshot().await.intervention_queue.len(), 1);
            assert!(stale_handle.snapshot().await.intervention_queue.is_empty());

            // Drain the durable queue file before the tempdir goes away.
            let _ = fresh_handle
                .purge_queue(test_persistence("registry-purge-r3-retry"), false)
                .await;
            GLOBAL_CHANNEL_MAILBOXES.remove(&channel);
        });
    }

    /// #3297 r3 — the retry helpers are BOUNDED: when a tombstoned actor is
    /// never unlinked (a purge future dropped between verdict and unlink),
    /// the helper gives up after its fixed attempts and surfaces the refusal
    /// instead of spinning. Tombstoning the actor directly (no unlink) pins
    /// the registry to the tombstone for every retry deterministically.
    #[tokio::test]
    async fn closed_retry_helpers_give_up_when_tombstone_never_unlinks() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(93_293_104);
        let handle = registry.handle(channel);
        assert_eq!(handle.close_if_idle().await, Ok(()));

        let enqueue = registry
            .enqueue_with_closed_retry(
                channel,
                make_intervention(13, "never accepted"),
                test_persistence("registry-purge-r3-bounded"),
            )
            .await;
        assert!(!enqueue.enqueued);
        assert_eq!(
            enqueue.refusal_reason,
            Some(EnqueueRefusalReason::MailboxClosed)
        );

        let kickoff = registry
            .recovery_kickoff_with_closed_retry(
                channel,
                Arc::new(CancelToken::new()),
                UserId::new(7),
                None,
            )
            .await;
        assert!(kickoff.refused_closed);
        assert!(!kickoff.activated_turn);
        assert!(!handle.has_active_turn().await);
        GLOBAL_CHANNEL_MAILBOXES.remove(&channel);
    }

    /// #3297 r3 — kickoff twin of the enqueue retry test: after the purge
    /// unlink, the retry helper resolves a fresh actor that ACCEPTS the
    /// recovery kickoff (anchoring the recovery turn on the registered
    /// mailbox, where cancel/dedup gates can see it).
    #[tokio::test]
    async fn recovery_kickoff_closed_retry_lands_on_a_fresh_actor() {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(93_293_105);
        let _ = registry.handle(channel);
        assert_eq!(
            registry.remove_idle_entry(channel).await,
            MailboxPurgeOutcome::Removed
        );

        let result = registry
            .recovery_kickoff_with_closed_retry(
                channel,
                Arc::new(CancelToken::new()),
                UserId::new(7),
                Some(MessageId::new(11)),
            )
            .await;
        assert!(!result.refused_closed);
        assert!(result.activated_turn);
        let fresh_handle = registry.handle(channel);
        assert!(fresh_handle.has_active_turn().await);

        let _ = fresh_handle.hard_stop().await;
        GLOBAL_CHANNEL_MAILBOXES.remove(&channel);
    }

    /// #3297 r3 — a `TakeNextSoft` head handed out for dispatch but not yet
    /// claimed (`pending_user_dispatch` reservation) is live work: the idle
    /// verdict must refuse to tombstone during that dequeue→claim window.
    #[test]
    fn close_verdict_refuses_during_dequeue_dispatch_window() {
        let mut state = ChannelMailboxState {
            pending_user_dispatch: Some(MessageId::new(11)),
            ..ChannelMailboxState::default()
        };
        assert_eq!(
            super::close_if_idle_verdict(&mut state),
            Err("pending_user_dispatch")
        );
        assert!(!state.closed, "a refused verdict must not tombstone");

        state.pending_user_dispatch = None;
        assert_eq!(super::close_if_idle_verdict(&mut state), Ok(()));
        assert!(state.closed);
    }
}
