use super::*;
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::cluster::relay_producer_registry::RelayProducerRegistry;
use crate::services::cluster::stream_relay::RelayProducer;
use crate::services::discord::InflightTurnState;
use crate::services::discord::task_notification_delivery::merge_context;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

#[path = "turn_stream_collector/chunk_forward.rs"]
mod chunk_forward;
use chunk_forward::{forward_turn_chunk_to_supervisor_relay, forwarded_chunk};
#[path = "turn_stream_collector/state.rs"]
mod state;
pub(super) use state::*;

pub(super) async fn collect_turn_stream_until_terminal(
    ctx: &TurnStreamCollectorContext,
    io: TurnStreamCollectorIo,
    parser: &mut TurnParseState<'_>,
    relay: &mut SupervisorRelayState<'_>,
    monitor: &mut MonitorAutoTurnState,
    render_seed: &mut RenderSeedState,
) -> CollectOutcome {
    let mut continuation = parser.continuation.take();
    // A terminal already parsed by the outgoing task goes through the ordinary
    // receipt/lease path. EOF is not a new terminal and no frame is forwarded twice.
    if continuation.as_ref().is_some_and(|turn| turn.found_result) {
        return CollectOutcome::Fallthrough(continuation.take().unwrap());
    }
    let http = ctx.http.clone();
    let shared = ctx.shared.clone();
    let channel_id = ctx.channel_id;
    let watcher_provider = ctx.watcher_provider.clone();
    let tmux_session_name = ctx.tmux_session_name.clone();
    let output_path = ctx.output_path.clone();
    let input_fifo_path = ctx.input_fifo_path.clone();
    let watcher_thread_channel_id = ctx.watcher_thread_channel_id;
    let cancel = ctx.cancel.clone();
    let paused = ctx.paused.clone();
    let pause_epoch = ctx.pause_epoch.clone();
    let turn_delivered = ctx.turn_delivered.clone();
    let last_heartbeat_ts_ms = ctx.last_heartbeat_ts_ms.clone();
    let jsonl_notify = ctx.jsonl_notify.clone();
    let dead_marker_notify = ctx.dead_marker_notify.clone();
    let turn_result_relayed = ctx.turn_result_relayed;
    let restored_injected_prompt_message_id = ctx.restored_injected_prompt_message_id;
    let data = io.data;
    let data_start_offset = io.data_start_offset;
    let epoch_snapshot = io.epoch_snapshot;
    let source_authority = io.source_authority;
    let utf8_decoder = &mut *parser.utf8_decoder;
    let watcher_turn_identity = (*parser.watcher_turn_identity).clone();
    let producer_registry = (*relay.producer_registry).clone();
    let mut current_offset = *parser.current_offset;
    let mut all_data = (*parser.all_data).clone();
    let mut all_data_start_offset = *parser.all_data_start_offset;
    let mut pending_terminal_rewind_seed = (*parser.pending_terminal_rewind_seed).clone();
    let mut restored_turn = (*parser.restored_turn).clone();
    let mut terminal_rewind_attempt_key = (*parser.terminal_rewind_attempt_key).clone();
    let mut terminal_rewind_attempts = *parser.terminal_rewind_attempts;
    let mut last_activity_heartbeat_at = *parser.last_activity_heartbeat_at;
    let mut active_stream_inflight_reacquire_logged =
        *parser.active_stream_inflight_reacquire_logged;
    let mut cached_relay_producer = (*relay.cached_relay_producer).clone();
    let mut all_data_fully_mirrored_to_session_relay =
        *relay.all_data_fully_mirrored_to_session_relay;
    let mut all_data_session_bound_relay_ack = (*relay.all_data_session_bound_relay_ack).clone();
    let mut all_data_first_forwarded_relay_sequence =
        *relay.all_data_first_forwarded_relay_sequence;

    macro_rules! commit_persistent_state {
        () => {{
            *parser.current_offset = current_offset;
            *parser.all_data = all_data.clone();
            *parser.all_data_start_offset = all_data_start_offset;
            *parser.pending_terminal_rewind_seed = pending_terminal_rewind_seed.clone();
            *parser.restored_turn = restored_turn.clone();
            *parser.terminal_rewind_attempt_key = terminal_rewind_attempt_key.clone();
            *parser.terminal_rewind_attempts = terminal_rewind_attempts;
            *parser.last_activity_heartbeat_at = last_activity_heartbeat_at;
            *parser.active_stream_inflight_reacquire_logged =
                active_stream_inflight_reacquire_logged;
            *relay.cached_relay_producer = cached_relay_producer.clone();
            *relay.all_data_fully_mirrored_to_session_relay =
                all_data_fully_mirrored_to_session_relay;
            *relay.all_data_session_bound_relay_ack = all_data_session_bound_relay_ack.clone();
            *relay.all_data_first_forwarded_relay_sequence =
                all_data_first_forwarded_relay_sequence;
        }};
    }

    let initial_buffer_was_empty = all_data.is_empty();
    let decoded_data = utf8_decoder.decode_source_for_buffer(
        &data,
        data_start_offset,
        source_authority,
        &all_data,
    );
    let initial_source_authority =
        authority_for_decoded_text(source_authority, decoded_data.mixed_read_provenance);
    if initial_buffer_was_empty {
        all_data_start_offset = decoded_data.start_offset.unwrap_or(data_start_offset);
    }
    if decoded_data.text.is_empty() && all_data.is_empty() && continuation.is_none() {
        commit_persistent_state!();
        return CollectOutcome::ContinueWatcherLoop;
    }
    all_data.push_str(&decoded_data.text);
    let initial_buffer_start_offset = all_data_start_offset;
    let turn_data_start_offset = continuation
        .as_ref()
        .map_or(all_data_start_offset, |turn| turn.turn_data_start_offset);
    reset_rewind_attempts(
        &mut terminal_rewind_attempt_key,
        &mut terminal_rewind_attempts,
        watcher_rewind_attempt_key(turn_data_start_offset, watcher_turn_identity.as_ref()),
    );
    // #3041 P1-3 R7: reset carried ACKs after terminal/next-turn splits so later turns cannot inherit them and black-hole.
    let mut split_trailing_turn_follows = false;
    let mut state = continuation
        .as_ref()
        .map(|turn| turn.state.clone())
        .unwrap_or_default();
    let restored_turn_seed =
        take_pending_or_restored_rewind_seed(&mut pending_terminal_rewind_seed, &mut restored_turn);
    let prompt_anchor_for_seed_discard =
        crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
            watcher_provider.as_str(),
            &tmux_session_name,
            channel_id.get(),
        );
    let seed_disposition = watcher_stream_seed_after_restored_seed_discard(
        restored_turn_seed,
        watcher_turn_identity.as_ref(),
        prompt_anchor_for_seed_discard.map(|anchor| anchor.message_id),
    );
    if !seed_disposition.discard_restored_seed
        && seed_disposition.prompt_anchor_present
        && seed_disposition.restored_seed_undelivered_body_len > 0
    {
        tracing::info!(
            channel_id = channel_id.get(),
            body_len = seed_disposition.restored_seed_undelivered_body_len,
            tmux_session = %tmux_session_name,
            "watcher: preserving restored stream seed with undelivered body for idle SSH-direct prompt"
        );
    }
    if seed_disposition.discard_restored_seed {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 👁 watcher: discarding restored stream seed for idle SSH-direct prompt on channel {} (tmux={}, cross_turn={})",
            channel_id.get(),
            tmux_session_name,
            seed_disposition.seed_reassigned_to_different_turn
        );
    }
    let stream_seed = continuation
        .as_ref()
        .map(|turn| WatcherStreamSeed {
            full_response: turn.full_response.clone(),
            placeholder_msg_id: turn.placeholder_msg_id,
            status_panel_msg_id: turn.status_panel_msg_id,
            last_edit_text: turn.last_edit_text.clone(),
            response_sent_offset: turn.response_sent_offset,
            streaming_rollover_frozen_msg_ids: turn
                .watcher_streaming_rollover_frozen_msg_ids
                .clone(),
            task_notification_kind: turn.task_notification_kind,
            finish_mailbox_on_completion: turn.finish_mailbox_on_completion,
        })
        .unwrap_or(seed_disposition.stream_seed);
    let restored_response_seed = continuation
        .as_ref()
        .map(|turn| turn.restored_response_seed.clone())
        .unwrap_or_else(|| stream_seed.full_response.clone());
    let restored_assistant_text_seen = !stream_seed.full_response.trim().is_empty();
    // #3041 P1-3 B1: restored assistant text was not mirrored into StreamRelay,
    // so reset it after the deferred initial forward and keep watcher ownership.
    let mut full_response = stream_seed.full_response;

    let mut spin_idx: usize = 0;
    let mut placeholder_msg_id: Option<serenity::MessageId> = stream_seed.placeholder_msg_id;
    let mut placeholder_from_restored_inflight = placeholder_msg_id.is_some();
    let mut status_panel_msg_id: Option<serenity::MessageId> = stream_seed.status_panel_msg_id;
    let single_message_panel_footer_mode =
        watcher_single_message_panel_footer_enabled(shared.ui.status_panel_v2_enabled);
    if single_message_panel_footer_mode {
        status_panel_msg_id = None;
    }
    // #3003 (codex P2 r4): cache whether this turn is a TUI-direct
    // external-input turn while the inflight row is still present, so the
    // orphan-panel reclaim can run after a stop/cancel clears inflight.
    let Ok((mut tool_state, startup_inflight_snapshot)) = (if let Some(turn) = continuation.as_ref()
    {
        Ok((
            turn.tool_state.clone(),
            turn.startup_inflight_snapshot.clone(),
        ))
    } else {
        source_authority.restore_stream_decoder(ctx, turn_data_start_offset, &mut full_response)
    }) else {
        *parser.current_offset = data_start_offset;
        utf8_decoder.clear_pending();
        return CollectOutcome::ContinueWatcherLoop;
    };
    let completion_actor = if let Some(turn) = continuation.as_ref() {
        turn.completion_actor.clone()
    } else {
        cancel_handoff::completion::capture_actor(
            &shared,
            channel_id,
            startup_inflight_snapshot.as_ref(),
        )
        .await
    };
    // #3805 P2 (PR-C): this turn's status-panel generation epoch, SEEDED from
    // the on-disk row so a restart re-hydrating an existing panel carries the
    // SAME epoch it was created with (a stale-epoch completion is thus never
    // falsely skipped). The two-message create bumps it (opens the epoch on a
    // fresh bind); the completion guard proves it against the on-disk epoch.
    // Inert on the default-OFF path (stays 0) and while no mid-turn re-anchor
    // exists yet (PR-D) — this turn's epoch always equals the on-disk epoch.
    let mut this_turn_status_panel_generation: u64 = startup_inflight_snapshot
        .as_ref()
        .map(|state| state.status_panel_generation)
        .unwrap_or(0);
    // status-panel-v2: panel eligibility (external-input OR synthetic
    // monitor/self-paced-loop) drives the panel-lifecycle sites that read
    // this flag. The lease/⏳-anchor sites keep the narrower external-input
    // predicate and are untouched.
    let mut turn_is_external_input_for_session = watcher_inflight_is_panel_eligible_for_session(
        startup_inflight_snapshot.as_ref(),
        &tmux_session_name,
    );
    // #3003 (codex P2 r11): snapshot this turn's identity so the abandon check
    // can treat a *replaced* inflight (a new turn on the same channel) as
    // abandoned, not just a missing one. user_msg_id is 0 for external input,
    // so `started_at` is the discriminator between consecutive TUI-direct turns.
    let mut turn_identity_for_panel = startup_inflight_snapshot
        .as_ref()
        .filter(|state| state.tmux_session_name.as_deref() == Some(tmux_session_name.as_str()))
        .map(crate::services::discord::inflight::InflightTurnIdentity::from_state);
    let (mut status_panel_started_at, footer_owner) =
        make_owner_now(turn_identity_for_panel.as_ref());
    // #3003 P2: rehydrate a watcher-owned persisted panel id while the row
    // still exists; footer mode intentionally has no separate panel handle.
    if !single_message_panel_footer_mode
        && status_panel_msg_id.is_none()
        && turn_is_external_input_for_session
    {
        status_panel_msg_id = watcher_persisted_status_panel_msg_id(
            startup_inflight_snapshot.as_ref(),
            &tmux_session_name,
        );
    }
    // #3003 P2: reset per-channel live-status state on a genuinely fresh
    // watcher frame. This is deliberately not gated on external-input because
    // the inflight row may not exist yet; restored/bridge-owned frames are
    // excluded by the seed guards.
    let watcher_fresh_turn_frame = placeholder_msg_id.is_none()
        && status_panel_msg_id.is_none()
        && !restored_assistant_text_seen;
    if continuation.is_none()
        && watcher_fresh_turn_frame
        && (shared.ui.placeholder_live_events_enabled || shared.ui.status_panel_v2_enabled)
    {
        if single_message_panel_footer_mode {
            supersede_watcher_footer(&http, &shared, channel_id, footer_owner).await;
            shared
                .ui
                .placeholder_live_events
                .clear_channel_preserving_footer_residuals(channel_id);
        } else {
            shared.ui.placeholder_live_events.clear_channel(channel_id);
        }
    }
    let mut last_status_panel_text = String::new();
    let mut last_edit_text = stream_seed.last_edit_text;
    let mut response_sent_offset = stream_seed.response_sent_offset;
    // #3871: ids of streamed rollover prefixes frozen for this turn; deleted on a
    // terminal full-body fallback so the frozen prose is not duplicated (sink parity).
    // SEEDED from the persisted row so prefixes frozen in an earlier `'watcher_loop`
    // iteration / before a watcher restart survive to the fallback (no residual dup).
    let mut watcher_streaming_rollover_frozen_msg_ids: Vec<serenity::MessageId> =
        stream_seed.streaming_rollover_frozen_msg_ids.clone();
    let finish_mailbox_on_completion = stream_seed.finish_mailbox_on_completion;
    let mut monitor_auto_turn_claimed = false;
    let mut monitor_auto_turn_deferred = false;
    let mut monitor_auto_turn_finished = false;
    let mut completion_footer_terminal_target = None;
    // #3016 P1: the synthetic mailbox message id + process-monotonic ledger
    // generation the active monitor turn started under, threaded to
    // `finish_monitor_auto_turn_if_claimed` so it finalizes the EXACT monitor
    // turn (distinct ledger entries for sequential monitor turns even when
    // the byte-offset-derived synthetic id repeats after a wrapper respawn).
    let mut monitor_auto_turn_synthetic_msg_id: Option<MessageId> = None;
    let mut monitor_auto_turn_ledger_generation: Option<u64> = None;
    if let Some(turn) = continuation.as_ref() {
        this_turn_status_panel_generation = turn.this_turn_status_panel_generation;
        turn_is_external_input_for_session = turn.turn_is_external_input_for_session;
        turn_identity_for_panel = turn.turn_identity_for_panel.clone();
        status_panel_started_at = turn.status_panel_started_at;
        placeholder_from_restored_inflight = turn.placeholder_from_restored_inflight;
        last_status_panel_text = turn.last_status_panel_text.clone();
        monitor_auto_turn_claimed = turn.monitor_auto_turn_claimed;
        monitor_auto_turn_deferred = turn.monitor_auto_turn_deferred;
        monitor_auto_turn_finished = turn.monitor_auto_turn_finished;
        monitor_auto_turn_synthetic_msg_id = turn.monitor_auto_turn_synthetic_msg_id;
        monitor_auto_turn_ledger_generation = turn.monitor_auto_turn_ledger_generation;
        completion_footer_terminal_target = turn.completion_footer_terminal_target.clone();
        split_trailing_turn_follows = turn.split_trailing_turn_follows;
    }
    // NOTE(r3): defined after the reset-local declarations above — macro_rules
    // bodies resolve local identifiers with definition-site hygiene, so this
    // macro must come after every local it commits (E0425 otherwise).
    macro_rules! commit_collect_state {
        () => {{
            commit_persistent_state!();
            monitor.monitor_auto_turn_claimed = monitor_auto_turn_claimed;
            monitor.monitor_auto_turn_deferred = monitor_auto_turn_deferred;
            monitor.monitor_auto_turn_finished = monitor_auto_turn_finished;
            monitor.monitor_auto_turn_synthetic_msg_id = monitor_auto_turn_synthetic_msg_id;
            monitor.monitor_auto_turn_ledger_generation = monitor_auto_turn_ledger_generation;
            render_seed.placeholder_msg_id = placeholder_msg_id;
            render_seed.placeholder_from_restored_inflight = placeholder_from_restored_inflight;
            render_seed.status_panel_msg_id = status_panel_msg_id;
            render_seed.last_status_panel_text = last_status_panel_text.clone();
            render_seed.last_edit_text = last_edit_text.clone();
            render_seed.response_sent_offset = response_sent_offset;
            render_seed.watcher_streaming_rollover_frozen_msg_ids =
                watcher_streaming_rollover_frozen_msg_ids.clone();
            render_seed.completion_footer_terminal_target =
                completion_footer_terminal_target.clone();
        }};
    }
    // #4595: single expansion of the monitor-auto-turn synthetic inflight upsert.
    // Both watcher-loop call sites pass identical arguments; the actor read is
    // async (it snapshots the mailbox episode nonce), so this keeps the two
    // await sites from duplicating the nine-argument call verbatim.
    macro_rules! ensure_monitor_auto_turn_inflight_now {
        () => {
            let _ = ensure_monitor_auto_turn_inflight(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                &output_path,
                &input_fifo_path,
                state.last_session_id.as_deref(),
                data_start_offset,
                current_offset,
            )
            .await;
        };
    }
    // #1009: 1-shot tracker for the monitor-auto-turn preamble hint so the
    // hint text is emitted exactly once per watcher turn frame.
    let mut monitor_auto_turn_preamble_injected = false;
    // Process any complete lines we already have
    let initial_buffer_len = all_data.len();
    observe_qwen_user_prompts_in_buffer(&all_data, &watcher_provider, &tmux_session_name);
    let turn_terminal_start_offset = turn_identity_for_panel
        .as_ref()
        .and_then(|identity| identity.turn_start_offset)
        .unwrap_or(turn_data_start_offset);
    let initial_outcome = process_watcher_lines_for_turn(
        &mut all_data,
        &mut state,
        &mut full_response,
        &mut tool_state,
        Some(initial_buffer_start_offset),
        Some(turn_terminal_start_offset),
    );
    // #3041 P1-3 (Part a, B1): DEFERRED forward of the outer-read chunk. We now
    // know — from `initial_outcome.found_result` — whether THIS chunk is the
    // RESULT-bearing (terminal) one. If so, forward it as a TERMINAL frame
    // carrying the commit fence (`terminal_event_consumed_offset(..)` + the
    // pinned turn identity loaded at turn start), so the SAME frame that
    // triggers the sink's terminal delivery carries the consumed_end + identity
    // (FIFO single-task: a separate later frame would arrive after the sink
    // already dispatched). Non-terminal chunks forward exactly as before (no
    // fence, no streaming-latency change beyond the synchronous parse reorder).
    // The ACK target is captured from THIS forward, so the watcher's wait now
    // correlates to the terminal frame's sequence (more precise).
    let initial_chunk_forward = forwarded_chunk(
        &decoded_data.text,
        initial_buffer_len,
        initial_buffer_start_offset,
        initial_outcome.pre_turn_bytes_skipped,
        initial_source_authority,
    );
    let (
        mut session_bound_relay_turn_fully_mirrored,
        mut session_bound_relay_turn_first_forwarded_sequence,
    ) = if let Some(turn) = continuation.as_ref() {
        (
            turn.session_bound_relay_turn_fully_mirrored,
            turn.session_bound_relay_turn_first_forwarded_sequence,
        )
    } else {
        let initial_terminal_fence = watcher_terminal_commit_fence(
            initial_outcome.found_result,
            turn_data_start_offset,
            terminal_event_consumed_offset(current_offset, &all_data),
            turn_identity_for_panel.as_ref(),
            &tmux_session_name,
        );
        let data_mirrored_to_session_relay = forward_turn_chunk_to_supervisor_relay(
            &tmux_session_name,
            &initial_chunk_forward,
            all_data.len(),
            &producer_registry,
            &mut cached_relay_producer,
            turn_identity_for_panel.as_ref(),
            initial_terminal_fence,
        );
        let supervisor_turn_state = apply_initial_supervisor_relay_forward(
            &mut all_data_fully_mirrored_to_session_relay,
            &mut all_data_session_bound_relay_ack,
            &mut all_data_first_forwarded_relay_sequence,
            &mut split_trailing_turn_follows,
            &data_mirrored_to_session_relay,
            initial_buffer_was_empty,
            all_data.is_empty(),
            restored_assistant_text_seen,
            turn_identity_for_panel.as_ref(),
        );
        (
            supervisor_turn_state.fully_mirrored,
            supervisor_turn_state.first_forwarded_sequence,
        )
    };
    all_data_start_offset = advance_buffer_start_offset(
        initial_buffer_start_offset,
        initial_buffer_len,
        all_data.len(),
    );
    let live_events_dirty = flush_placeholder_live_events(&shared, channel_id, &mut tool_state);
    let mut found_result = initial_outcome.found_result;
    let mut terminal_kind = initial_outcome.terminal_kind;
    let mut terminal_evidence_offset = initial_outcome.terminal_evidence_offset;
    let mut soft_terminal_seen_at = if initial_outcome.soft_terminal_candidate {
        Some(tokio::time::Instant::now())
    } else {
        None
    };
    let mut is_prompt_too_long = initial_outcome.is_prompt_too_long;
    let mut stale_resume_detected = initial_outcome.stale_resume_detected;
    let mut auto_compaction_lifecycle_attempted = false;
    let mut task_notification_kind = stream_seed.task_notification_kind;
    let mut task_notification_context = initial_outcome.task_notification_context;
    let mut assistant_text_seen =
        restored_assistant_text_seen || initial_outcome.assistant_text_seen;
    let mut fresh_assistant_text_seen = initial_outcome.assistant_text_seen;
    if let Some(turn) = continuation.as_ref() {
        terminal_kind = terminal_kind.or(turn.terminal_kind);
        terminal_evidence_offset = terminal_evidence_offset.or(turn.terminal_evidence_offset);
        soft_terminal_seen_at = soft_terminal_seen_at.or(turn.soft_terminal_seen_at);
        is_prompt_too_long |= turn.is_prompt_too_long;
        stale_resume_detected |= turn.stale_resume_detected;
        assistant_text_seen |= turn.assistant_text_seen;
        fresh_assistant_text_seen |= turn.fresh_assistant_text_seen;
        task_notification_context = turn.task_notification_context.clone();
        auto_compaction_lifecycle_attempted = turn.auto_compaction_lifecycle_attempted;
        monitor_auto_turn_preamble_injected = turn.monitor_auto_turn_preamble_injected;
    }
    if let Some(kind) = initial_outcome.task_notification_kind {
        task_notification_kind = merge_task_notification_kind(task_notification_kind, kind);
    }
    if initial_outcome.auto_compacted && !auto_compaction_lifecycle_attempted {
        auto_compaction_lifecycle_attempted = emit_context_compacted_lifecycle_from_watcher(
            &shared,
            channel_id,
            &watcher_provider,
            state.last_model.as_deref(),
            stream_line_state_token_usage(&state),
        )
        .await;
    }
    let post_terminal_success_continuation_flush = should_flush_post_terminal_success_continuation(
        turn_result_relayed,
        found_result,
        current_offset > data_start_offset,
        &full_response,
    );
    if post_terminal_success_continuation_flush {
        found_result = true;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] 👁 post-terminal-success continuation: flushing relayed output for {tmux_session_name} immediately (offset {data_start_offset} -> {current_offset})"
        );
    }
    if !monitor_auto_turn_claimed
        && !cancel.load(Ordering::Acquire)
        && matches!(
            task_notification_kind,
            Some(TaskNotificationKind::MonitorAutoTurn)
        )
    {
        let start = start_monitor_auto_turn_when_available(
            &shared,
            &watcher_provider,
            channel_id,
            data_start_offset,
            cancel.as_ref(),
        )
        .await;
        monitor_auto_turn_claimed = start.acquired;
        monitor_auto_turn_deferred = monitor_auto_turn_deferred || start.deferred;
        if start.acquired {
            monitor_auto_turn_synthetic_msg_id = start.synthetic_message_id;
            monitor_auto_turn_ledger_generation = start.ledger_generation;
        }
        if !start.acquired {
            all_data.clear();
            all_data_start_offset = current_offset;
            all_data_fully_mirrored_to_session_relay = true;
            all_data_session_bound_relay_ack = None;
            all_data_first_forwarded_relay_sequence = None;
            commit_collect_state!();
            return CollectOutcome::ContinueWatcherLoop;
        }
        ensure_monitor_auto_turn_inflight_now!();
        if let Some(hint) =
            consume_monitor_auto_turn_preamble_once(&mut monitor_auto_turn_preamble_injected)
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🔔 monitor auto-turn preamble hint injected (channel {}): {}",
                channel_id.get(),
                hint
            );
        }
    }

    // Keep reading until result or timeout
    // Check if a Discord turn claimed this data since our epoch snapshot
    let epoch_changed = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
    let mut was_paused = paused.load(Ordering::Relaxed) || epoch_changed;
    if was_paused && !monitor_auto_turn_deferred && !cancel.load(Ordering::Acquire) {
        // A Discord turn took over — discard what we read
        all_data.clear();
        all_data_start_offset = current_offset;
        all_data_fully_mirrored_to_session_relay = true;
        all_data_session_bound_relay_ack = None;
        all_data_first_forwarded_relay_sequence = None;
        commit_collect_state!();
        return CollectOutcome::ContinueWatcherLoop;
    }
    let mut active_read_state = None;
    if !found_result {
        let mut last_status_update = tokio::time::Instant::now();
        let mut last_output_at = continuation
            .as_ref()
            .and_then(|turn| turn.active_read_state.as_ref())
            .map_or_else(tokio::time::Instant::now, |read| read.last_output_at);
        if watcher_live_events_dirty_should_force_status_update(
            live_events_dirty,
            single_message_panel_footer_mode,
        ) {
            force_next_watcher_status_update(&mut last_status_update);
        }
        let mut ready_for_input_tracker =
            crate::services::provider::ReadyForInputIdleTracker::default();
        let mut last_ready_probe_at: Option<std::time::Instant> = None;
        let mut last_liveness_probe_at = tokio::time::Instant::now();
        let mut tmux_death_observed = false;
        let mut ready_for_input_failure_notice: Option<String> = None;
        let mut ready_for_input_stall_dispatch_id: Option<String> = None;
        let mut ready_for_input_stall_inflight_snapshot: Option<
            crate::services::discord::inflight::InflightTurnState,
        > = None;
        let mut streaming_suppressed_by_recent_stop = false;
        let mut streaming_suppressed_by_missing_inflight = false;
        let mut fresh_ready_for_input_idle = false;

        while !found_result {
            // Loop can wait minutes for a long tool/test; keep the registry heartbeat
            // fresh so the sweeper does not cancel relay on a healthy streaming watcher.
            last_heartbeat_ts_ms.store(
                crate::services::discord::tmux_watcher_now_ms(),
                std::sync::atomic::Ordering::Release,
            );
            if cancel.load(Ordering::Relaxed)
                || shared.restart.shutting_down.load(Ordering::Relaxed)
            {
                break;
            }
            if paused.load(Ordering::Relaxed) {
                was_paused = true;
                break;
            }

            let (read_more, read_witness) = if let Some(source) = parser.retained_source.as_ref() {
                (
                    read_retained_watcher_source_chunk(source.clone(), current_offset).await,
                    None,
                )
            } else {
                read_watcher_source_chunk_with_witness(ctx, current_offset).await
            };

            match read_more.map(|r| r.map(|r| r.map(|batch| batch.into_parts()))) {
                Ok(Ok(Ok((chunk, off, file_identity)))) if !chunk.is_empty() => {
                    let authority = if parser.retained_source.is_some() {
                        // This is the original opened descriptor, not a new file/marker
                        // pair observed through a path that may have been replaced.
                        WatcherSourceAuthority {
                            source_file: file_identity,
                            ..source_authority
                        }
                    } else {
                        source_authority_for_read(
                            source_authority,
                            &tmux_session_name,
                            read_witness,
                            file_identity,
                        )
                    };
                    current_offset = off;
                    maybe_refresh_watcher_activity_heartbeat(
                        shared.pg_pool.as_ref(),
                        &shared.token_hash,
                        &watcher_provider,
                        &tmux_session_name,
                        watcher_thread_channel_id,
                        &mut last_activity_heartbeat_at,
                    );
                    ready_for_input_tracker.record_output();
                    let chunk_start = current_offset.saturating_sub(chunk.len() as u64);
                    let decoded_chunk = utf8_decoder.decode_source_for_buffer(
                        &chunk,
                        chunk_start,
                        authority,
                        &all_data,
                    );
                    let authority =
                        authority_for_decoded_text(authority, decoded_chunk.mixed_read_provenance);
                    // Defer forwarding until parsing attaches the terminal fence.
                    let chunk_buffer_was_empty = all_data.is_empty();
                    if chunk_buffer_was_empty {
                        all_data_start_offset = decoded_chunk.start_offset.unwrap_or(chunk_start);
                    }
                    if decoded_chunk.text.is_empty() && all_data.is_empty() {
                        continue;
                    }
                    all_data.push_str(&decoded_chunk.text);
                    let chunk_buffer_start_offset = all_data_start_offset;
                    let chunk_buffer_len = all_data.len();
                    observe_qwen_user_prompts_in_buffer(
                        &all_data,
                        &watcher_provider,
                        &tmux_session_name,
                    );
                    let turn_terminal_start_offset = turn_identity_for_panel
                        .as_ref()
                        .and_then(|identity| identity.turn_start_offset)
                        .unwrap_or(chunk_buffer_start_offset);
                    let outcome = process_watcher_lines_for_turn(
                        &mut all_data,
                        &mut state,
                        &mut full_response,
                        &mut tool_state,
                        Some(chunk_buffer_start_offset),
                        Some(turn_terminal_start_offset),
                    );
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
                    );
                    let streaming_terminal_fence = watcher_terminal_commit_fence(
                        outcome.found_result,
                        chunk_buffer_start_offset,
                        terminal_event_consumed_offset(current_offset, &all_data),
                        turn_identity_for_panel.as_ref(),
                        &tmux_session_name,
                    );
                    let chunk_forwarded_to_session_relay = forward_turn_chunk_to_supervisor_relay(
                        &tmux_session_name,
                        &chunk_forward,
                        all_data.len(),
                        &producer_registry,
                        &mut cached_relay_producer,
                        turn_identity_for_panel.as_ref(),
                        streaming_terminal_fence,
                    );
                    apply_streaming_supervisor_relay_forward(
                        &mut all_data_fully_mirrored_to_session_relay,
                        &mut all_data_session_bound_relay_ack,
                        &mut all_data_first_forwarded_relay_sequence,
                        &mut session_bound_relay_turn_fully_mirrored,
                        &mut session_bound_relay_turn_first_forwarded_sequence,
                        &mut split_trailing_turn_follows,
                        &chunk_forwarded_to_session_relay,
                        chunk_buffer_was_empty,
                        all_data.is_empty(),
                        turn_identity_for_panel.as_ref(),
                    );
                    last_output_at = tokio::time::Instant::now();
                    all_data_start_offset = advance_buffer_start_offset(
                        chunk_buffer_start_offset,
                        chunk_buffer_len,
                        all_data.len(),
                    );
                    if watcher_live_events_dirty_should_force_status_update(
                        flush_placeholder_live_events(&shared, channel_id, &mut tool_state),
                        single_message_panel_footer_mode,
                    ) {
                        force_next_watcher_status_update(&mut last_status_update);
                    }
                    found_result = found_result || outcome.found_result;
                    if outcome.found_result {
                        terminal_kind = outcome.terminal_kind.or(terminal_kind);
                        terminal_evidence_offset = outcome
                            .terminal_evidence_offset
                            .or(terminal_evidence_offset);
                    }
                    if outcome.soft_terminal_candidate && soft_terminal_seen_at.is_none() {
                        soft_terminal_seen_at = Some(tokio::time::Instant::now());
                        terminal_kind = outcome
                            .terminal_kind
                            .or(terminal_kind)
                            .or(Some(WatcherTerminalKind::SoftStopHookSummary));
                        terminal_evidence_offset = outcome
                            .terminal_evidence_offset
                            .or(terminal_evidence_offset);
                    }
                    is_prompt_too_long = is_prompt_too_long || outcome.is_prompt_too_long;
                    stale_resume_detected = stale_resume_detected || outcome.stale_resume_detected;
                    if let Some(kind) = outcome.task_notification_kind {
                        task_notification_kind =
                            merge_task_notification_kind(task_notification_kind, kind);
                    }
                    if let Some(context) = outcome.task_notification_context {
                        task_notification_context =
                            merge_context(task_notification_context.take(), context);
                    }
                    assistant_text_seen |= outcome.assistant_text_seen;
                    fresh_assistant_text_seen |= outcome.assistant_text_seen;
                    if matches!(
                        task_notification_kind,
                        Some(TaskNotificationKind::MonitorAutoTurn)
                    ) {
                        if !monitor_auto_turn_claimed {
                            let start = start_monitor_auto_turn_when_available(
                                &shared,
                                &watcher_provider,
                                channel_id,
                                data_start_offset,
                                cancel.as_ref(),
                            )
                            .await;
                            monitor_auto_turn_claimed = start.acquired;
                            monitor_auto_turn_deferred =
                                monitor_auto_turn_deferred || start.deferred;
                            if start.acquired {
                                monitor_auto_turn_synthetic_msg_id = start.synthetic_message_id;
                                monitor_auto_turn_ledger_generation = start.ledger_generation;
                            }
                            if !start.acquired {
                                was_paused = true;
                                break;
                            }
                        }
                        ensure_monitor_auto_turn_inflight_now!();
                        if let Some(hint) = consume_monitor_auto_turn_preamble_once(
                            &mut monitor_auto_turn_preamble_injected,
                        ) {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] 🔔 monitor auto-turn preamble hint injected (channel {}): {}",
                                channel_id.get(),
                                hint
                            );
                        }
                    }
                    if outcome.auto_compacted && !auto_compaction_lifecycle_attempted {
                        auto_compaction_lifecycle_attempted =
                            emit_context_compacted_lifecycle_from_watcher(
                                &shared,
                                channel_id,
                                &watcher_provider,
                                state.last_model.as_deref(),
                                stream_line_state_token_usage(&state),
                            )
                            .await;
                    }
                }
                Ok(Ok(Ok((_, off, _)))) => {
                    current_offset = off;
                    if should_probe_tmux_liveness(
                        last_liveness_probe_at.elapsed(),
                        tmux_dead_marker_exists(&tmux_session_name),
                    ) {
                        last_liveness_probe_at = tokio::time::Instant::now();
                        match watcher_output_poll_decision(
                            0,
                            Some(tmux_liveness_decision(
                                cancel.load(Ordering::Relaxed),
                                shared.restart.shutting_down.load(Ordering::Relaxed),
                                probe_tmux_session_liveness(&tmux_session_name).await,
                            )),
                        ) {
                            WatcherOutputPollDecision::DrainOutput => {}
                            WatcherOutputPollDecision::Continue => {}
                            WatcherOutputPollDecision::QuietStop => break,
                            WatcherOutputPollDecision::TmuxDied => {
                                tmux_death_observed = true;
                                break;
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
                        &jsonl_notify,
                        &dead_marker_notify,
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
                    let sentinel_ready =
                        !matches!(
                            watcher_provider,
                            crate::services::provider::ProviderKind::Claude
                        ) && jsonl_tail_contains_ready_for_input_sentinel(&output_path);
                    let should_probe_ready = sentinel_ready
                        || last_ready_probe_at
                            .map(|last| {
                                now.duration_since(last) >= READY_FOR_INPUT_IDLE_PROBE_INTERVAL
                            })
                            .unwrap_or(true);
                    if should_probe_ready {
                        last_ready_probe_at = Some(now);
                        let ready_for_input = if sentinel_ready {
                            true
                        } else {
                            tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                tokio::task::spawn_blocking({
                                    let name = tmux_session_name.clone();
                                    let provider = watcher_provider.clone();
                                    let path = output_path.clone();
                                    let offset = current_offset;
                                    move || {
                                        watcher_session_ready_for_input(
                                            &name, &provider, &path, offset,
                                        )
                                    }
                                }),
                            )
                            .await
                            .unwrap_or(Ok(false))
                            .unwrap_or(false)
                        };
                        if soft_terminal_seen_at.is_some()
                            && ready_for_input
                            && !full_response.trim().is_empty()
                        {
                            terminal_kind.get_or_insert(WatcherTerminalKind::SoftStopHookSummary);
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] 👁 watcher committed soft stop_hook_summary after ready-for-input for {tmux_session_name} at offset {current_offset}"
                            );
                            break;
                        }
                        let post_work_observed = watcher_has_post_work_ready_evidence(
                            &full_response,
                            &tool_state,
                            task_notification_kind,
                        );
                        match watcher_ready_for_input_turn_completed(
                            &mut ready_for_input_tracker,
                            data_start_offset,
                            current_offset,
                            ready_for_input,
                            post_work_observed,
                            now,
                        ) {
                            crate::services::provider::ReadyForInputIdleState::None => {}
                            crate::services::provider::ReadyForInputIdleState::FreshIdle => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] 👁 watcher observed fresh ready-for-input idle for {tmux_session_name} at offset {current_offset}; leaving session untouched"
                                );
                                fresh_ready_for_input_idle = true;
                                break;
                            }
                            crate::services::provider::ReadyForInputIdleState::PostWorkIdleTimeout => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                let stall_inflight_snapshot = crate::services::discord::inflight::load_inflight_state(
                                    &watcher_provider,
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
                                ready_for_input_stall_inflight_snapshot = stall_inflight_snapshot;
                                if let Some(dispatch_id) = dispatch_id {
                                    ready_for_input_stall_dispatch_id = Some(dispatch_id);
                                    ready_for_input_failure_notice = Some(format!(
                                        "⚠️ 작업 후 `Ready for input` 상태에서 멈춰 dispatch를 실패 처리합니다.\n사유: {READY_FOR_INPUT_STUCK_REASON}"
                                    ));
                                } else {
                                    tracing::info!(
                                        "  [{ts}] 👁 watcher detected post-work Ready-for-input idle for {} with no dispatch; suppressing dispatch-failure notice",
                                        tmux_session_name
                                    );
                                }
                                watcher_handle_no_dispatch_post_work_idle_body(
                                    &mut full_response,
                                    &mut terminal_kind,
                                    ready_for_input_stall_inflight_snapshot.as_ref(),
                                    ready_for_input_stall_dispatch_id.is_some(),
                                    &tmux_session_name,
                                    fresh_assistant_text_seen,
                                    current_offset,
                                );
                                break;
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
                            "  [{ts}] 👁 watcher committed soft stop_hook_summary after debounce for {tmux_session_name} at offset {current_offset}"
                        );
                        break;
                    }
                }
                _ => {
                    // #2441 (H1) — notify-backed wake-up for the
                    // inner-loop read-error retry path.
                    sleep_or_jsonl_event(
                        tokio::time::Duration::from_millis(200),
                        &jsonl_notify,
                        &dead_marker_notify,
                    )
                    .await;
                }
            }

            // A resumed reader may consume the complete remaining turn in one
            // read. Hand its terminal to the existing receipt/lease path instead
            // of awaiting a streaming preview that races the terminal sink.
            if continuation.is_some() && found_result {
                break;
            }

            // Check for stale session error during streaming — abort relay immediately.
            // Only structured error/result events can trip this flag.
            if stale_resume_detected {
                break;
            }

            match update_streaming_status_tick(
                &StreamingStatusTickContext {
                    http: &http,
                    shared: &shared,
                    channel_id,
                    watcher_provider: &watcher_provider,
                    tmux_session_name: &tmux_session_name,
                    output_path: &output_path,
                    turn_delivered: &turn_delivered,
                },
                StreamingStatusTickTurn {
                    data_start_offset,
                    current_offset,
                    full_response: &full_response,
                    tool_state: &tool_state,
                    task_notification_kind,
                    status_panel_started_at,
                    single_message_panel_footer_mode,
                    restored_injected_prompt_message_id,
                },
                &mut StreamingRenderState {
                    last_status_update: &mut last_status_update,
                    spin_idx: &mut spin_idx,
                    placeholder_msg_id: &mut placeholder_msg_id,
                    placeholder_from_restored_inflight: &mut placeholder_from_restored_inflight,
                    last_edit_text: &mut last_edit_text,
                    response_sent_offset: &mut response_sent_offset,
                    watcher_streaming_rollover_frozen_msg_ids:
                        &mut watcher_streaming_rollover_frozen_msg_ids,
                },
                &mut StatusPanelState {
                    status_panel_msg_id: &mut status_panel_msg_id,
                    last_status_panel_text: &mut last_status_panel_text,
                },
                &mut StreamingSuppressState {
                    turn_is_external_input_for_session: &mut turn_is_external_input_for_session,
                    turn_identity_for_panel: &mut turn_identity_for_panel,
                    streaming_suppressed_by_recent_stop: &mut streaming_suppressed_by_recent_stop,
                    streaming_suppressed_by_missing_inflight:
                        &mut streaming_suppressed_by_missing_inflight,
                    active_stream_inflight_reacquire_logged:
                        &mut active_stream_inflight_reacquire_logged,
                },
                &mut PanelGenerationState {
                    this_turn_status_panel_generation: &mut this_turn_status_panel_generation,
                },
            )
            .await
            {
                StreamingStatusTickOutcome::ContinueStreamingLoop => continue,
                StreamingStatusTickOutcome::Fallthrough => {}
            }
        }
        active_read_state = Some(ActiveReadState {
            last_output_at,
            tmux_death_observed,
            ready_for_input_failure_notice,
            ready_for_input_stall_dispatch_id,
            ready_for_input_stall_inflight_snapshot,
            fresh_ready_for_input_idle,
        });
    }

    commit_collect_state!();
    let source_authority = WatcherSourceAuthority {
        source_stamp: None,
        ..source_authority
    };
    return CollectOutcome::Fallthrough(CollectedTurnStream {
        turn_data_start_offset,
        source_authority,
        split_trailing_turn_follows,
        state,
        restored_response_seed,
        full_response,
        tool_state,
        placeholder_msg_id,
        placeholder_from_restored_inflight,
        status_panel_msg_id,
        single_message_panel_footer_mode,
        startup_inflight_snapshot,
        completion_actor,
        this_turn_status_panel_generation,
        turn_is_external_input_for_session,
        turn_identity_for_panel,
        status_panel_started_at,
        last_status_panel_text,
        last_edit_text,
        response_sent_offset,
        watcher_streaming_rollover_frozen_msg_ids,
        finish_mailbox_on_completion,
        monitor_auto_turn_claimed,
        monitor_auto_turn_deferred,
        monitor_auto_turn_finished,
        monitor_auto_turn_synthetic_msg_id,
        monitor_auto_turn_ledger_generation,
        completion_footer_terminal_target,
        session_bound_relay_turn_fully_mirrored,
        session_bound_relay_turn_first_forwarded_sequence,
        found_result,
        terminal_kind,
        terminal_evidence_offset,
        is_prompt_too_long,
        stale_resume_detected,
        task_notification_kind,
        task_notification_context,
        assistant_text_seen,
        fresh_assistant_text_seen,
        was_paused,
        active_read_state,
        soft_terminal_seen_at,
        auto_compaction_lifecycle_attempted,
        monitor_auto_turn_preamble_injected,
    });
}
