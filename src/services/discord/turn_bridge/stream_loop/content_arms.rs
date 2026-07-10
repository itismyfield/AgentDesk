//! Content/status/terminal stream-loop arms for `turn_bridge::spawn_turn_bridge`.

use super::super::thinking::{redacted_thinking_transcript_event, thinking_status_line};
use super::*;
use std::sync::Arc;

pub(super) enum StreamContentArmMessage {
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
pub(super) enum StreamContentArmOutcome {
    ContinueDraining,
    SkipRemainderOfDrainIteration,
}

pub(super) struct StreamContentArmContext<'a> {
    pub(super) shared_owned: &'a Arc<SharedData>,
    pub(super) gateway: &'a Arc<dyn TurnGateway>,
    pub(super) channel_id: ChannelId,
    pub(super) provider: &'a ProviderKind,
    pub(super) voice_progress_playback_channel_id: Option<ChannelId>,
    pub(super) watcher_owns_assistant_relay: bool,
    pub(super) watcher_relay_available_for_turn: bool,
    pub(super) standby_relay_owns_output: bool,
    pub(super) terminal_control_ready_observed: bool,
    pub(super) streaming_rollover_frozen_msg_ids: &'a Vec<MessageId>,
    pub(super) context_window_tokens: u64,
    pub(super) context_compact_percent: u64,
}

pub(super) struct StreamContentArmState<'a> {
    pub(super) state_dirty: &'a mut bool,
    pub(super) full_response: &'a mut String,
    pub(super) current_tool_line: &'a mut Option<String>,
    pub(super) prev_tool_status: &'a mut Option<String>,
    pub(super) last_tool_name: &'a mut Option<String>,
    pub(super) last_tool_summary: &'a mut Option<String>,
    pub(super) any_tool_used: &'a mut bool,
    pub(super) has_post_tool_text: &'a mut bool,
    pub(super) response_sent_offset: &'a mut usize,
    pub(super) last_edit_text: &'a mut String,
    pub(super) new_session_id: &'a mut Option<String>,
    pub(super) new_raw_provider_session_id: &'a mut Option<String>,
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) transcript_events: &'a mut Vec<SessionTranscriptEvent>,
    pub(super) session_handshake_seen: &'a mut bool,
    pub(super) streamed_assistant_text_this_turn: &'a mut bool,
    pub(super) last_assistant_text_line: &'a mut Option<String>,
    pub(super) status_panel_dirty: &'a mut bool,
    pub(super) recovery_retry: &'a mut bool,
    pub(super) pending_long_running_open_after_state_save:
        &'a mut PendingLongRunningOpenAfterStateSave,
    pub(super) long_running_placeholder_active: &'a mut LongRunningPlaceholderActive,
    pub(super) pending_long_running_retarget_after_state_save:
        &'a mut PendingLongRunningRetargetAfterStateSave,
    pub(super) terminal_full_replay_cleanup_msg_ids: &'a mut Vec<MessageId>,
    pub(super) active_background_child_session_ids: &'a mut Vec<i64>,
    pub(super) done: &'a mut bool,
    pub(super) terminal_control_drain_until: &'a mut Option<std::time::Instant>,
    pub(super) transport_error: &'a mut bool,
    pub(super) resume_failure_detected: &'a mut bool,
    pub(super) bridge_confirmed_response_sent_offset: &'a mut usize,
    pub(super) terminal_session_reset_required: &'a mut bool,
    pub(super) accumulated_input_tokens: &'a mut u64,
    pub(super) accumulated_cache_create_tokens: &'a mut u64,
    pub(super) accumulated_cache_read_tokens: &'a mut u64,
    pub(super) accumulated_output_tokens: &'a mut u64,
}

