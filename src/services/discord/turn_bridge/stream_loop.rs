//! #4230 S6 stream loop shell for `turn_bridge::spawn_turn_bridge`.
//!
//! Moved from the main stream receive/drain loop of `spawn_turn_bridge`:
//! cancel finalization gates, ready-frame drain, remaining stream event arms,
//! long-running placeholder open/retarget state, runtime handoff delegation,
//! stream/status ticks, and bridge latency span emission.

use std::collections::VecDeque;
use std::sync::Arc;

use super::bridge_latency_spans::BridgeLatencySpans;
use super::runtime_handoff_loop::{
    RuntimeHandoffLoopContext, RuntimeHandoffLoopMessage, RuntimeHandoffLoopOutcome,
    RuntimeHandoffLoopState, handle_runtime_handoff_loop_message,
};
use super::stream_tick::{
    BridgeStreamTickContext, BridgeStreamTickState, LongRunningPlaceholderActive,
    PendingLongRunningOpenAfterStateSave, PendingLongRunningRetargetAfterStateSave,
    run_bridge_stream_tick,
};
use super::{streaming_edit_text::TuiErrorClassification, *};
use content_arms::{
    StreamContentArmContext, StreamContentArmMessage, StreamContentArmOutcome,
    StreamContentArmState, handle_stream_content_message,
};
use tool_arms::{
    StreamToolArmContext, StreamToolArmMessage, StreamToolArmOutcome, StreamToolArmState,
    handle_stream_tool_message,
};

mod content_arms;
mod tool_arms;

pub(super) struct StreamLoopContext {
    pub(super) shared_owned: Arc<SharedData>,
    pub(super) gateway: Arc<dyn TurnGateway>,
    pub(super) channel_id: ChannelId,
    pub(super) provider: ProviderKind,
    pub(super) cancel_token: Arc<crate::services::provider::CancelToken>,
    pub(super) user_text_owned: String,
    pub(super) request_owner_name: String,
    pub(super) adk_session_key: Option<String>,
    pub(super) adk_session_name: Option<String>,
    pub(super) adk_session_info: Option<String>,
    pub(super) adk_cwd: Option<String>,
    pub(super) dispatch_id: Option<String>,
    pub(super) role_binding: Option<RoleBinding>,
    pub(super) turn_id: String,
    pub(super) voice_progress_playback_channel_id: Option<ChannelId>,
    pub(super) single_message_panel_footer_mode: bool,
    pub(super) footer_owner: super::super::footer_view_reconciler::CompletionFooterOwner,
    pub(super) status_panel_started_at: i64,
    pub(super) status_interval: std::time::Duration,
    pub(super) context_window_tokens: u64,
    pub(super) context_compact_percent: u64,
}

