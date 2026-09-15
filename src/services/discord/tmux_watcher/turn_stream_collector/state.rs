//! State carried by the watcher collector, including cooperative continuation.
use super::*;

#[allow(clippy::large_enum_variant)]
pub(in crate::services::discord::tmux::tmux_watcher) enum CollectOutcome {
    ContinueWatcherLoop,
    Fallthrough(CollectedTurnStream),
}

pub(in crate::services::discord::tmux::tmux_watcher) struct TurnStreamCollectorContext {
    pub(in crate::services::discord::tmux::tmux_watcher) http: Arc<serenity::Http>,
    pub(in crate::services::discord::tmux::tmux_watcher) shared: Arc<SharedData>,
    pub(in crate::services::discord::tmux::tmux_watcher) channel_id: ChannelId,
    pub(in crate::services::discord::tmux::tmux_watcher) watcher_provider: ProviderKind,
    pub(in crate::services::discord::tmux::tmux_watcher) tmux_session_name: String,
    pub(in crate::services::discord::tmux::tmux_watcher) output_path: String,
    pub(in crate::services::discord::tmux::tmux_watcher) input_fifo_path: String,
    pub(in crate::services::discord::tmux::tmux_watcher) watcher_thread_channel_id: Option<u64>,
    pub(in crate::services::discord::tmux::tmux_watcher) cancel: Arc<AtomicBool>,
    pub(in crate::services::discord::tmux::tmux_watcher) paused: Arc<AtomicBool>,
    pub(in crate::services::discord::tmux::tmux_watcher) pause_epoch: Arc<AtomicU64>,
    pub(in crate::services::discord::tmux::tmux_watcher) turn_delivered: Arc<AtomicBool>,
    pub(in crate::services::discord::tmux::tmux_watcher) last_heartbeat_ts_ms: Arc<AtomicI64>,
    pub(in crate::services::discord::tmux::tmux_watcher) jsonl_notify: Arc<tokio::sync::Notify>,
    pub(in crate::services::discord::tmux::tmux_watcher) dead_marker_notify:
        Arc<tokio::sync::Notify>,
    pub(in crate::services::discord::tmux::tmux_watcher) turn_result_relayed: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) restored_injected_prompt_message_id:
        Option<u64>,
}

pub(in crate::services::discord::tmux::tmux_watcher) struct TurnStreamCollectorIo {
    pub(in crate::services::discord::tmux::tmux_watcher) data: Vec<u8>,
    pub(in crate::services::discord::tmux::tmux_watcher) data_start_offset: u64,
    pub(in crate::services::discord::tmux::tmux_watcher) epoch_snapshot: u64,
    pub(in crate::services::discord::tmux::tmux_watcher) source_authority: WatcherSourceAuthority,
}

pub(in crate::services::discord::tmux::tmux_watcher) struct TurnParseState<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) retained_source:
        &'a Option<Arc<std::fs::File>>,
    pub(in crate::services::discord::tmux::tmux_watcher) continuation:
        &'a mut Option<CollectedTurnStream>,
    pub(in crate::services::discord::tmux::tmux_watcher) current_offset: &'a mut u64,
    pub(in crate::services::discord::tmux::tmux_watcher) all_data: &'a mut String,
    pub(in crate::services::discord::tmux::tmux_watcher) all_data_start_offset: &'a mut u64,
    pub(in crate::services::discord::tmux::tmux_watcher) utf8_decoder: &'a mut Utf8ChunkDecoder,
    pub(in crate::services::discord::tmux::tmux_watcher) pending_terminal_rewind_seed:
        &'a mut Option<RestoredWatcherTurn>,
    pub(in crate::services::discord::tmux::tmux_watcher) restored_turn:
        &'a mut Option<RestoredWatcherTurn>,
    pub(in crate::services::discord::tmux::tmux_watcher) terminal_rewind_attempt_key:
        &'a mut Option<WatcherRewindAttemptKey>,
    pub(in crate::services::discord::tmux::tmux_watcher) terminal_rewind_attempts: &'a mut u8,
    pub(in crate::services::discord::tmux::tmux_watcher) watcher_turn_identity:
        &'a Option<crate::services::discord::inflight::InflightTurnIdentity>,
    pub(in crate::services::discord::tmux::tmux_watcher) last_activity_heartbeat_at:
        &'a mut Option<std::time::Instant>,
    pub(in crate::services::discord::tmux::tmux_watcher) active_stream_inflight_reacquire_logged:
        &'a mut bool,
}

