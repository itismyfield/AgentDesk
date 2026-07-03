use super::*;

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