pub(super) struct StreamLoopState<'a> {
    pub(super) rx: &'a mut super::StreamMessageReceiverAdapter,
    pub(super) full_response: &'a mut String,
    pub(super) last_edit_text: &'a mut String,
    pub(super) done: &'a mut bool,
    pub(super) cancelled: &'a mut bool,
    pub(super) rx_disconnected: &'a mut bool,
    pub(super) current_tool_line: &'a mut Option<String>,
    pub(super) prev_tool_status: &'a mut Option<String>,
    pub(super) last_tool_name: &'a mut Option<String>,
    pub(super) last_tool_summary: &'a mut Option<String>,
    pub(super) accumulated_input_tokens: &'a mut u64,
    pub(super) accumulated_cache_create_tokens: &'a mut u64,
    pub(super) accumulated_cache_read_tokens: &'a mut u64,
    pub(super) accumulated_output_tokens: &'a mut u64,
    pub(super) spin_idx: &'a mut usize,
    pub(super) restart_followup_pending: &'a mut bool,
    pub(super) any_tool_used: &'a mut bool,
    pub(super) has_post_tool_text: &'a mut bool,
    pub(super) tmux_handed_off: &'a mut bool,
    pub(super) watcher_owns_assistant_relay: &'a mut bool,
    pub(super) watcher_relay_available_for_turn: &'a mut bool,
    pub(super) watcher_handoff_claim_outcome: &'a mut WatcherHandoffClaimOutcome,
    pub(super) standby_relay_owns_output: &'a mut bool,
    pub(super) last_assistant_text_line: &'a mut Option<String>,
    pub(super) long_running_placeholder_active: &'a mut LongRunningPlaceholderActive,
    pub(super) active_background_child_session_ids: &'a mut Vec<i64>,
    pub(super) transport_error: &'a mut bool,
    pub(super) transcript_events: &'a mut Vec<SessionTranscriptEvent>,
    pub(super) resume_failure_detected: &'a mut bool,
    pub(super) session_handshake_seen: &'a mut bool,
    pub(super) terminal_session_reset_required: &'a mut bool,
    pub(super) recovery_retry: &'a mut bool,
    pub(super) last_adk_heartbeat: &'a mut std::time::Instant,
    pub(super) pending_stream_messages: &'a mut VecDeque<StreamMessage>,
    pub(super) pending_status_tool_results: &'a mut VecDeque<String>,
    pub(super) pending_status_tool_results_by_id: &'a mut std::collections::HashMap<String, String>,
    pub(super) last_inflight_long_run_heartbeat: &'a mut std::time::Instant,
    pub(super) last_activity_heartbeat_at: &'a mut Option<std::time::Instant>,
    pub(super) terminal_control_ready_observed: &'a mut bool,
    pub(super) terminal_control_drain_until: &'a mut Option<std::time::Instant>,
    pub(super) current_msg_id: &'a mut MessageId,
    pub(super) response_sent_offset: &'a mut usize,
    pub(super) bridge_confirmed_response_sent_offset: &'a mut usize,
    pub(super) streamed_assistant_text_this_turn: &'a mut bool,
    pub(super) streaming_rollover_frozen_msg_ids: &'a mut Vec<MessageId>,
    pub(super) terminal_full_replay_cleanup_msg_ids: &'a mut Vec<MessageId>,
    pub(super) tmux_last_offset: &'a mut Option<u64>,
    pub(super) watcher_owner_channel_id: &'a mut ChannelId,
    pub(super) new_session_id: &'a mut Option<String>,
    pub(super) new_raw_provider_session_id: &'a mut Option<String>,
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) last_status_edit: &'a mut tokio::time::Instant,
    pub(super) first_answer_relayed: &'a mut bool,
    pub(super) last_session_panel_lifecycle_refresh: &'a mut tokio::time::Instant,
    pub(super) status_panel_msg_id: &'a mut Option<MessageId>,
    pub(super) last_status_panel_text: &'a mut String,
    pub(super) status_panel_dirty: &'a mut bool,
    pub(super) last_status_panel_edit: &'a mut tokio::time::Instant,
    pub(super) bridge_spans: &'a mut BridgeLatencySpans,
    pub(super) status_panel_generation: &'a mut u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StreamLoopOutcome {
    Completed,
}

