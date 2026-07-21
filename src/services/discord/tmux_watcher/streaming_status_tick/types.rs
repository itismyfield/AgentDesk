use super::*;

pub(in crate::services::discord::tmux::tmux_watcher) fn watcher_should_suppress_streaming_after_bridge_delivery(
    bridge_delivered_turn: bool,
    has_assistant_response: bool,
    observed_range: (u64, u64),
    committed_range: Option<(u64, u64)>,
) -> bool {
    if !bridge_delivered_turn || !has_assistant_response {
        return false;
    }
    committed_range.is_some_and(|(start, end)| {
        let (observed_start, observed_end) = observed_range;
        start <= observed_start && observed_start < observed_end && observed_end <= end
    })
}

pub(in crate::services::discord::tmux::tmux_watcher) struct StreamingStatusTickContext<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) http: &'a Arc<serenity::Http>,
    pub(in crate::services::discord::tmux::tmux_watcher) shared: &'a Arc<SharedData>,
    pub(in crate::services::discord::tmux::tmux_watcher) channel_id: serenity::ChannelId,
    pub(in crate::services::discord::tmux::tmux_watcher) watcher_provider: &'a ProviderKind,
    pub(in crate::services::discord::tmux::tmux_watcher) tmux_session_name: &'a String,
    pub(in crate::services::discord::tmux::tmux_watcher) output_path: &'a String,
    pub(in crate::services::discord::tmux::tmux_watcher) turn_delivered: &'a Arc<AtomicBool>,
}

pub(in crate::services::discord::tmux::tmux_watcher) struct StreamingStatusTickTurn<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) data_start_offset: u64,
    pub(in crate::services::discord::tmux::tmux_watcher) current_offset: u64,
    pub(in crate::services::discord::tmux::tmux_watcher) full_response: &'a String,
    pub(in crate::services::discord::tmux::tmux_watcher) tool_state: &'a WatcherToolState,
    pub(in crate::services::discord::tmux::tmux_watcher) task_notification_kind:
        Option<crate::services::agent_protocol::TaskNotificationKind>,
    pub(in crate::services::discord::tmux::tmux_watcher) status_panel_started_at: i64,
    pub(in crate::services::discord::tmux::tmux_watcher) single_message_panel_footer_mode: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) restored_injected_prompt_message_id:
        Option<u64>,
}

pub(in crate::services::discord::tmux::tmux_watcher) struct StreamingRenderState<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) last_status_update:
        &'a mut tokio::time::Instant,
    pub(in crate::services::discord::tmux::tmux_watcher) spin_idx: &'a mut usize,
    pub(in crate::services::discord::tmux::tmux_watcher) placeholder_msg_id:
        &'a mut Option<serenity::MessageId>,
    pub(in crate::services::discord::tmux::tmux_watcher) placeholder_from_restored_inflight:
        &'a mut bool,
    pub(in crate::services::discord::tmux::tmux_watcher) last_edit_text: &'a mut String,
    pub(in crate::services::discord::tmux::tmux_watcher) response_sent_offset: &'a mut usize,
    pub(in crate::services::discord::tmux::tmux_watcher) watcher_streaming_rollover_frozen_msg_ids:
        &'a mut Vec<serenity::MessageId>,
}

pub(in crate::services::discord::tmux::tmux_watcher) struct StatusPanelState<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) status_panel_msg_id:
        &'a mut Option<serenity::MessageId>,
    pub(in crate::services::discord::tmux::tmux_watcher) last_status_panel_text: &'a mut String,
}

pub(in crate::services::discord::tmux::tmux_watcher) struct StreamingSuppressState<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) turn_is_external_input_for_session:
        &'a mut bool,
    pub(in crate::services::discord::tmux::tmux_watcher) turn_identity_for_panel:
        &'a mut Option<crate::services::discord::inflight::InflightTurnIdentity>,
    pub(in crate::services::discord::tmux::tmux_watcher) streaming_suppressed_by_recent_stop:
        &'a mut bool,
    pub(in crate::services::discord::tmux::tmux_watcher) streaming_suppressed_by_missing_inflight:
        &'a mut bool,
    pub(in crate::services::discord::tmux::tmux_watcher) active_stream_inflight_reacquire_logged:
        &'a mut bool,
}

pub(in crate::services::discord::tmux::tmux_watcher) struct PanelGenerationState<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) this_turn_status_panel_generation:
        &'a mut u64,
}
