//! Managed-turn release bundle: mailbox release, active-count decrement, queue wake and owned
//! role-override cleanup, callable without body or status-panel handling.

use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::Ordering;

#[cfg(unix)]
use serenity::model::id::MessageId;

use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;

// Only unix terminal paths (tmux watcher, restart recovery) call this today.
#[cfg(unix)]
pub(in crate::services::discord) async fn release_restored_watcher_active_turn_before_panel_edit(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    key: Option<crate::services::discord::turn_finalizer::TurnKey>,
) -> bool {
    let Some(key) = key else {
        return false;
    };
    if crate::services::discord::recovery_engine::producer_resume::waiting_for_receipt(
        shared, provider, key,
    ) {
        return false;
    }
    let channel_id = key.channel_id;
    let finalizer_turn_id = key.user_msg_id;
    if finalizer_turn_id == 0 {
        return false;
    }

    // #4106 review-fix (codex): snapshot the channel role override THIS turn owns
    // BEFORE any await. The removal below runs after awaits (mailbox finish +
    // queue-parent handoff), during which a fresh same-channel
    // counter-model follow-up can insert its OWN override (intake_turn.rs) even
    // before it claims the slot. A bare channel-keyed remove would clobber that;
    // remove_owned_role_override only drops the value we still own.
    let pre_release_role_override =
        crate::services::discord::turn_finalizer::cleanup::snapshot_role_override(
            shared, channel_id,
        );

    // Managed queued intake needs the completed episode's row retired before the
    // panel await; other sources retain their existing terminal cleanup order.
    let clear_managed_inflight = crate::services::discord::inflight::load_inflight_state_read_only(
        provider,
        channel_id.get(),
    )
    .is_some_and(|row| row.turn_source == crate::services::discord::inflight::TurnSource::Managed);
    let finish = match crate::services::discord::turn_finalizer::claim_normal_episode(
        shared,
        provider,
        key,
        clear_managed_inflight,
        None,
    )
    .await
    {
        Ok(Some(captured)) => {
            captured.publish_release(shared, key);
            captured.finish
        }
        Ok(None) => {
            crate::services::discord::mailbox_finish_turn_if_matches(
                shared,
                provider,
                channel_id,
                MessageId::new(finalizer_turn_id),
            )
            .await
        }
        Err(()) => return false,
    };
    let Some(token) = finish.removed_token.as_ref() else {
        return false;
    };
    shared
        .turn_finalizer
        .note_mailbox_released(key, shared.clone());

    // #4106 review-fix: cancel the removed token, decrement the counter, AND run
    // the finalizer's D-side channel cleanup here. Hoisting the release ahead of
    // the awaited panel edit makes the LATE do_finalize see removed_token=None
    // and take the guarded-miss SKIP branch (finalize.rs), so without this the
    // cleanup would be dropped on every normal completion. Running it here is
    // safe: we release turn A into a still-idle channel (no newer turn has
    // claimed yet, since a follow-up needs cancel_token.is_none() which this
    // release just produced), so it cannot clobber a follow-up's channel state.
    // Mirrors the finalizer non-miss branch (finalize.rs D-section) and the
    // recovery release bundle (health/recovery.rs); voice drain is omitted
    // because the watcher finalize path sets drain_voice=false.
    token.cancelled.store(true, Ordering::Relaxed);
    crate::services::discord::saturating_decrement_global_active(shared);

    crate::services::discord::turn_finalizer::cleanup::kick_thread_parents_after_turn_release(
        shared, provider, channel_id,
    )
    .await;
    crate::services::discord::turn_finalizer::cleanup::rearm_queue_backstop_after_mailbox_release(
        shared,
        provider,
        channel_id,
        finish.has_pending,
        "watcher_pre_panel_mailbox_release",
    )
    .await;
    if !finish.has_pending {
        crate::services::discord::turn_finalizer::cleanup::remove_owned_role_override(
            shared,
            channel_id,
            pre_release_role_override,
        );
    }
    true
}

