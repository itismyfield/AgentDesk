use super::*;

pub(super) enum IdleReadOutcome {
    Break,
    Continue,
    Fallthrough,
}

pub(super) struct IdleReadState<'a> {
    pub(super) ctx: &'a TurnStreamCollectorContext,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) watcher_provider: &'a ProviderKind,
    pub(super) tmux_session_name: &'a String,
    pub(super) channel_id: ChannelId,
    pub(super) output_path: &'a String,
    pub(super) cancel: &'a Arc<AtomicBool>,
    pub(super) jsonl_notify: &'a Arc<tokio::sync::Notify>,
    pub(super) dead_marker_notify: &'a Arc<tokio::sync::Notify>,
    pub(super) watcher_thread_channel_id: Option<u64>,
    pub(super) data_start_offset: u64,
    pub(super) current_offset: &'a mut u64,
    pub(super) last_liveness_probe_at: &'a mut tokio::time::Instant,
    pub(super) tmux_death_observed: &'a mut bool,
    pub(super) last_ready_probe_at: &'a mut Option<std::time::Instant>,
    pub(super) soft_terminal_seen_at: &'a Option<tokio::time::Instant>,
    pub(super) full_response: &'a mut String,
    pub(super) terminal_kind: &'a mut Option<WatcherTerminalKind>,
    pub(super) tool_state: &'a WatcherToolState,
    pub(super) task_notification_kind: Option<TaskNotificationKind>,
    pub(super) ready_for_input_tracker: &'a mut crate::services::provider::ReadyForInputIdleTracker,
    pub(super) fresh_ready_for_input_idle: &'a mut bool,
    pub(super) ready_for_input_stall_inflight_snapshot: &'a mut Option<InflightTurnState>,
    pub(super) ready_for_input_stall_dispatch_id: &'a mut Option<String>,
    pub(super) ready_for_input_failure_notice: &'a mut Option<String>,
    pub(super) fresh_assistant_text_seen: bool,
    pub(super) last_output_at: &'a tokio::time::Instant,
}

