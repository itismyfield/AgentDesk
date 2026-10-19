use super::*;
use crate::services::cluster::stream_relay::{SourceFileIdentity, SourceWitness};
use std::ops::ControlFlow;

pub(super) struct StreamChunkState<'a> {
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) watcher_provider: &'a ProviderKind,
    pub(super) tmux_session_name: &'a str,
    pub(super) channel_id: ChannelId,
    pub(super) watcher_instance_id: u64,
    pub(super) watcher_thread_channel_id: Option<u64>,
    pub(super) source_authority: WatcherSourceAuthority,
    pub(super) retained_source: bool,
    pub(super) read_witness: Option<SourceWitness>,
    pub(super) chunk: &'a [u8],
    pub(super) off: u64,
    pub(super) file_identity: SourceFileIdentity,
    pub(super) current_offset: &'a mut u64,
    pub(super) last_activity_heartbeat_at: &'a mut Option<std::time::Instant>,
    pub(super) ready_for_input_tracker: &'a mut crate::services::provider::ReadyForInputIdleTracker,
    pub(super) utf8_decoder: &'a mut Utf8ChunkDecoder,
    pub(super) all_data: &'a mut String,
    pub(super) clean_decoded_provenance: &'a mut bool,
    pub(super) all_data_start_offset: &'a mut u64,
    pub(super) state: &'a mut StreamLineState,
    pub(super) full_response: &'a mut String,
    pub(super) tool_state: &'a mut WatcherToolState,
    pub(super) turn_identity_for_panel:
        &'a Option<crate::services::discord::inflight::InflightTurnIdentity>,
    pub(super) producer_registry: &'a Arc<RelayProducerRegistry>,
    pub(super) cached_relay_producer: &'a mut Option<RelayProducer>,
    pub(super) all_data_fully_mirrored_to_session_relay: &'a mut bool,
    pub(super) all_data_session_bound_relay_ack: &'a mut Option<SessionBoundRelayAckTarget>,
    pub(super) all_data_first_forwarded_relay_sequence: &'a mut Option<u64>,
    pub(super) session_bound_relay_turn_fully_mirrored: &'a mut bool,
    pub(super) session_bound_relay_turn_first_forwarded_sequence: &'a mut Option<u64>,
    pub(super) split_trailing_turn_follows: &'a mut bool,
    pub(super) last_output_at: &'a mut tokio::time::Instant,
    pub(super) single_message_panel_footer_mode: bool,
    pub(super) last_status_update: &'a mut tokio::time::Instant,
    pub(super) found_result: &'a mut bool,
    pub(super) terminal_kind: &'a mut Option<WatcherTerminalKind>,
    pub(super) terminal_evidence_offset: &'a mut Option<u64>,
    pub(super) soft_terminal_seen_at: &'a mut Option<tokio::time::Instant>,
    pub(super) is_prompt_too_long: &'a mut bool,
    pub(super) stale_resume_detected: &'a mut bool,
    pub(super) task_notification_kind: &'a mut Option<TaskNotificationKind>,
    pub(super) task_notification_context: &'a mut Option<
        crate::services::discord::task_notification_delivery::TaskNotificationContext,
    >,
    pub(super) assistant_text_seen: &'a mut bool,
    pub(super) fresh_assistant_text_seen: &'a mut bool,
}