pub(super) struct StreamLoopOutput {
    pub(super) outcome: StreamLoopOutcome,
    pub(super) tui_error_classification: TuiErrorClassification,
    pub(super) pending_long_running_open_after_state_save: PendingLongRunningOpenAfterStateSave,
    pub(super) pending_long_running_retarget_after_state_save:
        PendingLongRunningRetargetAfterStateSave,
}

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

    let rx = &mut *state.rx;
    let mut full_response = std::mem::take(state.full_response);
    let mut last_edit_text = std::mem::take(state.last_edit_text);
    let mut done = *state.done;
    let mut cancelled = *state.cancelled;
    let mut rx_disconnected = *state.rx_disconnected;
    let mut current_tool_line = state.current_tool_line.take();
    let mut prev_tool_status = state.prev_tool_status.take();
    let mut last_tool_name = state.last_tool_name.take();
    let mut last_tool_summary = state.last_tool_summary.take();
    let mut accumulated_input_tokens = *state.accumulated_input_tokens;
    let mut accumulated_cache_create_tokens = *state.accumulated_cache_create_tokens;
    let mut accumulated_cache_read_tokens = *state.accumulated_cache_read_tokens;
    let mut accumulated_output_tokens = *state.accumulated_output_tokens;
    let mut spin_idx = *state.spin_idx;
    let mut restart_followup_pending = *state.restart_followup_pending;
    let mut any_tool_used = *state.any_tool_used;
    let mut has_post_tool_text = *state.has_post_tool_text;
    let mut tmux_handed_off = *state.tmux_handed_off;
    let mut watcher_owns_assistant_relay = *state.watcher_owns_assistant_relay;
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

    // #2289 cancel finalization helper. Both the pre-`try_recv` guard
    // and the post-`try_recv` re-sample funnel through this so the
    // cancellation bookkeeping (inflight sync, `cancelled = true`,
    // background-child abort) stays in lock step and cannot drift.
    // Implemented as a macro so it can mutate locals owned by the
    // surrounding `while` body without moving them into a closure that
    // would conflict with `&mut` borrows held elsewhere. The macro
    // performs the bookkeeping; callers must follow with `break 'outer`
    // (or be in a position where falling through hits the outer loop
    // boundary) so the loop exits to the cancel post-processing path.
    macro_rules! finalize_cancel_inner {
        () => {{
            if sync_inflight_restart_mode_from_cancel(cancel_token.as_ref(), &mut inflight_state) {
                let _ = save_inflight_state(&inflight_state);
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

    let mut pending_long_running_open_after_state_save = None;
    let mut pending_long_running_retarget_after_state_save = None;

    'outer: while !done
        || terminal_control_drain_until.is_some_and(|deadline| std::time::Instant::now() < deadline)
    {
        let mut state_dirty = false;

        // #2172 cancel boundary: once `done` is true the turn's
        // terminal outcome (Completed / Done message) has already been
        // observed; the loop continues only to drain residual control
        // frames during `terminal_control_drain_until`. A cancel that
        // arrives in that drain window MUST NOT reclassify the
        // already-completed turn as cancelled (which would re-run
        // stop_active_turn and dispatch-cancel finalisation). The
        // documented "whichever comes first wins" priority is
        // enforced by gating the cancel arm on `!done`. Cancels
        // arriving during the drain window break out of the loop
        // normally as a completed turn.
        if !done && cancel_requested(Some(cancel_token.as_ref())) {
            finalize_cancel_inner!();
            break 'outer;
        }
        if done && cancel_requested(Some(cancel_token.as_ref())) {
            // #2172: cancel-after-Done during terminal drain — exit
            // the drain immediately as a completed turn instead of
            // burning the rest of the drain window. Suppressing the
            // reclassification still preserves the "stop after
            // completion is a no-op" UX.
            break 'outer;
        }

        // #2426 H3/H4 graduation: wait for the next stream frame and
        // treat the duration as a safety wake, not a pre-drain sleep.
        // Explicit handoff frames (`TmuxReady` / `ProcessReady` /
        // `RuntimeReady`) wake this loop immediately and clear
        // `terminal_control_drain_until` in their handlers.
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
        if done && cancel_requested(Some(cancel_token.as_ref())) {
            // See note above on the cancel-after-Done race.
            break 'outer;
        }

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
                    match msg {
                        content_message @ (StreamMessage::RetryBoundary
                        | StreamMessage::Init { .. }
                        | StreamMessage::Text { .. }
                        | StreamMessage::Thinking { .. }
                        | StreamMessage::Done { .. }
                        | StreamMessage::Error { .. }
                        | StreamMessage::StatusUpdate { .. }
                        | StreamMessage::StatusEvents { .. }) => {
                            let message = match content_message {
                                StreamMessage::RetryBoundary => {
                                    StreamContentArmMessage::RetryBoundary
                                }
                                StreamMessage::Init {
                                    session_id,
                                    raw_session_id,
                                } => StreamContentArmMessage::Init {
                                    session_id,
                                    raw_session_id,
                                },
                                StreamMessage::Text { content } => {
                                    StreamContentArmMessage::Text { content }
                                }
                                StreamMessage::Thinking { summary } => {
                                    StreamContentArmMessage::Thinking { summary }
                                }
                                StreamMessage::Done { result, session_id } => {
                                    StreamContentArmMessage::Done { result, session_id }
                                }
                                StreamMessage::Error {
                                    message, stderr, ..
                                } => StreamContentArmMessage::Error { message, stderr },
                                StreamMessage::StatusUpdate {
                                    input_tokens,
                                    cache_create_tokens,
                                    cache_read_tokens,
                                    output_tokens,
                                    ..
                                } => StreamContentArmMessage::StatusUpdate {
                                    input_tokens,
                                    cache_create_tokens,
                                    cache_read_tokens,
                                    output_tokens,
                                },
                                StreamMessage::StatusEvents { events } => {
                                    StreamContentArmMessage::StatusEvents { events }
                                }
                                _ => unreachable!("content-message pattern must stay exhaustive"),
                            };
                            let outcome = handle_stream_content_message(
                                message,
                                StreamContentArmContext {
                                    shared_owned: &shared_owned,
                                    gateway: &gateway,
                                    channel_id,
                                    provider: &provider,
                                    voice_progress_playback_channel_id,
                                    watcher_owns_assistant_relay,
                                    watcher_relay_available_for_turn,
                                    standby_relay_owns_output,
                                    terminal_control_ready_observed,
                                    streaming_rollover_frozen_msg_ids:
                                        &streaming_rollover_frozen_msg_ids,
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
                                    current_msg_id,
                                },
                                StreamToolArmState {
                                    state_dirty: &mut state_dirty,
                                    inflight_state: &mut inflight_state,
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
                                    status_panel_dirty: &mut status_panel_dirty,
                                },
                            )
                            .await;
                            match outcome {
                                StreamToolArmOutcome::Continue => {}
                            }
                        }
                        StreamMessage::ToolResult {
                            content,
                            is_error,
                            tool_use_id,
                        } => {
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
                                    current_msg_id,
                                },
                                StreamToolArmState {
                                    state_dirty: &mut state_dirty,
                                    inflight_state: &mut inflight_state,
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
                                    status_panel_dirty: &mut status_panel_dirty,
                                },
                            )
                            .await;
                            match outcome {
                                StreamToolArmOutcome::Continue => {}
                            }
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
                                    current_msg_id,
                                },
                                StreamToolArmState {
                                    state_dirty: &mut state_dirty,
                                    inflight_state: &mut inflight_state,
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
                                    status_panel_dirty: &mut status_panel_dirty,
                                },
                            )
                            .await;
                            match outcome {
                                StreamToolArmOutcome::Continue => {}
                            }
                        }
                        StreamMessage::TmuxReady {
                            output_path,
                            input_fifo_path,
                            tmux_session_name,
                            last_offset,
                        } => {
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
                            match outcome {
                                RuntimeHandoffLoopOutcome::ContinueDraining => {}
                            }
                        }
                        StreamMessage::RuntimeReady { handoff } => {
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
                            match outcome {
                                RuntimeHandoffLoopOutcome::ContinueDraining => {}
                            }
                        }
                        StreamMessage::ProcessReady {
                            output_path,
                            session_name,
                            last_offset,
                        } => {
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
                            match outcome {
                                RuntimeHandoffLoopOutcome::ContinueDraining => {}
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
                            match outcome {
                                RuntimeHandoffLoopOutcome::ContinueDraining => {}
                            }
                        }
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

        run_bridge_stream_tick(
            BridgeStreamTickContext {
                shared_owned: shared_owned.clone(),
                gateway: gateway.clone(),
                channel_id,
                provider: &provider,
                turn_id: turn_id.as_str(),
                status_interval,
                single_message_panel_footer_mode,
                footer_owner,
                status_panel_started_at,
                done,
                standby_relay_owns_output,
                watcher_owner_channel_id,
                full_response: full_response.as_str(),
                dispatch_id: dispatch_id.clone(),
                adk_session_key: adk_session_key.clone(),
                adk_session_name: adk_session_name.clone(),
                adk_session_info: adk_session_info.clone(),
                adk_cwd: adk_cwd.clone(),
                role_binding: role_binding.clone(),
                current_tool_line: current_tool_line.clone(),
                last_tool_name: last_tool_name.clone(),
                last_tool_summary: last_tool_summary.clone(),
                prev_tool_status: prev_tool_status.clone(),
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
                response_sent_offset: &mut response_sent_offset,
                bridge_confirmed_response_sent_offset: &mut bridge_confirmed_response_sent_offset,
                streaming_rollover_frozen_msg_ids: &mut streaming_rollover_frozen_msg_ids,
                current_msg_id: &mut current_msg_id,
                last_edit_text: &mut last_edit_text,
                first_answer_relayed: &mut first_answer_relayed,
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

    StreamLoopOutput {
        outcome: StreamLoopOutcome::Completed,
        tui_error_classification,
        pending_long_running_open_after_state_save,
        pending_long_running_retarget_after_state_save,
    }
}
