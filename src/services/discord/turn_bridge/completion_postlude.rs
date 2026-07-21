//! #4230 S3 completion postlude + inflight epilogue for `turn_bridge::spawn_turn_bridge`.
//!
//! Moved from the final post-loop tail of `spawn_turn_bridge`: status-panel
//! completion, final ADK status, watcher resume, transcript/memory/analytics
//! persistence, metrics, restart-report cleanup, inflight preserve/clear,
//! mailbox recovery marker cleanup, and the final queued-turn drain.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::*;

mod channel_writeback;

pub(super) struct CompletionPostludeContext {
    pub(super) shared_owned: Arc<SharedData>,
    pub(super) gateway: Arc<dyn TurnGateway>,
    pub(super) channel_id: ChannelId,
    pub(super) provider: ProviderKind,
    pub(super) cancel_token: Arc<crate::services::provider::CancelToken>,
    pub(super) user_msg_id: Option<MessageId>,
    pub(super) turn_id: String,
    pub(super) request_owner_name: String,
    pub(super) final_session_status: &'static str,
    pub(super) status_panel_started_at: i64,
    pub(super) has_queued_turns: bool,
    pub(super) defer_watcher_resume: bool,
    pub(super) can_chain_locally: bool,
    pub(super) single_message_panel_footer_mode: bool,
    pub(super) is_external_input_tui_direct: bool,
    pub(super) context_window_tokens: u64,
    pub(super) context_compact_percent: u64,
    pub(super) turn_start: std::time::Instant,
}

pub(super) struct CompletionPostludeState {
    pub(super) full_response: String,
    pub(super) user_text_owned: String,
    pub(super) role_binding: Option<RoleBinding>,
    pub(super) adk_session_key: Option<String>,
    pub(super) adk_session_name: Option<String>,
    pub(super) adk_session_info: Option<String>,
    pub(super) adk_cwd: Option<String>,
    pub(super) dispatch_id: Option<String>,
    pub(super) dispatch_kind: Option<String>,
    pub(super) new_session_id: Option<String>,
    pub(super) new_raw_provider_session_id: Option<String>,
    pub(super) status_panel_terminal_committed: bool,
    pub(super) bridge_should_emit_completion: bool,
    pub(super) current_msg_id: MessageId,
    pub(super) status_panel_msg_id: Option<MessageId>,
    pub(super) last_status_panel_text: String,
    pub(super) completion_footer_terminal_text: Option<String>,
    pub(super) spin_idx: usize,
    pub(super) status_panel_generation: u64,
    pub(super) preserve_inflight_for_cleanup_retry: bool,
    pub(super) tmux_last_offset: Option<u64>,
    pub(super) watcher_owner_channel_id: ChannelId,
    pub(super) bridge_relay_delegated_to_watcher: bool,
    pub(super) is_prompt_too_long: bool,
    pub(super) resume_failure_detected: bool,
    pub(super) recovery_retry: bool,
    pub(super) rx_disconnected: bool,
    pub(super) tmux_handed_off: bool,
    pub(super) bridge_output_owner: Option<BridgeOutputOwner>,
    pub(super) terminal_delivery_committed: bool,
    pub(super) terminal_session_reset_required: bool,
    pub(super) transcript_events: Vec<SessionTranscriptEvent>,
    pub(super) accumulated_input_tokens: u64,
    pub(super) accumulated_cache_create_tokens: u64,
    pub(super) accumulated_cache_read_tokens: u64,
    pub(super) accumulated_output_tokens: u64,
    pub(super) accumulated_memory_input_tokens: u64,
    pub(super) accumulated_memory_output_tokens: u64,
    pub(super) transport_error: bool,
    pub(super) api_friction_reports: Vec<crate::services::api_friction::ApiFrictionReport>,
    pub(super) cancelled: bool,
    pub(super) restart_followup_pending: bool,
    pub(super) bridge_skip_holder_owns_inflight: bool,
    pub(super) inflight_guard: InflightCleanupGuard,
    pub(super) inflight_state: InflightTurnState,
}

