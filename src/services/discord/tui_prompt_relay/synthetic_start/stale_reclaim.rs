use super::*;

pub(super) const STALE_SYNTHETIC_MAILBOX_OWNER_MIN_AGE_SECS: i64 = 120;

#[derive(Clone, Copy)]
struct StaleMailboxRelease {
    had_pending_queue: bool,
}

async fn finalize_stale_mailbox_owner_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    active_user_message_id: MessageId,
) -> Option<StaleMailboxRelease> {
    let outcome = shared
        .turn_finalizer
        .submit_terminal(
            super::super::super::turn_finalizer::TurnKey::new(
                channel_id,
                active_user_message_id.get(),
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::super::turn_finalizer::TerminalEvent::Cancel,
            super::super::super::turn_finalizer::FinalizeContext::watcher(),
            shared.clone(),
        )
        .await;

    let super::super::super::turn_finalizer::FinalizeOutcome::Finalized {
        removed_token: Some(token),
        has_pending,
        ..
    } = outcome
    else {
        return None;
    };
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    Some(StaleMailboxRelease {
        had_pending_queue: has_pending,
    })
}

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

    let Some(release) = finalize_stale_mailbox_owner_if_current(
        shared,
        provider,
        channel_id,
        active_user_message_id,
    )
    .await
    else {
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
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        stale_user_message_id = active_user_message_id.get(),
        anchor_message_id = anchor_message_id.get(),
        global_active_decremented = true,
        had_pending_queue = release.had_pending_queue,
        "released stale ownerless TUI-direct mailbox before claiming new synthetic inflight"
    );
    true
}

#[derive(Clone, Copy)]
enum StaleSyntheticReclaimReason {
    OwnerInflightAbsent,
    OwnerInflightReplaced,
    OwnerInflightFinalized,
}

impl StaleSyntheticReclaimReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::OwnerInflightAbsent => "owner_inflight_absent",
            Self::OwnerInflightReplaced => "owner_inflight_replaced",
            Self::OwnerInflightFinalized => "owner_inflight_finalized",
        }
    }

    fn requires_positive_owner_age(self) -> bool {
        matches!(
            self,
            Self::OwnerInflightAbsent | Self::OwnerInflightReplaced
        )
    }
}

fn owner_age_permits_positive_stale_reclaim(
    turn_started_at: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    let Some(turn_started_at) = turn_started_at else {
        return false;
    };
    chrono::Utc::now()
        .signed_duration_since(turn_started_at)
        .num_seconds()
        >= STALE_SYNTHETIC_MAILBOX_OWNER_MIN_AGE_SECS
}

fn stale_synthetic_mailbox_owner_reclaim_reason(
    state: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
    active_user_message_id: MessageId,
) -> Option<StaleSyntheticReclaimReason> {
    let Some(state) = state else {
        return Some(StaleSyntheticReclaimReason::OwnerInflightAbsent);
    };
    if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return None;
    };
    if state.user_msg_id != active_user_message_id.get() {
        return Some(StaleSyntheticReclaimReason::OwnerInflightReplaced);
    }
    state
        .terminal_delivery_committed
        .then_some(StaleSyntheticReclaimReason::OwnerInflightFinalized)
}

pub(super) async fn release_reclaimable_stale_synthetic_mailbox_owner_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    active_user_message_id: MessageId,
    active_request_owner: Option<serenity::UserId>,
    active_turn_kind: crate::services::turn_orchestrator::ActiveTurnKind,
    turn_started_at: Option<chrono::DateTime<chrono::Utc>>,
    anchor_message_id: MessageId,
) -> bool {
    if active_request_owner != Some(serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID)) {
        return false;
    }
    if active_turn_kind.is_monitor_auto_turn() {
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
    if reason.requires_positive_owner_age()
        && !owner_age_permits_positive_stale_reclaim(turn_started_at)
    {
        tracing::debug!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            stale_user_message_id = active_user_message_id.get(),
            anchor_message_id = anchor_message_id.get(),
            reclaim_reason = reason.as_str(),
            min_owner_age_secs = STALE_SYNTHETIC_MAILBOX_OWNER_MIN_AGE_SECS,
            "skipping TUI-direct synthetic mailbox reclaim; owner age has not positively crossed the stale threshold"
        );
        return false;
    }

    let Some(release) = finalize_stale_mailbox_owner_if_current(
        shared,
        provider,
        channel_id,
        active_user_message_id,
    )
    .await
    else {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            stale_user_message_id = active_user_message_id.get(),
            anchor_message_id = anchor_message_id.get(),
            reclaim_reason = reason.as_str(),
            "TUI-direct stale synthetic mailbox reclaim skipped because mailbox identity changed"
        );
        return false;
    };
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        stale_user_message_id = active_user_message_id.get(),
        anchor_message_id = anchor_message_id.get(),
        reclaim_reason = reason.as_str(),
        global_active_decremented = true,
        had_pending_queue = release.had_pending_queue,
        "reclaimed stale TUI-direct synthetic mailbox owner before claiming new synthetic inflight"
    );
    true
}
