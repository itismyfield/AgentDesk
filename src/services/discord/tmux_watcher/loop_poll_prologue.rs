use super::*;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PollOutcome {
    ContinueWatcherLoop,
    BreakWatcherLoop,
    OutputReady {
        data: Vec<u8>,
        data_start_offset: u64,
        epoch_snapshot: u64,
    },
}

pub(super) struct PollWatcherContext<'a> {
    pub(super) http: &'a Arc<serenity::Http>,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) channel_id: ChannelId,
    pub(super) watcher_provider: &'a ProviderKind,
    pub(super) tmux_session_name: &'a str,
    pub(super) output_path: &'a str,
    pub(super) watcher_thread_channel_id: Option<u64>,
    pub(super) watcher_instance_id: u64,
}

pub(super) struct PollWatcherControls<'a> {
    pub(super) cancel: &'a Arc<AtomicBool>,
    pub(super) paused: &'a Arc<AtomicBool>,
    pub(super) resume_offset: &'a Arc<std::sync::Mutex<Option<u64>>>,
    pub(super) pause_epoch: &'a Arc<AtomicU64>,
    pub(super) turn_delivered: &'a Arc<AtomicBool>,
    pub(super) last_heartbeat_ts_ms: &'a Arc<AtomicI64>,
    pub(super) jsonl_notify: &'a Arc<tokio::sync::Notify>,
    pub(super) dead_marker_notify: &'a Arc<tokio::sync::Notify>,
}

pub(super) struct RelayOffsetState<'a> {
    pub(super) current_offset: &'a mut u64,
    pub(super) terminal_delivery_observed: &'a mut bool,
    pub(super) last_relayed_offset: &'a mut Option<u64>,
    pub(super) last_observed_generation_mtime_ns: &'a mut Option<i64>,
    pub(super) rotation_tick: &'a mut u32,
    pub(super) watcher_turn_identity:
        &'a mut Option<crate::services::discord::inflight::InflightTurnIdentity>,
    pub(super) watcher_turn_nonce: &'a mut Option<String>,
}

pub(super) struct LoopPollState<'a> {
    pub(super) prompt_too_long_killed: bool,
    pub(super) all_data: &'a String,
    pub(super) utf8_decoder: &'a mut Utf8ChunkDecoder,
    pub(super) completion_footer_idle: &'a mut WatcherCompletionFooterIdleState,
    pub(super) last_activity_heartbeat_at: &'a mut Option<std::time::Instant>,
}

pub(super) struct PostTerminalState<'a> {
    pub(super) turn_result_relayed: bool,
    pub(super) post_terminal_continuation_logged: &'a mut bool,
    pub(super) last_post_terminal_suppressed_range: &'a mut Option<(u64, u64)>,
    pub(super) active_stream_inflight_reacquire_logged: &'a mut bool,
    pub(super) restored_turn: &'a Option<RestoredWatcherTurn>,
    pub(super) restored_injected_prompt_message_id: Option<u64>,
}

