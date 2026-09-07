use super::*;

pub(in crate::services::discord) async fn release_restored_watcher_active_turn_before_panel_edit(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    key: Option<crate::services::discord::turn_finalizer::TurnKey>,
) -> bool {
    let Some(key) = key else {
        return false;
    };
    let channel_id = key.channel_id;
    let finalizer_turn_id = key.user_msg_id;
    if finalizer_turn_id == 0 {
        return false;
    }

    // #4106 review-fix (codex): snapshot the channel role override THIS turn owns
    // BEFORE any await. The removal below runs after awaits (mailbox finish +
    // clear_watchdog_deadline_override), during which a fresh same-channel
    // counter-model follow-up can insert its OWN override (intake_turn.rs) even
    // before it claims the slot. A bare channel-keyed remove would clobber that;
    // remove_owned_role_override only drops the value we still own.
    let pre_release_role_override =
        crate::services::discord::turn_finalizer::cleanup::snapshot_role_override(
            shared, channel_id,
        );

    let finish = match crate::services::discord::turn_finalizer::claim_normal_episode(
        shared, provider, key, false,
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

    crate::services::discord::turn_finalizer::cleanup::clear_watchdog_and_kick_thread_parents_after_turn_release(
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

pub(super) fn watcher_completion_key(
    shared: &SharedData,
    channel_id: ChannelId,
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
    current_offset: u64,
) -> Option<crate::services::discord::turn_finalizer::TurnKey> {
    let finalizer_turn_id = pinned_finalizer_turn_id(state, tmux_session_name, current_offset);
    state
        .filter(|row| {
            finalizer_turn_id != 0 && row.effective_finalizer_turn_id() == finalizer_turn_id
        })
        .map(|row| {
            crate::services::discord::turn_finalizer::TurnKey::new(
                channel_id,
                finalizer_turn_id,
                shared.restart.current_generation,
            )
            .with_episode_nonce(row.turn_nonce.as_deref())
        })
}

pub(super) fn note_watcher_terminal_projection_settled(
    shared: &Arc<SharedData>,
    key: Option<crate::services::discord::turn_finalizer::TurnKey>,
) {
    let Some(key) = key else {
        return;
    };
    shared
        .turn_finalizer
        .note_terminal_projection_settled(key, true, shared.clone());
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_watcher_terminal_footer_or_status_panel_with_sniffer<S, SniffFuture>(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    started_at_unix: i64,
    single_message_panel_footer_mode: bool,
    spin_idx: &mut usize,
    terminal_target: Option<WatcherCompletionFooterTerminalTarget>,
    placeholder_msg_id: Option<serenity::MessageId>,
    last_edit_text: &str,
    status_panel_msg_id: Option<serenity::MessageId>,
    last_status_panel_text: &mut String,
    task_notification_kind: Option<TaskNotificationKind>,
    tmux_session_name: Option<String>,
    sniff_background_agent_pending: S,
    status_panel_completion_user_msg_id: Option<u64>,
    turn_is_external_input_for_session: bool,
    turn_is_non_managed_tui_mirror: bool,
    two_message_status_panel_generation_superseded: bool,
) where
    S: FnOnce(Option<String>) -> SniffFuture,
    SniffFuture: std::future::Future<Output = bool>,
{
    let completion_background = matches!(
        task_notification_kind,
        Some(TaskNotificationKind::Background | TaskNotificationKind::MonitorAutoTurn)
    );
    let background_agent_pending = sniff_background_agent_pending(tmux_session_name).await;
    complete_watcher_terminal_footer_or_status_panel(
        http,
        shared,
        channel_id,
        provider,
        started_at_unix,
        single_message_panel_footer_mode,
        spin_idx,
        terminal_target,
        placeholder_msg_id,
        last_edit_text,
        status_panel_msg_id,
        last_status_panel_text,
        completion_background,
        background_agent_pending,
        status_panel_completion_user_msg_id,
        turn_is_external_input_for_session,
        turn_is_non_managed_tui_mirror,
        two_message_status_panel_generation_superseded,
    )
    .await;
}
