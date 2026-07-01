use super::*;

pub(in crate::services::discord::tui_prompt_relay) async fn release_stale_ownerless_tui_direct_mailbox_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    active_user_message_id: MessageId,
    anchor_message_id: MessageId,
) -> bool {
    let Some(state) =
        super::super::super::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return false;
    };
    if state.user_msg_id != active_user_message_id.get()
        || state.tmux_session_name.as_deref() != Some(tmux_session_name)
        || !super::super::super::inflight::ownerless_external_input_inflight_is_stale(&state)
    {
        return false;
    }

    let finish = super::super::super::mailbox_finish_turn_if_matches(
        shared,
        provider,
        channel_id,
        active_user_message_id,
    )
    .await;
    let Some(token) = finish.removed_token.as_ref() else {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            stale_user_message_id = active_user_message_id.get(),
            anchor_message_id = anchor_message_id.get(),
            "TUI-direct stale ownerless mailbox release skipped because mailbox identity changed"
        );
        return false;
    };
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let global_active_decremented = super::super::super::saturating_decrement_global_active(shared);
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        stale_user_message_id = active_user_message_id.get(),
        anchor_message_id = anchor_message_id.get(),
        global_active_decremented,
        had_pending_queue = finish.has_pending,
        "released stale ownerless TUI-direct mailbox before claiming new synthetic inflight"
    );
    true
}

fn stale_synthetic_mailbox_owner_reclaim_reason(
    state: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
    active_user_message_id: MessageId,
) -> Option<&'static str> {
    let Some(state) = state else {
        return Some("owner_inflight_absent");
    };
    if state.user_msg_id != active_user_message_id.get() {
        return Some("owner_inflight_replaced");
    }
    if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return None;
    }
    state
        .terminal_delivery_committed
        .then_some("owner_inflight_finalized")
}

pub(super) async fn release_reclaimable_stale_synthetic_mailbox_owner_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    active_user_message_id: MessageId,
    active_request_owner: Option<serenity::UserId>,
    anchor_message_id: MessageId,
) -> bool {
    if active_request_owner != Some(serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID)) {
        return false;
    }
    let state = super::super::super::inflight::load_inflight_state(provider, channel_id.get());
    let Some(reason) = stale_synthetic_mailbox_owner_reclaim_reason(
        state.as_ref(),
        tmux_session_name,
        active_user_message_id,
    ) else {
        return false;
    };

    let finish = super::super::super::mailbox_finish_turn_if_matches(
        shared,
        provider,
        channel_id,
        active_user_message_id,
    )
    .await;
    let Some(token) = finish.removed_token.as_ref() else {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            stale_user_message_id = active_user_message_id.get(),
            anchor_message_id = anchor_message_id.get(),
            reclaim_reason = reason,
            "TUI-direct stale synthetic mailbox reclaim skipped because mailbox identity changed"
        );
        return false;
    };
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let global_active_decremented = super::super::super::saturating_decrement_global_active(shared);
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        stale_user_message_id = active_user_message_id.get(),
        anchor_message_id = anchor_message_id.get(),
        reclaim_reason = reason,
        global_active_decremented,
        had_pending_queue = finish.has_pending,
        "reclaimed stale TUI-direct synthetic mailbox owner before claiming new synthetic inflight"
    );
    true
}
