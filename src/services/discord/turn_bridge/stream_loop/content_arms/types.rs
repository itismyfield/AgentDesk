use super::*;

pub(in super::super) enum StreamContentArmMessage {
    RetryBoundary,
    Init {
        session_id: String,
        raw_session_id: Option<String>,
    },
    Text {
        content: String,
    },
    Thinking {
        summary: Option<String>,
    },
    Done {
        result: String,
        session_id: Option<String>,
    },
    Error {
        message: String,
        stderr: String,
    },
    ActiveUsageSnapshot {
        model: Option<String>,
        input_tokens: u64,
        cache_create_tokens: u64,
        cache_read_tokens: u64,
    },
    StatusUpdate {
        input_tokens: Option<u64>,
        cache_create_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    StatusEvents {
        events: Vec<StatusEvent>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in super::super) enum StreamContentArmOutcome {
    ContinueDraining,
    SkipRemainderOfDrainIteration,
}

pub(in super::super) struct StreamContentArmContext<'a> {
    pub(in super::super) shared_owned: &'a Arc<SharedData>,
    pub(in super::super) gateway: &'a Arc<dyn TurnGateway>,
    pub(in super::super) channel_id: ChannelId,
    pub(in super::super) provider: &'a ProviderKind,
    pub(in super::super) voice_progress_playback_channel_id: Option<ChannelId>,
    pub(in super::super) watcher_owns_assistant_relay: bool,
    pub(in super::super) watcher_relay_available_for_turn: bool,
    pub(in super::super) standby_relay_owns_output: bool,
    pub(in super::super) terminal_control_ready_observed: bool,
    pub(in super::super) streaming_rollover_frozen_msg_ids: &'a Vec<MessageId>,
    pub(in super::super) context_compact_lower_bound_tokens: u64,
    pub(in super::super) context_window_tokens: u64,
    pub(in super::super) context_compact_percent: u64,
}

pub(in super::super) struct StreamContentArmState<'a> {
    pub(in super::super) state_dirty: &'a mut bool,
    pub(in super::super) full_response: &'a mut String,
    pub(in super::super) current_tool_line: &'a mut Option<String>,
    pub(in super::super) prev_tool_status: &'a mut Option<String>,
    pub(in super::super) last_tool_name: &'a mut Option<String>,
    pub(in super::super) last_tool_summary: &'a mut Option<String>,
    pub(in super::super) any_tool_used: &'a mut bool,
    pub(in super::super) has_post_tool_text: &'a mut bool,
    pub(in super::super) response_sent_offset: &'a mut usize,
    pub(in super::super) last_edit_text: &'a mut String,
    pub(in super::super) new_session_id: &'a mut Option<String>,
    pub(in super::super) new_raw_provider_session_id: &'a mut Option<String>,
    pub(in super::super) inflight_state: &'a mut InflightTurnState,
    pub(in super::super) transcript_events: &'a mut Vec<SessionTranscriptEvent>,
    pub(in super::super) session_handshake_seen: &'a mut bool,
    pub(in super::super) streamed_assistant_text_this_turn: &'a mut bool,
    pub(in super::super) last_assistant_text_line: &'a mut Option<String>,
    pub(in super::super) status_panel_dirty: &'a mut bool,
    pub(in super::super) recovery_retry: &'a mut bool,
    pub(in super::super) pending_long_running_open_after_state_save:
        &'a mut PendingLongRunningOpenAfterStateSave,
    pub(in super::super) long_running_placeholder_active: &'a mut LongRunningPlaceholderActive,
    pub(in super::super) pending_long_running_retarget_after_state_save:
        &'a mut PendingLongRunningRetargetAfterStateSave,
    pub(in super::super) terminal_full_replay_cleanup_msg_ids: &'a mut Vec<MessageId>,
    pub(in super::super) active_background_child_session_ids: &'a mut Vec<i64>,
    pub(in super::super) done: &'a mut bool,
    pub(in super::super) terminal_control_drain_until: &'a mut Option<std::time::Instant>,
    pub(in super::super) transport_error: &'a mut bool,
    pub(in super::super) tui_error_classification: &'a mut TuiErrorClassification,
    pub(in super::super) resume_failure_detected: &'a mut bool,
    pub(in super::super) bridge_confirmed_response_sent_offset: &'a mut usize,
    pub(in super::super) terminal_session_reset_required: &'a mut bool,
    pub(in super::super) accumulated_input_tokens: &'a mut u64,
    pub(in super::super) accumulated_cache_create_tokens: &'a mut u64,
    pub(in super::super) accumulated_cache_read_tokens: &'a mut u64,
    pub(in super::super) accumulated_output_tokens: &'a mut u64,
}