pub(in crate::services::discord::tmux::tmux_watcher) struct SupervisorRelayState<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) producer_registry:
        &'a Arc<RelayProducerRegistry>,
    pub(in crate::services::discord::tmux::tmux_watcher) cached_relay_producer:
        &'a mut Option<RelayProducer>,
    pub(in crate::services::discord::tmux::tmux_watcher) all_data_fully_mirrored_to_session_relay:
        &'a mut bool,
    pub(in crate::services::discord::tmux::tmux_watcher) all_data_session_bound_relay_ack:
        &'a mut Option<SessionBoundRelayAckTarget>,
    pub(in crate::services::discord::tmux::tmux_watcher) all_data_first_forwarded_relay_sequence:
        &'a mut Option<u64>,
}

#[derive(Default)]
pub(in crate::services::discord::tmux::tmux_watcher) struct MonitorAutoTurnState {
    pub(in crate::services::discord::tmux::tmux_watcher) monitor_auto_turn_claimed: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) monitor_auto_turn_deferred: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) monitor_auto_turn_finished: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) monitor_auto_turn_synthetic_msg_id:
        Option<MessageId>,
    pub(in crate::services::discord::tmux::tmux_watcher) monitor_auto_turn_ledger_generation:
        Option<u64>,
}

#[derive(Default)]
pub(in crate::services::discord::tmux::tmux_watcher) struct RenderSeedState {
    pub(in crate::services::discord::tmux::tmux_watcher) placeholder_msg_id:
        Option<serenity::MessageId>,
    pub(in crate::services::discord::tmux::tmux_watcher) placeholder_from_restored_inflight: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) status_panel_msg_id:
        Option<serenity::MessageId>,
    pub(in crate::services::discord::tmux::tmux_watcher) last_status_panel_text: String,
    pub(in crate::services::discord::tmux::tmux_watcher) last_edit_text: String,
    pub(in crate::services::discord::tmux::tmux_watcher) response_sent_offset: usize,
    pub(in crate::services::discord::tmux::tmux_watcher) watcher_streaming_rollover_frozen_msg_ids:
        Vec<serenity::MessageId>,
    pub(in crate::services::discord::tmux::tmux_watcher) completion_footer_terminal_target:
        Option<WatcherCompletionFooterTerminalTarget>,
}

#[derive(Clone)]
pub(in crate::services::discord::tmux::tmux_watcher) struct ActiveReadState {
    pub(in crate::services::discord::tmux::tmux_watcher) turn_start: tokio::time::Instant,
    pub(in crate::services::discord::tmux::tmux_watcher) turn_timeout: std::time::Duration,
    pub(in crate::services::discord::tmux::tmux_watcher) turn_idle_timeout: std::time::Duration,
    pub(in crate::services::discord::tmux::tmux_watcher) last_output_at: tokio::time::Instant,
    pub(in crate::services::discord::tmux::tmux_watcher) tmux_death_observed: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) ready_for_input_failure_notice:
        Option<String>,
    pub(in crate::services::discord::tmux::tmux_watcher) ready_for_input_stall_dispatch_id:
        Option<String>,
    pub(in crate::services::discord::tmux::tmux_watcher) ready_for_input_stall_inflight_snapshot:
        Option<InflightTurnState>,
    pub(in crate::services::discord::tmux::tmux_watcher) fresh_ready_for_input_idle: bool,
}

