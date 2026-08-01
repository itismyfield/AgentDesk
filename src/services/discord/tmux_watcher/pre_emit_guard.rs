use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PreEmitGuardOutcome {
    ContinueWatcherLoop,
    Proceed,
}

pub(super) struct PreEmitGuardContext<'a> {
    pub(super) http: &'a Arc<serenity::Http>,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) channel_id: serenity::ChannelId,
    pub(super) watcher_provider: &'a ProviderKind,
    pub(super) tmux_session_name: &'a String,
    pub(super) output_path: &'a String,
    pub(super) paused: &'a Arc<AtomicBool>,
    pub(super) pause_epoch: &'a Arc<AtomicU64>,
    pub(super) turn_delivered: &'a Arc<AtomicBool>,
}

pub(super) struct PreEmitGuardLocals<'a> {
    pub(super) epoch_snapshot: u64,
    pub(super) monitor_auto_turn_deferred: bool,
    pub(super) placeholder_msg_id: Option<serenity::MessageId>,
    pub(super) turn_data_start_offset: u64,
    pub(super) current_offset: u64,
    pub(super) response_sent_offset: usize,
    pub(super) data_start_offset: u64,
    pub(super) stale_resume_detected: bool,
    pub(super) last_edit_text: &'a String,
}

pub(super) struct PreEmitGuardState<'a> {
    pub(super) monitor_auto_turn_claimed: &'a mut bool,
    pub(super) monitor_auto_turn_finished: &'a mut bool,
    pub(super) monitor_auto_turn_synthetic_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) monitor_auto_turn_ledger_generation: &'a mut Option<u64>,
    pub(super) all_data: &'a mut String,
    pub(super) all_data_start_offset: &'a mut u64,
    pub(super) all_data_fully_mirrored_to_session_relay: &'a mut bool,
    pub(super) all_data_session_bound_relay_ack: &'a mut Option<SessionBoundRelayAckTarget>,
    pub(super) all_data_first_forwarded_relay_sequence: &'a mut Option<u64>,
    pub(super) last_relayed_offset: &'a mut Option<u64>,
    pub(super) last_observed_generation_mtime_ns: &'a mut Option<i64>,
    pub(super) full_response: &'a mut String,
}