// Apply one nonempty read before the monitor claim and streaming status phases.
pub(super) fn process_stream_chunk(
    phase: StreamChunkState<'_>,
) -> ControlFlow<(), (bool, SupervisorRelayForward)> {
    let StreamChunkState {
        shared,
        watcher_provider,
        tmux_session_name,
        channel_id,
        watcher_instance_id,
        watcher_thread_channel_id,
        source_authority,
        retained_source,
        read_witness,
        chunk,
        off,
        file_identity,
        current_offset,
        last_activity_heartbeat_at,
        ready_for_input_tracker,
        utf8_decoder,
        all_data,
        clean_decoded_provenance,
        all_data_start_offset,
        state,
        full_response,
        tool_state,
        turn_identity_for_panel,
        producer_registry,
        cached_relay_producer,
        all_data_fully_mirrored_to_session_relay,
        all_data_session_bound_relay_ack,
        all_data_first_forwarded_relay_sequence,
        session_bound_relay_turn_fully_mirrored,
        session_bound_relay_turn_first_forwarded_sequence,
        split_trailing_turn_follows,
        last_output_at,
        single_message_panel_footer_mode,
        last_status_update,
        found_result,
        terminal_kind,
        terminal_evidence_offset,
        soft_terminal_seen_at,
        is_prompt_too_long,
        stale_resume_detected,
        task_notification_kind,
        task_notification_context,
        assistant_text_seen,
        fresh_assistant_text_seen,
    } = phase;
    let authority = if retained_source {
        // This is the original opened descriptor, not a new file/marker
        // pair observed through a path that may have been replaced.
        WatcherSourceAuthority {
            source_file: file_identity,
            ..source_authority
        }
    } else {
        source_authority_for_read(
            source_authority,
            tmux_session_name,
            read_witness,
            file_identity,
        )
    };
    *current_offset = off;
    maybe_refresh_watcher_activity_heartbeat(
        shared.pg_pool.as_ref(),
        &shared.token_hash,
        watcher_provider,
        tmux_session_name,
        watcher_thread_channel_id,
        last_activity_heartbeat_at,
    );
    ready_for_input_tracker.record_output();
    let chunk_start = current_offset.saturating_sub(chunk.len() as u64);
    let decoded_chunk =
        utf8_decoder.decode_source_for_buffer(chunk, chunk_start, authority, all_data);
    let authority = authority_for_decoded_text(authority, decoded_chunk.mixed_read_provenance);
    *clean_decoded_provenance &= !decoded_chunk.mixed_read_provenance;
    // Defer forwarding until parsing attaches the terminal fence.
    let chunk_buffer_was_empty = all_data.is_empty();
    if chunk_buffer_was_empty {
        *all_data_start_offset = decoded_chunk.start_offset.unwrap_or(chunk_start);
    }
    if decoded_chunk.text.is_empty() && all_data.is_empty() {
        return ControlFlow::Break(());
    }
    all_data.push_str(&decoded_chunk.text);
    let chunk_buffer_start_offset = *all_data_start_offset;
    let chunk_buffer_len = all_data.len();
    observe_qwen_user_prompts_in_buffer(all_data, watcher_provider, tmux_session_name);
    let turn_terminal_start_offset = turn_identity_for_panel
        .as_ref()
        .and_then(|identity| identity.turn_start_offset)
        .unwrap_or(chunk_buffer_start_offset);
    let outcome = process_watcher_lines_for_turn(
        all_data,
        state,
        full_response,
        tool_state,
        Some(chunk_buffer_start_offset),
        Some(turn_terminal_start_offset),
    );
    #[cfg(test)]
    o_test::response_inner(watcher_provider.clone(), channel_id, full_response.clone());
    // #3041 P1-3 (Part a, B1): deferred forward of THIS streaming
    // chunk. `outcome.found_result` now tells us whether this is
    // the RESULT-bearing chunk; if so it rides a TERMINAL frame
    // carrying the commit fence (consumed_end + pinned identity).
    // E5 (#2412): every decoded chunk is still pushed into the
    // relay MPSC; only the terminality of the frame changed.
    let chunk_forward = forwarded_chunk(
        &decoded_chunk.text,
        chunk_buffer_len,
        chunk_buffer_start_offset,
        outcome.pre_turn_bytes_skipped,
        authority,
        watcher_instance_id,
    );
    let streaming_terminal_fence = watcher_terminal_commit_fence(
        outcome.found_result,
        chunk_buffer_start_offset,
        terminal_event_consumed_offset(*current_offset, all_data),
        turn_identity_for_panel.as_ref(),
        tmux_session_name,
    );
    let chunk_forwarded_to_session_relay = forward_turn_chunk_to_supervisor_relay(
        tmux_session_name,
        &chunk_forward,
        all_data.len(),
        producer_registry,
        cached_relay_producer,
        turn_identity_for_panel.as_ref(),
        streaming_terminal_fence,
    );
    apply_streaming_supervisor_relay_forward(
        all_data_fully_mirrored_to_session_relay,
        all_data_session_bound_relay_ack,
        all_data_first_forwarded_relay_sequence,
        session_bound_relay_turn_fully_mirrored,
        session_bound_relay_turn_first_forwarded_sequence,
        split_trailing_turn_follows,
        &chunk_forwarded_to_session_relay,
        chunk_buffer_was_empty,
        all_data.is_empty(),
        turn_identity_for_panel.as_ref(),
    );
    *last_output_at = tokio::time::Instant::now();
    *all_data_start_offset =
        advance_buffer_start_offset(chunk_buffer_start_offset, chunk_buffer_len, all_data.len());
    if watcher_live_events_dirty_should_force_status_update(
        flush_placeholder_live_events(shared, channel_id, tool_state),
        single_message_panel_footer_mode,
    ) {
        force_next_watcher_status_update(last_status_update);
    }
    *found_result = *found_result || outcome.found_result;
    if outcome.found_result {
        *terminal_kind = outcome.terminal_kind.or(*terminal_kind);
        *terminal_evidence_offset = outcome
            .terminal_evidence_offset
            .or(*terminal_evidence_offset);
    }
    if outcome.soft_terminal_candidate && soft_terminal_seen_at.is_none() {
        *soft_terminal_seen_at = Some(tokio::time::Instant::now());
        *terminal_kind = outcome
            .terminal_kind
            .or(*terminal_kind)
            .or(Some(WatcherTerminalKind::SoftStopHookSummary));
        *terminal_evidence_offset = outcome
            .terminal_evidence_offset
            .or(*terminal_evidence_offset);
    }
    *is_prompt_too_long = *is_prompt_too_long || outcome.is_prompt_too_long;
    *stale_resume_detected = *stale_resume_detected || outcome.stale_resume_detected;
    if let Some(kind) = outcome.task_notification_kind {
        *task_notification_kind = merge_task_notification_kind(*task_notification_kind, kind);
    }
    if let Some(context) = outcome.task_notification_context {
        *task_notification_context = merge_context(task_notification_context.take(), context);
    }
    *assistant_text_seen |= outcome.assistant_text_seen;
    *fresh_assistant_text_seen |= outcome.assistant_text_seen;
    #[cfg(test)]
    o_test::inner(
        watcher_provider.clone(),
        channel_id,
        *task_notification_kind,
    );
    ControlFlow::Continue((outcome.auto_compacted, chunk_forwarded_to_session_relay))
}