pub(super) async fn run_completion_postlude(
    ctx: CompletionPostludeContext,
    state: CompletionPostludeState,
) {
    let shared_owned = ctx.shared_owned;
    let gateway = ctx.gateway;
    let channel_id = ctx.channel_id;
    let provider = ctx.provider;
    let cancel_token = ctx.cancel_token;
    let user_msg_id = ctx.user_msg_id;
    let turn_id = ctx.turn_id;
    let request_owner_name = ctx.request_owner_name;
    let final_session_status = ctx.final_session_status;
    let status_panel_started_at = ctx.status_panel_started_at;
    let has_queued_turns = ctx.has_queued_turns;
    let defer_watcher_resume = ctx.defer_watcher_resume;
    let can_chain_locally = ctx.can_chain_locally;
    let single_message_panel_footer_mode = ctx.single_message_panel_footer_mode;
    let is_external_input_tui_direct = ctx.is_external_input_tui_direct;
    let context_window_tokens = ctx.context_window_tokens;
    let context_compact_percent = ctx.context_compact_percent;
    let turn_start = ctx.turn_start;

    let full_response = state.full_response;
    let user_text_owned = state.user_text_owned;
    let role_binding = state.role_binding;
    let adk_session_key = state.adk_session_key;
    let adk_session_name = state.adk_session_name;
    let adk_session_info = state.adk_session_info;
    let adk_cwd = state.adk_cwd;
    let dispatch_id = state.dispatch_id;
    let dispatch_kind = state.dispatch_kind;
    let new_session_id = state.new_session_id;
    let new_raw_provider_session_id = state.new_raw_provider_session_id;
    let status_panel_terminal_committed = state.status_panel_terminal_committed;
    let bridge_should_emit_completion = state.bridge_should_emit_completion;
    let current_msg_id = state.current_msg_id;
    let status_panel_msg_id = state.status_panel_msg_id;
    let mut last_status_panel_text = state.last_status_panel_text;
    let completion_footer_terminal_text = state.completion_footer_terminal_text;
    let spin_idx = state.spin_idx;
    let status_panel_generation = state.status_panel_generation;
    let preserve_inflight_for_cleanup_retry = state.preserve_inflight_for_cleanup_retry;
    let tmux_last_offset = state.tmux_last_offset;
    let watcher_owner_channel_id = state.watcher_owner_channel_id;
    let bridge_relay_delegated_to_watcher = state.bridge_relay_delegated_to_watcher;
    let is_prompt_too_long = state.is_prompt_too_long;
    let resume_failure_detected = state.resume_failure_detected;
    let recovery_retry = state.recovery_retry;
    let rx_disconnected = state.rx_disconnected;
    let tmux_handed_off = state.tmux_handed_off;
    let bridge_output_owner = state.bridge_output_owner;
    let terminal_delivery_committed = state.terminal_delivery_committed;
    let terminal_session_reset_required = state.terminal_session_reset_required;
    let mut transcript_events = state.transcript_events;
    let accumulated_input_tokens = state.accumulated_input_tokens;
    let accumulated_cache_create_tokens = state.accumulated_cache_create_tokens;
    let accumulated_cache_read_tokens = state.accumulated_cache_read_tokens;
    let accumulated_output_tokens = state.accumulated_output_tokens;
    let mut accumulated_memory_input_tokens = state.accumulated_memory_input_tokens;
    let mut accumulated_memory_output_tokens = state.accumulated_memory_output_tokens;
    let transport_error = state.transport_error;
    let api_friction_reports = state.api_friction_reports;
    let cancelled = state.cancelled;
    let restart_followup_pending = state.restart_followup_pending;
    let bridge_skip_holder_owns_inflight = state.bridge_skip_holder_owns_inflight;
    let mut inflight_guard = state.inflight_guard;
    let inflight_state = state.inflight_state;

    let mut status_panel_completion_committed = true;
    if status_panel_terminal_committed
        && bridge_should_emit_completion
        && (single_message_panel_footer_mode
            || bridge_should_complete_separate_status_panel(
                shared_owned.ui.status_panel_v2_enabled,
            ))
    {
        // #2849: before rendering the completed panel, backfill exact final
        // context usage when the live StatusUpdates never carried it (e.g.
        // silent/background turns). resolve_exact_completion_usage prefers
        // the live accumulated snapshot, else re-parses the output JSONL the
        // same way persisted analytics does, and returns None when no exact
        // usage exists — so we never fabricate or reuse stale numbers.
        // set_context_panel_usage is a no-op when the live path already set
        // the same values, and is gated to context_window_tokens != 0.
        if shared_owned.ui.status_panel_v2_enabled {
            let context_provider_session_id = new_raw_provider_session_id
                .as_deref()
                .or(new_session_id.as_deref())
                .or(inflight_state.session_id.as_deref());
            let accumulated_usage = TurnTokenUsage {
                input_tokens: accumulated_input_tokens,
                cache_create_tokens: accumulated_cache_create_tokens,
                cache_read_tokens: accumulated_cache_read_tokens,
                output_tokens: accumulated_output_tokens,
            };
            if let Some(usage) = resolve_exact_completion_usage(
                &inflight_state,
                context_provider_session_id,
                accumulated_usage,
            ) {
                shared_owned
                    .ui
                    .placeholder_live_events
                    .set_context_panel_usage(
                        channel_id,
                        context_provider_session_id,
                        usage.input_tokens,
                        usage.cache_create_tokens,
                        usage.cache_read_tokens,
                        context_window_tokens,
                        context_compact_percent,
                    );
            }
        }
        let indicator =
            super::super::single_message_panel::single_message_panel_spinner_frame(spin_idx);
        status_panel_completion_committed = complete_bridge_terminal_footer_or_status_panel(
            shared_owned.as_ref(),
            gateway.as_ref(),
            channel_id,
            current_msg_id,
            user_msg_id,
            status_panel_msg_id,
            &provider,
            status_panel_started_at,
            &mut last_status_panel_text,
            single_message_panel_footer_mode,
            is_external_input_tui_direct, // #3959: suppress mirror chrome footer
            completion_footer_terminal_text.as_deref(),
            indicator,
            status_panel_generation, // #3805 P2: prove this turn's panel epoch
            inflight_state.tmux_session_name.as_deref(),
        )
        .await;
    }

    if status_panel_terminal_committed
        && status_panel_completion_committed
        && !preserve_inflight_for_cleanup_retry
    {
        post_adk_session_status(
            adk_session_key.as_deref(),
            adk_session_name.as_deref(),
            Some(provider.as_str()),
            final_session_status,
            &provider,
            adk_session_info.as_deref(),
            persisted_context_tokens(
                accumulated_input_tokens,
                accumulated_cache_create_tokens,
                accumulated_cache_read_tokens,
                accumulated_output_tokens,
            ),
            adk_cwd.as_deref(),
            dispatch_id.as_deref(),
            adk_session_name
                .as_deref()
                .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name),
            Some(channel_id),
            role_binding
                .as_ref()
                .map(|binding| binding.role_id.as_str()),
            shared_owned.api_port,
        )
        .await;
    } else if status_panel_terminal_committed && !status_panel_completion_committed {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            "turn bridge withheld final idle status because status-panel completion edit did not commit"
        );
    }

    if !bridge_relay_delegated_to_watcher
        && should_resume_watcher_after_turn(
            defer_watcher_resume,
            has_queued_turns,
            can_chain_locally,
        )
        && let Some(offset) = tmux_last_offset
        && let Some(watcher) = shared_owned.tmux_watchers.get(&watcher_owner_channel_id)
    {
        if let Ok(mut guard) = watcher.resume_offset.lock() {
            *guard = Some(offset);
        }
        // NOTE: turn_delivered is NOT cleared here — the watcher clears it
        // when it consumes resume_offset, ensuring the flag stays active
        // until the watcher actually starts reading from the new offset.
        watcher.paused.store(false, Ordering::Relaxed);
    }

    let should_record_final_turn = should_record_final_turn_transcript(
        is_prompt_too_long,
        resume_failure_detected,
        recovery_retry,
        rx_disconnected,
        tmux_handed_off,
        bridge_output_owner.is_some(),
        terminal_delivery_committed,
        preserve_inflight_for_cleanup_retry,
        &full_response,
    );

    // Update in-memory session under lock.
    let mut should_persist_transcript = false;
    let mut should_analyze_recall_feedback = false;
    let mut should_spawn_memory_capture = false;
    let mut reflect_request = None;
    let mut clear_provider_session = false;
    let capture_memory_settings = settings::memory_settings_for_binding(role_binding.as_ref());
    // #4658 F1 completion-side isolation: detect a scheduled-snapshot turn by its
    // ISOLATED session_key. A snapshot turn derives its `session_key` from the
    // reservation label (AC-2), so it differs from the channel's canonical
    // (channel-name-basis) key. Recompute the canonical key with the same
    // production helper (`build_adk_session_key(.., None)`) — which normal intake
    // and headless turns already use verbatim — and compare. When the turn's key
    // is present and differs, the turn does NOT own the channel's live session and
    // must produce ZERO channel-scoped side-effects a later LIVE turn can observe.
    // The full isolation invariant (the enumerated gated effects #1..#5 and the
    // F-2 mid-turn-rebind recompute limitation) lives in the `channel_writeback`
    // module doc — the single source of truth for what `!isolated_from_channel`
    // gates below. (#4634 bug class, completion side.)
    let channel_canonical_session_key = super::super::adk_session::build_adk_session_key(
        &shared_owned,
        channel_id,
        &provider,
        None,
    )
    .await;
    let isolated_from_channel = match adk_session_key.as_deref() {
        Some(turn_key) => channel_canonical_session_key.as_deref() != Some(turn_key),
        None => false,
    };
    let session_id_to_persist = {
        let mut data = shared_owned.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            if let Some(memory_plan) = plan_turn_end_memory(
                session,
                capture_memory_settings.backend,
                is_prompt_too_long,
                resume_failure_detected,
                terminal_session_reset_required,
                should_record_final_turn,
            ) {
                clear_provider_session = memory_plan.clear_provider_session;
                // #4658 F1: the writeback helper leaves the channel session
                // completely unchanged for a scheduled-snapshot turn.
                let writeback = channel_writeback::apply_channel_turn_writeback(
                    session,
                    isolated_from_channel,
                    &memory_plan,
                    &user_text_owned,
                    &full_response,
                    new_session_id.as_deref(),
                );
                should_persist_transcript = writeback.persist_transcript;
                // A snapshot turn must not reflect/capture the channel session
                // either — both mutate or summarize `data.sessions[channel_id]`.
                if !isolated_from_channel {
                    if let Some(reason) = memory_plan.session_end_reason {
                        reflect_request = take_memento_reflect_request(
                            session,
                            &capture_memory_settings,
                            &provider,
                            role_binding.as_ref(),
                            channel_id.get(),
                            reason,
                        );
                    }
                    should_spawn_memory_capture = memory_plan.spawn_capture;
                }
                should_analyze_recall_feedback = memory_plan.analyze_recall_feedback;
                writeback.session_id_to_persist
            } else {
                None
            }
        } else {
            None
        }
    };

    // Persist or clear provider session_id in DB so fresh-session transitions
    // survive dcserver restarts and idle cleanup.
    if clear_provider_session {
        if let Some(session_key) = adk_session_key.as_deref() {
            super::super::adk_session::clear_provider_session_id(
                session_key,
                shared_owned.api_port,
            )
            .await;
        }
    } else if let (Some(session_key), Some(persisted_sid)) =
        (adk_session_key.as_deref(), session_id_to_persist.as_deref())
    {
        super::super::adk_session::save_provider_session_id(
            session_key,
            persisted_sid,
            new_raw_provider_session_id.as_deref(),
            &provider,
            channel_id,
            shared_owned.api_port,
        )
        .await;
    }

    let memory_role_id = resolve_memory_role_id(role_binding.as_ref());
    let mut recall_feedback_analysis = if should_analyze_recall_feedback
        || transcript_contains_explicit_memento_tool_call(&transcript_events)
    {
        Some(analyze_recall_feedback_turn(&transcript_events))
    } else {
        None
    };
    if let Some(analysis) = recall_feedback_analysis.as_ref()
        && let Some(reminder) = channel_writeback::feedback_reminder_to_stash(
            isolated_from_channel,
            build_voluntary_feedback_reminder(analysis),
        )
    {
        // #4658 F1: gated on channel ownership above — a scheduled-snapshot turn
        // never reaches this stash (see feedback_reminder_to_stash).
        // #4307 PR-B: stash the reminder (provider-scoped key) so the NEXT turn's
        // intake takes it and injects it into the model context (turn N+1). The
        // transcript event below only records it in the session_transcripts DB —
        // the stash is the channel that reaches the next prompt. Borrow before
        // the move into `reminder_transcript_event`. A stash failure only loses
        // the next-turn nudge (the transcript record still lands), so warn+skip.
        if let Err(error) = super::recovery_text::store_voluntary_feedback_reminder(
            shared_owned.pg_pool.as_ref(),
            &provider,
            channel_id.get(),
            &reminder,
        ) {
            tracing::warn!(
                channel_id = channel_id.get(),
                turn_id = turn_id.as_str(),
                provider = provider.as_str(),
                error = %error,
                "failed to stash voluntary tool_feedback reminder for next-turn injection"
            );
        }
        push_transcript_event(&mut transcript_events, reminder_transcript_event(reminder));
        recall_feedback_analysis = Some(analyze_recall_feedback_turn(&transcript_events));
    }
    // #4196: if this turn ends with uncommitted changes in its worktree, stash a
    // WIP warning (provider-scoped key) so the NEXT turn's intake takes it and
    // injects it into the model context (turn N+1). Reuses the #3792 detector via
    // `turn_end_wip_warning_text` — no re-implementation of git status parsing.
    // Gated on channel ownership (mirrors the feedback stash) so a scheduled or
    // isolated snapshot turn never nudges the interactive session. A clean
    // worktree yields `None` here, so nothing is stashed and turn N+1 is
    // byte-for-byte unchanged. A stash failure only loses the next-turn nudge
    // (the channel-post backstop still fires), so warn+skip.
    if !isolated_from_channel
        && let Some(wip_warning) =
            super::super::turn_end_wip_warning::turn_end_wip_warning_text(Some(&inflight_state))
        && let Err(error) = super::recovery_text::store_turn_end_wip_warning(
            shared_owned.pg_pool.as_ref(),
            &provider,
            channel_id.get(),
            &wip_warning,
        )
    {
        tracing::warn!(
            channel_id = channel_id.get(),
            turn_id = turn_id.as_str(),
            provider = provider.as_str(),
            error = %error,
            "failed to stash turn-end WIP warning for next-turn injection"
        );
    }
    let model_token_usage = TurnTokenUsage {
        input_tokens: accumulated_input_tokens,
        cache_create_tokens: accumulated_cache_create_tokens,
        cache_read_tokens: accumulated_cache_read_tokens,
        output_tokens: accumulated_output_tokens,
    };
    let turn_outcome = if cancelled {
        "cancelled"
    } else if recovery_retry {
        "recovery_retry"
    } else if is_prompt_too_long {
        "prompt_too_long"
    } else if transport_error {
        "transport_error"
    } else if bridge_output_owner == Some(BridgeOutputOwner::WatcherRelay) {
        "watcher_relay"
    } else if bridge_output_owner == Some(BridgeOutputOwner::StandbyRelay) {
        "standby_relay"
    } else if rx_disconnected && tmux_handed_off && full_response.is_empty() {
        "tmux_handoff"
    } else if full_response.trim().is_empty() {
        "empty_response"
    } else {
        "completed"
    };
    crate::services::observability::emit_turn_finished_with_dispatch_kind(
        provider.as_str(),
        channel_id.get(),
        dispatch_id.as_deref(),
        adk_session_key.as_deref(),
        Some(turn_id.as_str()),
        turn_outcome,
        turn_duration_ms(turn_start),
        rx_disconnected && tmux_handed_off && full_response.is_empty(),
        dispatch_kind.as_deref(),
    );
    let turn_quality_event_type = if matches!(
        turn_outcome,
        "completed" | "tmux_handoff" | "watcher_relay" | "standby_relay"
    ) {
        "turn_complete"
    } else {
        "turn_error"
    };
    emit_turn_quality_event(
        &provider,
        channel_id,
        dispatch_id.as_deref(),
        adk_session_key.as_deref(),
        turn_id.as_str(),
        role_binding.as_ref(),
        turn_quality_event_type,
        serde_json::json!({
            "outcome": turn_outcome,
            "duration_ms": turn_duration_ms(turn_start),
            "cancelled": cancelled,
            "recovery_retry": recovery_retry,
            "transport_error": transport_error,
            "tmux_handoff": rx_disconnected && tmux_handed_off && full_response.is_empty(),
            "watcher_relay": bridge_relay_delegated_to_watcher,
            "standby_relay": bridge_output_owner == Some(BridgeOutputOwner::StandbyRelay),
        }),
    );

    if should_persist_transcript && shared_owned.pg_pool.is_some() {
        let channel_id_text = channel_id.get().to_string();
        if let Err(e) = crate::db::session_transcripts::persist_turn_db(
            shared_owned.pg_pool.as_ref(),
            crate::db::session_transcripts::PersistSessionTranscript {
                turn_id: turn_id.as_str(),
                session_key: adk_session_key.as_deref(),
                channel_id: Some(channel_id_text.as_str()),
                agent_id: role_binding
                    .as_ref()
                    .map(|binding| binding.role_id.as_str()),
                provider: Some(provider.as_str()),
                dispatch_id: dispatch_id.as_deref(),
                user_message: &user_text_owned,
                assistant_message: &full_response,
                events: &transcript_events,
                duration_ms: Some(turn_duration_ms(turn_start)),
            },
        )
        .await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ failed to persist session transcript: {e}");
        }
    }

    // #4307 PR-A: persist per-turn memento recall/feedback stats so the
    // /api/stats reader (load_memento_feedback_counts) surfaces real
    // compliance/coverage counts. Restores the writer a1492c05 dropped when it
    // removed the SQLite twin without porting the PG path. auto_tool_feedback_count
    // is always 0 now that the dead auto-submit fallback is gone.
    if shared_owned.pg_pool.is_some()
        && let Some(analysis) = recall_feedback_analysis.as_ref()
        && analysis.recall_count > 0
    {
        let stat = crate::db::session_transcripts::MementoFeedbackTurnStat {
            turn_id: turn_id.clone(),
            stat_date: chrono::Local::now().format("%Y-%m-%d").to_string(),
            agent_id: memory_role_id.clone(),
            provider: provider.as_str().to_string(),
            recall_count: i64::try_from(analysis.recall_count).unwrap_or(i64::MAX),
            manual_tool_feedback_count: i64::try_from(analysis.manual_feedback_count)
                .unwrap_or(i64::MAX),
            manual_covered_recall_count: i64::try_from(analysis.manual_covered_recall_count)
                .unwrap_or(i64::MAX),
            auto_tool_feedback_count: 0,
            covered_recall_count: i64::try_from(analysis.manual_covered_recall_count)
                .unwrap_or(i64::MAX),
        };
        if let Err(error) = crate::db::session_transcripts::record_memento_feedback_turn_stats(
            shared_owned.pg_pool.as_ref(),
            &stat,
        )
        .await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ failed to persist memento feedback stats: {error}");
        }
    }

    // #4658 F1: `record_api_friction_reports` calls `backend.remember(..)`,
    // landing in the agent's memento memory a live turn's recall can surface, so
    // a scheduled-snapshot turn must skip it (isolation invariant, effect #5).
    if !isolated_from_channel && shared_owned.pg_pool.is_some() && !api_friction_reports.is_empty()
    {
        match crate::services::api_friction::record_api_friction_reports(
            shared_owned.pg_pool.as_ref(),
            &capture_memory_settings,
            crate::services::api_friction::ApiFrictionRecordContext {
                channel_id: channel_id.get(),
                session_key: adk_session_key.as_deref(),
                dispatch_id: dispatch_id.as_deref(),
                provider: provider.as_str(),
            },
            &api_friction_reports,
        )
        .await
        {
            Ok(summary) => {
                accumulated_memory_input_tokens = accumulated_memory_input_tokens
                    .saturating_add(summary.token_usage.input_tokens);
                accumulated_memory_output_tokens = accumulated_memory_output_tokens
                    .saturating_add(summary.token_usage.output_tokens);
                for error in summary.memory_errors {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!("  [{ts}] ⚠ failed to store API friction memory: {error}");
                }
            }
            Err(error) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!("  [{ts}] ⚠ failed to record API friction: {error}");
            }
        }
    }

    // No user message (user_msg_id == 0) → no analytics row to key
    // (`discord:<channel>:0` is the bogus form); skip the persist.
    if shared_owned.pg_pool.is_some()
        && let Some(user_msg_id) = user_msg_id
    {
        persist_turn_analytics_row_with_handles(
            shared_owned.pg_pool.as_ref(),
            &provider,
            channel_id,
            user_msg_id,
            role_binding.as_ref(),
            dispatch_id.as_deref(),
            adk_session_key.as_deref(),
            new_session_id
                .as_deref()
                .or(session_id_to_persist.as_deref()),
            &inflight_state,
            model_token_usage,
            turn_duration_ms(turn_start),
        );
    }

    let mut background_memory_tasks = Vec::new();
    if let Some(reflect_request) = reflect_request {
        background_memory_tasks.push(BackgroundMemoryTask {
            kind: BackgroundMemoryTaskKind::Reflect,
            handle: spawn_memory_reflect_task(
                channel_id,
                capture_memory_settings.clone(),
                reflect_request,
            ),
        });
    }
    if should_spawn_memory_capture {
        let capture_request = CaptureRequest {
            provider: provider.clone(),
            role_id: memory_role_id,
            channel_id: channel_id.get(),
            session_id: resolve_memory_session_id(
                session_id_to_persist.as_deref(),
                channel_id.get(),
            ),
            dispatch_id: dispatch_id.clone(),
            user_text: user_text_owned.clone(),
            assistant_text: full_response.clone(),
        };
        background_memory_tasks.push(BackgroundMemoryTask {
            kind: BackgroundMemoryTaskKind::Capture,
            handle: spawn_memory_capture_task(channel_id, capture_memory_settings, capture_request),
        });
    }

    if !background_memory_tasks.is_empty() {
        observe_background_memory_tasks(
            channel_id,
            background_memory_tasks,
            &mut accumulated_memory_input_tokens,
            &mut accumulated_memory_output_tokens,
        )
        .await;
    }

    {
        let duration = shared_owned
            .turn_start_times
            .remove(&channel_id)
            .map(|(_, start)| start.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let memory_usage = TokenUsage {
            input_tokens: accumulated_memory_input_tokens,
            output_tokens: accumulated_memory_output_tokens,
        };
        let (memory_input_tokens, memory_output_tokens) =
            optional_metric_token_fields(memory_usage);
        let provider_name = {
            let settings = shared_owned.settings.read().await;
            settings.provider.as_str().to_string()
        };
        let total_input_tokens = total_model_input_tokens(
            accumulated_input_tokens,
            accumulated_cache_create_tokens,
            accumulated_cache_read_tokens,
        );
        super::super::metrics::record_turn(&super::super::metrics::TurnMetric {
            channel_id: channel_id.get(),
            provider: provider_name,
            timestamp: chrono::Local::now().to_rfc3339(),
            duration_secs: duration,
            model: None, // model info from StatusUpdate not yet accumulated in turn_bridge
            input_tokens: if total_input_tokens > 0 {
                Some(total_input_tokens)
            } else {
                None
            },
            output_tokens: if accumulated_output_tokens > 0 {
                Some(accumulated_output_tokens)
            } else {
                None
            },
            memory_input_tokens,
            memory_output_tokens,
        });
    }

    // Clear restart report BEFORE clearing inflight state (which removes
    // the cancel token) to prevent the flush loop from processing the
    // report in the gap between cancel token removal and report deletion.
    if restart_followup_pending {
        clear_restart_report(&provider, channel_id.get());
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ✓ Cleared restart report for channel {} (turn completed normally)",
            channel_id
        );
    }

    if cancelled && cancel_token.restart_mode().is_some() {
        use crate::services::discord::inflight::{
            GuardedSaveOutcome, patch_restart_full_response_if_identity_unchanged,
            save_inflight_state_if_identity_unchanged,
        };

        let guarded_outcome = save_inflight_state_if_identity_unchanged(
            &inflight_state,
            "turn_bridge::restart_mode_preserve@6330",
        );
        if matches!(guarded_outcome, GuardedSaveOutcome::IdentityMismatch) {
            let _ = patch_restart_full_response_if_identity_unchanged(
                &inflight_state,
                "turn_bridge::restart_full_response_patch@6330",
            );
        }
        inflight_guard.provider.take();
    } else if preserve_inflight_for_cleanup_retry || bridge_output_owner.is_some() {
        // #3041 P1-2 (codex P1-2 R3): on a delivery-lease `Skip` the live
        // HOLDER (the watcher) owns this turn's inflight lifecycle and CLEARS
        // the row on its own success. A blind `save_inflight_state` here would
        // RACE the holder's clear: if the clear wins first and our save runs
        // second, we resurrect a STALE inflight row for an already-delivered
        // turn (recovery then sees it delivered and returns WITHOUT clearing
        // → permanent stale leak). So on the skip-holder path we use an
        // IDENTITY-GUARDED save that only rewrites when the on-disk row STILL
        // matches this turn — a holder-cleared row (Missing) or a newer turn
        // (IdentityMismatch) no-ops. When the holder FAILED (did not clear),
        // the row is still present + matching, so the refresh lands and retry
        // survives. The delegated-owner path uses the same guard so a watcher
        // clear cannot race with a bridge re-save and resurrect a delivered row.
        let identity_guarded_skip_save =
            bridge_epilogue_skip_save_is_identity_guarded(bridge_skip_holder_owns_inflight);
        if identity_guarded_skip_save {
            let guarded_outcome =
                crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
                    &inflight_state,
                    "turn_bridge::skip_holder_preserve@6355",
                );
            crate::services::observability::emit_inflight_lifecycle_event(
                provider.as_str(),
                channel_id.get(),
                dispatch_id.as_deref(),
                adk_session_key.as_deref(),
                Some(turn_id.as_str()),
                "skip_identity_guarded_save",
                serde_json::json!({
                    "guarded_save_outcome": format!("{guarded_outcome:?}"),
                    "user_msg_id": inflight_state.user_msg_id,
                    "turn_start_offset": inflight_state.turn_start_offset,
                }),
            );
        } else {
            let _ = crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
                &inflight_state,
                "turn_bridge::delegated_owner_preserve@6374",
            );
        }
        inflight_guard.provider.take();
        if let Some(owner) = bridge_output_owner {
            let lifecycle_event = match owner {
                BridgeOutputOwner::WatcherRelay => "delegated_to_watcher",
                BridgeOutputOwner::StandbyRelay => "delegated_to_standby_relay",
            };
            crate::services::observability::emit_inflight_lifecycle_event(
                provider.as_str(),
                channel_id.get(),
                dispatch_id.as_deref(),
                adk_session_key.as_deref(),
                Some(turn_id.as_str()),
                lifecycle_event,
                serde_json::json!({
                    "preserve_inflight_for_cleanup_retry": preserve_inflight_for_cleanup_retry,
                    "full_response_len": inflight_state.full_response.len(),
                    "response_sent_offset": inflight_state.response_sent_offset,
                    "watcher_owns_live_relay": inflight_state.watcher_owns_live_relay,
                    "standby_relay_owns_output": owner == BridgeOutputOwner::StandbyRelay,
                    // #1671 — record the dispatch outcome and notification
                    // kind on every bridge-side lifecycle event so
                    // same-class incidents (orphan inflight after the
                    // bridge handed off) can be triaged from log payloads
                    // alone instead of requiring a watcher-state hit.
                    "dispatch_ok": false,
                    "task_notification_kind": inflight_state
                        .task_notification_kind
                        .map(|kind| kind.as_str()),
                }),
            );
        }
    } else {
        // #3041 P1-2 (codex P1-c): the clear branch is reached IFF the pure
        // epilogue seam agrees inflight must be cleared — i.e. NOT preserving
        // for retry (a B2 Skip sets `preserve_inflight_for_cleanup_retry`) and
        // NOT delegating output. This keeps the production fork and the
        // unit-tested `bridge_epilogue_clears_inflight` seam in lockstep so a
        // Skip can never silently reach this destroy-inflight path.
        debug_assert!(
            bridge_epilogue_clears_inflight(
                preserve_inflight_for_cleanup_retry,
                bridge_output_owner.is_some(),
                cancelled && cancel_token.restart_mode().is_some(),
            ),
            "inflight clear must only run when neither preserving for retry nor delegating output"
        );
        // #2838 (relay-stability P0-1): detect the missing-answer vector
        // (root causes #1b / #4). We are about to clear inflight (not
        // preserving, no delegated owner). If a non-empty full_response was
        // never committed to Discord on a NORMAL turn — excluding the
        // intentional cancelled / prompt-too-long paths, which deliver a
        // [Stopped]/notice via status_panel_terminal_committed rather than
        // terminal_delivery_committed — the generated answer is being
        // destroyed with no retry. Each increment is a leaked answer.
        if !cancelled
            && !is_prompt_too_long
            && !terminal_delivery_committed
            && !inflight_state.full_response.trim().is_empty()
        {
            crate::services::observability::metrics::record_relay_uncommitted_inflight_cleared(
                channel_id.get(),
                provider.as_str(),
            );
            crate::services::observability::emit_relay_delivery(
                provider.as_str(),
                channel_id.get(),
                dispatch_id.as_deref(),
                adk_session_key.as_deref(),
                Some(turn_id.as_str()),
                Some(current_msg_id.get()),
                "turn_bridge",
                "skip",
                None,
                None,
                false,
                Some("inflight cleared with undelivered full_response"),
            );
        }
        // #3161 (codex P1): identity-guard the epilogue inflight-row
        // removal. The status-panel completion EDIT above is alias-skipped
        // (`panel_edit_aliases_newer_turn`) when a NEWER turn now owns this
        // turn's captured panel, but THIS removal was unconditional — so an
        // OLD turn that correctly skipped its edit would still delete the
        // on-disk inflight row, which by then belongs to the NEWER owner.
        // That wipes the newer turn's inflight and leaves its status panel
        // permanently non-complete. We now route a real (non-zero) this-turn
        // identity through the guarded clear, which removes the row only when
        // the on-disk `user_msg_id` still matches THIS turn (atomically under
        // the inflight sidecar lock — no read-then-clear TOCTOU); a newer
        // owner yields `UserMsgMismatch` and the row is preserved.
        //
        // The id==0 case (TUI-direct / external-input bridge turns that
        // cannot be identity-guarded) keeps the unconditional clear — the
        // same over-suppression carve-out the alias predicate uses, so those
        // turns still clean up their own row. `bridge_epilogue_identity_guards_inflight_clear`
        // is the pure seam shared with the unit test so the production fork
        // and the test stay in lockstep.
        let this_turn_user_msg_id = user_msg_id.map(|id| id.get()).unwrap_or(0);
        if bridge_epilogue_identity_guards_inflight_clear(this_turn_user_msg_id) {
            use super::super::inflight::GuardedClearOutcome;
            match super::super::inflight::clear_inflight_state_if_matches(
                &provider,
                channel_id.get(),
                this_turn_user_msg_id,
            ) {
                GuardedClearOutcome::Cleared | GuardedClearOutcome::Missing => {}
                GuardedClearOutcome::UserMsgMismatch => {
                    tracing::debug!(
                        "[turn_bridge] preserving inflight row in channel {}: a newer turn now owns it (this turn user_msg_id {})",
                        channel_id,
                        this_turn_user_msg_id
                    );
                }
                GuardedClearOutcome::PlannedRestartSkipped
                | GuardedClearOutcome::RebindOriginSkipped => {}
                GuardedClearOutcome::IoError => {
                    tracing::warn!(
                        provider = %provider.as_str(),
                        channel_id = channel_id.get(),
                        this_turn_user_msg_id,
                        "turn bridge epilogue inflight guarded-clear hit IoError; sweeper will retry"
                    );
                }
            }
        } else {
            // #3161 (codex P1): a zero-id turn (recovery / external-input /
            // cluster-relay synthesized) cannot be identity-guarded against a
            // non-zero id, but it MUST NOT blind-clear a row a NEWER real
            // (non-zero) owner has since written — that wipes the newer
            // owner's inflight and leaves its status panel permanently
            // non-complete (the same bug for zero-id callers). The
            // zero-owned guarded clear removes the row ONLY when the on-disk
            // `user_msg_id` is itself 0 (this zero-id turn's own row), and
            // returns `UserMsgMismatch` when a newer non-zero owner is on
            // disk — so recovery cleanup still works while a newer owner is
            // preserved.
            use super::super::inflight::GuardedClearOutcome;
            match super::super::inflight::clear_inflight_state_if_matches_zero_owned(
                &provider,
                channel_id.get(),
            ) {
                GuardedClearOutcome::Cleared | GuardedClearOutcome::Missing => {}
                GuardedClearOutcome::UserMsgMismatch => {
                    tracing::debug!(
                        "[turn_bridge] preserving inflight row in channel {}: a newer non-zero turn now owns it (this turn is zero-id)",
                        channel_id
                    );
                }
                GuardedClearOutcome::PlannedRestartSkipped
                | GuardedClearOutcome::RebindOriginSkipped => {}
                GuardedClearOutcome::IoError => {
                    tracing::warn!(
                        provider = %provider.as_str(),
                        channel_id = channel_id.get(),
                        "turn bridge epilogue zero-owned inflight guarded-clear hit IoError; sweeper will retry"
                    );
                }
            }
        }
        // Defuse the guard — cleanup already done above.
        inflight_guard.provider.take();
        crate::services::observability::emit_inflight_lifecycle_event(
            provider.as_str(),
            channel_id.get(),
            dispatch_id.as_deref(),
            adk_session_key.as_deref(),
            Some(turn_id.as_str()),
            "cleared_by_bridge",
            serde_json::json!({
                "full_response_len": inflight_state.full_response.len(),
                "response_sent_offset": inflight_state.response_sent_offset,
                // #1671 — `dispatch_ok=true` here marks "bridge handled
                // the full lifecycle without delegation"; pair with the
                // notification kind so a stale `task_notification_kind`
                // pattern is searchable directly off the lifecycle event.
                "dispatch_ok": true,
                "task_notification_kind": inflight_state
                    .task_notification_kind
                    .map(|kind| kind.as_str()),
            }),
        );
    }
    super::super::mailbox_clear_recovery_marker(&shared_owned, channel_id).await;

    // Dispatch thread sessions now stay alive after finalization so the next
    // implementation/review/rework turn can warm-resume from the same tmux.
    // New dispatch arrivals validate the managed tmux session before reuse.

    // #3038: finalization epilogue (counter decrement + queued-turn drain)
    // extracted verbatim to `finalize_epilogue.rs`. This is the LAST block of
    // the async body, so every capture is threaded by value with its original
    // ownership; behavior-preserving (see the module doc for the seam-fix note).
    finalize_epilogue::finalize_and_drain_queued_turns(
        shared_owned,
        has_queued_turns,
        preserve_inflight_for_cleanup_retry,
        gateway,
        channel_id,
        provider,
        request_owner_name,
        tmux_last_offset,
        watcher_owner_channel_id,
    )
    .await;

    // completion_tx is sent automatically by CompletionGuard on drop
}