// Classify an empty read while preserving the caller's streaming-loop decisions.
pub(super) async fn process_idle_read(phase: IdleReadState<'_>, off: u64) -> IdleReadOutcome {
    let IdleReadState {
        ctx,
        shared,
        watcher_provider,
        tmux_session_name,
        channel_id,
        output_path,
        cancel,
        jsonl_notify,
        dead_marker_notify,
        watcher_thread_channel_id,
        data_start_offset,
        current_offset,
        last_liveness_probe_at,
        tmux_death_observed,
        last_ready_probe_at,
        soft_terminal_seen_at,
        full_response,
        terminal_kind,
        tool_state,
        task_notification_kind,
        ready_for_input_tracker,
        fresh_ready_for_input_idle,
        ready_for_input_stall_inflight_snapshot,
        ready_for_input_stall_dispatch_id,
        ready_for_input_failure_notice,
        fresh_assistant_text_seen,
        last_output_at,
    } = phase;
    #[cfg(test)]
    o_test::eof(watcher_provider.clone(), channel_id);
    *current_offset = off;
    if should_probe_tmux_liveness(
        last_liveness_probe_at.elapsed(),
        tmux_dead_marker_exists(tmux_session_name),
    ) {
        *last_liveness_probe_at = tokio::time::Instant::now();
        match watcher_output_poll_decision(
            0,
            Some(tmux_liveness_decision(
                cancel.load(Ordering::Relaxed),
                shared.restart.shutting_down.load(Ordering::Relaxed),
                host_gate::tmux_alive_for_watcher(shared, tmux_session_name, channel_id, &ctx.host)
                    .await,
            )),
        ) {
            WatcherOutputPollDecision::DrainOutput => {}
            WatcherOutputPollDecision::Continue => {}
            WatcherOutputPollDecision::QuietStop => return IdleReadOutcome::Break,
            WatcherOutputPollDecision::TmuxDied => {
                *tmux_death_observed = true;
                return IdleReadOutcome::Break;
            }
        }
    }
    // #2441 (H1) — notify-backed wake-up for the
    // "no new bytes, waiting for more" tail of the
    // inner streaming loop. A wrapper write wakes us
    // immediately; the sleep stays as the upper
    // bound.
    sleep_or_jsonl_event(
        tokio::time::Duration::from_millis(200),
        jsonl_notify,
        dead_marker_notify,
    )
    .await;
    let now = std::time::Instant::now();
    // #2442 (H3) — wrapper emits a `ready_for_input` JSONL
    // sentinel on transitioning back to accepting stdin; seeing
    // it in the tail bytes is a free readiness signal that
    // short-circuits the 2s probe cadence (legacy
    // `should_probe_ready` stays a SIGKILL/sentinel-lost fallback).
    // Claude TUI is transcript-backed (composer can stay on-screen
    // during work) so completion uses JSONL turn state, not chrome.
    let sentinel_ready = !matches!(
        watcher_provider,
        crate::services::provider::ProviderKind::Claude
    ) && jsonl_tail_contains_ready_for_input_sentinel(output_path);
    let should_probe_ready = sentinel_ready
        || (*last_ready_probe_at)
            .map(|last| now.duration_since(last) >= READY_FOR_INPUT_IDLE_PROBE_INTERVAL)
            .unwrap_or(true);
    if should_probe_ready {
        *last_ready_probe_at = Some(now);
        let ready_for_input = if sentinel_ready {
            true
        } else {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                tokio::task::spawn_blocking({
                    let name = tmux_session_name.clone();
                    let provider = watcher_provider.clone();
                    let path = output_path.clone();
                    let offset = *current_offset;
                    move || watcher_session_ready_for_input(&name, &provider, &path, offset)
                }),
            )
            .await
            .unwrap_or(Ok(false))
            .unwrap_or(false)
        };
        if soft_terminal_seen_at.is_some() && ready_for_input && !full_response.trim().is_empty() {
            terminal_kind.get_or_insert(WatcherTerminalKind::SoftStopHookSummary);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                target: "agentdesk::services::discord::tmux::tmux_watcher::turn_stream_collector",
                "  [{ts}] 👁 watcher committed soft stop_hook_summary after ready-for-input for {tmux_session_name} at offset {current_offset}"
            );
            return IdleReadOutcome::Break;
        }
        let post_work_observed =
            watcher_has_post_work_ready_evidence(full_response, tool_state, task_notification_kind);
        match watcher_ready_for_input_turn_completed(
            ready_for_input_tracker,
            data_start_offset,
            *current_offset,
            ready_for_input,
            post_work_observed,
            now,
        ) {
            crate::services::provider::ReadyForInputIdleState::None => {}
            crate::services::provider::ReadyForInputIdleState::FreshIdle => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    target: "agentdesk::services::discord::tmux::tmux_watcher::turn_stream_collector",
                    "  [{ts}] 👁 watcher observed fresh ready-for-input idle for {tmux_session_name} at offset {current_offset}; leaving session untouched"
                );
                #[cfg(test)]
                o_test::fresh_idle(watcher_provider.clone(), channel_id);
                *fresh_ready_for_input_idle = true;
                return IdleReadOutcome::Break;
            }
            crate::services::provider::ReadyForInputIdleState::PostWorkIdleTimeout => {
                #[cfg(test)]
                o_test::post_work_idle(watcher_provider.clone(), channel_id);
                let ts = chrono::Local::now().format("%H:%M:%S");
                let stall_inflight_snapshot =
                    crate::services::discord::inflight::load_inflight_state(
                        watcher_provider,
                        channel_id.get(),
                    );
                let dispatch_id = resolve_dispatched_thread_dispatch_from_db(
                    shared.pg_pool.as_ref(),
                    watcher_thread_channel_id.unwrap_or_else(|| channel_id.get()),
                )
                .or_else(|| {
                    stall_inflight_snapshot
                        .as_ref()
                        .and_then(|state| state.dispatch_id.clone())
                });
                *ready_for_input_stall_inflight_snapshot = stall_inflight_snapshot;
                if let Some(dispatch_id) = dispatch_id {
                    *ready_for_input_stall_dispatch_id = Some(dispatch_id);
                    *ready_for_input_failure_notice = Some(format!(
                        "⚠️ 작업 후 `Ready for input` 상태에서 멈춰 dispatch를 실패 처리합니다.\n사유: {READY_FOR_INPUT_STUCK_REASON}"
                    ));
                } else {
                    tracing::info!(
                        target: "agentdesk::services::discord::tmux::tmux_watcher::turn_stream_collector",
                        "  [{ts}] 👁 watcher detected post-work Ready-for-input idle for {} with no dispatch; suppressing dispatch-failure notice",
                        tmux_session_name
                    );
                }
                watcher_handle_no_dispatch_post_work_idle_body(
                    full_response,
                    terminal_kind,
                    ready_for_input_stall_inflight_snapshot.as_ref(),
                    ready_for_input_stall_dispatch_id.is_some(),
                    tmux_session_name,
                    fresh_assistant_text_seen,
                    *current_offset,
                );
                return IdleReadOutcome::Break;
            }
        }
    }
    if soft_terminal_seen_at.is_some()
        && !full_response.trim().is_empty()
        && last_output_at.elapsed() >= SOFT_TERMINAL_DEBOUNCE
    {
        terminal_kind.get_or_insert(WatcherTerminalKind::SoftStopHookSummary);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            target: "agentdesk::services::discord::tmux::tmux_watcher::turn_stream_collector",
            "  [{ts}] 👁 watcher committed soft stop_hook_summary after debounce for {tmux_session_name} at offset {current_offset}"
        );
        return IdleReadOutcome::Break;
    }
    IdleReadOutcome::Fallthrough
}