#[rustfmt::skip]
pub(super) async fn handle_stream_content_message(
    message: StreamContentArmMessage,
    ctx: StreamContentArmContext<'_>,
    state: StreamContentArmState<'_>,
) -> StreamContentArmOutcome {
    let shared_owned = Arc::clone(ctx.shared_owned);
    let gateway = Arc::clone(ctx.gateway);
    let channel_id = ctx.channel_id;
    let provider = ctx.provider.clone();
    let voice_progress_playback_channel_id = ctx.voice_progress_playback_channel_id;
    let watcher_owns_assistant_relay = ctx.watcher_owns_assistant_relay;
    let watcher_relay_available_for_turn = ctx.watcher_relay_available_for_turn;
    let standby_relay_owns_output = ctx.standby_relay_owns_output;
    let terminal_control_ready_observed = ctx.terminal_control_ready_observed;
    let streaming_rollover_frozen_msg_ids = ctx.streaming_rollover_frozen_msg_ids;
    let context_window_tokens = ctx.context_window_tokens;
    let context_compact_percent = ctx.context_compact_percent;

    let mut state_dirty = *state.state_dirty;
    let mut full_response = std::mem::take(state.full_response);
    let mut current_tool_line = state.current_tool_line.take();
    let mut prev_tool_status = state.prev_tool_status.take();
    let mut last_tool_name = state.last_tool_name.take();
    let mut last_tool_summary = state.last_tool_summary.take();
    let mut any_tool_used = *state.any_tool_used;
    let mut has_post_tool_text = *state.has_post_tool_text;
    let mut response_sent_offset = *state.response_sent_offset;
    let mut last_edit_text = std::mem::take(state.last_edit_text);
    let mut new_session_id = state.new_session_id.take();
    let mut new_raw_provider_session_id = state.new_raw_provider_session_id.take();
    let inflight_state = &mut *state.inflight_state;
    let mut transcript_events = std::mem::take(state.transcript_events);
    let mut session_handshake_seen = *state.session_handshake_seen;
    let mut streamed_assistant_text_this_turn = *state.streamed_assistant_text_this_turn;
    let mut last_assistant_text_line = state.last_assistant_text_line.take();
    let mut status_panel_dirty = *state.status_panel_dirty;
    let mut recovery_retry = *state.recovery_retry;
    let mut pending_long_running_open_after_state_save =
        state.pending_long_running_open_after_state_save.take();
    let mut long_running_placeholder_active = state.long_running_placeholder_active.take();
    let mut pending_long_running_retarget_after_state_save =
        state.pending_long_running_retarget_after_state_save.take();
    let mut terminal_full_replay_cleanup_msg_ids =
        std::mem::take(state.terminal_full_replay_cleanup_msg_ids);
    let mut active_background_child_session_ids =
        std::mem::take(state.active_background_child_session_ids);
    let mut done = *state.done;
    let mut terminal_control_drain_until = *state.terminal_control_drain_until;
    let mut transport_error = *state.transport_error;
    let mut resume_failure_detected = *state.resume_failure_detected;
    let mut bridge_confirmed_response_sent_offset =
        *state.bridge_confirmed_response_sent_offset;
    let mut terminal_session_reset_required = *state.terminal_session_reset_required;
    let mut accumulated_input_tokens = *state.accumulated_input_tokens;
    let mut accumulated_cache_create_tokens = *state.accumulated_cache_create_tokens;
    let mut accumulated_cache_read_tokens = *state.accumulated_cache_read_tokens;
    let mut accumulated_output_tokens = *state.accumulated_output_tokens;

    macro_rules! finish {
        ($outcome:expr) => {{
            *state.state_dirty = state_dirty;
            *state.full_response = full_response;
            *state.current_tool_line = current_tool_line;
            *state.prev_tool_status = prev_tool_status;
            *state.last_tool_name = last_tool_name;
            *state.last_tool_summary = last_tool_summary;
            *state.any_tool_used = any_tool_used;
            *state.has_post_tool_text = has_post_tool_text;
            *state.response_sent_offset = response_sent_offset;
            *state.last_edit_text = last_edit_text;
            *state.new_session_id = new_session_id;
            *state.new_raw_provider_session_id = new_raw_provider_session_id;
            *state.transcript_events = transcript_events;
            *state.session_handshake_seen = session_handshake_seen;
            *state.streamed_assistant_text_this_turn = streamed_assistant_text_this_turn;
            *state.last_assistant_text_line = last_assistant_text_line;
            *state.status_panel_dirty = status_panel_dirty;
            *state.recovery_retry = recovery_retry;
            *state.pending_long_running_open_after_state_save =
                pending_long_running_open_after_state_save;
            *state.long_running_placeholder_active = long_running_placeholder_active;
            *state.pending_long_running_retarget_after_state_save =
                pending_long_running_retarget_after_state_save;
            *state.terminal_full_replay_cleanup_msg_ids = terminal_full_replay_cleanup_msg_ids;
            *state.active_background_child_session_ids = active_background_child_session_ids;
            *state.done = done;
            *state.terminal_control_drain_until = terminal_control_drain_until;
            *state.transport_error = transport_error;
            *state.resume_failure_detected = resume_failure_detected;
            *state.bridge_confirmed_response_sent_offset =
                bridge_confirmed_response_sent_offset;
            *state.terminal_session_reset_required = terminal_session_reset_required;
            *state.accumulated_input_tokens = accumulated_input_tokens;
            *state.accumulated_cache_create_tokens = accumulated_cache_create_tokens;
            *state.accumulated_cache_read_tokens = accumulated_cache_read_tokens;
            *state.accumulated_output_tokens = accumulated_output_tokens;
            return $outcome;
        }};
    }

    match message {
        StreamContentArmMessage::RetryBoundary => {
                            if provider == ProviderKind::Gemini
                                && handle_gemini_retry_boundary(
                                    &mut full_response,
                                    &mut current_tool_line,
                                    &mut prev_tool_status,
                                    &mut last_tool_name,
                                    &mut last_tool_summary,
                                    &mut any_tool_used,
                                    &mut has_post_tool_text,
                                    &mut response_sent_offset,
                                    &mut last_edit_text,
                                    &mut new_session_id,
                                    &mut new_raw_provider_session_id,
                                    inflight_state,
                                )
                            {
                                transcript_events.clear();
                                state_dirty = true;
                            }
                        }
        StreamContentArmMessage::Init {
                            session_id: sid,
                            raw_session_id,
                        } => {
                            new_session_id = Some(sid.clone());
                            new_raw_provider_session_id =
                                raw_session_id.or_else(|| Some(sid.clone()));
                            inflight_state.session_id = Some(sid);
                            // #2451 H5: explicit handshake witness — the
                            // provider has answered with a bound session.
                            // Any subsequent empty-response classification
                            // can rely on this instead of elapsed-time
                            // guessing.
                            session_handshake_seen = true;
                            state_dirty = true;
                        }
        StreamContentArmMessage::Text { content } => {
                            let (content, progress_markers) =
                                if inflight_state.source == crate::dispatch::Source::Voice {
                                    crate::voice::progress::extract_progress_markers(&content)
                                } else {
                                    (content, Vec::new())
                                };
                            for marker in progress_markers {
                                shared_owned.voice_barge_in.publish_progress_for_playback(
                                    channel_id,
                                    voice_progress_playback_channel_id,
                                    marker,
                                );
                            }
                            if content.is_empty() {
                                finish!(StreamContentArmOutcome::SkipRemainderOfDrainIteration);
                            }
                            streamed_assistant_text_this_turn = true;
                            // #3608: normalize the chunk boundary so a tool-use
                            // `\n\n` separator + a chunk that itself begins with
                            // blank lines does not accumulate into `\n\n\n\n`.
                            chunk_compose::append_streamed_text_chunk(&mut full_response, &content);
                            if (watcher_owns_assistant_relay && watcher_relay_available_for_turn)
                                || standby_relay_owns_output
                            {
                                response_sent_offset = full_response.len();
                                inflight_state.response_sent_offset = response_sent_offset;
                            }
                            // #1255: remember the last non-empty single-line
                            // assistant prose so we can surface it on a
                            // long-running tool placeholder card. Mid-stream
                            // chunks routinely contain newlines, so we walk
                            // backwards through the lines and pick the most
                            // recent non-empty one.  `Text` events arrive
                            // before the immediately-following `ToolUse` event
                            // in Claude Code's stream ordering, so this
                            // captures the right hint without buffering.
                            if let Some(line) = content
                                .lines()
                                .filter(|l| !l.trim().is_empty())
                                .next_back()
                                .map(str::trim)
                                .map(str::to_string)
                            {
                                last_assistant_text_line = Some(line);
                            }
                            push_transcript_event(
                                &mut transcript_events,
                                SessionTranscriptEvent {
                                    kind: SessionTranscriptEventKind::Assistant,
                                    tool_name: None,
                                    summary: None,
                                    content: content.clone(),
                                    status: Some("success".to_string()),
                                    is_error: false,
                                },
                            );
                            if any_tool_used {
                                has_post_tool_text = true;
                                inflight_state.has_post_tool_text = true;
                            }
                            super::super::super::formatting::preserve_previous_tool_status(
                                &mut prev_tool_status,
                                current_tool_line.as_deref(),
                                None,
                            );
                            current_tool_line = None;
                            last_tool_name = None;
                            last_tool_summary = None;
                            inflight_state.full_response = full_response.clone();
                            state_dirty = true;
                        }
        StreamContentArmMessage::Thinking { summary } => {
                            let display = thinking_status_line();
                            status_panel_dirty |= record_status_panel_events(
                                shared_owned.as_ref(),
                                channel_id,
                                vec![StatusEvent::Heartbeat],
                            );
                            if inflight_state.source == crate::dispatch::Source::Voice {
                                shared_owned.voice_barge_in.publish_progress_for_playback(
                                    channel_id,
                                    voice_progress_playback_channel_id,
                                    "thinking",
                                );
                            }
                            // #1113 implicit-terminate: a Thinking event after an
                            // unfinished ToolUse means the agent moved on without
                            // emitting a ToolResult. Promote the orphaned tool to
                            // its terminal (⚠) form before stashing in prev so the
                            // user does not see a stale ⚙ indicator.
                            let prev_for_preserve = current_tool_line
                                .as_deref()
                                .map(super::super::super::formatting::finalize_in_progress_tool_status);
                            super::super::super::formatting::preserve_previous_tool_status(
                                &mut prev_tool_status,
                                prev_for_preserve.as_deref(),
                                Some(display.as_str()),
                            );
                            current_tool_line = Some(display);
                            last_tool_name = None;
                            last_tool_summary = None;
                            // Thinking payloads can contain raw model reasoning.
                            // Keep them out of the user-visible Discord response
                            // and use only a neutral progress marker.
                            state_dirty = true;
                            push_transcript_event(
                                &mut transcript_events,
                                redacted_thinking_transcript_event(summary),
                            );
                        }
        StreamContentArmMessage::Done {
                            result,
                            session_id: sid,
                        } => {
                            let session_died_retry = result == "__session_died_retry__";
                            if session_died_retry {
                                // Recovery reader requests the generic Discord-history
                                // auto-retry when the resumed session dies pre-completion.
                                recovery_retry = true;
                            }
                            if pending_long_running_open_after_state_save.take().is_some() {
                                inflight_state.long_running_placeholder_active = false;
                                let _ = save_inflight_state(inflight_state);
                            }
                            // #1255: turn finished while a long-running placeholder
                            // is still Active — close it now so the user does not
                            // stare at a stale 🔄 card. Idempotent if a prior
                            // ToolResult already fired Completed.
                            if let Some((key, snapshot, close_trigger, ack_consumed)) =
                                long_running_placeholder_active.take()
                            {
                                let target = if session_died_retry {
                                    super::super::super::placeholder_controller::PlaceholderLifecycle::Aborted
                                } else {
                                    super::super::super::placeholder_controller::PlaceholderLifecycle::Completed
                                };
                                let pending_retarget_matches_key =
                                    pending_long_running_retarget_after_state_save
                                        .as_ref()
                                        .is_some_and(|(pending_key, _, _, _, _)| {
                                            *pending_key == key
                                        });
                                if pending_retarget_matches_key {
                                    let _ = pending_long_running_retarget_after_state_save.take();
                                    shared_owned.ui.placeholder_controller.detach(&key);
                                    inflight_state.long_running_placeholder_active = false;
                                } else {
                                    let outcome = shared_owned
                                        .ui
                                        .placeholder_controller
                                        .transition(gateway.as_ref(), key.clone(), target)
                                        .await;
                                    // codex round-10/11 P2/P3: on `EditFailed`,
                                    // re-stash the tuple so subsequent
                                    // streaming/sweeper paths can retry the
                                    // terminal edit. Only clear the persisted
                                    // flag on a committed (or already-terminal)
                                    // transition.
                                    use super::super::super::placeholder_controller::PlaceholderControllerOutcome::*;
                                    if matches!(outcome, Edited | Coalesced | AlreadyTerminal) {
                                        inflight_state.long_running_placeholder_active = false;
                                    } else {
                                        long_running_placeholder_active =
                                            Some((key, snapshot, close_trigger, ack_consumed));
                                    }
                                }
                            }
                            if let Some(resolved) = resolve_done_response(
                                &full_response,
                                &result,
                                any_tool_used,
                                has_post_tool_text,
                            ) {
                                full_response = resolved;
                                // #3419 R3 (codex MEDIUM): a short sentinel/tool-only
                                // resolved body shrinks full_response below a prior
                                // streamed offset that the >8000-byte replay gate
                                // (`done_result_requires_full_terminal_replay`) does not
                                // reset → out of bounds (watcher empty-slice wedge).
                                // `sync_response_delivery_state` clamps to len AND walks
                                // back to a valid char boundary (a bare `.min(len())`
                                // could land mid multibyte char) and mirrors both.
                                sync_response_delivery_state(
                                    &full_response,
                                    &mut response_sent_offset,
                                    inflight_state,
                                );
                            }
                            if done_result_requires_full_terminal_replay(
                                &full_response,
                                &result,
                                response_sent_offset,
                                streamed_assistant_text_this_turn,
                            ) {
                                tracing::info!(
                                    target: "agentdesk::codex_rollout_handoff",
                                    provider = %provider.as_str(),
                                    channel_id = channel_id.get(),
                                    previous_response_sent_offset = response_sent_offset,
                                    full_response_len = full_response.len(),
                                    done_result_len = result.len(),
                                    frozen_rollover_messages = streaming_rollover_frozen_msg_ids.len(),
                                    "turn_bridge reset terminal delivery offset for authoritative Done body"
                                );
                                terminal_full_replay_cleanup_msg_ids =
                                    streaming_rollover_frozen_msg_ids.clone();
                                response_sent_offset = 0;
                            }
                            if let Some(s) = sid {
                                new_session_id = Some(s.clone());
                                inflight_state.session_id = Some(s);
                            }
                            if !session_died_retry {
                                push_transcript_event(
                                    &mut transcript_events,
                                    SessionTranscriptEvent {
                                        kind: SessionTranscriptEventKind::Result,
                                        tool_name: None,
                                        summary: Some(if result.trim().is_empty() {
                                            "Turn completed".to_string()
                                        } else {
                                            truncate_str(&result, 120).to_string()
                                        }),
                                        content: result,
                                        status: Some("success".to_string()),
                                        is_error: false,
                                    },
                                );
                            }
                            if session_died_retry {
                                close_all_tracked_background_children(
                                    shared_owned.pg_pool.as_ref(),
                                    &mut active_background_child_session_ids,
                                    "aborted",
                                    "turn done",
                                )
                                .await;
                            }
                            state_dirty = true;
                            done = true;
                            // #2449: only arm the 250ms drain when handoff is still
                            // ambiguous; fresh managed turns may pre-stamp runtime_kind.
                            if !terminal_control_ready_observed {
                                terminal_control_drain_until = Some(
                                    std::time::Instant::now()
                                        + std::time::Duration::from_millis(250),
                                );
                            }
                        }
        StreamContentArmMessage::Error { message, stderr } => {
                            let is_stale_resume =
                                stream_error_has_stale_resume_error(&message, &stderr);
                            let session_reset_required =
                                stream_error_requires_terminal_session_reset(&message, &stderr);
                            transport_error = true;
                            let combined = format!("{} {}", message, stderr).to_lowercase();
                            if combined.contains("prompt is too long")
                                || combined.contains("prompt too long")
                                || combined.contains("context_length_exceeded")
                                || combined.contains("max_tokens")
                                || combined.contains("context window")
                                || combined.contains("token limit")
                            {
                                // Prompt too long is not a terminal failure — user can retry
                                // with a shorter message or /compact. Don't mark as transport error.
                                transport_error = false;
                                full_response = "⚠️ __prompt too long__".to_string();
                            } else if is_stale_resume {
                                // Recoverable stale resume: auto-retry with a fresh provider
                                // session instead of failing the current dispatch/turn.
                                transport_error = false;
                                resume_failure_detected = true;
                                if !stderr.is_empty() {
                                    full_response = format!(
                                        "Error: {}\nstderr: {}",
                                        message,
                                        truncate_str(&stderr, 500)
                                    );
                                } else {
                                    full_response = format!("Error: {}", message);
                                }
                            } else if !stderr.is_empty() {
                                full_response = format!(
                                    "Error: {}\nstderr: {}",
                                    message,
                                    truncate_str(&stderr, 500)
                                );
                            } else {
                                full_response = format!("Error: {}", message);
                            }
                            sync_terminal_error_delivery_state_for_bridge_owner(
                                &full_response,
                                &mut response_sent_offset,
                                &mut bridge_confirmed_response_sent_offset,
                                inflight_state,
                                channel_id,
                                watcher_owns_assistant_relay && watcher_relay_available_for_turn,
                            );
                            push_transcript_event(
                                &mut transcript_events,
                                SessionTranscriptEvent {
                                    kind: SessionTranscriptEventKind::Error,
                                    tool_name: last_tool_name.clone(),
                                    summary: Some(message.clone()),
                                    content: if stderr.trim().is_empty() {
                                        message.clone()
                                    } else {
                                        format!("{message}\n{stderr}")
                                    },
                                    status: Some("error".to_string()),
                                    is_error: true,
                                },
                            );
                            if session_reset_required {
                                terminal_session_reset_required = true;
                                clear_local_session_state(
                                    &mut new_session_id,
                                    &mut new_raw_provider_session_id,
                                    inflight_state,
                                );
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    "  [{ts}] ⚠ Clearing stored provider session after terminal {} session failure (channel {})",
                                    provider.as_str(),
                                    channel_id,
                                );
                            }
                            close_all_tracked_background_children(
                                shared_owned.pg_pool.as_ref(),
                                &mut active_background_child_session_ids,
                                "aborted",
                                "stream error",
                            )
                            .await;
                            state_dirty = true;
                            done = true;
                            terminal_control_drain_until = None;
                        }
        StreamContentArmMessage::StatusUpdate {
                            input_tokens,
                            cache_create_tokens,
                            cache_read_tokens,
                            output_tokens,
                        } => {
                            let has_context_token_data = input_tokens.is_some()
                                || cache_create_tokens.is_some()
                                || cache_read_tokens.is_some();
                            // Token fields are provider-normalized snapshots,
                            // not deltas. Claude reports uncached input,
                            // cache writes, and cache reads separately, while
                            // other providers may omit unavailable cache
                            // fields. Keep the largest context-occupancy
                            // snapshot seen in this turn so late partial
                            // status events cannot make the live panel shrink.
                            if has_context_token_data {
                                apply_context_token_update(
                                    &mut accumulated_input_tokens,
                                    &mut accumulated_cache_create_tokens,
                                    &mut accumulated_cache_read_tokens,
                                    input_tokens,
                                    cache_create_tokens,
                                    cache_read_tokens,
                                );
                            }
                            if let Some(ot) = output_tokens {
                                accumulated_output_tokens = accumulated_output_tokens.max(ot);
                            }
                            if shared_owned.ui.status_panel_v2_enabled && has_context_token_data {
                                let context_provider_session_id = new_raw_provider_session_id
                                    .as_deref()
                                    .or(new_session_id.as_deref())
                                    .or(inflight_state.session_id.as_deref());
                                let context_dirty = shared_owned
                                    .ui
                                    .placeholder_live_events
                                    .set_context_panel_usage(
                                        channel_id,
                                        context_provider_session_id,
                                        accumulated_input_tokens,
                                        accumulated_cache_create_tokens,
                                        accumulated_cache_read_tokens,
                                        context_window_tokens,
                                        context_compact_percent,
                                    );
                                status_panel_dirty |= context_dirty;
                            }
                        }
        StreamContentArmMessage::StatusEvents { events } => {
                            status_panel_dirty |= record_status_panel_events(
                                shared_owned.as_ref(),
                                channel_id,
                                events,
                            );
                        }
    }

    finish!(StreamContentArmOutcome::ContinueDraining);
}
