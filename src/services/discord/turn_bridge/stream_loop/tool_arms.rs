//! Tool-use/result/task-notification stream-loop arms for `turn_bridge::spawn_turn_bridge`.

use std::sync::Arc;

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::tool_output_guard::matched_or_last_tool;

use super::super::stream_tick::guarded_persist::{
    VisibleMutationAuthority, visible_mutation_authority_after_guarded_save,
};
use super::*;

pub(super) enum StreamToolArmMessage {
    ToolUse {
        name: String,
        input: String,
        tool_use_id: Option<String>,
    },
    ToolResult {
        content: String,
        is_error: bool,
        tool_use_id: Option<String>,
    },
    TaskNotification {
        tool_use_id: Option<String>,
        summary: String,
        status: String,
        kind: TaskNotificationKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StreamToolArmOutcome {
    Continue,
    RetryExactFrame,
    AuthorityLost,
}

fn stream_tool_outcome_after_restart_authority(
    authority: Option<VisibleMutationAuthority>,
) -> StreamToolArmOutcome {
    if authority == Some(VisibleMutationAuthority::AuthorityLost) {
        StreamToolArmOutcome::AuthorityLost
    } else {
        StreamToolArmOutcome::Continue
    }
}

fn terminal_tool_result_transition_permission(
    authority: VisibleMutationAuthority,
) -> Result<bool, StreamToolArmOutcome> {
    match authority {
        VisibleMutationAuthority::Authorized => Ok(true),
        VisibleMutationAuthority::Suppressed => Ok(false),
        VisibleMutationAuthority::Retry => Err(StreamToolArmOutcome::RetryExactFrame),
        VisibleMutationAuthority::AuthorityLost => Err(StreamToolArmOutcome::AuthorityLost),
    }
}

pub(super) fn retain_exact_stream_frame_on_tool_retry(
    pending: &mut std::collections::VecDeque<StreamMessage>,
    frame: StreamMessage,
    outcome: StreamToolArmOutcome,
) -> bool {
    if outcome != StreamToolArmOutcome::RetryExactFrame {
        return false;
    }
    pending.push_front(frame);
    true
}

pub(super) struct StreamToolArmContext<'a> {
    pub(super) shared_owned: &'a Arc<SharedData>,
    pub(super) gateway: &'a Arc<dyn TurnGateway>,
    pub(super) channel_id: ChannelId,
    pub(super) provider: &'a ProviderKind,
    pub(super) user_text_owned: &'a String,
    pub(super) request_owner_name: &'a str,
    pub(super) adk_session_key: &'a Option<String>,
    pub(super) adk_session_name: &'a Option<String>,
    pub(super) role_binding: &'a Option<RoleBinding>,
    pub(super) voice_progress_playback_channel_id: Option<ChannelId>,
    pub(super) single_message_panel_footer_mode: bool,
    pub(super) footer_owner: super::super::super::footer_view_reconciler::CompletionFooterOwner,
    pub(super) current_msg_id: MessageId,
}

pub(super) struct StreamToolArmState<'a> {
    pub(super) state_dirty: &'a mut bool,
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) persisted_inflight_baseline: &'a mut InflightTurnState,
    pub(super) stream_tick_expected_identity:
        &'a crate::services::discord::inflight::InflightTurnIdentity,
    pub(super) expected_current_message: &'a mut (u64, usize),
    pub(super) current_tool_line: &'a mut Option<String>,
    pub(super) prev_tool_status: &'a mut Option<String>,
    pub(super) last_tool_name: &'a mut Option<String>,
    pub(super) last_tool_summary: &'a mut Option<String>,
    pub(super) any_tool_used: &'a mut bool,
    pub(super) has_post_tool_text: &'a mut bool,
    pub(super) last_assistant_text_line: &'a mut Option<String>,
    pub(super) spin_idx: &'a mut usize,
    pub(super) transcript_events: &'a mut Vec<SessionTranscriptEvent>,
    pub(super) pending_status_tool_results: &'a mut VecDeque<String>,
    pub(super) pending_status_tool_results_by_id: &'a mut std::collections::HashMap<String, String>,
    pub(super) long_running_placeholder_active: &'a mut LongRunningPlaceholderActive,
    pub(super) active_background_child_session_ids: &'a mut Vec<i64>,
    pub(super) pending_long_running_open_after_state_save:
        &'a mut PendingLongRunningOpenAfterStateSave,
    pub(super) pending_long_running_retarget_after_state_save:
        &'a mut PendingLongRunningRetargetAfterStateSave,
    pub(super) restart_followup_pending: &'a mut bool,
    pub(super) last_edit_text: &'a mut String,
    pub(super) full_response: &'a mut String,
    pub(super) status_panel_dirty: &'a mut bool,
}

