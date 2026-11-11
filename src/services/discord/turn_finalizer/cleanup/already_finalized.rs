//! A late duplicate terminal's own active-state cleanup, and what that cleanup itself removed.
use super::*;
use crate::services::discord::inflight::GuardedClearOutcome;
use crate::services::discord::turn_finalizer::{FinalizeOutcome, TurnFinalizer};

/// What a late duplicate's cleanup removed itself: the episode's row and its mailbox token.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct DuplicateCleanup {
    pub(in crate::services::discord) row_cleared: bool,
    pub(in crate::services::discord) token_released: bool,
}

/// Late `AlreadyFinalized` losers still perform guarded active-state cleanup.
/// This is narrower than `do_finalize`: only the same real turn id, or a row its own
/// release proves is that episode's, can lose mailbox/inflight state.
pub(in crate::services::discord::turn_finalizer) async fn already_finalized_active_state(
    key: TurnKey,
    provider: &ProviderKind,
    event: &TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
    witness: super::super::episode::RuntimeWitness<'_>,
) -> DuplicateCleanup {
    if key.user_msg_id == 0 {
        return DuplicateCleanup::default();
    }

    let Ok(expected_actor) = captured_recovery_actor(witness.claim_snapshot) else {
        return DuplicateCleanup::default();
    };
    // Recovery owns row retirement with its exact committed snapshot. A ledger
    // duplicate must not adopt the currently loaded row as cleanup authority;
    // only this call's own proven release may retire its episode's drifted row.
    let clear_inflight = expected_actor.is_none();
    let owned_role_override = snapshot_role_override(shared, key.channel_id);
    let captured = match super::super::episode::claim_episode(
        shared,
        provider,
        key,
        clear_inflight,
        expected_actor,
        Some(witness),
    )
    .await
    {
        Ok(captured) => captured,
        Err(()) => return DuplicateCleanup::default(),
    };
    let (finish, row_cleared) = if let Some(capture) = captured {
        capture.publish_release(shared, key);
        (capture.finish, capture.row_cleared)
    } else {
        let kept = clear_inflight_state_if_matches(provider, key.channel_id.get(), key.user_msg_id);
        super::super::episode::warn_same_id_row_kept(provider, key, kept);
        let finish = super::super::super::mailbox_finish_turn_if_matches(
            shared,
            provider,
            key.channel_id,
            serenity::model::id::MessageId::new(key.user_msg_id),
        )
        .await;
        (finish, kept == GuardedClearOutcome::Cleared)
    };
    let Some(token) = finish.removed_token.as_ref() else {
        return DuplicateCleanup {
            row_cleared,
            token_released: false,
        };
    };
    shared
        .turn_finalizer
        .note_mailbox_released(key, shared.clone());
    rearm_queue_backstop_after_mailbox_release(
        shared,
        provider,
        key.channel_id,
        finish.has_pending,
        "already_finalized_mailbox_release",
    )
    .await;

    if ctx.allow_completion_cleanup && !matches!(event, TerminalEvent::Cancel) {
        token.mark_completion_cleanup();
    }
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    super::super::super::saturating_decrement_global_active(shared);
    kick_thread_parents_after_turn_release(shared, provider, key.channel_id).await;
    if !finish.has_pending {
        remove_owned_role_override(shared, key.channel_id, owned_role_override);
    }
    DuplicateCleanup {
        row_cleared,
        token_released: true,
    }
}

impl TurnFinalizer {
    /// A recovery's terminal under its committed claim snapshot, also naming what a late
    /// duplicate's own cleanup removed, so the recovery knows whether it ended the row itself.
    pub(in crate::services::discord) async fn submit_recovery_terminal(
        &self,
        key: TurnKey,
        (provider, event, ctx): (ProviderKind, TerminalEvent, FinalizeContext),
        claim_snapshot: Option<SyntheticClaimSnapshot>,
        shared: Arc<SharedData>,
    ) -> (FinalizeOutcome, DuplicateCleanup) {
        let evidence = super::super::episode::TerminalEvidence::from_snapshot(claim_snapshot);
        self.submit_cleaning(key, provider, event, ctx, evidence, shared)
            .await
    }
}
