//! #4230 S6 stream receive/drain loop and its cancel, handoff, tick, and latency gates.

use super::runtime_handoff_loop::{
    RuntimeHandoffLoopContext, RuntimeHandoffLoopMessage, RuntimeHandoffLoopState,
    handle_runtime_handoff_loop_message,
};
use super::stream_tick::{
    BridgeStreamTickContext, BridgeStreamTickState, StreamTickOutcome, run_bridge_stream_tick,
};
use super::{streaming_edit_text::TuiErrorClassification, *};
use content_arms::{
    StreamContentArmContext, StreamContentArmMessage, StreamContentArmOutcome,
    StreamContentArmState, handle_stream_content_message,
};
use tool_arms::{
    StreamToolArmContext, StreamToolArmMessage, StreamToolArmOutcome, StreamToolArmState,
    handle_stream_tool_message, reconcile_exact_stream_frame_after_tool_outcome,
};
mod content_arms;
mod exit_reconcile;
mod expected_identity;
#[cfg(test)]
#[path = "stream_loop/expected_identity_tests.rs"]
mod expected_identity_tests;
mod message_conversion;
mod tool_arms;
pub(super) mod types;
pub(super) use exit_reconcile::StreamLoopOutcome;
use exit_reconcile::{
    StreamLoopExitCandidateContext, retained_stream_retry_backoff,
    settle_and_reconcile_exit_candidate, should_exit_completed_turn_on_cancel,
    stream_loop_should_continue,
};
use expected_identity::refresh_stream_tick_expected_identity_after_handoff;
pub(super) use types::{StreamLoopContext, StreamLoopOutput, StreamLoopState};
pub(super) async fn run_stream_loop(
    ctx: StreamLoopContext,
    state: StreamLoopState<'_>,
) -> StreamLoopOutput {
    let (shared_owned, gateway) = (ctx.shared_owned, ctx.gateway);
    let (channel_id, provider) = (ctx.channel_id, ctx.provider);
    let (cancel_token, user_text_owned) = (ctx.cancel_token, ctx.user_text_owned);
    let (request_owner_name, adk_session_key) = (ctx.request_owner_name, ctx.adk_session_key);
    let (adk_session_name, adk_session_info) = (ctx.adk_session_name, ctx.adk_session_info);
    let (adk_cwd, dispatch_id) = (ctx.adk_cwd, ctx.dispatch_id);
    let (role_binding, turn_id) = (ctx.role_binding, ctx.turn_id);
    let voice_progress_playback_channel_id = ctx.voice_progress_playback_channel_id;
    let single_message_panel_footer_mode = ctx.single_message_panel_footer_mode;
    let footer_owner = ctx.footer_owner;
    let status_panel_started_at = ctx.status_panel_started_at;
    let status_interval = ctx.status_interval;
    let context_window_tokens = ctx.context_window_tokens;
    let context_compact_percent = ctx.context_compact_percent;
    let context_compact_lower_bound_tokens =
        crate::services::discord::adk_session::fetch_context_thresholds(shared_owned.api_port)
            .await
            .compact_lower_bound_tokens;

    let rx = &mut *state.rx;
    let mut full_response = std::mem::take(state.full_response);
    let mut last_edit_text = std::mem::take(state.last_edit_text);
    #[rustfmt::skip]
    let (mut done, mut cancelled, mut rx_disconnected) = (*state.done, *state.cancelled, *state.rx_disconnected);
    let mut current_tool_line = state.current_tool_line.take();
    let mut prev_tool_status = state.prev_tool_status.take();
    let mut last_tool_name = state.last_tool_name.take();
    let mut last_tool_summary = state.last_tool_summary.take();
    let mut accumulated_input_tokens = *state.accumulated_input_tokens;
    #[rustfmt::skip]
    let (mut accumulated_cache_create_tokens, mut accumulated_cache_read_tokens, mut accumulated_output_tokens) = (*state.accumulated_cache_create_tokens, *state.accumulated_cache_read_tokens, *state.accumulated_output_tokens);
    #[rustfmt::skip]
    let (mut spin_idx, mut restart_followup_pending, mut any_tool_used) = (*state.spin_idx, *state.restart_followup_pending, *state.any_tool_used);
    #[rustfmt::skip]
    let (mut has_post_tool_text, mut tmux_handed_off, mut watcher_owns_assistant_relay) = (*state.has_post_tool_text, *state.tmux_handed_off, *state.watcher_owns_assistant_relay);
    let mut watcher_relay_available_for_turn = *state.watcher_relay_available_for_turn;
    let mut watcher_handoff_claim_outcome = *state.watcher_handoff_claim_outcome;
    let mut standby_relay_owns_output = *state.standby_relay_owns_output;
    let mut last_assistant_text_line = state.last_assistant_text_line.take();
    let mut long_running_placeholder_active = state.long_running_placeholder_active.take();
    let mut active_background_child_session_ids =
        std::mem::take(state.active_background_child_session_ids);
    let mut transport_error = *state.transport_error;
    let mut tui_error_classification = TuiErrorClassification::default();
    let mut transcript_events = std::mem::take(state.transcript_events);
    let mut resume_failure_detected = *state.resume_failure_detected;
    let mut session_handshake_seen = *state.session_handshake_seen;
    let mut terminal_session_reset_required = *state.terminal_session_reset_required;
    let mut recovery_retry = *state.recovery_retry;
    let mut last_adk_heartbeat = *state.last_adk_heartbeat;
    let mut pending_stream_messages = std::mem::take(state.pending_stream_messages);
    let mut pending_status_tool_results = std::mem::take(state.pending_status_tool_results);
    let mut pending_status_tool_results_by_id =
        std::mem::take(state.pending_status_tool_results_by_id);
    let mut last_inflight_long_run_heartbeat = *state.last_inflight_long_run_heartbeat;
    let mut last_activity_heartbeat_at = *state.last_activity_heartbeat_at;
    let mut terminal_control_ready_observed = *state.terminal_control_ready_observed;
    let mut terminal_control_drain_until = *state.terminal_control_drain_until;
    let mut current_msg_id = *state.current_msg_id;
    let mut expected_current_message = *state.expected_current_message;
    let mut pending_current_message_candidate =
        unbound_current_message_candidate(current_msg_id, expected_current_message.0);
    let mut bridge_created_response_placeholder_msg_id =
        *state.bridge_created_response_placeholder_msg_id;
    let mut response_sent_offset = *state.response_sent_offset;
    let mut bridge_confirmed_response_sent_offset = *state.bridge_confirmed_response_sent_offset;
    let mut streamed_assistant_text_this_turn = *state.streamed_assistant_text_this_turn;
    let mut streaming_rollover_frozen_msg_ids =
        std::mem::take(state.streaming_rollover_frozen_msg_ids);
    let mut terminal_full_replay_cleanup_msg_ids =
        std::mem::take(state.terminal_full_replay_cleanup_msg_ids);
    let mut tmux_last_offset = *state.tmux_last_offset;
    let mut watcher_owner_channel_id = *state.watcher_owner_channel_id;
    let mut new_session_id = state.new_session_id.take();
    let mut new_raw_provider_session_id = state.new_raw_provider_session_id.take();
    let mut inflight_state = state.inflight_state.clone();
    let mut last_status_edit = *state.last_status_edit;
    let mut first_answer_relayed = *state.first_answer_relayed;
    let mut last_session_panel_lifecycle_refresh = *state.last_session_panel_lifecycle_refresh;
    let mut status_panel_msg_id = *state.status_panel_msg_id;
    let mut last_status_panel_text = std::mem::take(state.last_status_panel_text);
    let mut status_panel_dirty = *state.status_panel_dirty;
    let mut last_status_panel_edit = *state.last_status_panel_edit;
    let mut bridge_spans = *state.bridge_spans;
    let mut status_panel_generation = *state.status_panel_generation;
    let mut stream_tick_expected_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(&inflight_state);
    let mut persisted_inflight_baseline = inflight_state.clone();

    macro_rules! refresh_expected_after_handoff {
        ($outcome:expr) => {
            refresh_stream_tick_expected_identity_after_handoff(
                &mut stream_tick_expected_identity,
                &mut persisted_inflight_baseline,
                &inflight_state,
                $outcome,
            )
        };
    }

    macro_rules! refresh_or_retain_runtime_handoff {
        ($outcome:expr, $retry_pending:ident, $retry_retained:ident) => {{
            let outcome = $outcome;
            refresh_expected_after_handoff!(outcome.guarded_save_outcome);
            if let Some(retry_message) = outcome.retry_message {
                pending_stream_messages.push_front(retry_message.into_stream_message());
                $retry_pending = true;
                $retry_retained = true;
                true
            } else {
                $retry_retained = false;
                false
            }
        }};
    }

    macro_rules! stop_on_tool_authority_loss {
        ($outcome:expr, $loop_outcome:ident, $label:lifetime) => {
            if matches!($outcome, StreamToolArmOutcome::AuthorityLost) {
                $loop_outcome = StreamLoopOutcome::AuthorityLost;
                break $label;
            }
        };
    }

    // #2289: both cancel guards share this macro to keep inflight sync, cancellation, and child abort atomic without closure borrow conflicts.
    // Callers must then exit `'outer` (explicitly or by fallthrough) into cancel post-processing.
    macro_rules! finalize_cancel_inner {
        () => {{
            let previous_restart_mode = inflight_state.restart_mode;
            let previous_restart_generation = inflight_state.restart_generation;
            if sync_inflight_restart_mode_from_cancel(cancel_token.as_ref(), &mut inflight_state) {
                let outcome =
                    crate::services::discord::inflight::patch_restart_mode_if_matches_identity(
                        &inflight_state,
                        &stream_tick_expected_identity,
                        previous_restart_mode,
                        previous_restart_generation,
                        "turn_bridge::stream_loop::cancel_restart_mode",
                    );
                refresh_expected_after_handoff!(Some(outcome));
            }
            cancelled = true;
            close_all_tracked_background_children(
                shared_owned.pg_pool.as_ref(),
                &mut active_background_child_session_ids,
                "aborted",
                "turn cancel",
            )
            .await;
        }};
    }

    let mut state_dirty = false;
    #[rustfmt::skip]
    let (mut pending_long_running_open_after_state_save, mut pending_long_running_retarget_after_state_save, mut loop_outcome, mut runtime_handoff_retry_retained, mut admitted_codex_terminal_range, mut guarded_tool_frame_retry_retained) = (None, None, StreamLoopOutcome::Completed, false, None, false);

    'outer: while stream_loop_should_continue(
        done,
        terminal_control_drain_until,
        runtime_handoff_retry_retained,
        guarded_tool_frame_retry_retained,
        std::time::Instant::now(),
    ) {
        // #2172: Done wins over a later cancel during residual-control drain;
        // only `!done` may run cancel finalization.
        if !done && cancel_requested(Some(cancel_token.as_ref())) {
            finalize_cancel_inner!();
            break 'outer;
        }
        if should_exit_completed_turn_on_cancel(
            done,
            cancel_requested(Some(cancel_token.as_ref())),
            guarded_tool_frame_retry_retained,
        ) {
            // Exit residual drain without reclassifying the completed turn.
            break 'outer;
        }

        // #2426: timeout is only a safety wake; handoff frames wake immediately.
        let stream_wait = turn_bridge_stream_wait_duration(
            done,
            terminal_control_drain_until,
            std::time::Instant::now(),
        );
        if stream_wait.is_zero() {
            if let Ok(msg) = rx.try_recv() {
                pending_stream_messages.push_back(msg);
            }
        } else {
            match tokio::time::timeout(stream_wait, rx.recv()).await {
                Ok(Some(msg)) => pending_stream_messages.push_back(msg),
                Ok(None) | Err(_) => {}
            }
        }

        if !done && cancel_requested(Some(cancel_token.as_ref())) {
            finalize_cancel_inner!();
            break 'outer;
        }
        if should_exit_completed_turn_on_cancel(
            done,
            cancel_requested(Some(cancel_token.as_ref())),
            guarded_tool_frame_retry_retained,
        ) {
            break 'outer;
        }

        let mut runtime_handoff_retry_pending = false;
        let mut guarded_tool_frame_retry_pending = false;
        loop {
            // #2172 cancel boundary: re-check the cancel flag between
            // drained messages. Without this, the outer loop samples
            // `cancel_requested` once and then drains EVERY queued
            // StreamMessage to completion — so a cancel that flips
            // mid-drain can let a queued `Done` set `done = true`
            // before the outer cancel-arm runs, which then can no
            // longer classify the turn as cancelled (the `!done`
            // gate suppresses it). Break out of the drain on cancel
            // so the outer cancel-arm gets first claim on the
            // turn outcome. Frames already pulled before cancel was
            // observed have been processed (acceptable: they
            // happened before the user pressed stop); subsequent
            // frames are left in `rx` and dropped by the bridge
            // shutdown path.
            if !done && cancel_requested(Some(cancel_token.as_ref())) {
                break;
            }
            let next_message = if let Some(msg) = pending_stream_messages.pop_front() {
                Ok(msg)
            } else {
                rx.try_recv()
            };
            match next_message {
                Ok(msg) => {
                    // #2289 cancel boundary: re-sample `cancel_requested`
                    // AFTER `try_recv`, but ONLY for variants that flip
                    // `done = true` (`Done`/`Error`). The pre-recv guard
                    // samples before the receive; if `/stop` flips the token
                    // in that gap, letting a terminal arm run sets `done` and
                    // suppresses the outer cancel arm — recording a completed/
                    // failed turn the user actually stopped. Drop the frame
                    // and jump to cancel-finalize.
                    //
                    // Scoped to done-setting variants so non-terminal frames
                    // (`RuntimeReady`, `TmuxReady`, `ProcessReady`,
                    // `OutputOffset`, `Text`, `RetryBoundary`, …) are still
                    // processed (they carry handoff paths, offsets, watcher
                    // debt, session-reset that the cancel path needs); none
                    // flip `done`, so the next pre-recv cancel guard finalizes
                    // cancel cleanly. A new terminal variant MUST be added here
                    // too — see `is_done_setting_terminal_frame`.
                    if is_done_setting_terminal_frame(&msg)
                        && should_finalize_cancel_after_recv(
                            done,
                            cancel_requested(Some(cancel_token.as_ref())),
                        )
                    {
                        // The dropped frame's bookkeeping (full_response
                        // resolution, transcript Result/Error,
                        // placeholder close, transport_error edge)
                        // is intentionally skipped: the cancel path
                        // is the authoritative finalizer for the
                        // turn outcome and runs its own placeholder
                        // teardown.
                        finalize_cancel_inner!();
                        break 'outer;
                    }
                    #[rustfmt::skip]
                    let (msg, admission, was_codex_terminal) = inflight_state.admit_codex_tui_terminal_frame(&mut persisted_inflight_baseline, &stream_tick_expected_identity, gateway.can_chain_locally(), msg);
                    admitted_codex_terminal_range = admission.or(admitted_codex_terminal_range);
                    terminal_control_ready_observed |= was_codex_terminal;
                    match msg {
                        content_message @ (StreamMessage::RetryBoundary
                        | StreamMessage::ActiveUsageSnapshot { .. }
                        | StreamMessage::Init { .. }
                        | StreamMessage::Text { .. }
                        | StreamMessage::Thinking { .. }
                        | StreamMessage::Done { .. }
                        | StreamMessage::Error { .. }
                        | StreamMessage::StatusUpdate { .. }
                        | StreamMessage::StatusEvents { .. }) => {
                            let message = message_conversion::into_content_message(content_message)
                                .expect("content-message pattern must stay exhaustive");
                            let outcome = handle_stream_content_message(
                                message,
                                StreamContentArmContext {
                                    shared_owned: &shared_owned,
                                    gateway: &gateway,
                                    channel_id,
                                    provider: &provider,
                                    expected_identity: &stream_tick_expected_identity,
                                    voice_progress_playback_channel_id,
                                    watcher_owns_assistant_relay,
                                    watcher_relay_available_for_turn,
                                    standby_relay_owns_output,
                                    terminal_control_ready_observed,
                                    streaming_rollover_frozen_msg_ids:
                                        &streaming_rollover_frozen_msg_ids,
                                    context_compact_lower_bound_tokens,
                                    context_window_tokens,
                                    context_compact_percent,
                                },
                                StreamContentArmState {
                                    state_dirty: &mut state_dirty,
                                    full_response: &mut full_response,
                                    current_tool_line: &mut current_tool_line,
                                    prev_tool_status: &mut prev_tool_status,
                                    last_tool_name: &mut last_tool_name,
                                    last_tool_summary: &mut last_tool_summary,
                                    any_tool_used: &mut any_tool_used,
                                    has_post_tool_text: &mut has_post_tool_text,
                                    response_sent_offset: &mut response_sent_offset,
                                    last_edit_text: &mut last_edit_text,
                                    new_session_id: &mut new_session_id,
                                    new_raw_provider_session_id: &mut new_raw_provider_session_id,
                                    inflight_state: &mut inflight_state,
                                    transcript_events: &mut transcript_events,
                                    session_handshake_seen: &mut session_handshake_seen,
                                    streamed_assistant_text_this_turn:
                                        &mut streamed_assistant_text_this_turn,
                                    last_assistant_text_line: &mut last_assistant_text_line,
                                    status_panel_dirty: &mut status_panel_dirty,
                                    recovery_retry: &mut recovery_retry,
                                    pending_long_running_open_after_state_save:
                                        &mut pending_long_running_open_after_state_save,
                                    long_running_placeholder_active:
                                        &mut long_running_placeholder_active,
                                    pending_long_running_retarget_after_state_save:
                                        &mut pending_long_running_retarget_after_state_save,
                                    terminal_full_replay_cleanup_msg_ids:
                                        &mut terminal_full_replay_cleanup_msg_ids,
                                    active_background_child_session_ids:
                                        &mut active_background_child_session_ids,
                                    done: &mut done,
                                    terminal_control_drain_until: &mut terminal_control_drain_until,
                                    transport_error: &mut transport_error,
                                    tui_error_classification: &mut tui_error_classification,
                                    resume_failure_detected: &mut resume_failure_detected,
                                    bridge_confirmed_response_sent_offset:
                                        &mut bridge_confirmed_response_sent_offset,
                                    terminal_session_reset_required:
                                        &mut terminal_session_reset_required,
                                    accumulated_input_tokens: &mut accumulated_input_tokens,
                                    accumulated_cache_create_tokens:
                                        &mut accumulated_cache_create_tokens,
                                    accumulated_cache_read_tokens:
                                        &mut accumulated_cache_read_tokens,
                                    accumulated_output_tokens: &mut accumulated_output_tokens,
                                },
                            )
                            .await;
                            if was_codex_terminal {
                                break;
                            }
                            match outcome {
                                StreamContentArmOutcome::ContinueDraining => {}
                                StreamContentArmOutcome::SkipRemainderOfDrainIteration => continue,
                            }
                        }
                        StreamMessage::ToolUse {
                            name,
                            input,
                            tool_use_id,
                        } => {
                            let outcome = handle_stream_tool_message(
                                StreamToolArmMessage::ToolUse {
                                    name,
                                    input,
                                    tool_use_id,
                                },
                                StreamToolArmContext {
                                    shared_owned: &shared_owned,
                                    gateway: &gateway,
                                    channel_id,
                                    provider: &provider,
                                    user_text_owned: &user_text_owned,
                                    request_owner_name: &request_owner_name,
                                    adk_session_key: &adk_session_key,
                                    adk_session_name: &adk_session_name,
                                    role_binding: &role_binding,
                                    voice_progress_playback_channel_id,
                                    single_message_panel_footer_mode,
                                    footer_owner,
                                    current_msg_id: &mut current_msg_id,
                                },
                                StreamToolArmState {
                                    state_dirty: &mut state_dirty,
                                    inflight_state: &mut inflight_state,
                                    persisted_inflight_baseline: &mut persisted_inflight_baseline,
                                    stream_tick_expected_identity: &stream_tick_expected_identity,
                                    expected_current_message: &mut expected_current_message,
                                    current_tool_line: &mut current_tool_line,
                                    prev_tool_status: &mut prev_tool_status,
                                    last_tool_name: &mut last_tool_name,
                                    last_tool_summary: &mut last_tool_summary,
                                    any_tool_used: &mut any_tool_used,
                                    has_post_tool_text: &mut has_post_tool_text,
                                    last_assistant_text_line: &mut last_assistant_text_line,
                                    spin_idx: &mut spin_idx,
                                    transcript_events: &mut transcript_events,
                                    pending_status_tool_results: &mut pending_status_tool_results,
                                    pending_status_tool_results_by_id:
                                        &mut pending_status_tool_results_by_id,
                                    long_running_placeholder_active:
                                        &mut long_running_placeholder_active,
                                    active_background_child_session_ids:
                                        &mut active_background_child_session_ids,
                                    pending_long_running_open_after_state_save:
                                        &mut pending_long_running_open_after_state_save,
                                    pending_long_running_retarget_after_state_save:
                                        &mut pending_long_running_retarget_after_state_save,
                                    restart_followup_pending: &mut restart_followup_pending,
                                    last_edit_text: &mut last_edit_text,
                                    full_response: &mut full_response,
                                    response_sent_offset: &mut response_sent_offset,
                                    confirmed_offset: &mut bridge_confirmed_response_sent_offset,
                                    status_panel_dirty: &mut status_panel_dirty,
                                },
                            )
                            .await;
                            stop_on_tool_authority_loss!(outcome, loop_outcome, 'outer);
                        }
                        StreamMessage::ToolResult {
                            content,
                            is_error,
                            tool_use_id,
                        } => {
                            let retry_frame = StreamMessage::ToolResult {
                                content: content.clone(),
                                is_error,
                                tool_use_id: tool_use_id.clone(),
                            };
                            let outcome = handle_stream_tool_message(
                                StreamToolArmMessage::ToolResult {
                                    content,
                                    is_error,
                                    tool_use_id,
                                },
                                StreamToolArmContext {
                                    shared_owned: &shared_owned,
                                    gateway: &gateway,
                                    channel_id,
                                    provider: &provider,
                                    user_text_owned: &user_text_owned,
                                    request_owner_name: &request_owner_name,
                                    adk_session_key: &adk_session_key,
                                    adk_session_name: &adk_session_name,
                                    role_binding: &role_binding,
                                    voice_progress_playback_channel_id,
                                    single_message_panel_footer_mode,
                                    footer_owner,
                                    current_msg_id: &mut current_msg_id,
                                },
                                StreamToolArmState {
                                    state_dirty: &mut state_dirty,
                                    inflight_state: &mut inflight_state,
                                    persisted_inflight_baseline: &mut persisted_inflight_baseline,
                                    stream_tick_expected_identity: &stream_tick_expected_identity,
                                    expected_current_message: &mut expected_current_message,
                                    current_tool_line: &mut current_tool_line,
                                    prev_tool_status: &mut prev_tool_status,
                                    last_tool_name: &mut last_tool_name,
                                    last_tool_summary: &mut last_tool_summary,
                                    any_tool_used: &mut any_tool_used,
                                    has_post_tool_text: &mut has_post_tool_text,
                                    last_assistant_text_line: &mut last_assistant_text_line,
                                    spin_idx: &mut spin_idx,
                                    transcript_events: &mut transcript_events,
                                    pending_status_tool_results: &mut pending_status_tool_results,
                                    pending_status_tool_results_by_id:
                                        &mut pending_status_tool_results_by_id,
                                    long_running_placeholder_active:
                                        &mut long_running_placeholder_active,
                                    active_background_child_session_ids:
                                        &mut active_background_child_session_ids,
                                    pending_long_running_open_after_state_save:
                                        &mut pending_long_running_open_after_state_save,
                                    pending_long_running_retarget_after_state_save:
                                        &mut pending_long_running_retarget_after_state_save,
                                    restart_followup_pending: &mut restart_followup_pending,
                                    last_edit_text: &mut last_edit_text,
                                    full_response: &mut full_response,
                                    response_sent_offset: &mut response_sent_offset,
                                    confirmed_offset: &mut bridge_confirmed_response_sent_offset,
                                    status_panel_dirty: &mut status_panel_dirty,
                                },
                            )
                            .await;
                            if reconcile_exact_stream_frame_after_tool_outcome(
                                &mut pending_stream_messages,
                                retry_frame,
                                outcome,
                                &mut guarded_tool_frame_retry_retained,
                            ) {
                                guarded_tool_frame_retry_pending = true;
                                break;
                            }
                            stop_on_tool_authority_loss!(outcome, loop_outcome, 'outer);
                        }
                        StreamMessage::TaskNotification {
                            tool_use_id,
                            summary,
                            status,
                            kind,
                            ..
                        } => {
                            let outcome = handle_stream_tool_message(
                                StreamToolArmMessage::TaskNotification {
                                    tool_use_id,
                                    summary,
                                    status,
                                    kind,
                                },
                                StreamToolArmContext {
                                    shared_owned: &shared_owned,
                                    gateway: &gateway,
                                    channel_id,
                                    provider: &provider,
                                    user_text_owned: &user_text_owned,
                                    request_owner_name: &request_owner_name,
                                    adk_session_key: &adk_session_key,
                                    adk_session_name: &adk_session_name,
                                    role_binding: &role_binding,
                                    voice_progress_playback_channel_id,
                                    single_message_panel_footer_mode,
                                    footer_owner,
                                    current_msg_id: &mut current_msg_id,
                                },
                                StreamToolArmState {
                                    state_dirty: &mut state_dirty,
                                    inflight_state: &mut inflight_state,
                                    persisted_inflight_baseline: &mut persisted_inflight_baseline,
                                    stream_tick_expected_identity: &stream_tick_expected_identity,
                                    expected_current_message: &mut expected_current_message,
                                    current_tool_line: &mut current_tool_line,
                                    prev_tool_status: &mut prev_tool_status,
                                    last_tool_name: &mut last_tool_name,
                                    last_tool_summary: &mut last_tool_summary,
                                    any_tool_used: &mut any_tool_used,
                                    has_post_tool_text: &mut has_post_tool_text,
                                    last_assistant_text_line: &mut last_assistant_text_line,
                                    spin_idx: &mut spin_idx,
                                    transcript_events: &mut transcript_events,
                                    pending_status_tool_results: &mut pending_status_tool_results,
                                    pending_status_tool_results_by_id:
                                        &mut pending_status_tool_results_by_id,
                                    long_running_placeholder_active:
                                        &mut long_running_placeholder_active,
                                    active_background_child_session_ids:
                                        &mut active_background_child_session_ids,
                                    pending_long_running_open_after_state_save:
                                        &mut pending_long_running_open_after_state_save,
                                    pending_long_running_retarget_after_state_save:
                                        &mut pending_long_running_retarget_after_state_save,
                                    restart_followup_pending: &mut restart_followup_pending,
                                    last_edit_text: &mut last_edit_text,
                                    full_response: &mut full_response,
                                    response_sent_offset: &mut response_sent_offset,
                                    confirmed_offset: &mut bridge_confirmed_response_sent_offset,
                                    status_panel_dirty: &mut status_panel_dirty,
                                },
                            )
                            .await;
                            stop_on_tool_authority_loss!(outcome, loop_outcome, 'outer);
                        }
                        StreamMessage::TmuxReady {
                            output_path,
                            input_fifo_path,
                            tmux_session_name,
                            last_offset,
                        } => {
                            *state.entry_watcher_epoch_current = false;
                            let outcome = handle_runtime_handoff_loop_message(
                                RuntimeHandoffLoopMessage::TmuxReady {
                                    output_path,
                                    input_fifo_path,
                                    tmux_session_name,
                                    last_offset,
                                },
                                RuntimeHandoffLoopContext {
                                    shared_owned: &shared_owned,
                                    provider: &provider,
                                    channel_id,
                                    done,
                                    adk_session_name: &adk_session_name,
                                },
                                RuntimeHandoffLoopState {
                                    terminal_control_ready_observed:
                                        &mut terminal_control_ready_observed,
                                    tmux_last_offset: &mut tmux_last_offset,
                                    inflight_state: &mut inflight_state,
                                    watcher_owner_channel_id: &mut watcher_owner_channel_id,
                                    standby_relay_owns_output: &mut standby_relay_owns_output,
                                    watcher_relay_available_for_turn:
                                        &mut watcher_relay_available_for_turn,
                                    watcher_handoff_claim_outcome:
                                        &mut watcher_handoff_claim_outcome,
                                    tmux_handed_off: &mut tmux_handed_off,
                                    watcher_owns_assistant_relay: &mut watcher_owns_assistant_relay,
                                    state_dirty: &mut state_dirty,
                                    terminal_control_drain_until: &mut terminal_control_drain_until,
                                    last_activity_heartbeat_at: &mut last_activity_heartbeat_at,
                                },
                            )
                            .await;
                            if refresh_or_retain_runtime_handoff!(
                                outcome,
                                runtime_handoff_retry_pending,
                                runtime_handoff_retry_retained
                            ) {
                                break;
                            }
                        }
                        StreamMessage::RuntimeReady { handoff } => {
                            *state.entry_watcher_epoch_current = false;
                            let outcome = handle_runtime_handoff_loop_message(
                                RuntimeHandoffLoopMessage::RuntimeReady { handoff },
                                RuntimeHandoffLoopContext {
                                    shared_owned: &shared_owned,
                                    provider: &provider,
                                    channel_id,
                                    done,
                                    adk_session_name: &adk_session_name,
                                },
                                RuntimeHandoffLoopState {
                                    terminal_control_ready_observed:
                                        &mut terminal_control_ready_observed,
                                    tmux_last_offset: &mut tmux_last_offset,
                                    inflight_state: &mut inflight_state,
                                    watcher_owner_channel_id: &mut watcher_owner_channel_id,
                                    standby_relay_owns_output: &mut standby_relay_owns_output,
                                    watcher_relay_available_for_turn:
                                        &mut watcher_relay_available_for_turn,
                                    watcher_handoff_claim_outcome:
                                        &mut watcher_handoff_claim_outcome,
                                    tmux_handed_off: &mut tmux_handed_off,
                                    watcher_owns_assistant_relay: &mut watcher_owns_assistant_relay,
                                    state_dirty: &mut state_dirty,
                                    terminal_control_drain_until: &mut terminal_control_drain_until,
                                    last_activity_heartbeat_at: &mut last_activity_heartbeat_at,
                                },
                            )
                            .await;
                            if refresh_or_retain_runtime_handoff!(
                                outcome,
                                runtime_handoff_retry_pending,
                                runtime_handoff_retry_retained
                            ) {
                                break;
                            }
                        }
                        StreamMessage::ProcessReady {
                            output_path,
                            session_name,
                            last_offset,
                        } => {
                            *state.entry_watcher_epoch_current = false;
                            let outcome = handle_runtime_handoff_loop_message(
                                RuntimeHandoffLoopMessage::ProcessReady {
                                    output_path,
                                    session_name,
                                    last_offset,
                                },
                                RuntimeHandoffLoopContext {
                                    shared_owned: &shared_owned,
                                    provider: &provider,
                                    channel_id,
                                    done,
                                    adk_session_name: &adk_session_name,
                                },
                                RuntimeHandoffLoopState {
                                    terminal_control_ready_observed:
                                        &mut terminal_control_ready_observed,
                                    tmux_last_offset: &mut tmux_last_offset,
                                    inflight_state: &mut inflight_state,
                                    watcher_owner_channel_id: &mut watcher_owner_channel_id,
                                    standby_relay_owns_output: &mut standby_relay_owns_output,
                                    watcher_relay_available_for_turn:
                                        &mut watcher_relay_available_for_turn,
                                    watcher_handoff_claim_outcome:
                                        &mut watcher_handoff_claim_outcome,
                                    tmux_handed_off: &mut tmux_handed_off,
                                    watcher_owns_assistant_relay: &mut watcher_owns_assistant_relay,
                                    state_dirty: &mut state_dirty,
                                    terminal_control_drain_until: &mut terminal_control_drain_until,
                                    last_activity_heartbeat_at: &mut last_activity_heartbeat_at,
                                },
                            )
                            .await;
                            if refresh_or_retain_runtime_handoff!(
                                outcome,
                                runtime_handoff_retry_pending,
                                runtime_handoff_retry_retained
                            ) {
                                break;
                            }
                        }
                        StreamMessage::OutputOffset { offset } => {
                            let outcome = handle_runtime_handoff_loop_message(
                                RuntimeHandoffLoopMessage::OutputOffset { offset },
                                RuntimeHandoffLoopContext {
                                    shared_owned: &shared_owned,
                                    provider: &provider,
                                    channel_id,
                                    done,
                                    adk_session_name: &adk_session_name,
                                },
                                RuntimeHandoffLoopState {
                                    terminal_control_ready_observed:
                                        &mut terminal_control_ready_observed,
                                    tmux_last_offset: &mut tmux_last_offset,
                                    inflight_state: &mut inflight_state,
                                    watcher_owner_channel_id: &mut watcher_owner_channel_id,
                                    standby_relay_owns_output: &mut standby_relay_owns_output,
                                    watcher_relay_available_for_turn:
                                        &mut watcher_relay_available_for_turn,
                                    watcher_handoff_claim_outcome:
                                        &mut watcher_handoff_claim_outcome,
                                    tmux_handed_off: &mut tmux_handed_off,
                                    watcher_owns_assistant_relay: &mut watcher_owns_assistant_relay,
                                    state_dirty: &mut state_dirty,
                                    terminal_control_drain_until: &mut terminal_control_drain_until,
                                    last_activity_heartbeat_at: &mut last_activity_heartbeat_at,
                                },
                            )
                            .await;
                            if refresh_or_retain_runtime_handoff!(
                                outcome,
                                runtime_handoff_retry_pending,
                                runtime_handoff_retry_retained
                            ) {
                                break;
                            }
                        }
                        StreamMessage::CodexTuiTerminalDone { .. } => unreachable!(),
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    // #2289 cancel boundary: re-sample cancel AFTER the
                    // receiver reports disconnect. If `/stop` flipped
                    // the token between the pre-recv guard and this
                    // arm, letting the disconnect set `done = true`
                    // and exit the inner loop would cause the outer
                    // cancel arm (gated on `!done`) to skip
                    // finalisation, leaving the turn recorded as
                    // completed/empty instead of stopped.
                    if should_finalize_cancel_after_recv(
                        done,
                        cancel_requested(Some(cancel_token.as_ref())),
                    ) {
                        finalize_cancel_inner!();
                        break 'outer;
                    }
                    rx_disconnected = true;
                    done = true;
                    terminal_control_drain_until = None;
                    break;
                }
            }
        }

        if runtime_handoff_retry_pending || guarded_tool_frame_retry_pending {
            // A guarded frame hit transient I/O and remains at the front of the
            // exact stream queue. A multi-step handoff may also have durably
            // landed its first snapshot while retaining its original identity.
            // Running the generic tick here could misclassify either retry as
            // authority loss or advance state past the retained ToolResult.
            // Retry the exact frame on the next normal stream wake instead.
            // Once a completed turn's residual drain expires there is no next
            // receiver wake, so keep the loop alive with a bounded retry
            // backoff rather than dropping the exact frame or busy-spinning.
            tokio::time::sleep(retained_stream_retry_backoff(
                runtime_handoff_retry_pending,
                guarded_tool_frame_retry_pending,
            ))
            .await;
            continue 'outer;
        }

        let tick_outcome = run_bridge_stream_tick(
            BridgeStreamTickContext {
                shared_owned: shared_owned.clone(),
                gateway: gateway.clone(),
                channel_id,
                provider: &provider,
                turn_id: turn_id.as_str(),
                expected_identity: &stream_tick_expected_identity,
                status_interval,
                single_message_panel_footer_mode,
                footer_owner,
                status_panel_started_at,
                done,
                dispatch_id: dispatch_id.clone(),
                adk_session_key: adk_session_key.clone(),
                adk_session_name: adk_session_name.clone(),
                adk_session_info: adk_session_info.clone(),
                adk_cwd: adk_cwd.clone(),
                role_binding: role_binding.clone(),
                spinner: SPINNER,
                live_long_run_heartbeat_interval: LIVE_LONG_RUN_HEARTBEAT_INTERVAL,
            },
            BridgeStreamTickState {
                state_dirty: &mut state_dirty,
                last_session_panel_lifecycle_refresh: &mut last_session_panel_lifecycle_refresh,
                status_panel_dirty: &mut status_panel_dirty,
                spin_idx: &mut spin_idx,
                last_status_panel_edit: &mut last_status_panel_edit,
                last_status_edit: &mut last_status_edit,
                status_panel_msg_id: &mut status_panel_msg_id,
                last_status_panel_text: &mut last_status_panel_text,
                watcher_owns_assistant_relay: &mut watcher_owns_assistant_relay,
                watcher_relay_available_for_turn: &mut watcher_relay_available_for_turn,
                standby_relay_owns_output: &mut standby_relay_owns_output,
                watcher_owner_channel_id: &mut watcher_owner_channel_id,
                full_response: &mut full_response,
                response_sent_offset: &mut response_sent_offset,
                bridge_confirmed_response_sent_offset: &mut bridge_confirmed_response_sent_offset,
                streaming_rollover_frozen_msg_ids: &mut streaming_rollover_frozen_msg_ids,
                current_msg_id: &mut current_msg_id,
                expected_current_message: &mut expected_current_message,
                pending_current_message_candidate: &mut pending_current_message_candidate,
                bridge_created_response_placeholder_msg_id:
                    &mut bridge_created_response_placeholder_msg_id,
                last_edit_text: &mut last_edit_text,
                first_answer_relayed: &mut first_answer_relayed,
                current_tool_line: &mut current_tool_line,
                prev_tool_status: &mut prev_tool_status,
                last_tool_name: &mut last_tool_name,
                last_tool_summary: &mut last_tool_summary,
                any_tool_used: &mut any_tool_used,
                has_post_tool_text: &mut has_post_tool_text,
                tmux_last_offset: &mut tmux_last_offset,
                persisted_inflight_baseline: &mut persisted_inflight_baseline,
                inflight_state: &mut inflight_state,
                bridge_spans: &mut bridge_spans,
                status_panel_generation: &mut status_panel_generation,
                pending_long_running_open_after_state_save:
                    &mut pending_long_running_open_after_state_save,
                pending_long_running_retarget_after_state_save:
                    &mut pending_long_running_retarget_after_state_save,
                long_running_placeholder_active: &mut long_running_placeholder_active,
                last_adk_heartbeat: &mut last_adk_heartbeat,
                last_inflight_long_run_heartbeat: &mut last_inflight_long_run_heartbeat,
            },
        )
        .await;
        if tick_outcome == StreamTickOutcome::AuthorityLost {
            loop_outcome = StreamLoopOutcome::AuthorityLost;
            break 'outer;
        }
    }
    // #3813 AC#1 tail: emit bridge-side latency spans once at loop exit
    // (observation-only; self-suppresses when no bridge relay happened).
    bridge_spans.log(channel_id.get(), provider.as_str());

    *state.full_response = full_response;
    *state.last_edit_text = last_edit_text;
    *state.done = done;
    *state.cancelled = cancelled;
    *state.rx_disconnected = rx_disconnected;
    *state.current_tool_line = current_tool_line;
    *state.prev_tool_status = prev_tool_status;
    *state.last_tool_name = last_tool_name;
    *state.last_tool_summary = last_tool_summary;
    *state.accumulated_input_tokens = accumulated_input_tokens;
    *state.accumulated_cache_create_tokens = accumulated_cache_create_tokens;
    *state.accumulated_cache_read_tokens = accumulated_cache_read_tokens;
    *state.accumulated_output_tokens = accumulated_output_tokens;
    *state.spin_idx = spin_idx;
    *state.restart_followup_pending = restart_followup_pending;
    *state.any_tool_used = any_tool_used;
    *state.has_post_tool_text = has_post_tool_text;
    *state.tmux_handed_off = tmux_handed_off;
    *state.watcher_owns_assistant_relay = watcher_owns_assistant_relay;
    *state.watcher_relay_available_for_turn = watcher_relay_available_for_turn;
    *state.watcher_handoff_claim_outcome = watcher_handoff_claim_outcome;
    *state.standby_relay_owns_output = standby_relay_owns_output;
    *state.last_assistant_text_line = last_assistant_text_line;
    *state.long_running_placeholder_active = long_running_placeholder_active;
    *state.active_background_child_session_ids = active_background_child_session_ids;
    *state.transport_error = transport_error;
    *state.transcript_events = transcript_events;
    *state.resume_failure_detected = resume_failure_detected;
    *state.session_handshake_seen = session_handshake_seen;
    *state.terminal_session_reset_required = terminal_session_reset_required;
    *state.recovery_retry = recovery_retry;
    *state.last_adk_heartbeat = last_adk_heartbeat;
    *state.pending_stream_messages = pending_stream_messages;
    *state.pending_status_tool_results = pending_status_tool_results;
    *state.pending_status_tool_results_by_id = pending_status_tool_results_by_id;
    *state.last_inflight_long_run_heartbeat = last_inflight_long_run_heartbeat;
    *state.last_activity_heartbeat_at = last_activity_heartbeat_at;
    *state.terminal_control_ready_observed = terminal_control_ready_observed;
    *state.terminal_control_drain_until = terminal_control_drain_until;
    *state.current_msg_id = current_msg_id;
    *state.expected_current_message = expected_current_message;
    *state.bridge_created_response_placeholder_msg_id = bridge_created_response_placeholder_msg_id;
    *state.response_sent_offset = response_sent_offset;
    *state.bridge_confirmed_response_sent_offset = bridge_confirmed_response_sent_offset;
    *state.streamed_assistant_text_this_turn = streamed_assistant_text_this_turn;
    *state.streaming_rollover_frozen_msg_ids = streaming_rollover_frozen_msg_ids;
    *state.terminal_full_replay_cleanup_msg_ids = terminal_full_replay_cleanup_msg_ids;
    *state.tmux_last_offset = tmux_last_offset;
    *state.watcher_owner_channel_id = watcher_owner_channel_id;
    *state.new_session_id = new_session_id;
    *state.new_raw_provider_session_id = new_raw_provider_session_id;
    *state.inflight_state = inflight_state;
    *state.last_status_edit = last_status_edit;
    *state.first_answer_relayed = first_answer_relayed;
    *state.last_session_panel_lifecycle_refresh = last_session_panel_lifecycle_refresh;
    *state.status_panel_msg_id = status_panel_msg_id;
    *state.last_status_panel_text = last_status_panel_text;
    *state.status_panel_dirty = status_panel_dirty;
    *state.last_status_panel_edit = last_status_panel_edit;
    *state.bridge_spans = bridge_spans;
    *state.status_panel_generation = status_panel_generation;

    settle_and_reconcile_exit_candidate(StreamLoopExitCandidateContext {
        shared: shared_owned.as_ref(),
        gateway: gateway.as_ref(),
        provider: &provider,
        token_hash: &shared_owned.token_hash,
        channel_id,
        persisted_inflight_baseline: &mut persisted_inflight_baseline,
        expected_identity: &stream_tick_expected_identity,
        pending_current_message_candidate: &mut pending_current_message_candidate,
        state,
    })
    .await;

    StreamLoopOutput {
        outcome: loop_outcome,
        tui_error_classification,
        codex_tui_terminal_range: admitted_codex_terminal_range,
        pending_long_running_open_after_state_save,
        pending_long_running_retarget_after_state_save,
    }
}