#[rustfmt::skip]
pub(super) async fn handle_stream_tool_message(
    message: StreamToolArmMessage,
    ctx: StreamToolArmContext<'_>,
    state: StreamToolArmState<'_>,
) -> StreamToolArmOutcome {
    let shared_owned = Arc::clone(ctx.shared_owned);
    let gateway = Arc::clone(ctx.gateway);
    let channel_id = ctx.channel_id;
    let provider = ctx.provider.clone();
    let user_text_owned = ctx.user_text_owned.clone();
    let request_owner_name = ctx.request_owner_name;
    let adk_session_key = ctx.adk_session_key;
    let adk_session_name = ctx.adk_session_name;
    let role_binding = ctx.role_binding;
    let voice_progress_playback_channel_id = ctx.voice_progress_playback_channel_id;
    let single_message_panel_footer_mode = ctx.single_message_panel_footer_mode;
    let footer_owner = ctx.footer_owner;
    let current_msg_id = ctx.current_msg_id;

    let mut state_dirty = *state.state_dirty;
    let inflight_state = &mut *state.inflight_state;
    let persisted_inflight_baseline = &mut *state.persisted_inflight_baseline;
    let stream_tick_expected_identity = state.stream_tick_expected_identity;
    let expected_current_message = &mut *state.expected_current_message;
    let mut current_tool_line = state.current_tool_line.take();
    let mut prev_tool_status = state.prev_tool_status.take();
    let mut last_tool_name = state.last_tool_name.take();
    let mut last_tool_summary = state.last_tool_summary.take();
    let mut any_tool_used = *state.any_tool_used;
    let mut has_post_tool_text = *state.has_post_tool_text;
    let mut last_assistant_text_line = state.last_assistant_text_line.take();
    let mut spin_idx = *state.spin_idx;
    let mut transcript_events = std::mem::take(state.transcript_events);
    let mut pending_status_tool_results = std::mem::take(state.pending_status_tool_results);
    let mut pending_status_tool_results_by_id =
        std::mem::take(state.pending_status_tool_results_by_id);
    let mut long_running_placeholder_active = state.long_running_placeholder_active.take();
    let mut active_background_child_session_ids =
        std::mem::take(state.active_background_child_session_ids);
    let mut pending_long_running_open_after_state_save =
        state.pending_long_running_open_after_state_save.take();
    let mut pending_long_running_retarget_after_state_save =
        state.pending_long_running_retarget_after_state_save.take();
    let mut restart_followup_pending = *state.restart_followup_pending;
    let mut last_edit_text = std::mem::take(state.last_edit_text);
    let mut full_response = std::mem::take(state.full_response);
    let mut status_panel_dirty = *state.status_panel_dirty;
    let mut restart_visible_authority = None;
    let mut tool_outcome = StreamToolArmOutcome::Continue;

    match message {
        StreamToolArmMessage::ToolUse {
            name,
            input,
            tool_use_id,
        } => {
            // #3084: index the tool name by its tool-use id when
            // present so the matching ToolResult resolves the
            // exact tool regardless of interleaving; otherwise
            // fall back to FIFO ordering.
            match tool_use_id.as_deref() {
                Some(id) => {
                    pending_status_tool_results_by_id
                        .insert(id.to_string(), name.clone());
                }
                None => pending_status_tool_results.push_back(name.clone()),
            }
            any_tool_used = true;
            has_post_tool_text = false;
            inflight_state.any_tool_used = true;
            inflight_state.has_post_tool_text = false;
            if let (Some(pg_pool), Some(session_key)) =
                (shared_owned.pg_pool.as_ref(), adk_session_key.as_deref())
            {
                if let Err(error) =
                    mark_session_tool_use_pg(pg_pool, session_key).await
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Failed to update last_tool_at for session {}: {}",
                        session_key,
                        error
                    );
                }
            }
            if shared_owned.pg_pool.is_some() {
                match record_skill_usage_from_tool_use(
                    shared_owned.pg_pool.as_ref(),
                    &name,
                    &input,
                    adk_session_key.as_deref(),
                    role_binding.as_ref(),
                )
                .await
                {
                    Ok(Some(_)) => {}
                    Ok(None) => {}
                    Err(e) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ Failed to record skill usage for tool {}: {}",
                            name,
                            e
                        );
                    }
                }
            }
            let summary = format_tool_input(&name, &input);
            let display_summary = if summary.trim().is_empty() {
                "…".to_string()
            } else {
                truncate_str(&summary, 120).to_string()
            };
            let display = format!("⚙ {}: {}", name, display_summary);
            if inflight_state.source == crate::dispatch::Source::Voice {
                shared_owned.voice_barge_in.publish_progress_for_playback(
                    channel_id,
                    voice_progress_playback_channel_id,
                    format!("tool:{name}:{display_summary}"),
                );
            }
            record_placeholder_live_event(
                shared_owned.as_ref(),
                channel_id,
                super::super::super::placeholder_live_events::RecentPlaceholderEvent::tool_use(
                    &name, &input,
                ),
            );
            status_panel_dirty |= record_status_panel_events(
                shared_owned.as_ref(),
                channel_id,
                super::super::super::placeholder_live_events::status_events_from_tool_use_with_id(
                    &name,
                    &input,
                    tool_use_id.as_deref(),
                ),
            );
            // #1113 implicit-terminate: a new ToolUse arriving
            // before the prior ToolResult means the previous
            // tool is orphaned (parser miss, parallel-tool
            // collapse, or genuine hang upstream). Promote the
            // stale ⚙ marker to its terminal ⚠ form before
            // stashing in prev_tool_status so the placeholder
            // never claims a tool is still running once the
            // agent has moved on.
            let prev_for_preserve = current_tool_line
                .as_deref()
                .map(super::super::super::formatting::finalize_in_progress_tool_status);
            super::super::super::formatting::preserve_previous_tool_status(
                &mut prev_tool_status,
                prev_for_preserve.as_deref(),
                Some(display.as_str()),
            );
            current_tool_line = Some(display.clone());
            last_tool_name = Some(name.clone());
            last_tool_summary = Some(display_summary.clone());
            // #1255 live-turn long-running tool detection.
            //
            // Classifier returns `Some` for Monitor and for
            // Bash/Task/Agent calls with explicit
            // `run_in_background=true`.  Everything else
            // streams its result inline and never touches the
            // placeholder card.
            // codex round-11 P2 on PR #1308: restart commands
            // already own a planned ♻️ handoff message via the
            // `is_dcserver_restart_command(&input)` branch
            // below; opening a long-running placeholder on
            // the same message_id would let a later
            // Done/Result write a generic background card
            // over the planned restart notice. Skip setup
            // for those.
            let long_running_tool = if !is_dcserver_restart_command(&input) {
                super::super::super::formatting::classify_long_running_tool(&name, &input)
            } else {
                None
            };
            if matches!(
                long_running_tool,
                Some((
                    _,
                    super::super::super::formatting::LongRunningCloseTrigger::BackgroundDispatch,
                    _
                ))
            ) {
                if let (Some(pg_pool), Some(parent_session_key)) =
                    (shared_owned.pg_pool.as_ref(), adk_session_key.as_deref())
                {
                    let spawn = BackgroundChildSpawn {
                        parent_session_key: parent_session_key.to_string(),
                        provider: Some(provider.as_str().to_string()),
                        tool_name: name.clone(),
                        tool_input: input.clone(),
                    };
                    match insert_background_child_pg(pg_pool, &spawn).await {
                        Ok(Some(child_session_id)) => {
                            active_background_child_session_ids
                                .push(child_session_id);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ Failed to insert background child session for {}: {}",
                                parent_session_key,
                                error
                            );
                        }
                    }
                }
            }
            if should_open_long_running_placeholder_controller(
                shared_owned.ui.status_panel_v2_enabled,
            ) && long_running_placeholder_active.is_none()
                && pending_long_running_open_after_state_save.is_none()
                && pending_long_running_retarget_after_state_save.is_none()
            {
                if let Some((reason, close_trigger, reason_detail)) =
                    long_running_tool
                {
                    let started_at_unix = chrono::Utc::now().timestamp();
                    let key =
                        super::super::super::placeholder_controller::PlaceholderKey {
                            provider: provider.clone(),
                            channel_id,
                            message_id: current_msg_id,
                        };
                    let input_payload =
                        super::super::super::placeholder_controller::PlaceholderActiveInput {
                            reason,
                            started_at_unix,
                            tool_summary: Some(name.clone()),
                            command_summary: Some(display_summary.clone()),
                            reason_detail,
                            context_line: last_assistant_text_line.clone(),
                            request_line: first_request_line(&user_text_owned),
                            progress_line: child_progress_line(
                                shared_owned.pg_pool.as_ref(),
                                adk_session_key.as_deref(),
                            )
                            .await,
                        };
                    pending_long_running_open_after_state_save =
                        Some((key, input_payload, close_trigger, false));
                    inflight_state.long_running_placeholder_active = true;
                    state_dirty = true;
                }
            }
            push_transcript_event(
                &mut transcript_events,
                SessionTranscriptEvent {
                    kind: SessionTranscriptEventKind::ToolUse,
                    tool_name: Some(name.clone()),
                    summary: last_tool_summary.clone(),
                    content: input.clone(),
                    status: Some("running".to_string()),
                    is_error: false,
                },
            );
            let restart_bridge_authorized = if !restart_followup_pending
                && is_dcserver_restart_command(&input)
            {
                let intended_authority =
                    crate::services::discord::inflight::StreamRelayAuthority::from_state(
                        inflight_state,
                    );
                let outcome =
                    crate::services::discord::inflight::save_stream_tick_state_if_bridge_authority(
                        persisted_inflight_baseline,
                        inflight_state,
                        stream_tick_expected_identity,
                        expected_current_message.0,
                        expected_current_message.1,
                        "turn_bridge::stream_loop::tool_restart_visible_fence",
                    );
                if matches!(
                    outcome,
                    crate::services::discord::inflight::GuardedSaveOutcome::Saved
                ) {
                    *expected_current_message = (
                        inflight_state.current_msg_id,
                        inflight_state.current_msg_len,
                    );
                }
                let authority = visible_mutation_authority_after_guarded_save(
                    outcome,
                    inflight_state,
                    intended_authority,
                );
                restart_visible_authority = Some(authority);
                authority == VisibleMutationAuthority::Authorized
            } else {
                false
            };
            if restart_bridge_authorized {
                let mut report = RestartCompletionReport::new(
                    provider.clone(),
                    channel_id.get(),
                    "pending",
                    format!(
                        "dcserver restart requested by `{}`; 새 프로세스가 후속 보고를 이어받을 예정입니다.",
                        request_owner_name
                    ),
                );
                report.current_msg_id =
                    optional_durable_current_msg_id_from_detached(current_msg_id);
                report.channel_name = adk_session_name.clone();
                if save_restart_report(&report).is_ok() {
                    restart_followup_pending = true;
                    inflight_state.set_restart_mode(
                        crate::services::discord::InflightRestartMode::DrainRestart,
                    );
                    let handoff_text = "♻️ dcserver 재시작 중...\n\n재시작 후 현재 turn은 자동 새 턴으로 이어가지 않고, 상태만 다시 확인합니다.";
                    if edit_bound_current_message(
                        gateway.as_ref(), channel_id, current_msg_id, inflight_state, handoff_text,
                    ).await {
                        last_edit_text = handoff_text.to_string();
                    }
                    state_dirty = true;
                }
            }
            if !full_response.is_empty() {
                // #3608: paragraph separator via the shared
                // composition primitive (matched pair with
                // `append_streamed_text_chunk`). inflight_state
                // / state_dirty stay inline (hot-file #3016:
                // only full_response composition is extracted).
                chunk_compose::append_tool_boundary_separator(&mut full_response);
                inflight_state.full_response = full_response.clone();
                state_dirty = true;
            }
        }
        StreamToolArmMessage::ToolResult {
            content,
            is_error,
            tool_use_id,
        } => 'tool_result: {
            // A queued ToolResult can arrive after a watcher/standby handoff or
            // after another bridge advanced the Discord message epoch. Fence
            // the terminal PATCH before any ToolResult side effect so an I/O
            // failure can retain and replay this exact frame without duplicate
            // transcript/status mutations.
            let terminal_transition = long_running_placeholder_active.as_ref().and_then(
                |(key, _, close_trigger, ack_consumed)| {
                    let monitor_like = matches!(
                        close_trigger,
                        super::super::super::formatting::LongRunningCloseTrigger::MonitorLike
                    );
                    let is_dispatch_ack = !monitor_like && !ack_consumed;
                    let pending_retarget_matches_key =
                        pending_long_running_retarget_after_state_save
                            .as_ref()
                            .is_some_and(|(pending_key, _, _, _, _)| pending_key == key);
                    (monitor_like || (is_dispatch_ack && is_error))
                        .then_some(())
                        .filter(|_| !pending_retarget_matches_key)
                        .map(|()| {
                            let target = if is_error {
                                super::super::super::placeholder_controller::PlaceholderLifecycle::Aborted
                            } else {
                                super::super::super::placeholder_controller::PlaceholderLifecycle::Completed
                            };
                            (key.clone(), target)
                        })
                },
            );
            let mut prefenced_terminal_transition = None;
            let mut terminal_transition_suppressed = false;
            if let Some((key, target)) = terminal_transition {
                let intended_authority =
                    crate::services::discord::inflight::StreamRelayAuthority::from_state(
                        inflight_state,
                    );
                let outcome =
                    crate::services::discord::inflight::save_stream_tick_state_if_bridge_authority(
                        persisted_inflight_baseline,
                        inflight_state,
                        stream_tick_expected_identity,
                        expected_current_message.0,
                        expected_current_message.1,
                        "turn_bridge::stream_loop::terminal_tool_result_visible_fence",
                    );
                if outcome == crate::services::discord::inflight::GuardedSaveOutcome::Saved {
                    *expected_current_message = (
                        inflight_state.current_msg_id,
                        inflight_state.current_msg_len,
                    );
                }
                let authority = visible_mutation_authority_after_guarded_save(
                    outcome,
                    inflight_state,
                    intended_authority,
                );
                match terminal_tool_result_transition_permission(authority) {
                    Ok(true) => {
                        prefenced_terminal_transition = Some(
                            shared_owned
                                .ui
                                .placeholder_controller
                                .transition(gateway.as_ref(), key, target)
                                .await,
                        );
                    }
                    Ok(false) => terminal_transition_suppressed = true,
                    Err(outcome) => {
                        tool_outcome = outcome;
                        break 'tool_result;
                    }
                }
            }
            // Resolve delayed results by id; preserve the FIFO fallback.
            let status_tool_name = match tool_use_id.as_deref() {
                Some(id) => pending_status_tool_results_by_id
                    .remove(id)
                    .or_else(|| pending_status_tool_results.pop_front()),
                None => pending_status_tool_results.pop_front(),
            };
            if inflight_state.source == crate::dispatch::Source::Voice {
                let label = if is_error {
                    "tool_result:error"
                } else {
                    "tool_result:ok"
                };
                shared_owned.voice_barge_in.publish_progress_for_playback(
                    channel_id,
                    voice_progress_playback_channel_id,
                    label,
                );
            }
            let projected = crate::services::tool_output_guard::project_for_relay(
                matched_or_last_tool(status_tool_name.as_deref(), last_tool_name.as_deref()),
                is_error,
                &content,
            );
            crate::services::tool_output_guard::observe_projection(
                matched_or_last_tool(status_tool_name.as_deref(), last_tool_name.as_deref()),
                is_error,
                &projected,
            );
            let content = projected.content.into_owned();
            if is_error {
                record_placeholder_live_event(
                    &shared_owned,
                    channel_id,
                    super::super::super::placeholder_live_events::RecentPlaceholderEvent::tool_error(
                        &content,
                    ),
                );
            }
            status_panel_dirty |= record_status_panel_events(
                shared_owned.as_ref(),
                channel_id,
                super::super::super::placeholder_live_events::status_events_from_tool_result_with_id(
                    status_tool_name.as_deref(),
                    is_error,
                    tool_use_id.as_deref(),
                ),
            );
            // #1255: a long-running tool's ToolResult can move
            // the background card terminal while keeping the
            // placeholder visible for the rest of the turn; the
            // controller remains idempotent. codex round-2 P1:
            // only Monitor-style ToolResults are real
            // completions. Background Bash/Task/Agent results
            // are job/task acks, so keep those open until
            // Done/cancel.
            if let Some((key, snapshot, close_trigger, ack_consumed)) =
                long_running_placeholder_active.take()
            {
                let monitor_like = matches!(
                    close_trigger,
                    super::super::super::formatting::LongRunningCloseTrigger::MonitorLike
                );
                // codex round-6 P2: only the FIRST ToolResult
                // after the background ToolUse can be its
                // dispatch ack. Subsequent ToolResults belong
                // to other foreground tools and must not close
                // the still-running background card. Once the
                // ack is consumed, re-stash unconditionally
                // until `Done`/cancel.
                let is_dispatch_ack = !monitor_like && !ack_consumed;
                // codex round-5 P2: an `is_error` ToolResult on
                // the background dispatch ack (launch failure)
                // closes the card as Aborted; otherwise `Done`
                // would later mark it Completed and Discord
                // would report a failed background launch as ✅.
                if monitor_like || (is_dispatch_ack && is_error) {
                    if is_dispatch_ack && is_error {
                        close_next_tracked_background_child(
                            shared_owned.pg_pool.as_ref(),
                            &mut active_background_child_session_ids,
                            "aborted",
                            "failed background dispatch ack",
                        )
                        .await;
                    }
                    let pending_retarget_matches_key =
                        pending_long_running_retarget_after_state_save
                            .as_ref()
                            .is_some_and(|(pending_key, _, _, _, _)| {
                                *pending_key == key
                            });
                    if pending_retarget_matches_key {
                        let _ =
                            pending_long_running_retarget_after_state_save.take();
                        shared_owned.ui.placeholder_controller.detach(&key);
                        inflight_state.long_running_placeholder_active = false;
                        state_dirty = true;
                    } else if terminal_transition_suppressed {
                        // The exact same-turn durable row delegated visible
                        // relay ownership. Its owner will settle the card; this
                        // bridge keeps local retry state but must not PATCH it.
                        long_running_placeholder_active =
                            Some((key, snapshot, close_trigger, ack_consumed));
                    } else if let Some(outcome) = prefenced_terminal_transition.take() {
                        // codex round-10 P2: only clear flag on
                        // committed/already-terminal outcome.
                        use super::super::super::placeholder_controller::PlaceholderControllerOutcome::*;
                        if matches!(outcome, Edited | Coalesced | AlreadyTerminal) {
                            inflight_state.long_running_placeholder_active = false;
                            state_dirty = true;
                        } else {
                            // EditFailed — keep the placeholder
                            // active so the next event/sweeper
                            // can retry the terminal edit.
                            long_running_placeholder_active =
                                Some((key, snapshot, close_trigger, ack_consumed));
                        }
                    } else {
                        // Every visible terminal transition must pass the
                        // strict lock-held fence above. Fail closed if a future
                        // control-flow edit violates that invariant.
                        tracing::error!(
                            channel_id = channel_id.get(),
                            "terminal ToolResult reached placeholder mutation without authority fence"
                        );
                        long_running_placeholder_active =
                            Some((key, snapshot, close_trigger, ack_consumed));
                    }
                } else {
                    // Successful background dispatch ack OR a
                    // later unrelated ToolResult — re-stash so
                    // `Done`/cancel can close the card. Mark
                    // the ack consumed so future is_error
                    // results from other tools don't abort us.
                    let active_key_for_pending_update = key.clone();
                    long_running_placeholder_active =
                        Some((key, snapshot, close_trigger, true));
                    if let Some((pending_key, _, _, pending_ack_consumed, _)) =
                        pending_long_running_retarget_after_state_save.as_mut()
                        && *pending_key == active_key_for_pending_update
                    {
                        *pending_ack_consumed = true;
                    }
                }
            } else if let Some((_, _, close_trigger, ack_consumed)) =
                pending_long_running_open_after_state_save.as_mut()
            {
                let monitor_like = matches!(
                    *close_trigger,
                    super::super::super::formatting::LongRunningCloseTrigger::MonitorLike
                );
                let is_dispatch_ack = !monitor_like && !*ack_consumed;
                if monitor_like || (is_dispatch_ack && is_error) {
                    pending_long_running_open_after_state_save = None;
                    inflight_state.long_running_placeholder_active = false;
                    state_dirty = true;
                } else {
                    *ack_consumed = true;
                }
            }
            // Reset the assistant-line summary so the next
            // long-running tool call captures its own context.
            last_assistant_text_line = None;
            if let Some(ref tn) = last_tool_name {
                let status = if is_error { "✗" } else { "✓" };
                let detail = last_tool_summary
                    .as_deref()
                    .filter(|s| !s.is_empty() && *s != "…")
                    .map(|s| format!("{} {}: {}", status, tn, s))
                    .unwrap_or_else(|| format!("{} {}", status, tn));
                super::super::super::formatting::preserve_previous_tool_status(
                    &mut prev_tool_status,
                    current_tool_line.as_deref(),
                    Some(detail.as_str()),
                );
                current_tool_line = Some(detail);
            }
            push_transcript_event(
                &mut transcript_events,
                SessionTranscriptEvent {
                    kind: if is_error {
                        SessionTranscriptEventKind::Error
                    } else {
                        SessionTranscriptEventKind::ToolResult
                    },
                    tool_name: last_tool_name.clone(),
                    summary: last_tool_summary.clone(),
                    content,
                    status: Some(
                        if is_error { "error" } else { "success" }.to_string(),
                    ),
                    is_error,
                },
            );
        }
        StreamToolArmMessage::TaskNotification {
            tool_use_id,
            summary,
            status,
            kind,
            ..
        } => {
            inflight_state.task_notification_kind = merge_task_notification_kind(
                inflight_state.task_notification_kind,
                kind,
            );
            state_dirty = true;
            record_placeholder_live_event(
                shared_owned.as_ref(),
                channel_id,
                super::super::super::placeholder_live_events::RecentPlaceholderEvent::task_notification(
                    kind.as_str(),
                    &status,
                    &summary,
                ),
            );
            status_panel_dirty |= record_status_panel_events(
                shared_owned.as_ref(),
                channel_id,
                super::super::super::placeholder_live_events::status_events_from_task_notification_with_tool_use_id(
                    kind.as_str(),
                    &status,
                    &summary,
                    tool_use_id.as_deref(),
                ),
            );
            if single_message_panel_footer_mode {
                let indicator =
                    super::super::super::single_message_panel::single_message_panel_spinner_frame(
                        spin_idx,
                    );
                spin_idx = spin_idx.wrapping_add(1);
                refresh_bridge_footer(
                    shared_owned.as_ref(),
                    channel_id,
                    footer_owner,
                    indicator,
                )
                .await;
            }
            if task_notification_closes_background_child(kind, &status) {
                let close_status = if matches!(
                    status.trim().to_ascii_lowercase().as_str(),
                    "aborted" | "cancelled" | "canceled" | "failed" | "error"
                ) {
                    "aborted"
                } else {
                    "completed"
                };
                close_next_tracked_background_child(
                    shared_owned.pg_pool.as_ref(),
                    &mut active_background_child_session_ids,
                    close_status,
                    "task notification",
                )
                .await;
                // #1670: `merge_task_notification_kind` is an
                // absorb operator (priority-max). Without an
                // explicit release on the terminal status the
                // outer `inflight_state.task_notification_kind`
                // sticks at Subagent/Background past the child
                // close, which then misroutes downstream
                // suppression decisions and persists into the
                // saved inflight when the watcher takes over.
                //
                // codex P2 followup: only release the closed
                // child's kind once ALL tracked children have
                // closed. If a lower-priority child is the
                // last tracked one while a higher-priority
                // active classification is absorbed, keep the
                // higher-priority kind instead of clearing it.
                if active_background_child_session_ids.is_empty() {
                    inflight_state.task_notification_kind =
                        release_task_notification_kind(
                            inflight_state.task_notification_kind,
                            kind,
                        );
                }
            }
            push_transcript_event(
                &mut transcript_events,
                SessionTranscriptEvent {
                    kind: SessionTranscriptEventKind::Task,
                    tool_name: None,
                    summary: Some(summary.clone()),
                    content: summary,
                    status: Some("info".to_string()),
                    is_error: false,
                },
            );
        }
    }

    *state.state_dirty = state_dirty;
    *state.current_tool_line = current_tool_line;
    *state.prev_tool_status = prev_tool_status;
    *state.last_tool_name = last_tool_name;
    *state.last_tool_summary = last_tool_summary;
    *state.any_tool_used = any_tool_used;
    *state.has_post_tool_text = has_post_tool_text;
    *state.last_assistant_text_line = last_assistant_text_line;
    *state.spin_idx = spin_idx;
    *state.transcript_events = transcript_events;
    *state.pending_status_tool_results = pending_status_tool_results;
    *state.pending_status_tool_results_by_id = pending_status_tool_results_by_id;
    *state.long_running_placeholder_active = long_running_placeholder_active;
    *state.active_background_child_session_ids = active_background_child_session_ids;
    *state.pending_long_running_open_after_state_save = pending_long_running_open_after_state_save;
    *state.pending_long_running_retarget_after_state_save =
        pending_long_running_retarget_after_state_save;
    *state.restart_followup_pending = restart_followup_pending;
    *state.last_edit_text = last_edit_text;
    *state.full_response = full_response;
    *state.status_panel_dirty = status_panel_dirty;

    if tool_outcome == StreamToolArmOutcome::Continue {
        stream_tool_outcome_after_restart_authority(restart_visible_authority)
    } else {
        tool_outcome
    }
}