#[derive(Clone)]
pub(in crate::services::discord::tmux::tmux_watcher) struct CollectedTurnStream {
    pub(in crate::services::discord::tmux::tmux_watcher) turn_data_start_offset: u64,
    pub(in crate::services::discord::tmux::tmux_watcher) source_authority: WatcherSourceAuthority,
    pub(in crate::services::discord::tmux::tmux_watcher) split_trailing_turn_follows: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) state: StreamLineState,
    pub(in crate::services::discord::tmux::tmux_watcher) restored_response_seed: String,
    pub(in crate::services::discord::tmux::tmux_watcher) full_response: String,
    pub(in crate::services::discord::tmux::tmux_watcher) tool_state: WatcherToolState,
    pub(in crate::services::discord::tmux::tmux_watcher) placeholder_msg_id:
        Option<serenity::MessageId>,
    pub(in crate::services::discord::tmux::tmux_watcher) placeholder_from_restored_inflight: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) status_panel_msg_id:
        Option<serenity::MessageId>,
    pub(in crate::services::discord::tmux::tmux_watcher) single_message_panel_footer_mode: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) startup_inflight_snapshot:
        Option<InflightTurnState>,
    pub(in crate::services::discord::tmux::tmux_watcher) completion_actor:
        Option<std::sync::Weak<crate::services::provider::CancelToken>>,
    pub(in crate::services::discord::tmux::tmux_watcher) this_turn_status_panel_generation: u64,
    pub(in crate::services::discord::tmux::tmux_watcher) turn_is_external_input_for_session: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) turn_identity_for_panel:
        Option<crate::services::discord::inflight::InflightTurnIdentity>,
    pub(in crate::services::discord::tmux::tmux_watcher) status_panel_started_at: i64,
    pub(in crate::services::discord::tmux::tmux_watcher) last_status_panel_text: String,
    pub(in crate::services::discord::tmux::tmux_watcher) last_edit_text: String,
    pub(in crate::services::discord::tmux::tmux_watcher) response_sent_offset: usize,
    pub(in crate::services::discord::tmux::tmux_watcher) watcher_streaming_rollover_frozen_msg_ids:
        Vec<serenity::MessageId>,
    pub(in crate::services::discord::tmux::tmux_watcher) finish_mailbox_on_completion: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) monitor_auto_turn_claimed: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) monitor_auto_turn_deferred: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) monitor_auto_turn_finished: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) monitor_auto_turn_synthetic_msg_id:
        Option<MessageId>,
    pub(in crate::services::discord::tmux::tmux_watcher) monitor_auto_turn_ledger_generation:
        Option<u64>,
    pub(in crate::services::discord::tmux::tmux_watcher) completion_footer_terminal_target:
        Option<WatcherCompletionFooterTerminalTarget>,
    pub(in crate::services::discord::tmux::tmux_watcher) session_bound_relay_turn_fully_mirrored:
        bool,
    pub(in crate::services::discord::tmux::tmux_watcher) session_bound_relay_turn_first_forwarded_sequence:
        Option<u64>,
    pub(in crate::services::discord::tmux::tmux_watcher) found_result: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) terminal_kind: Option<WatcherTerminalKind>,
    pub(in crate::services::discord::tmux::tmux_watcher) terminal_evidence_offset: Option<u64>,
    pub(in crate::services::discord::tmux::tmux_watcher) is_prompt_too_long: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) stale_resume_detected: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) task_notification_kind:
        Option<TaskNotificationKind>,
    pub(in crate::services::discord::tmux::tmux_watcher) task_notification_context:
        Option<crate::services::discord::task_notification_delivery::TaskNotificationContext>,
    pub(in crate::services::discord::tmux::tmux_watcher) assistant_text_seen: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) fresh_assistant_text_seen: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) was_paused: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) active_read_state: Option<ActiveReadState>,
    pub(in crate::services::discord::tmux::tmux_watcher) soft_terminal_seen_at:
        Option<tokio::time::Instant>,
    pub(in crate::services::discord::tmux::tmux_watcher) auto_compaction_lifecycle_attempted: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) monitor_auto_turn_preamble_injected: bool,
}

impl TurnStreamCollectorContext {
    pub(in crate::services::discord::tmux::tmux_watcher) fn from_poll(
        context: &PollWatcherContext<'_>,
        controls: &PollWatcherControls<'_>,
        input_fifo_path: &str,
        turn_result_relayed: bool,
        restored_injected_prompt_message_id: Option<u64>,
    ) -> Self {
        Self {
            http: context.http.clone(),
            shared: context.shared.clone(),
            channel_id: context.channel_id,
            watcher_provider: context.watcher_provider.clone(),
            tmux_session_name: context.tmux_session_name.into(),
            output_path: context.output_path.into(),
            input_fifo_path: input_fifo_path.into(),
            watcher_thread_channel_id: context.watcher_thread_channel_id,
            cancel: controls.cancel.clone(),
            paused: controls.paused.clone(),
            pause_epoch: controls.pause_epoch.clone(),
            turn_delivered: controls.turn_delivered.clone(),
            last_heartbeat_ts_ms: controls.last_heartbeat_ts_ms.clone(),
            jsonl_notify: controls.jsonl_notify.clone(),
            dead_marker_notify: controls.dead_marker_notify.clone(),
            turn_result_relayed,
            restored_injected_prompt_message_id,
        }
    }
}
