use super::*;

#[cfg(test)]
pub(in crate::services::discord) use super::cancel_handoff::completion::capture_actors;

// Watcher callers keep this path; the release bundle itself lives in the finalizer cleanup.
pub(in crate::services::discord) use crate::services::discord::turn_finalizer::cleanup::terminal_release::release_restored_watcher_active_turn_before_panel_edit;

pub(in crate::services::discord) fn watcher_completion_key(
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
pub(in crate::services::discord) async fn complete_watcher_terminal_footer_or_status_panel_with_sniffer<
    S,
    SniffFuture,
>(
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
    summary: Option<crate::services::discord::turn_end_summary::LiveSummary>,
    completion_actor: Option<&std::sync::Weak<crate::services::provider::CancelToken>>,
) where
    S: FnOnce(Option<String>) -> SniffFuture,
    SniffFuture: std::future::Future<Output = bool>,
{
    let completion_background = matches!(
        task_notification_kind,
        Some(TaskNotificationKind::Background | TaskNotificationKind::MonitorAutoTurn)
    );
    // Withhold body footers for O ownership or unresolved selected identities. A footer edit that
    // shows assistant text claims the channel as it is sent; chrome-only and no-op edits never do.
    let session = tmux_session_name.clone();
    if single_message_panel_footer_mode
        && crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel_tmux(
            channel_id.get(),
            session.as_deref(),
        )
        .unwrap_or(true)
    {
        return;
    }
    let body_claim =
        crate::services::tui_o::cutover::BodyClaim::tmux(channel_id.get(), session.as_deref());
    let summary = crate::services::discord::turn_end_summary::WatcherSummary::capture(
        summary,
        completion_actor,
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
        body_claim,
    )
    .await;
    crate::services::discord::turn_end_summary::post_watcher(http, shared, provider, summary).await;
}
