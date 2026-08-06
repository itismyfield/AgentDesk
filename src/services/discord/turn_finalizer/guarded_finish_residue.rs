use std::sync::Arc;

use serenity::model::id::{ChannelId, MessageId};

use super::SharedData;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxSnapshot;

pub(super) type GuardedFinishResidues = dashmap::DashMap<ChannelId, GuardedFinishResidue>;

impl super::TurnFinalizer {
    pub(in crate::services::discord) const fn guarded_finish_residues(
        &self,
    ) -> &GuardedFinishResidues {
        &self.guarded_finish_residues
    }
}

/// A guarded finalize that could not release the mailbox owner it observed.
///
/// This is deliberately richer than a log line: the actor reconciler keeps
/// retrying the exact observed owner and health can distinguish residual
/// foreground ownership from ordinary queued work. Missing episode nonces are
/// fail-closed and therefore never authorize a release on their own.
///
/// KNOWN GAP (#5068 r2, deliberate): in-memory only, so a dcserver restart
/// drops any residue awaiting recovery. Restart rebuilds mailbox state from the
/// persisted inflight rows, so it does not strand the mailbox; what is lost is
/// the skip reason and owner identity #5068 §1 asked to keep durable. Follow-up
/// work, not smuggled into this fix.
#[derive(Clone, Debug)]
pub(in crate::services::discord) struct GuardedFinishResidue {
    pub(in crate::services::discord) expected_user_msg_id: u64,
    pub(in crate::services::discord) active_user_msg_id: u64,
    pub(in crate::services::discord) generation: u64,
    pub(in crate::services::discord) provider: ProviderKind,
    pub(in crate::services::discord) terminal_turn_nonce: Option<String>,
    pub(in crate::services::discord) active_turn_nonce: Option<String>,
    pub(in crate::services::discord) observed_before: std::time::Instant,
    pub(in crate::services::discord) allow_completion_cleanup: bool,
    pub(in crate::services::discord) drain_voice: bool,
    pub(in crate::services::discord) terminal_was_cancel: bool,
}

impl GuardedFinishResidue {
    pub(in crate::services::discord) const fn reason(&self) -> &'static str {
        "active_owner_identity_mismatch"
    }

    /// Does `snapshot` still show the exact foreground owner this residue
    /// recorded? A successor can reuse a Discord message id during recovery, so
    /// the nonce is mandatory alongside the id.
    pub(in crate::services::discord) fn matches_observed_owner(
        &self,
        snapshot: &ChannelMailboxSnapshot,
    ) -> bool {
        snapshot.cancel_token.is_some()
            && snapshot.active_user_message_id.map(|id| id.get()) == Some(self.active_user_msg_id)
            && snapshot.active_turn_nonce == self.active_turn_nonce
    }

    /// The recorded terminal provably belongs to the SAME episode the mailbox
    /// still anchors. Fail-closed: an absent or empty terminal nonce proves
    /// nothing, including against an owner that itself has no nonce.
    pub(super) fn same_terminal_episode(&self, snapshot: &ChannelMailboxSnapshot) -> bool {
        self.terminal_turn_nonce
            .as_deref()
            .filter(|nonce| !nonce.is_empty())
            .is_some_and(|terminal_nonce| {
                snapshot.active_turn_nonce.as_deref() == Some(terminal_nonce)
            })
    }

    /// #5068 r2 — THE release-authority question, asked in exactly one place.
    /// The reconciler ACTS on it and health REPORTS it; a second spelling is the
    /// #5052 failure mode (health once read `None`/`None` nonces as "same
    /// episode", fail-open, where this is fail-closed).
    pub(in crate::services::discord) fn release_authorized(
        &self,
        snapshot: &ChannelMailboxSnapshot,
    ) -> bool {
        self.same_terminal_episode(snapshot)
            || snapshot
                .cancel_token
                .as_ref()
                .is_some_and(|token| token.cancelled.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// Release the exact residual episode after the reconciler has proved it
/// terminal. The mailbox actor rechecks owner id, episode nonce, and the
/// pre-observation start cutoff atomically, closing both the different-id and
/// same-id-successor races before it removes the token.
pub(super) async fn release(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    residue: &GuardedFinishResidue,
) -> bool {
    let owned_role_override = super::cleanup::snapshot_role_override(shared, channel_id);
    let finish = super::super::mailbox_finish_turn_if_matches_episode_started_before(
        shared,
        &residue.provider,
        channel_id,
        MessageId::new(residue.active_user_msg_id),
        residue.active_turn_nonce.clone(),
        residue.observed_before,
    )
    .await;
    let Some(token) = finish.removed_token.as_ref() else {
        return false;
    };

    if residue.allow_completion_cleanup && !residue.terminal_was_cancel {
        token.mark_completion_cleanup();
    }
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    super::super::saturating_decrement_global_active(shared);
    super::cleanup::clear_watchdog_and_kick_thread_parents_after_turn_release(
        shared,
        &residue.provider,
        channel_id,
    )
    .await;

    let voice_deferred_enqueued = if residue.drain_voice {
        shared
            .voice_barge_in
            .drain_deferred_after_turn(shared, &residue.provider, channel_id)
            .await
    } else {
        false
    };
    let has_pending = finish.has_pending || voice_deferred_enqueued;
    if !has_pending {
        super::cleanup::remove_owned_role_override(shared, channel_id, owned_role_override);
    }
    super::cleanup::rearm_queue_backstop_after_mailbox_release(
        shared,
        &residue.provider,
        channel_id,
        has_pending,
        "guarded_finish_residue_release",
    )
    .await;
    true
}
