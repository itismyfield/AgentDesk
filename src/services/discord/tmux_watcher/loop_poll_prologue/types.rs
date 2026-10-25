use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct WatcherSourceAuthority {
    pub(in crate::services::discord::tmux::tmux_watcher) source_file:
        crate::services::cluster::stream_relay::SourceFileIdentity,
    pub(in crate::services::discord::tmux::tmux_watcher) generation_mtime_ns: i64,
    pub(in crate::services::discord::tmux::tmux_watcher) reset_incarnation: u64,
    pub(in crate::services::discord::tmux::tmux_watcher) source_stamp:
        Option<crate::services::cluster::stream_relay::SourceStamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord::tmux::tmux_watcher) enum PollOutcome {
    ContinueWatcherLoop,
    /// #4911 R10: the 20 MB cap rewrote the jsonl from its head, so every byte
    /// still pending in the watcher's accumulation buffer belongs to a coordinate
    /// space that no longer exists. The root must drop that buffer before the next
    /// poll, otherwise pre-rotation bytes would be delivered under the NEW source
    /// incarnation captured after the rewrite.
    DiscardPendingBufferAndContinue,
    BreakWatcherLoop,
    OutputReady {
        data: Vec<u8>,
        data_start_offset: u64,
        epoch_snapshot: u64,
        source_authority: WatcherSourceAuthority,
    },
}

pub(in crate::services::discord::tmux::tmux_watcher) struct PollWatcherContext<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) http: &'a Arc<serenity::Http>,
    pub(in crate::services::discord::tmux::tmux_watcher) shared: &'a Arc<SharedData>,
    pub(in crate::services::discord::tmux::tmux_watcher) channel_id: ChannelId,
    pub(in crate::services::discord::tmux::tmux_watcher) watcher_provider: &'a ProviderKind,
    pub(in crate::services::discord::tmux::tmux_watcher) tmux_session_name: &'a str,
    pub(in crate::services::discord::tmux::tmux_watcher) output_path: &'a str,
    pub(in crate::services::discord::tmux::tmux_watcher) watcher_thread_channel_id: Option<u64>,
    pub(in crate::services::discord::tmux::tmux_watcher) watcher_instance_id: u64,
    pub(in crate::services::discord::tmux::tmux_watcher) host: &'a Arc<HostSnapshot>,
}

pub(in crate::services::discord::tmux::tmux_watcher) struct PollWatcherControls<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) cancel: &'a Arc<AtomicBool>,
    pub(in crate::services::discord::tmux::tmux_watcher) paused: &'a Arc<AtomicBool>,
    pub(in crate::services::discord::tmux::tmux_watcher) resume_offset:
        &'a Arc<std::sync::Mutex<Option<u64>>>,
    pub(in crate::services::discord::tmux::tmux_watcher) pause_epoch: &'a Arc<AtomicU64>,
    pub(in crate::services::discord::tmux::tmux_watcher) turn_delivered: &'a Arc<AtomicBool>,
    pub(in crate::services::discord::tmux::tmux_watcher) last_heartbeat_ts_ms: &'a Arc<AtomicI64>,
    pub(in crate::services::discord::tmux::tmux_watcher) jsonl_notify: &'a Arc<tokio::sync::Notify>,
    pub(in crate::services::discord::tmux::tmux_watcher) dead_marker_notify:
        &'a Arc<tokio::sync::Notify>,
}

pub(in crate::services::discord::tmux::tmux_watcher) struct RelayOffsetState<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) current_offset: &'a mut u64,
    pub(in crate::services::discord::tmux::tmux_watcher) terminal_delivery_observed: &'a mut bool,
    pub(in crate::services::discord::tmux::tmux_watcher) last_relayed_offset: &'a mut Option<u64>,
    pub(in crate::services::discord::tmux::tmux_watcher) last_observed_generation_mtime_ns:
        &'a mut Option<i64>,
    pub(in crate::services::discord::tmux::tmux_watcher) rotation_tick: &'a mut u32,
    pub(in crate::services::discord::tmux::tmux_watcher) watcher_turn_identity:
        &'a mut Option<crate::services::discord::inflight::InflightTurnIdentity>,
    pub(in crate::services::discord::tmux::tmux_watcher) watcher_turn_nonce: &'a mut Option<String>,
}

pub(in crate::services::discord::tmux::tmux_watcher) struct LoopPollState<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) retained_source:
        &'a mut Option<std::sync::Arc<std::fs::File>>,
    pub(in crate::services::discord::tmux::tmux_watcher) prompt_too_long_killed: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) all_data: &'a String,
    pub(in crate::services::discord::tmux::tmux_watcher) utf8_decoder: &'a mut Utf8ChunkDecoder,
    pub(in crate::services::discord::tmux::tmux_watcher) completion_footer_idle:
        &'a mut WatcherCompletionFooterIdleState,
    pub(in crate::services::discord::tmux::tmux_watcher) last_activity_heartbeat_at:
        &'a mut Option<std::time::Instant>,
}

pub(in crate::services::discord::tmux::tmux_watcher) struct PostTerminalState<'a> {
    pub(in crate::services::discord::tmux::tmux_watcher) turn_result_relayed: bool,
    pub(in crate::services::discord::tmux::tmux_watcher) post_terminal_continuation_logged:
        &'a mut bool,
    pub(in crate::services::discord::tmux::tmux_watcher) last_post_terminal_suppressed_range:
        &'a mut Option<(u64, u64)>,
    pub(in crate::services::discord::tmux::tmux_watcher) active_stream_inflight_reacquire_logged:
        &'a mut bool,
    pub(in crate::services::discord::tmux::tmux_watcher) restored_turn:
        &'a Option<RestoredWatcherTurn>,
    pub(in crate::services::discord::tmux::tmux_watcher) restored_injected_prompt_message_id:
        Option<u64>,
}