pub(super) async fn poll_watcher_output_or_continue(
    context: &PollWatcherContext<'_>,
    controls: &PollWatcherControls<'_>,
    relay_offset_state: &mut RelayOffsetState<'_>,
    loop_poll_state: &mut LoopPollState<'_>,
    post_terminal_state: &mut PostTerminalState<'_>,
) -> PollOutcome {
    let http = context.http;
    let shared = context.shared;
    let channel_id = context.channel_id;
    let watcher_provider = context.watcher_provider;
    let tmux_session_name = context.tmux_session_name;
    let output_path = context.output_path;
    let watcher_thread_channel_id = context.watcher_thread_channel_id;
    let watcher_instance_id = context.watcher_instance_id;
    let cancel = controls.cancel;
    let paused = controls.paused;
    let resume_offset = controls.resume_offset;
    let pause_epoch = controls.pause_epoch;
    let turn_delivered = controls.turn_delivered;
    let last_heartbeat_ts_ms = controls.last_heartbeat_ts_ms;
    let jsonl_notify = controls.jsonl_notify;
    let dead_marker_notify = controls.dead_marker_notify;
    let prompt_too_long_killed = loop_poll_state.prompt_too_long_killed;
    let all_data = loop_poll_state.all_data;
    let turn_result_relayed = post_terminal_state.turn_result_relayed;
    let restored_injected_prompt_message_id =
        post_terminal_state.restored_injected_prompt_message_id;
    let mut current_offset = *relay_offset_state.current_offset;
    let mut terminal_delivery_observed = *relay_offset_state.terminal_delivery_observed;
    let mut last_relayed_offset = *relay_offset_state.last_relayed_offset;
    let mut last_observed_generation_mtime_ns =
        *relay_offset_state.last_observed_generation_mtime_ns;
    let mut rotation_tick = *relay_offset_state.rotation_tick;
    let mut post_terminal_continuation_logged =
        *post_terminal_state.post_terminal_continuation_logged;
    let mut last_post_terminal_suppressed_range =
        *post_terminal_state.last_post_terminal_suppressed_range;
    let mut active_stream_inflight_reacquire_logged =
        *post_terminal_state.active_stream_inflight_reacquire_logged;

    macro_rules! commit_poll_state {
        () => {{
            *relay_offset_state.current_offset = current_offset;
            *relay_offset_state.terminal_delivery_observed = terminal_delivery_observed;
            *relay_offset_state.last_relayed_offset = last_relayed_offset;
            *relay_offset_state.last_observed_generation_mtime_ns =
                last_observed_generation_mtime_ns;
            *relay_offset_state.rotation_tick = rotation_tick;
            *post_terminal_state.post_terminal_continuation_logged =
                post_terminal_continuation_logged;
            *post_terminal_state.last_post_terminal_suppressed_range =
                last_post_terminal_suppressed_range;
            *post_terminal_state.active_stream_inflight_reacquire_logged =
                active_stream_inflight_reacquire_logged;
        }};
    }

    last_heartbeat_ts_ms.store(
        crate::services::discord::tmux_watcher_now_ms(),
        std::sync::atomic::Ordering::Release,
    );
    // Always consume resume_offset first — the turn bridge may have set it
    // between the previous paused check and now, so reading it here prevents
    // the watcher from using a stale current_offset after unpausing.
    if let Some(new_offset) = resume_offset.lock().ok().and_then(|mut g| g.take()) {
        current_offset = new_offset;
        let bridge_delivered_turn = turn_delivered.load(Ordering::Acquire);
        terminal_delivery_observed = watcher_lifecycle_terminal_delivery_observed(
            terminal_delivery_observed,
            bridge_delivered_turn,
        );
        // If the bridge already delivered the previous turn, treat this resume
        // point as already consumed once so the watcher doesn't re-relay the
        // same batch after unpausing.
        last_relayed_offset = if bridge_delivered_turn {
            Some(new_offset)
        } else {
            None
        };
        // #1275 P2 #2: snapshot the current `.generation` mtime alongside
        // the resumed offset. Without this, the local mtime baseline stays
        // at whatever the previous setter left it (often `None` for
        // restored offsets that haven't gone through a relay/rotation
        // cycle yet). A later same-wrapper jsonl rotation would then take
        // the fresh-wrapper branch in `watermark_after_output_regression`,
        // clear `last_relayed_offset`, and re-relay surviving bytes.
        // Pair the mtime with the offset only when we keep the offset (the
        // turn_delivered branch); otherwise the next loop walks from 0
        // anyway and a baseline would be misleading.
        if last_relayed_offset.is_some() {
            last_observed_generation_mtime_ns =
                Some(read_generation_file_mtime_ns(tmux_session_name));
        }
        // Clear turn_delivered after preserving the duplicate-relay guard so
        // future turns beyond this resume point can be relayed normally.
        turn_delivered.store(false, Ordering::Relaxed);
    }

    // Check cancel or global shutdown (no "session ended" message). #3277
    // (Defect B): log the stop reason — a silent break here made a
    // replaced incumbent's exit look like an unexplained watcher death.
    if cancel.load(Ordering::Relaxed) || shared.restart.shutting_down.load(Ordering::Relaxed) {
        tracing::info!(
            instance = watcher_instance_id,
            cancel = cancel.load(Ordering::Relaxed),
            shutting_down = shared.restart.shutting_down.load(Ordering::Relaxed),
            "tmux watcher stopping for #{tmux_session_name}: cancelled/shutdown"
        );
        commit_poll_state!();
        return PollOutcome::BreakWatcherLoop;
    }

    refresh_watcher_turn_identity(
        &mut *relay_offset_state.watcher_turn_identity,
        &mut *relay_offset_state.watcher_turn_nonce,
        watcher_provider,
        channel_id,
        tmux_session_name,
        current_offset,
    );

    // If paused (Discord handler is processing its own turn), keep the
    // liveness monitor active so a dead pane still clears watcher state.
    if paused.load(Ordering::Relaxed) {
        match tmux_liveness_decision(
            cancel.load(Ordering::Relaxed),
            shared.restart.shutting_down.load(Ordering::Relaxed),
            probe_tmux_session_liveness(tmux_session_name).await,
        ) {
            TmuxLivenessDecision::Continue => {
                // #2441 (H1) — graduate the fixed 200ms paused-loop
                // poll onto the notify-backed JsonlWatcher. A wrapper
                // write wakes us early; the sleep stays as the upper
                // bound.
                sleep_or_jsonl_event(
                    tokio::time::Duration::from_millis(200),
                    jsonl_notify,
                    dead_marker_notify,
                )
                .await;
                commit_poll_state!();
                return PollOutcome::ContinueWatcherLoop;
            }
            TmuxLivenessDecision::QuietStop => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                );
                commit_poll_state!();
                return PollOutcome::BreakWatcherLoop;
            }
            TmuxLivenessDecision::TmuxDied => {
                handle_tmux_watcher_observed_death(
                    channel_id,
                    http,
                    shared,
                    tmux_session_name,
                    output_path,
                    watcher_provider,
                    prompt_too_long_killed,
                    watcher_lifecycle_terminal_delivery_observed(
                        terminal_delivery_observed,
                        turn_delivered.load(Ordering::Acquire),
                    ),
                )
                .await;
                commit_poll_state!();
                return PollOutcome::BreakWatcherLoop;
            }
        }
    }

    rotation_tick = rotation_tick.wrapping_add(1);
    (
        current_offset,
        last_relayed_offset,
        last_observed_generation_mtime_ns,
    ) = rotate_watcher_jsonl_if_due(
        rotation_tick,
        output_path,
        tmux_session_name,
        current_offset,
        last_relayed_offset,
        last_observed_generation_mtime_ns,
        shared,
        channel_id,
    )
    .await;

    // Snapshot pause epoch — if this changes later, a Discord turn claimed this data
    let epoch_snapshot = pause_epoch.load(Ordering::Relaxed);

    // Try to read new data from output file
    let read_result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking({
            let path = output_path.to_string();
            let offset = current_offset;
            move || -> Result<(Vec<u8>, u64), String> {
                let mut file = std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                file.seek(SeekFrom::Start(offset))
                    .map_err(|e| format!("seek: {}", e))?;
                let mut buf = vec![0u8; 16384];
                let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                buf.truncate(n);
                Ok((buf, offset + n as u64))
            }
        }),
    )
    .await;

    let (data, new_offset) = match read_result {
        Ok(Ok(Ok((data, off)))) => (data, off),
        _ => {
            match tmux_liveness_decision(
                cancel.load(Ordering::Relaxed),
                shared.restart.shutting_down.load(Ordering::Relaxed),
                probe_tmux_session_liveness(tmux_session_name).await,
            ) {
                TmuxLivenessDecision::Continue => {
                    // #2441 (H1) — notify-backed wake-up for the
                    // initial-read failure retry.
                    sleep_or_jsonl_event(
                        tokio::time::Duration::from_millis(250),
                        jsonl_notify,
                        dead_marker_notify,
                    )
                    .await;
                    commit_poll_state!();
                    return PollOutcome::ContinueWatcherLoop;
                }
                TmuxLivenessDecision::QuietStop => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                    );
                    commit_poll_state!();
                    return PollOutcome::BreakWatcherLoop;
                }
                TmuxLivenessDecision::TmuxDied => {
                    handle_tmux_watcher_observed_death(
                        channel_id,
                        http,
                        shared,
                        tmux_session_name,
                        output_path,
                        watcher_provider,
                        prompt_too_long_killed,
                        watcher_lifecycle_terminal_delivery_observed(
                            terminal_delivery_observed,
                            turn_delivered.load(Ordering::Acquire),
                        ),
                    )
                    .await;
                    commit_poll_state!();
                    return PollOutcome::BreakWatcherLoop;
                }
            }
        }
    };

    let bytes_available = data.len().saturating_add(all_data.len());
    let poll_decision = if bytes_available == 0 {
        watcher_output_poll_decision(
            bytes_available,
            Some(tmux_liveness_decision(
                cancel.load(Ordering::Relaxed),
                shared.restart.shutting_down.load(Ordering::Relaxed),
                probe_tmux_session_liveness(tmux_session_name).await,
            )),
        )
    } else {
        watcher_output_poll_decision(bytes_available, None)
    };
    match poll_decision {
        WatcherOutputPollDecision::DrainOutput => {}
        WatcherOutputPollDecision::Continue => {
            refresh_watcher_completion_footer_if_due(
                http,
                shared,
                channel_id,
                shared.ui.status_panel_v2_enabled,
                &mut *loop_poll_state.completion_footer_idle,
            )
            .await;
            // #2441 (H1) — notify-backed wake-up for the
            // poll-decision "wait more" branch.
            sleep_or_jsonl_event(
                tokio::time::Duration::from_millis(250),
                jsonl_notify,
                dead_marker_notify,
            )
            .await;
            commit_poll_state!();
            return PollOutcome::ContinueWatcherLoop;
        }
        WatcherOutputPollDecision::QuietStop => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
            );
            commit_poll_state!();
            return PollOutcome::BreakWatcherLoop;
        }
        WatcherOutputPollDecision::TmuxDied => {
            handle_tmux_watcher_observed_death(
                channel_id,
                http,
                shared,
                tmux_session_name,
                output_path,
                watcher_provider,
                prompt_too_long_killed,
                watcher_lifecycle_terminal_delivery_observed(
                    terminal_delivery_observed,
                    turn_delivered.load(Ordering::Acquire),
                ),
            )
            .await;
            commit_poll_state!();
            return PollOutcome::BreakWatcherLoop;
        }
    }

    // We got new data while not paused — this means terminal input triggered a response
    let data_start_offset = current_offset; // offset where this read batch started
    current_offset = new_offset;
    // #3956: re-stamp the submit prompt anchor on this observed streaming output
    // so a turn streaming continuously past PROMPT_ANCHOR_SUBMIT_TTL (4h) keeps a
    // live anchor for the #3885 same-input follow-up-requeue peek (no duplicate
    // prose). No-op unless an anchor already exists for THIS channel; the helper
    // touches only the submit anchor and never the #3459/#3303 relayed-entry
    // ledger (its own decoupled 30min TTL). Refresh-on-activity, not a lifecycle.
    crate::services::tui_prompt_dedupe::touch_prompt_anchor_on_activity(
        watcher_provider.as_str(),
        tmux_session_name,
        channel_id.get(),
    );
    // #1137: surface a single warning when output keeps arriving after a
    // terminal-success relay. The watcher will keep running (the legacy
    // single-event exit was the bug); this log makes the continuation
    // observable in the operational timeline.
    if turn_result_relayed && !post_terminal_continuation_logged {
        post_terminal_continuation_logged = true;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] 👁 post-terminal-success continuation: new output arrived for {tmux_session_name} after terminal success (offset {data_start_offset} -> {new_offset}); watcher staying alive"
        );
    }
    // Compute the SSH-direct bypass signal lazily — the dedupe state
    // lookup grabs a global Mutex and walks the purge maps, so we only
    // pay that cost when the cheap (terminal + no-inflight) prefix is
    // already true and we are about to suppress.
    let post_terminal_inflight_missing =
        crate::services::discord::inflight::load_inflight_state(watcher_provider, channel_id.get())
            .is_none();
    let runtime_kind_marker = if turn_result_relayed && post_terminal_inflight_missing {
        crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux_session_name)
    } else {
        None
    };
    if matches!(
        runtime_kind_marker,
        Some(crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper)
    ) && watcher_batch_contains_relayable_response(&data)
    {
        let _ = observe_legacy_wrapper_direct_prompt_from_pane(
            watcher_provider,
            tmux_session_name,
            channel_id,
            data_start_offset,
            current_offset,
        );
    }
    let ssh_direct_prompt_pending = if turn_result_relayed && post_terminal_inflight_missing {
        crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
            watcher_provider.as_str(),
            tmux_session_name,
            channel_id.get(),
        )
        .is_some()
            || crate::services::tui_prompt_dedupe::is_ssh_direct_observation_pending(
                watcher_provider.as_str(),
                tmux_session_name,
            )
    } else {
        false
    };
    let external_input_lease_present = if turn_result_relayed && post_terminal_inflight_missing {
        crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
            watcher_provider.as_str(),
            tmux_session_name,
            channel_id.get(),
        )
    } else {
        false
    };
    let post_terminal_payload =
        (turn_result_relayed && post_terminal_inflight_missing).then(|| {
            let mut post_terminal_payload = String::with_capacity(all_data.len() + data.len());
            post_terminal_payload.push_str(all_data);
            post_terminal_payload.push_str(&String::from_utf8_lossy(&data));
            post_terminal_payload
        });
    let post_terminal_payload_allows_external_relay =
        post_terminal_payload.as_deref().is_some_and(|payload| {
            post_terminal_jsonl_payload_contains_init_without_user_event(payload.as_bytes())
        });
    let post_terminal_payload_contains_assistant_event = post_terminal_payload
        .as_deref()
        .is_some_and(|payload| watcher_batch_contains_assistant_event(payload.as_bytes()));
    // #3107: lazy pane-busy probe — capture the pane only when the cheap
    // (terminal + no-inflight) prefix already holds (keeps `tmux capture-pane` off the hot path).
    let post_terminal_pane_actively_streaming = turn_result_relayed
        && post_terminal_inflight_missing
        && watcher_pane_actively_streaming(tmux_session_name);
    if post_terminal_pane_actively_streaming {
        // Self-heal: a live turn lost its inflight but kept streaming post-terminal;
        // re-establish a watcher-owned inflight (reusing the restored turn's persisted ids).
        let restored_panel = post_terminal_state
            .restored_turn
            .as_ref()
            .and_then(|turn| turn.status_message_id);
        let restored_placeholder = post_terminal_state
            .restored_turn
            .as_ref()
            .and_then(|turn| (turn.current_msg_id.get() != 0).then_some(turn.current_msg_id));
        let reacquired = reacquire_watcher_inflight_for_active_stream(
            watcher_provider,
            channel_id,
            tmux_session_name,
            output_path,
            data_start_offset,
            restored_panel,
            restored_placeholder,
            restored_injected_prompt_message_id,
        );
        if reacquired && !active_stream_inflight_reacquire_logged {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 🩹 watcher: re-acquired watcher-owned inflight for actively-streaming pane after post-terminal output without inflight (channel {}, tmux={}, range {}..{})",
                channel_id.get(),
                tmux_session_name,
                data_start_offset,
                current_offset
            );
            active_stream_inflight_reacquire_logged = true;
        }
    }
    // #3154: a deferred synthetic turn-start pending for this channel means
    // the per-channel worker has not yet saved the matching inflight; keep
    // the bytes buffered (do NOT suppress / advance confirmed offset) so the
    // wakeup turn's response batch survives the wait window.
    let pending_synthetic_start_present = post_terminal_inflight_missing
        && crate::services::discord::tui_direct_pending_start::pending_synthetic_start_present(
            watcher_provider.as_str(),
            channel_id.get(),
        );
    let post_terminal_no_inflight_should_suppress =
        should_suppress_post_terminal_output_without_inflight(
            turn_result_relayed,
            post_terminal_inflight_missing,
            ssh_direct_prompt_pending,
            external_input_lease_present,
            post_terminal_payload_contains_assistant_event,
            post_terminal_pane_actively_streaming,
            pending_synthetic_start_present,
        ) && !post_terminal_payload_allows_external_relay;
    if post_terminal_payload_allows_external_relay {
        tracing::info!(
            channel_id = channel_id.get(),
            tmux_session = %tmux_session_name,
            range_start = data_start_offset,
            range_end = current_offset,
            "watcher allowed post-terminal no-inflight JSONL init payload for external relay"
        );
    }
    if post_terminal_no_inflight_should_suppress {
        let suppressed_range = (data_start_offset, current_offset);
        if last_post_terminal_suppressed_range != Some(suppressed_range) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 🛑 watcher: suppressed post-terminal output without inflight for channel {} (tmux={}, range {}..{})",
                channel_id.get(),
                tmux_session_name,
                data_start_offset,
                current_offset
            );
            last_post_terminal_suppressed_range = Some(suppressed_range);
        } else {
            tracing::debug!(
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                range_start = data_start_offset,
                range_end = current_offset,
                "watcher: repeated post-terminal suppress for same range"
            );
        }
        last_relayed_offset = Some(current_offset);
        last_observed_generation_mtime_ns = Some(read_generation_file_mtime_ns(tmux_session_name));
        advance_watcher_confirmed_end(
            shared,
            watcher_provider,
            channel_id,
            tmux_session_name,
            suppressed_terminal_confirmed_end(current_offset, all_data),
            "src/services/discord/tmux.rs:post_terminal_no_inflight_suppressed_output",
        );
        // #3053: suppressing post-terminal output is NOT idleness — the
        // wrapper is still alive and producing JSONL. The original code
        // `continue`d here before reaching the heartbeat refresh below, so
        // a live TUI session that only ever emitted post-terminal output
        // (e.g. provider selector continuation) never refreshed its
        // idle-kill heartbeat and was killed as "idle". Touch it here too.
        touch_session_activity(
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            watcher_provider,
            tmux_session_name,
            watcher_thread_channel_id,
            "post_terminal_suppressed_output_while_tmux_alive",
            "tmux_watcher.rs:post_terminal_no_inflight_suppressed_output",
        );
        loop_poll_state.utf8_decoder.clear_pending();
        commit_poll_state!();
        return PollOutcome::ContinueWatcherLoop;
    }
    maybe_refresh_watcher_activity_heartbeat(
        shared.pg_pool.as_ref(),
        &shared.token_hash,
        watcher_provider,
        tmux_session_name,
        watcher_thread_channel_id,
        &mut *loop_poll_state.last_activity_heartbeat_at,
    );
    commit_poll_state!();
    PollOutcome::OutputReady {
        data,
        data_start_offset,
        epoch_snapshot,
    }
}