pub(super) async fn run_pre_emit_guard(
    context: &PreEmitGuardContext<'_>,
    locals: PreEmitGuardLocals<'_>,
    state: &mut PreEmitGuardState<'_>,
) -> PreEmitGuardOutcome {
    let http = context.http;
    let shared = context.shared;
    let channel_id = context.channel_id;
    let watcher_provider = context.watcher_provider;
    let tmux_session_name = context.tmux_session_name;
    let output_path = context.output_path;
    let paused = context.paused;
    let pause_epoch = context.pause_epoch;
    let turn_delivered = context.turn_delivered;
    let PreEmitGuardLocals {
        epoch_snapshot,
        monitor_auto_turn_deferred,
        placeholder_msg_id,
        turn_data_start_offset,
        current_offset,
        response_sent_offset,
        data_start_offset,
        stale_resume_detected,
        last_edit_text,
    } = locals;

    // Final guard: re-check epoch and turn_delivered right before relay.
    // Closes the race window where a Discord turn starts between the epoch check
    // above (line 277) and this relay — the turn_bridge may have already delivered
    // the same response to its own placeholder.
    let paused_now = paused.load(Ordering::Relaxed);
    let epoch_changed_now = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
    let turn_delivered_now = turn_delivered.load(Ordering::Relaxed);
    let deferred_monitor_ready =
        *state.monitor_auto_turn_claimed && monitor_auto_turn_deferred && !paused_now;
    if should_suppress_relay_before_emit(
        paused_now,
        epoch_changed_now,
        turn_delivered_now,
        deferred_monitor_ready,
    ) {
        if let Some(msg_id) = placeholder_msg_id {
            let inflight_before_cleanup = crate::services::discord::inflight::load_inflight_state(
                watcher_provider,
                channel_id.get(),
            );
            let _ = delete_nonterminal_placeholder_unless_delivered(
                http,
                channel_id,
                shared,
                watcher_provider,
                tmux_session_name,
                msg_id,
                inflight_before_cleanup.as_ref(),
                Some((
                    turn_data_start_offset,
                    terminal_event_consumed_offset(current_offset, &*state.all_data),
                )),
                response_sent_offset,
                last_edit_text,
                "watcher_late_epoch_guard_cleanup",
            )
            .await;
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] 👁 Late epoch/delivered guard: suppressed duplicate relay for {}",
            tmux_session_name
        );
        finish_monitor_auto_turn_if_claimed(
            shared,
            watcher_provider,
            channel_id,
            &mut *state.monitor_auto_turn_claimed,
            &mut *state.monitor_auto_turn_finished,
            &mut *state.monitor_auto_turn_synthetic_msg_id,
            &mut *state.monitor_auto_turn_ledger_generation,
        )
        .await;
        discard_watcher_pending_buffer_after_suppressed_turn(
            &mut *state.all_data,
            &mut *state.all_data_start_offset,
            &mut *state.all_data_fully_mirrored_to_session_relay,
            &mut *state.all_data_session_bound_relay_ack,
            &mut *state.all_data_first_forwarded_relay_sequence,
            current_offset,
        );
        return PreEmitGuardOutcome::ContinueWatcherLoop;
    }

    if watcher_should_yield_to_active_bridge_turn(
        watcher_provider,
        channel_id,
        tmux_session_name,
        data_start_offset,
        current_offset,
    ) {
        let matched_reattach = matching_recent_watcher_reattach_offset(
            channel_id,
            tmux_session_name,
            data_start_offset,
        );
        let reattach_detail = matched_reattach.as_ref().map(|r| {
            format!(
                "{} range {}..{} matches reattach at {}",
                tmux_session_name, data_start_offset, current_offset, r.offset
            )
        });
        let ctx = PlaceholderSuppressContext {
            origin: PlaceholderSuppressOrigin::ActiveBridgeTurnGuard,
            provider: watcher_provider,
            placeholder_msg_id,
            response_sent_offset,
            last_edit_text: last_edit_text,
            inflight_state: None,
            has_active_turn: false,
            tmux_session_name: tmux_session_name,
            task_notification_kind: None,
            reattach_offset_match: matched_reattach.is_some(),
        };
        apply_placeholder_suppression(
            http,
            channel_id,
            shared,
            watcher_provider,
            tmux_session_name,
            placeholder_msg_id,
            ctx.origin,
            decide_placeholder_suppression(&ctx),
            reattach_detail.as_deref(),
        )
        .await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] 👁 Active bridge turn guard: suppressed duplicate relay for {} (range {}..{})",
            tmux_session_name,
            data_start_offset,
            current_offset
        );
        finish_monitor_auto_turn_if_claimed(
            shared,
            watcher_provider,
            channel_id,
            &mut *state.monitor_auto_turn_claimed,
            &mut *state.monitor_auto_turn_finished,
            &mut *state.monitor_auto_turn_synthetic_msg_id,
            &mut *state.monitor_auto_turn_ledger_generation,
        )
        .await;
        discard_watcher_pending_buffer_after_suppressed_turn(
            &mut *state.all_data,
            &mut *state.all_data_start_offset,
            &mut *state.all_data_fully_mirrored_to_session_relay,
            &mut *state.all_data_session_bound_relay_ack,
            &mut *state.all_data_first_forwarded_relay_sequence,
            current_offset,
        );
        return PreEmitGuardOutcome::ContinueWatcherLoop;
    }

    // Duplicate-relay guard: if we already relayed from this same data
    // range, suppress. Use strict `<` so output starting exactly at the
    // previous boundary is treated as the next turn rather than a re-read.
    if let Ok(meta) = std::fs::metadata(output_path) {
        let observed_output_end = meta.len();
        reset_stale_relay_watermark_if_output_regressed(
            shared,
            channel_id,
            tmux_session_name,
            observed_output_end,
            "pre_local_dedupe",
        );
        reset_stale_local_relay_offset_if_output_regressed(
            &mut *state.last_relayed_offset,
            &mut *state.last_observed_generation_mtime_ns,
            channel_id,
            tmux_session_name,
            observed_output_end,
            "pre_local_dedupe",
        );
    }
    if let Some(prev_offset) = *state.last_relayed_offset {
        if data_start_offset < prev_offset {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Duplicate relay guard: suppressed re-relay for {} (data_start={}, last_relayed={:?})",
                tmux_session_name,
                data_start_offset,
                *state.last_relayed_offset,
            );
            if let Some(msg_id) = placeholder_msg_id {
                let inflight_before_cleanup =
                    crate::services::discord::inflight::load_inflight_state(
                        watcher_provider,
                        channel_id.get(),
                    );
                let _ = delete_nonterminal_placeholder_unless_delivered(
                    http,
                    channel_id,
                    shared,
                    watcher_provider,
                    tmux_session_name,
                    msg_id,
                    inflight_before_cleanup.as_ref(),
                    Some((
                        turn_data_start_offset,
                        terminal_event_consumed_offset(current_offset, &*state.all_data),
                    )),
                    response_sent_offset,
                    last_edit_text,
                    "watcher_duplicate_relay_guard_cleanup",
                )
                .await;
            }
            finish_monitor_auto_turn_if_claimed(
                shared,
                watcher_provider,
                channel_id,
                &mut *state.monitor_auto_turn_claimed,
                &mut *state.monitor_auto_turn_finished,
                &mut *state.monitor_auto_turn_synthetic_msg_id,
                &mut *state.monitor_auto_turn_ledger_generation,
            )
            .await;
            discard_watcher_pending_buffer_after_suppressed_turn(
                &mut *state.all_data,
                &mut *state.all_data_start_offset,
                &mut *state.all_data_fully_mirrored_to_session_relay,
                &mut *state.all_data_session_bound_relay_ack,
                &mut *state.all_data_first_forwarded_relay_sequence,
                current_offset,
            );
            return PreEmitGuardOutcome::ContinueWatcherLoop;
        }
    }

    // Detect stale session resume failure in watcher output
    let is_stale_resume = stale_resume_detected;
    if is_stale_resume {
        clear_provider_overload_retry_state(channel_id);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ Watcher detected stale session resume failure (channel {}), clearing session_id",
            channel_id
        );
        let stale_sid = {
            let mut data = shared.core.lock().await;
            let old = data
                .sessions
                .get(&channel_id)
                .and_then(|s| s.session_id.clone());
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                session.clear_provider_session();
            }
            old
        };
        // Clear DB session_id
        {
            let hostname = crate::services::platform::hostname_short();
            let session_key = format!("{}:{}", hostname, tmux_session_name);
            crate::services::discord::adk_session::clear_provider_session_id(
                &session_key,
                shared.api_port,
            )
            .await;
        }
        if let Some(ref sid) = stale_sid {
            let _ = crate::services::discord::internal_api::clear_stale_session_id(sid).await;
        }
        crate::services::termination_audit::record_termination_for_tmux(
            tmux_session_name,
            None,
            "tmux_watcher",
            "stale_resume_retry",
            Some("stale session resume detected — forcing fresh session before auto-retry"),
            None,
        );
        record_tmux_exit_reason(
            tmux_session_name,
            "stale session resume detected — forcing fresh session before auto-retry",
        );
        crate::services::platform::tmux::kill_session(
            tmux_session_name,
            "stale session resume detected — forcing fresh session before auto-retry",
        );
        // Replace placeholder with recovery notice (don't delete — avoids visual gap)
        if let Some(msg_id) = placeholder_msg_id {
            let _ = crate::services::discord::http::edit_channel_message(
                http,
                channel_id,
                msg_id,
                "↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다.",
            )
            .await;
        }
        // Auto-retry: persist Discord history for LLM injection, then queue the
        // original user message as an internal follow-up instead of self-routing
        // through /api/discord/send announce.
        //
        // #897 round-4 Medium: a `rebind_origin` inflight has no real
        // user message or text to retry with (`user_msg_id=0`,
        // user_text="/api/inflight/rebind"), so auto-retry would
        // enqueue a garbage internal follow-up. Skip the retry; the
        // operator is expected to re-invoke `/api/inflight/rebind`
        // once the tmux session is healthy again.
        match crate::services::discord::inflight::load_inflight_state(
            watcher_provider,
            channel_id.get(),
        ) {
            Some(state) if state.rebind_origin || state.user_msg_id == 0 => {
                // rebind_origin and user_msg_id == 0 (e.g. a TUI-direct
                // turn) both have no anchored user message to retry against;
                // `MessageId::new(0)` would panic.
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ Watcher auto-retry skipped for channel {} — inflight has no user message to retry",
                    channel_id
                );
            }
            Some(state) => {
                crate::services::discord::tmux_overload_retry::schedule_discord_retry_with_history_completion_release(
                    shared.clone(),
                    http.clone(),
                    watcher_provider.clone(),
                    channel_id,
                    serenity::MessageId::new(state.user_msg_id),
                    state.user_text,
                );
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ↻ Watcher auto-retry queued for channel {}",
                    channel_id
                );
            }
            None => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ Watcher auto-retry skipped: inflight state missing for channel {}",
                    channel_id
                );
            }
        }
        // Skip normal response relay
        *state.full_response = String::new();
    }

    PreEmitGuardOutcome::Proceed
}