#[cfg(test)]
mod authority_tests {
    use super::*;

    fn bridge_state(channel_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-4259-r8".to_string()),
            343_742_347_365_974_026,
            77_010,
            18,
            "queued restart".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-codex-r8-restart".to_string()),
            Some("/tmp/AgentDesk-codex-r8-restart.jsonl".to_string()),
            None,
            512,
        )
    }

    #[test]
    fn queued_restart_foreign_authority_propagates_loss_while_self_delegation_continues() {
        let bridge = bridge_state(42_593_120);
        let intended =
            crate::services::discord::inflight::StreamRelayAuthority::from_state(&bridge);
        let mut foreign = bridge.clone();
        foreign.set_watcher_owner_channel_id(foreign.channel_id + 1);
        foreign.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
        let foreign_authority = visible_mutation_authority_after_guarded_save(
            crate::services::discord::inflight::GuardedSaveOutcome::IdentityMismatch,
            &foreign,
            intended,
        );
        assert_eq!(foreign_authority, VisibleMutationAuthority::AuthorityLost);
        assert_eq!(
            stream_tool_outcome_after_restart_authority(Some(foreign_authority)),
            StreamToolArmOutcome::AuthorityLost,
        );

        let delegated =
            crate::services::discord::inflight::StreamRelayAuthority::from_state(&foreign);
        let self_delegated = visible_mutation_authority_after_guarded_save(
            crate::services::discord::inflight::GuardedSaveOutcome::Saved,
            &foreign,
            delegated,
        );
        assert_eq!(self_delegated, VisibleMutationAuthority::Suppressed);
        assert_eq!(
            stream_tool_outcome_after_restart_authority(Some(self_delegated)),
            StreamToolArmOutcome::Continue,
        );
    }

    #[test]
    fn terminal_tool_result_fence_maps_handoff_loss_and_io_retry_fail_closed() {
        assert_eq!(
            terminal_tool_result_transition_permission(VisibleMutationAuthority::Authorized),
            Ok(true),
        );
        assert_eq!(
            terminal_tool_result_transition_permission(VisibleMutationAuthority::Suppressed),
            Ok(false),
        );
        assert_eq!(
            terminal_tool_result_transition_permission(VisibleMutationAuthority::AuthorityLost),
            Err(StreamToolArmOutcome::AuthorityLost),
        );
        assert_eq!(
            terminal_tool_result_transition_permission(VisibleMutationAuthority::Retry),
            Err(StreamToolArmOutcome::RetryExactFrame),
        );
    }

    #[test]
    fn transient_terminal_tool_result_fence_requeues_the_exact_frame_at_front() {
        let mut pending = std::collections::VecDeque::from([StreamMessage::Text {
            content: "later frame".to_string(),
        }]);
        let frame = StreamMessage::ToolResult {
            content: "exact terminal payload".to_string(),
            is_error: true,
            tool_use_id: Some("tool-4259-r9".to_string()),
        };
        assert!(retain_exact_stream_frame_on_tool_retry(
            &mut pending,
            frame,
            StreamToolArmOutcome::RetryExactFrame,
        ));
        let Some(StreamMessage::ToolResult {
            content,
            is_error,
            tool_use_id,
        }) = pending.pop_front()
        else {
            panic!("exact ToolResult must remain at queue front");
        };
        assert_eq!(content, "exact terminal payload");
        assert!(is_error);
        assert_eq!(tool_use_id.as_deref(), Some("tool-4259-r9"));
        assert!(matches!(
            pending.pop_front(),
            Some(StreamMessage::Text { content }) if content == "later frame"
        ));
    }
}