pub(in crate::services::discord) async fn rearm_queue_backstop_after_mailbox_release(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::model::id::ChannelId,
    has_pending: bool,
    reason: &'static str,
) -> bool {
    if !has_pending {
        return false;
    }
    // The channel-scoped scheduler coalesces duplicate release/late-cleanup arms,
    // and its worker rechecks the mailbox before every dispatch attempt.
    crate::services::discord::arm_slow_idle_queue_backstop_if_queue_nonempty(
        shared, provider, channel_id, reason,
    )
    .await
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use serenity::model::id::{ChannelId, MessageId, UserId};

    use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
    use crate::services::discord::turn_finalizer::{FinalizeContext, TerminalEvent, TurnKey};
    use crate::services::discord::{
        increment_global_active, mailbox_snapshot, mailbox_try_start_turn,
        make_shared_data_for_tests,
    };
    use crate::services::provider::{CancelToken, ProviderKind};

    /// A follow-up that claims the slot during the panel await keeps its token, active count and
    /// role override through the old turn's late finalize and a repeated release.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn release_counts_once_and_keeps_the_followup_turn_state() {
        with_isolated_runtime_root(|| async move {
            let shared = make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(987_4207);
            let (turn_a, turn_b) = (4_207_001, 4_207_002);
            let (override_a, override_b) = (ChannelId::new(555_4207), ChannelId::new(556_4207));
            let active = || shared.restart.global_active.load(Ordering::Relaxed);
            // A live turn elsewhere keeps the count above zero, so a double decrement shows.
            increment_global_active(&shared, "other_channel_turn");

            let token_a = Arc::new(CancelToken::new());
            let start_a = MessageId::new(turn_a);
            assert!(
                mailbox_try_start_turn(&shared, channel, token_a.clone(), UserId::new(42), start_a)
                    .await
            );
            increment_global_active(&shared, "turn_a_start");
            shared.dispatch.role_overrides.insert(channel, override_a);
            let key_a = TurnKey::new(channel, turn_a, shared.restart.current_generation);

            assert!(
                super::release_restored_watcher_active_turn_before_panel_edit(
                    &shared,
                    &provider,
                    Some(key_a),
                )
                .await
            );
            assert!(token_a.cancelled.load(Ordering::Relaxed));
            assert_eq!(active(), 1, "turn A's release decrements exactly once");
            assert!(!shared.dispatch.role_overrides.contains_key(&channel));

            // Turn B claims the slot and installs its own override while A's panel edit is pending.
            let token_b = Arc::new(CancelToken::new());
            let start_b = MessageId::new(turn_b);
            assert!(
                mailbox_try_start_turn(&shared, channel, token_b.clone(), UserId::new(43), start_b)
                    .await
            );
            increment_global_active(&shared, "turn_b_start");
            shared.dispatch.role_overrides.insert(channel, override_b);

            let late = FinalizeContext::watcher_after_pre_panel_release();
            let _ = shared
                .turn_finalizer
                .submit_terminal(
                    key_a,
                    provider.clone(),
                    TerminalEvent::Complete,
                    late,
                    shared.clone(),
                )
                .await;
            assert!(
                !super::release_restored_watcher_active_turn_before_panel_edit(
                    &shared,
                    &provider,
                    Some(key_a),
                )
                .await,
                "a repeated release of turn A finds no token to release"
            );

            assert_eq!(active(), 2, "turn A is never released a second time");
            assert!(!token_b.cancelled.load(Ordering::Relaxed));
            let live = mailbox_snapshot(&shared, channel).await.cancel_token;
            assert!(live.is_some_and(|token| Arc::ptr_eq(&token, &token_b)));
            let kept = shared
                .dispatch
                .role_overrides
                .get(&channel)
                .map(|entry| *entry.value());
            assert_eq!(kept, Some(override_b));
        })
        .await;
    }
}
