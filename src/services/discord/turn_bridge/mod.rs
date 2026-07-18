mod activity_heartbeat;
mod bridge_latency_spans;
mod cancel_finalize_policy;
mod chunk_compose;
mod completion_guard;
mod completion_postlude;
mod context_window;
#[cfg(unix)]
mod early_tui_completion;
mod finalize_epilogue;
mod followup_requeue;
mod guards;
mod headless_delivery;
mod memory_lifecycle;
mod output_lifecycle;
mod panel_lifecycle;
mod post_loop_finalize;
mod recall_feedback;
pub(in crate::services::discord) mod recovery_text;
mod retry_state;
mod runtime_handoff_loop;
mod single_message_footer;
mod skill_usage;
mod stale_resume;
mod status_panel;
mod stream_loop;
mod stream_receiver;
mod stream_tick;
mod streaming_edit_text;
mod task_notification_lifecycle;
mod terminal_controller_cutover;
mod terminal_delivery;
mod terminal_outcome_delivery;
mod thinking;
mod tmux_runtime;
mod turn_analytics;
// #3805 P2 (PR-B): two-message sink creation order (answer-first, panel below)
// + the pure generation-epoch staleness guard. Isolated sibling so the EXTREME
// turn_bridge/mod.rs giant and the 700-capped status_panel.rs stay lean; the
// call sites here and in single_message_footer.rs are thin.
mod two_message_panel;
mod voice_completion;
mod watcher_handoff;
mod watcher_orphan_cleanup;
// #3479: the pure response-delivery + transcript-event helpers (the transcript
// event-recording filter, the response-after-offset slice, the terminal delivery
// sanitization+fallback builder, and the full-terminal-replay predicate) moved
// verbatim to a capped sibling module. All four are `pub(super)` and re-imported
// below so this parent's call sites (and the inline test modules) stay
// byte-identical; deps are reached via `use super::*;` (the two discord-level
// `super::` refs become `super::super::` from the child).
mod response_delivery;
use super::gateway::TurnGateway;
use super::restart_report::{RestartCompletionReport, clear_restart_report, save_restart_report};
use super::turn_view_reconciler::{
    note_intake_turn_cleared_via_shared as tv_clear,
    note_intake_turn_completed_via_shared as tv_done,
    note_intake_turn_failed_via_shared as tv_fail, note_intake_turn_stopped_via_shared as tv_stop,
};
use super::*;
use crate::db::session_observability::{
    BackgroundChildSpawn, close_background_child_pg, insert_background_child_pg,
    mark_session_tool_use_pg,
};
use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::{StatusEvent, TaskNotificationKind};
use crate::services::memory::{
    CaptureRequest, TokenUsage, resolve_memory_role_id, resolve_memory_session_id,
};
use crate::services::observability::session_inventory::{
    format_child_inventory_progress, load_child_inventory_by_parent_key_pg,
};
use crate::services::provider::cancel_requested;
use output_lifecycle::BridgeOutputOwner;
pub(super) use panel_lifecycle::record_placeholder_live_event;
use panel_lifecycle::{
    child_progress_line, ensure_active_placeholder_card, first_request_line,
    refresh_session_panel_line_from_lifecycle, refresh_task_panel_line_from_dispatch,
};
use response_delivery::{
    done_result_requires_full_terminal_replay, push_transcript_event,
    response_portion_after_offset, terminal_delivery_response_after_offset,
};
use std::collections::VecDeque;
// Re-exports for pub(super) items used by sibling modules in the discord package
pub(super) use activity_heartbeat::maybe_refresh_active_turn_activity_heartbeat;
use bridge_latency_spans::BridgeLatencySpans;
pub(super) use cancel_finalize_policy::sync_inflight_restart_mode_from_cancel;
pub(super) use cancel_finalize_policy::{
    classify_turn_finished_dispatch_kind, is_done_setting_terminal_frame,
    resolve_bridge_owner_channel, should_finalize_cancel_after_recv,
    should_record_final_turn_transcript, should_suppress_headless_delivery_for_cancel,
};
pub(crate) use completion_guard::build_work_dispatch_completion_result;
pub(super) use completion_guard::{
    fail_dispatch_auth_expired, fail_dispatch_tmux_session_died, fail_dispatch_with_retry,
    guard_review_dispatch_completion, queue_dispatch_followup_with_handles,
    runtime_db_fallback_complete_with_result, streaming_final_complete_dispatch_with_result,
};
pub(super) use recovery_text::{
    auto_retry_with_history, release_retry_pending, take_session_retry_context_for_turn_with_audit,
};
use single_message_footer::*;
pub(super) use stale_resume::result_event_has_stale_resume_error;
pub(in crate::services::discord) use status_panel::{
    complete_status_panel_v2_with_http, normalize_status_panel_message_id,
};
// #3805 P2 (PR-C): the ONE generation staleness rule shared by the sink (here)
// and the tmux WATCHER completion guard, so both paths supersede a stale
// status edit by the SAME epoch semantics (parity).
pub(super) use stream_receiver::{
    StreamMessageReceiverAdapter, spawn_stream_message_receiver_adapter,
    turn_bridge_stream_wait_duration,
};
pub(super) use streaming_edit_text::{
    CLAUDE_TUI_FOLLOWUP_REQUEUE_DELIVERY_NOTICE, bridge_claude_tui_followup_requeue_prompt_error,
    bridge_streaming_edit_gate_open, bridge_streaming_rollover_should_skip,
    bridge_tui_transport_error_should_skip_quiescence, build_turn_bridge_streaming_edit_text,
    claude_tui_followup_requeue_streaming_aware, claude_tui_followup_same_input_occupies_pane,
};
pub(super) use task_notification_lifecycle::{
    close_all_tracked_background_children, close_next_tracked_background_child,
    merge_task_notification_kind, release_task_notification_kind,
    task_notification_closes_background_child,
};
pub(crate) use tmux_runtime::TmuxCleanupPolicy;
pub(super) use tmux_runtime::bind_cancel_token_tmux_runtime;
pub(super) use tmux_runtime::cancel_active_token;
pub(super) use tmux_runtime::cancel_token_has_tmux_session;
pub(super) use tmux_runtime::handoff_interrupted_message;
pub(super) use tmux_runtime::stale_inflight_message;
pub(super) use tmux_runtime::stop_active_turn;
pub(super) use tmux_runtime::tmux_generation_file_mtime_ns;
pub(in crate::services::discord) use two_message_panel::{
    two_message_should_reanchor_panel_on_rollover, two_message_status_edit_generation_is_stale,
};
pub(super) use watcher_orphan_cleanup::{
    cleanup_or_preserve_watcher_orphan_spinner,
    should_delete_bridge_created_watcher_orphan_response,
};
// Re-export pub(crate) items
pub(crate) use tmux_runtime::tmux_runtime_paths;
// Items used by spawn_turn_bridge from submodules
use super::watcher_lifecycle_decision::should_resume_watcher_after_turn;
use crate::db::session_status::{AWAITING_BG, IDLE, TURN_ACTIVE};
use completion_guard::complete_work_dispatch_on_turn_end;
use context_window::{apply_context_token_update, persisted_context_tokens, resolve_done_response};
use guards::{CompletionGuard, InflightCleanupGuard};
use headless_delivery::{
    SYNTHETIC_HEADLESS_RECOVERY_PLACEHOLDER_ID,
    cleanup_headless_streaming_placeholder_after_delivery, enqueue_headless_delivery,
    is_synthetic_headless_message_id,
};
use memory_lifecycle::{
    BackgroundMemoryTask, BackgroundMemoryTaskKind, observe_background_memory_tasks,
    optional_metric_token_fields, plan_turn_end_memory, spawn_memory_capture_task,
};
pub(in crate::services::discord) use memory_lifecycle::{
    spawn_memory_reflect_task, take_memento_reflect_request,
};
use recall_feedback::{
    analyze_recall_feedback_turn, build_voluntary_feedback_reminder, reminder_transcript_event,
    transcript_contains_explicit_memento_tool_call,
};
pub(super) use retry_state::spawn_retry_with_history_with_release;
use retry_state::{
    bridge_confirmed_response_sent_offset_seed, bridge_should_reclaim_relay_from_missing_watcher,
    clear_local_session_state, handle_gemini_retry_boundary, reset_session_for_auto_retry,
    rewind_and_persist_delivery_on_reclaim, sync_response_delivery_state,
    sync_terminal_error_delivery_state_for_bridge_owner,
};
use skill_usage::record_skill_usage_from_tool_use;
use sqlx::Row;
use stale_resume::{
    output_file_has_stale_resume_error_after_offset, stream_error_has_stale_resume_error,
    stream_error_requires_terminal_session_reset,
};
use status_panel::{
    bridge_epilogue_identity_guards_inflight_clear, migrate_separate_status_panel_to_footer,
    record_status_panel_events, should_open_long_running_placeholder_controller,
    status_panel_completion_ready_after_terminal_body, status_panel_message_id_for_turn,
};
use terminal_delivery::{
    BridgeLeaseAcquire, bridge_delivery_lease_for_inflight, bridge_delivery_lease_key_for_inflight,
    bridge_epilogue_clears_inflight, bridge_epilogue_marks_watcher_delivered,
    bridge_epilogue_skip_save_is_identity_guarded, empty_sink_commits_fully_consumed_response,
    empty_sink_preserves_retry, mirror_frozen_prefix_ids, send_ordered_long_terminal_response,
    should_complete_work_dispatch_after_terminal_delivery,
    should_fail_dispatch_after_terminal_delivery, silent_turn_skip_marks_committed,
    terminal_delivery_should_send_new_chunks, turn_bridge_replace_outcome_committed,
    warn_preserved_uncommitted as td_warn,
};
use tmux_runtime::is_dcserver_restart_command;
pub(super) use turn_analytics::persist_turn_analytics_row_with_handles;
use turn_analytics::{
    assert_response_sent_offset_progress, discord_turn_id, emit_turn_quality_event,
    record_turn_bridge_invariant, resolve_exact_completion_usage, total_model_input_tokens,
    turn_duration_ms,
};
use voice_completion::{
    json_any_true_flag, resolve_voice_turn_link_for_playback, voice_background_completion_target,
};
use watcher_handoff::{live_watcher_registered_for_relay, should_delegate_bridge_relay_to_watcher};
pub(super) struct TurnBridgeContext {
    pub(super) provider: ProviderKind,
    pub(super) gateway: Arc<dyn TurnGateway>,
    pub(super) channel_id: ChannelId,
    /// `None` for a recovery turn with no anchored Discord user message
    /// (user_msg_id == 0, e.g. a TUI-direct turn). All Discord-message side
    /// effects keyed on it (reactions, analytics row, voice link) are skipped.
    pub(super) user_msg_id: Option<MessageId>,
    pub(super) user_text_owned: String,
    pub(super) request_owner_name: String,
    pub(super) role_binding: Option<RoleBinding>,
    pub(super) adk_session_key: Option<String>,
    pub(super) adk_session_name: Option<String>,
    pub(super) adk_session_info: Option<String>,
    pub(super) adk_cwd: Option<String>,
    pub(super) dispatch_id: Option<String>,
    pub(super) dispatch_kind: Option<String>,
    pub(super) memory_recall_usage: TokenUsage,
    pub(super) context_window_tokens: u64,
    pub(super) context_compact_percent: u64,
    /// `None` for a recovery turn that never anchored a Discord placeholder
    /// (current_msg_id == 0, e.g. a TUI-direct turn). The bridge then creates a
    /// fresh placeholder on first output instead of editing a nonexistent one.
    pub(super) current_msg_id: Option<MessageId>,
    pub(super) response_sent_offset: usize,
    pub(super) full_response: String,
    pub(super) tmux_last_offset: Option<u64>,
    pub(super) new_session_id: Option<String>,
    pub(super) defer_watcher_resume: bool,
    /// Reuse the persisted V2 status panel only when resuming the same
    /// in-flight turn. Fresh turns must allocate a new panel near the new
    /// response instead of editing an old panel buried in scrollback.
    pub(super) reuse_status_panel_message: bool,
    pub(super) completion_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// `true` ONLY at the two TUI external-input idle callers. Default `false`
    /// for every other bridge caller; used by footer/chrome decisions that need
    /// the origin without a `request_owner_name` string compare.
    pub(super) is_external_input_tui_direct: bool,
    pub(super) inflight_state: InflightTurnState,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WatcherHandoffClaimOutcome {
    None,
    ReusedExisting,
    Spawned,
}
// Shared by the bridge task body below and the extracted stream_loop.rs
// (#4230 S6) — must live at module scope so both resolve them.
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LIVE_LONG_RUN_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
pub(super) fn spawn_turn_bridge(
    shared_owned: Arc<SharedData>,
    cancel_token: Arc<CancelToken>,
    rx: mpsc::Receiver<StreamMessage>,
    bridge: TurnBridgeContext,
) {
    use tracing::Instrument;
    let bridge_turn_id = discord_turn_id(
        &bridge.provider,
        bridge.channel_id,
        bridge.user_msg_id,
        bridge.adk_session_key.as_deref(),
        bridge.inflight_state.turn_start_offset,
    );
    // Attach the span via `.instrument(..)` on the async block instead of
    // holding a sync `Span::enter()` guard across `.await`. The sync-guard
    // pattern leaks the span into unrelated tasks scheduled on the same
    // thread (tokio + tracing task propagation), which caused logs to be
    // attributed to the wrong channel_id (see retired issue #901).
    let bridge_span = tracing::info_span!(
        "discord_turn_bridge",
        channel_id = bridge.channel_id.get(),
        provider = bridge.provider.as_str(),
        dispatch_id = tracing::field::debug(bridge.dispatch_id.as_deref()),
        session_key = tracing::field::debug(bridge.adk_session_key.as_deref()),
        turn_id = %bridge_turn_id,
    );
    super::task_supervisor::spawn_observed("discord_turn_bridge", async move {
        let mut rx = spawn_stream_message_receiver_adapter(rx);
        let channel_id = bridge.channel_id;
        let provider = bridge.provider.clone();
        let gateway = bridge.gateway.clone();
        let user_msg_id = bridge.user_msg_id;
        let turn_id = discord_turn_id(
            &provider,
            bridge.channel_id,
            bridge.user_msg_id,
            bridge.adk_session_key.as_deref(),
            bridge.inflight_state.turn_start_offset,
        );
        let user_text_owned = bridge.user_text_owned.clone();
        let request_owner_name = bridge.request_owner_name.clone();
        let role_binding = bridge.role_binding.clone();
        let adk_session_key = bridge.adk_session_key.clone();
        let adk_session_name = bridge.adk_session_name.clone();
        let adk_session_info = bridge.adk_session_info.clone();
        let adk_cwd = bridge.adk_cwd.clone();
        let dispatch_id = bridge.dispatch_id.clone();
        let dispatch_kind = bridge.dispatch_kind.clone();
        let context_window_tokens = bridge.context_window_tokens;
        let context_compact_percent = bridge.context_compact_percent;
        let voice_progress_playback_channel_id =
            if bridge.inflight_state.source == crate::dispatch::Source::Voice {
                resolve_voice_turn_link_for_playback(
                    shared_owned.pg_pool.as_ref(),
                    dispatch_id.as_deref(),
                    user_msg_id,
                    Some(&turn_id),
                )
                .await
                .and_then(|link| {
                    (link.background_channel_id == channel_id.get())
                        .then(|| ChannelId::new(link.voice_channel_id))
                })
            } else {
                None
            };
        let mut full_response = bridge.full_response.clone();
        let mut terminal_empty_response_notice: Option<String> = None;
        let mut last_edit_text = String::new();
        let mut done = false;
        let mut cancelled = false;
        let mut rx_disconnected = false;
        let mut current_tool_line: Option<String> = bridge.inflight_state.current_tool_line.clone();
        let mut prev_tool_status: Option<String> = bridge.inflight_state.prev_tool_status.clone();
        let mut last_tool_name: Option<String> = bridge.inflight_state.last_tool_name.clone();
        let mut last_tool_summary: Option<String> = bridge.inflight_state.last_tool_summary.clone();
        let mut accumulated_input_tokens: u64 = 0;
        let mut accumulated_cache_create_tokens: u64 = 0;
        let mut accumulated_cache_read_tokens: u64 = 0;
        let mut accumulated_output_tokens: u64 = 0;
        let mut accumulated_memory_input_tokens: u64 = bridge.memory_recall_usage.input_tokens;
        let mut accumulated_memory_output_tokens: u64 = bridge.memory_recall_usage.output_tokens;
        let mut spin_idx: usize = 0;
        let mut restart_followup_pending = false;
        let mut any_tool_used = bridge.inflight_state.any_tool_used;
        let mut has_post_tool_text = bridge.inflight_state.has_post_tool_text;
        let mut tmux_handed_off = false;
        let initial_relay_owner_kind = bridge.inflight_state.effective_relay_owner_kind();
        // #2838 (relay-stability P0-1): count turns that begin relay with an
        // Unknown owner kind (root cause #3 — ownership not cleanly assigned
        // across the three relay-launch paths). Unknown is treated as a live
        // external owner just below, so a phantom owner can make the bridge skip
        // its own delivery (no-emit); this quantifies how often that ambiguity
        // actually occurs in production.
        if matches!(
            initial_relay_owner_kind,
            super::inflight::RelayOwnerKind::Unknown
        ) {
            crate::services::observability::metrics::record_relay_owner_unknown(
                channel_id.get(),
                provider.as_str(),
            );
        }
        // #3041 P1-2 (codex P1-a): resolve the AUTHORITATIVE owner channel for
        // this turn's tmux session BEFORE the watcher availability check and the
        // bridge delivery-lease acquisition. A RECOVERED/restored bridge that
        // REUSES an existing watcher (without going through the
        // `TmuxReady`/`RuntimeReady` claim paths, which set
        // `watcher_owner_channel_id = claim.owner_channel_id()`) would otherwise
        // keep `watcher_owner_channel_id == channel_id` (the bridge's dispatch
        // channel Y) while the reused watcher leases + advances on its owner
        // channel X — different cells, so both could acquire and deliver
        // (duplicate). Resolving the session's owner channel here makes EVERY
        // path (normal, claim, recovered/restored) key the availability check
        // AND the lease acquire+advance on the SAME channel the watcher uses.
        // When no reused watcher owns the session, this falls back to
        // `channel_id` (the bridge owns its own channel). The claim paths below
        // still re-assert `claim.owner_channel_id()` (which equals this resolved
        // value for the same session) so live truth always wins.
        let resolved_watcher_owner_channel_id = resolve_bridge_owner_channel(
            &shared_owned.tmux_watchers,
            bridge.inflight_state.tmux_session_name.as_deref(),
            channel_id,
        );
        let mut watcher_owns_assistant_relay =
            matches!(initial_relay_owner_kind, super::inflight::RelayOwnerKind::Watcher);
        let mut watcher_relay_available_for_turn = watcher_owns_assistant_relay
            && live_watcher_registered_for_relay(
                shared_owned.as_ref(),
                resolved_watcher_owner_channel_id,
            );
        let mut watcher_handoff_claim_outcome = WatcherHandoffClaimOutcome::None;
        // Durable recovery must honor typed non-bridge owners too. `Unknown`
        // is treated like a live external owner so future relay variants do
        // not fail open and duplicate bridge-owned Discord delivery.
        let mut standby_relay_owns_output = matches!(
            initial_relay_owner_kind,
            super::inflight::RelayOwnerKind::StandbyRelay
                | super::inflight::RelayOwnerKind::SessionBoundRelay
                | super::inflight::RelayOwnerKind::Unknown
        );
        // #1255 live-turn long-running tool placeholder card. Capture the last
        // non-empty assistant line for the placeholder `요약` slot, then reset
        // on tool result/completion so stale text never leaks. While Monitor /
        // background-Bash work is mid-flight, `long_running_placeholder_active`
        // stores the key so ToolResult/Done/cancel/abort hit the same handle.
        let mut last_assistant_text_line: Option<String> = None;
        // Pair the active key with the input snapshot, close-trigger kind, and
        // `ack_consumed`: rollover retargets to the new `current_msg_id`, the
        // trigger separates Monitor closes from background acks, and
        // `ack_consumed` blocks unrelated later ToolResults from closing the
        // still-running background card (codex round-6 P2 on #1308).
        let mut long_running_placeholder_active: Option<(
            super::placeholder_controller::PlaceholderKey,
            super::placeholder_controller::PlaceholderActiveInput,
            super::formatting::LongRunningCloseTrigger,
            bool, // ack_consumed
        )> = None;
        let mut active_background_child_session_ids: Vec<i64> = Vec::new();
        let (status_panel_started_at, footer_owner) = make_owner_now(user_msg_id);
        let single_message_panel_footer_mode =
            bridge_single_message_panel_footer_enabled(shared_owned.ui.status_panel_v2_enabled);
        if shared_owned.ui.placeholder_live_events_enabled || shared_owned.ui.status_panel_v2_enabled {
            if single_message_panel_footer_mode {
                supersede_bridge_footer(shared_owned.as_ref(), channel_id, footer_owner).await;
                shared_owned
                    .ui
                    .placeholder_live_events
                    .clear_channel_preserving_footer_residuals(channel_id);
            } else {
                shared_owned.ui.placeholder_live_events.clear_channel(channel_id);
            }
        }
        let mut transport_error = false;
        let mut api_friction_reports = Vec::new();
        let mut transcript_events = Vec::<SessionTranscriptEvent>::new();
        let mut resume_failure_detected = false;
        // #2451: Init is the authoritative resume-success witness; elapsed
        // time is only a backstop. Snapshot prior session state before any reset.
        let mut session_handshake_seen = false;
        let had_prior_session_id_at_turn_start = {
            let data = shared_owned.core.lock().await;
            data.sessions
                .get(&channel_id)
                .and_then(|s| s.session_id.as_ref())
                .is_some()
        };
        let mut terminal_session_reset_required = false;
        let mut recovery_retry = false;
        let mut last_adk_heartbeat = std::time::Instant::now();
        let mut pending_stream_messages: VecDeque<StreamMessage> = VecDeque::new();
        // #3084: pair tool_result → tool_use by provider tool-use id when the
        // backend supplies one. A long-running Task subagent returns its result
        // after intervening short foreground tools, so the old FIFO queue
        // popped the wrong tool name and the real subagent's SubagentEnd never
        // fired (ghost "running" marker). The HashMap pairs precisely by id;
        // the VecDeque remains the fallback for backends with no tool-use id.
        let mut pending_status_tool_results: VecDeque<String> = VecDeque::new();
        let mut pending_status_tool_results_by_id: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // codex round-8 P1 on PR #1308: while a long-running placeholder is
        // active, bump the inflight file's mtime so the sweeper sees the turn
        // as alive. Without this, a healthy 5+ minute background tool would
        // exceed `ABANDON_THRESHOLD_SECS` and the sweeper would cancel it.
        let mut last_inflight_long_run_heartbeat = std::time::Instant::now();
        let mut last_activity_heartbeat_at: Option<std::time::Instant> = None;
        let mut terminal_control_ready_observed = false;
        let mut terminal_control_drain_until: Option<std::time::Instant> = None;
        let mut bridge_created_response_placeholder_msg_id: Option<MessageId> = None;
        // Recovery turns without an anchored placeholder create one up front;
        // on failure the synthetic sentinel lets the first streaming edit retry.
        let mut current_msg_id = match bridge.current_msg_id {
            Some(id) => id,
            None => {
                let placeholder =
                    super::formatting::build_processing_status_block(SPINNER[0]).to_string();
                match gateway.send_message(channel_id, &placeholder).await {
                    Ok(created) => {
                        bridge_created_response_placeholder_msg_id = Some(created);
                        created
                    }
                    Err(error) => {
                        tracing::warn!(
                            channel_id = channel_id.get(),
                            "[turn_bridge] recovery turn has no anchored placeholder and creating one failed: {error}; continuing — streaming will retry placeholder creation"
                        );
                        // No placeholder yet. Use a synthetic-headless sentinel
                        // so the established `is_synthetic_headless_message_id`
                        // path (which already means "no real Discord message to
                        // edit") drives placeholder (re)creation on first output
                        // instead of editing a nonexistent message.
                        MessageId::new(SYNTHETIC_HEADLESS_RECOVERY_PLACEHOLDER_ID)
                    }
                }
            }
        };
        let mut response_sent_offset = bridge.response_sent_offset;
        let mut bridge_confirmed_response_sent_offset =
            bridge_confirmed_response_sent_offset_seed(initial_relay_owner_kind, response_sent_offset);
        let mut streamed_assistant_text_this_turn = false;
        let mut streaming_rollover_frozen_msg_ids: Vec<MessageId> = Vec::new();
        let mut terminal_full_replay_cleanup_msg_ids: Vec<MessageId> = Vec::new();
        let mut tmux_last_offset = bridge.tmux_last_offset;
        // #3041 P1-2 (codex P1-a): seed from the session's AUTHORITATIVE owner
        // channel (resolved above) so a recovered/restored bridge reusing an
        // existing watcher leases on the SAME cell the watcher leases+advances
        // on — not its dispatch `channel_id`. The claim paths below still
        // overwrite this with `claim.owner_channel_id()` (equal for the same
        // session) when they run.
        let mut watcher_owner_channel_id = resolved_watcher_owner_channel_id;
        let mut new_session_id = bridge.new_session_id.clone();
        let mut new_raw_provider_session_id: Option<String> = None;
        let defer_watcher_resume = bridge.defer_watcher_resume;
        let is_external_input_tui_direct = bridge.is_external_input_tui_direct;
        let _completion_guard = CompletionGuard {
            tx: bridge.completion_tx,
            broadcaster: shared_owned.inflight_signals.clone(),
            channel_id,
            turn_id: bridge.inflight_state.effective_finalizer_turn_id(),
        };
        let mut inflight_guard = InflightCleanupGuard {
            provider: Some(provider.clone()),
            channel_id: channel_id.get(),
            user_msg_id: user_msg_id.map(|id| id.get()).unwrap_or(0),
            token_hash: shared_owned.token_hash.clone(),
        };
        let mut inflight_state = bridge.inflight_state.clone();
        inflight_state.set_watcher_owner_channel_id(resolved_watcher_owner_channel_id.get());
        // Codex P2: a no-anchor recovery turn (bridge.current_msg_id == None)
        // had a fresh placeholder created above into the working `current_msg_id`,
        // but the cloned inflight still carries `current_msg_id == 0`. Mirror the
        // real id back NOW so the `save_inflight_state` below persists it; without
        // this, a restart before the first streaming edit would see id 0 again,
        // re-create a placeholder, and orphan the spinner we just sent. The
        // synthetic-headless fallback (creation failed) is intentionally NOT
        // persisted — it is not a real Discord message and the streaming path
        // re-creates a placeholder on first output.
        if bridge.current_msg_id.is_none()
            && !is_synthetic_headless_message_id(current_msg_id)
            && inflight_state.current_msg_id == 0
        {
            inflight_state.current_msg_id = current_msg_id.get();
        }
        let mut last_status_edit = tokio::time::Instant::now();
        // #3813 Phase 1b fast-lane: the first non-empty assistant text chunk may
        // bypass the status interval once, then normal throttling resumes.
        let mut first_answer_relayed = false;
        let status_interval = super::status_update_interval();
        let mut last_session_panel_lifecycle_refresh = tokio::time::Instant::now() - status_interval;
        let mut status_panel_msg_id = status_panel_message_id_for_turn(
            &mut inflight_state,
            bridge.reuse_status_panel_message,
        );
        if single_message_panel_footer_mode {
            // #3560 codex review: a turn that created a *separate* status panel
            // under default-OFF can be resumed here under footer mode. Clearing
            // the handle alone would orphan that Discord message, so reconcile
            // it (edit to a migration notice) before dropping it.
            migrate_separate_status_panel_to_footer(
                gateway.as_ref(),
                channel_id,
                &mut inflight_state,
            )
            .await;
            status_panel_msg_id = None;
        }
        let mut last_status_panel_text = String::new();
        let mut status_panel_dirty = shared_owned.ui.status_panel_v2_enabled;
        let mut last_status_panel_edit = tokio::time::Instant::now() - status_interval;
        let turn_start = std::time::Instant::now();
        // #3813 AC#1 tail: bridge-side latency spans (observation-only), anchored
        // on `turn_start` above. See bridge_latency_spans.rs for the invariants.
        let mut bridge_spans = BridgeLatencySpans::starting_at(turn_start);
        // #3805 P2 (PR-B): this turn's status-panel epoch. Seeded from the pinned
        // inflight snapshot and threaded through the two-message create (which
        // bumps it) and the terminal completion edit (which proves it against the
        // on-disk epoch). Inert on the default-OFF path (stays 0).
        let mut status_panel_generation = inflight_state.status_panel_generation;

        maybe_create_bridge_separate_status_panel_response(
            shared_owned.ui.two_message_panel_enabled,
            single_message_panel_footer_mode,
            shared_owned.ui.status_panel_v2_enabled,
            gateway.as_ref(),
            channel_id,
            SPINNER[0],
            &mut current_msg_id,
            &mut status_panel_msg_id,
            &mut bridge_created_response_placeholder_msg_id,
            &mut last_edit_text,
            &mut inflight_state,
            &mut status_panel_generation,
            response_sent_offset,
            &full_response,
            &mut status_panel_dirty,
            &shared_owned,
            &provider,
        )
        .await;

        if shared_owned.ui.status_panel_v2_enabled
            && let Some(dispatch_id) = dispatch_id.as_deref()
        {
            status_panel_dirty |= refresh_task_panel_line_from_dispatch(
                shared_owned.as_ref(),
                channel_id,
                dispatch_id,
            )
            .await;
        }

        // codex round-5 P2 on PR #1308: a dcserver restart resumed an inflight
        // turn whose persisted state still flags `long_running_placeholder_active`.
        // The in-memory controller is empty here and the original
        // `PlaceholderActiveInput` snapshot was never persisted, so we cannot
        // reconstruct the Active entry. Edit the stale 🔄 card to a generic
        // resumed-aborted notice and clear the flag so subsequent streaming /
        // sweeper logic treats this turn as a fresh resume.
        if inflight_state.long_running_placeholder_active {
            let resumed_notice =
                "🛑 백그라운드 카드 종료됨 — 서버 재시작으로 이전 흐름이 끊겨 새 응답으로 이어집니다.";
            let _ = gateway
                .edit_message(channel_id, current_msg_id, resumed_notice)
                .await;
            inflight_state.long_running_placeholder_active = false;
        }

        if let Err(error) = save_inflight_state(&inflight_state) {
            tracing::warn!(
                "[turn_bridge] failed to persist inflight state in channel {}: {}",
                channel_id,
                error
            );
        }
        crate::services::observability::emit_turn_started(
            provider.as_str(),
            channel_id.get(),
            dispatch_id.as_deref(),
            adk_session_key.as_deref(),
            Some(turn_id.as_str()),
        );
        emit_turn_quality_event(
            &provider,
            channel_id,
            dispatch_id.as_deref(),
            adk_session_key.as_deref(),
            turn_id.as_str(),
            role_binding.as_ref(),
            "turn_start",
            serde_json::json!({
                "user_msg_id": user_msg_id.map(|id| id.get()),
                "request_owner_name": request_owner_name.as_str(),
            }),
        );

        let stream_loop_output = stream_loop::run_stream_loop(
            stream_loop::StreamLoopContext {
                shared_owned: shared_owned.clone(),
                gateway: gateway.clone(),
                channel_id,
                provider: provider.clone(),
                cancel_token: cancel_token.clone(),
                user_text_owned: user_text_owned.clone(),
                request_owner_name: request_owner_name.clone(),
                adk_session_key: adk_session_key.clone(),
                adk_session_name: adk_session_name.clone(),
                adk_session_info: adk_session_info.clone(),
                adk_cwd: adk_cwd.clone(),
                dispatch_id: dispatch_id.clone(),
                role_binding: role_binding.clone(),
                turn_id: turn_id.clone(),
                voice_progress_playback_channel_id,
                single_message_panel_footer_mode,
                footer_owner,
                status_panel_started_at,
                status_interval,
                context_window_tokens,
                context_compact_percent,
            },
            stream_loop::StreamLoopState {
                rx: &mut rx,
                full_response: &mut full_response,
                last_edit_text: &mut last_edit_text,
                done: &mut done,
                cancelled: &mut cancelled,
                rx_disconnected: &mut rx_disconnected,
                current_tool_line: &mut current_tool_line,
                prev_tool_status: &mut prev_tool_status,
                last_tool_name: &mut last_tool_name,
                last_tool_summary: &mut last_tool_summary,
                accumulated_input_tokens: &mut accumulated_input_tokens,
                accumulated_cache_create_tokens: &mut accumulated_cache_create_tokens,
                accumulated_cache_read_tokens: &mut accumulated_cache_read_tokens,
                accumulated_output_tokens: &mut accumulated_output_tokens,
                spin_idx: &mut spin_idx,
                restart_followup_pending: &mut restart_followup_pending,
                any_tool_used: &mut any_tool_used,
                has_post_tool_text: &mut has_post_tool_text,
                tmux_handed_off: &mut tmux_handed_off,
                watcher_owns_assistant_relay: &mut watcher_owns_assistant_relay,
                watcher_relay_available_for_turn: &mut watcher_relay_available_for_turn,
                watcher_handoff_claim_outcome: &mut watcher_handoff_claim_outcome,
                standby_relay_owns_output: &mut standby_relay_owns_output,
                last_assistant_text_line: &mut last_assistant_text_line,
                long_running_placeholder_active: &mut long_running_placeholder_active,
                active_background_child_session_ids: &mut active_background_child_session_ids,
                transport_error: &mut transport_error,
                transcript_events: &mut transcript_events,
                resume_failure_detected: &mut resume_failure_detected,
                session_handshake_seen: &mut session_handshake_seen,
                terminal_session_reset_required: &mut terminal_session_reset_required,
                recovery_retry: &mut recovery_retry,
                last_adk_heartbeat: &mut last_adk_heartbeat,
                pending_stream_messages: &mut pending_stream_messages,
                pending_status_tool_results: &mut pending_status_tool_results,
                pending_status_tool_results_by_id: &mut pending_status_tool_results_by_id,
                last_inflight_long_run_heartbeat: &mut last_inflight_long_run_heartbeat,
                last_activity_heartbeat_at: &mut last_activity_heartbeat_at,
                terminal_control_ready_observed: &mut terminal_control_ready_observed,
                terminal_control_drain_until: &mut terminal_control_drain_until,
                current_msg_id: &mut current_msg_id,
                response_sent_offset: &mut response_sent_offset,
                bridge_confirmed_response_sent_offset: &mut bridge_confirmed_response_sent_offset,
                streamed_assistant_text_this_turn: &mut streamed_assistant_text_this_turn,
                streaming_rollover_frozen_msg_ids: &mut streaming_rollover_frozen_msg_ids,
                terminal_full_replay_cleanup_msg_ids: &mut terminal_full_replay_cleanup_msg_ids,
                tmux_last_offset: &mut tmux_last_offset,
                watcher_owner_channel_id: &mut watcher_owner_channel_id,
                new_session_id: &mut new_session_id,
                new_raw_provider_session_id: &mut new_raw_provider_session_id,
                inflight_state: &mut inflight_state,
                last_status_edit: &mut last_status_edit,
                first_answer_relayed: &mut first_answer_relayed,
                last_session_panel_lifecycle_refresh: &mut last_session_panel_lifecycle_refresh,
                status_panel_msg_id: &mut status_panel_msg_id,
                last_status_panel_text: &mut last_status_panel_text,
                status_panel_dirty: &mut status_panel_dirty,
                last_status_panel_edit: &mut last_status_panel_edit,
                bridge_spans: &mut bridge_spans,
                status_panel_generation: &mut status_panel_generation,
            },
        )
        .await;
        match stream_loop_output.outcome {
            stream_loop::StreamLoopOutcome::Completed => {}
        }
        let pending_long_running_open_after_state_save =
            stream_loop_output.pending_long_running_open_after_state_save;
        let pending_long_running_retarget_after_state_save =
            stream_loop_output.pending_long_running_retarget_after_state_save;

        let post_loop_finalize_output = post_loop_finalize::run_post_loop_finalize(
            post_loop_finalize::PostLoopFinalizeContext {
                shared_owned: shared_owned.clone(),
                gateway: gateway.clone(),
                channel_id,
                provider: provider.clone(),
                adk_session_key: adk_session_key.clone(),
                adk_session_name: adk_session_name.clone(),
                adk_session_info: adk_session_info.clone(),
                adk_cwd: adk_cwd.clone(),
                dispatch_id: dispatch_id.clone(),
                role_binding: role_binding.clone(),
                turn_id: turn_id.clone(),
                current_msg_id,
                cancelled,
                transport_error,
                tui_error_classification: stream_loop_output.tui_error_classification,
                recovery_retry,
                rx_disconnected,
                tmux_handed_off,
                standby_relay_owns_output,
                watcher_owns_assistant_relay,
                watcher_relay_available_for_turn,
                initial_relay_owner_kind,
                response_sent_offset,
                tmux_last_offset,
                watcher_owner_channel_id,
                accumulated_input_tokens,
                accumulated_cache_create_tokens,
                accumulated_cache_read_tokens,
                accumulated_output_tokens,
            },
            post_loop_finalize::PostLoopFinalizeState {
                full_response,
                active_background_child_session_ids,
                pending_long_running_open_after_state_save,
                pending_long_running_retarget_after_state_save,
                long_running_placeholder_active,
                current_tool_line,
                prev_tool_status,
                inflight_state,
                api_friction_reports,
            },
        )
        .await;
        let full_response = post_loop_finalize_output.full_response;
        let active_background_child_session_ids =
            post_loop_finalize_output.active_background_child_session_ids;
        let pending_long_running_open_after_state_save =
            post_loop_finalize_output.pending_long_running_open_after_state_save;
        let pending_long_running_retarget_after_state_save =
            post_loop_finalize_output.pending_long_running_retarget_after_state_save;
        let long_running_placeholder_active =
            post_loop_finalize_output.long_running_placeholder_active;
        let inflight_state = post_loop_finalize_output.inflight_state;
        let api_friction_reports = post_loop_finalize_output.api_friction_reports;
        let claude_tui_followup_pre_submit_requeue_candidate =
            post_loop_finalize_output.claude_tui_followup_pre_submit_requeue_candidate;
        let review_dispatch_warning = post_loop_finalize_output.review_dispatch_warning;
        let is_prompt_too_long = post_loop_finalize_output.is_prompt_too_long;
        let bridge_relay_delegated_to_watcher =
            post_loop_finalize_output.bridge_relay_delegated_to_watcher;
        let bridge_output_owner = post_loop_finalize_output.bridge_output_owner;
        let should_complete_work_dispatch_after_delivery =
            post_loop_finalize_output.should_complete_work_dispatch_after_delivery;
        let should_fail_dispatch_after_delivery =
            post_loop_finalize_output.should_fail_dispatch_after_delivery;
        let final_session_status = post_loop_finalize_output.final_session_status;
        let can_chain_locally = post_loop_finalize_output.can_chain_locally;
        let has_queued_turns = post_loop_finalize_output.has_queued_turns;
        #[cfg(unix)]
        let bridge_tui_gate_outcome_early =
            post_loop_finalize_output.bridge_tui_gate_outcome_early;

        let terminal_outcome_delivery_output =
            terminal_outcome_delivery::run_terminal_outcome_delivery(
                terminal_outcome_delivery::TerminalOutcomeDeliveryContext {
                    channel_id,
                    user_msg_id,
                    current_msg_id,
                    status_panel_msg_id,
                    cancelled,
                    transport_error,
                    recovery_retry,
                    rx_disconnected,
                    tmux_last_offset,
                    watcher_owner_channel_id,
                    watcher_handoff_claim_outcome,
                    bridge_created_response_placeholder_msg_id,
                    bridge_relay_delegated_to_watcher,
                    bridge_output_owner,
                    should_complete_work_dispatch_after_delivery,
                    should_fail_dispatch_after_delivery,
                    can_chain_locally,
                    single_message_panel_footer_mode,
                    is_prompt_too_long,
                    claude_tui_followup_pre_submit_requeue_candidate,
                    tui_error_classification: post_loop_finalize_output.tui_error_classification,
                    had_prior_session_id_at_turn_start,
                    session_handshake_seen,
                    turn_start,
                    #[cfg(unix)]
                    bridge_tui_gate_outcome_early,
                },
                terminal_outcome_delivery::TerminalOutcomeDeliveryState {
                    shared_owned,
                    gateway,
                    provider,
                    cancel_token,
                    turn_id,
                    user_text_owned,
                    adk_session_key,
                    adk_cwd,
                    dispatch_id,
                    new_session_id,
                    new_raw_provider_session_id,
                    full_response,
                    active_background_child_session_ids,
                    pending_long_running_open_after_state_save,
                    pending_long_running_retarget_after_state_save,
                    long_running_placeholder_active,
                    inflight_state,
                    api_friction_reports,
                    review_dispatch_warning,
                    last_edit_text,
                    terminal_empty_response_notice,
                    terminal_full_replay_cleanup_msg_ids,
                    resume_failure_detected,
                    response_sent_offset,
                },
            )
            .await;
        match terminal_outcome_delivery_output.outcome {
            terminal_outcome_delivery::TerminalOutcomeDeliveryOutcome::Completed => {}
        }
        let shared_owned = terminal_outcome_delivery_output.shared_owned;
        let gateway = terminal_outcome_delivery_output.gateway;
        let provider = terminal_outcome_delivery_output.provider;
        let cancel_token = terminal_outcome_delivery_output.cancel_token;
        let turn_id = terminal_outcome_delivery_output.turn_id;
        let user_text_owned = terminal_outcome_delivery_output.user_text_owned;
        let adk_session_key = terminal_outcome_delivery_output.adk_session_key;
        let adk_cwd = terminal_outcome_delivery_output.adk_cwd;
        let dispatch_id = terminal_outcome_delivery_output.dispatch_id;
        let new_session_id = terminal_outcome_delivery_output.new_session_id;
        let new_raw_provider_session_id =
            terminal_outcome_delivery_output.new_raw_provider_session_id;
        let full_response = terminal_outcome_delivery_output.full_response;
        let active_background_child_session_ids =
            terminal_outcome_delivery_output.active_background_child_session_ids;
        let pending_long_running_open_after_state_save =
            terminal_outcome_delivery_output.pending_long_running_open_after_state_save;
        let pending_long_running_retarget_after_state_save =
            terminal_outcome_delivery_output.pending_long_running_retarget_after_state_save;
        let long_running_placeholder_active =
            terminal_outcome_delivery_output.long_running_placeholder_active;
        let inflight_state = terminal_outcome_delivery_output.inflight_state;
        let api_friction_reports = terminal_outcome_delivery_output.api_friction_reports;
        let status_panel_terminal_committed =
            terminal_outcome_delivery_output.status_panel_terminal_committed;
        let bridge_should_emit_completion =
            terminal_outcome_delivery_output.bridge_should_emit_completion;
        let completion_footer_terminal_text =
            terminal_outcome_delivery_output.completion_footer_terminal_text;
        let preserve_inflight_for_cleanup_retry =
            terminal_outcome_delivery_output.preserve_inflight_for_cleanup_retry;
        let bridge_skip_holder_owns_inflight =
            terminal_outcome_delivery_output.bridge_skip_holder_owns_inflight;
        let terminal_delivery_committed =
            terminal_outcome_delivery_output.terminal_delivery_committed;
        let resume_failure_detected = terminal_outcome_delivery_output.resume_failure_detected;
        let _terminal_empty_response_notice =
            terminal_outcome_delivery_output.terminal_empty_response_notice;
        let _terminal_full_replay_cleanup_msg_ids =
            terminal_outcome_delivery_output.terminal_full_replay_cleanup_msg_ids;
        let _response_sent_offset = terminal_outcome_delivery_output.response_sent_offset;
        let turn_start = terminal_outcome_delivery_output.turn_start;

        completion_postlude::run_completion_postlude(
            completion_postlude::CompletionPostludeContext {
                shared_owned,
                gateway,
                channel_id,
                provider,
                cancel_token,
                user_msg_id,
                turn_id,
                request_owner_name,
                final_session_status,
                status_panel_started_at,
                has_queued_turns,
                defer_watcher_resume,
                can_chain_locally,
                single_message_panel_footer_mode,
                is_external_input_tui_direct,
                context_window_tokens,
                context_compact_percent,
                turn_start,
            },
            completion_postlude::CompletionPostludeState {
                full_response,
                user_text_owned,
                role_binding,
                adk_session_key,
                adk_session_name,
                adk_session_info,
                adk_cwd,
                dispatch_id,
                dispatch_kind,
                new_session_id,
                new_raw_provider_session_id,
                status_panel_terminal_committed,
                bridge_should_emit_completion,
                current_msg_id,
                status_panel_msg_id,
                last_status_panel_text,
                completion_footer_terminal_text,
                spin_idx,
                status_panel_generation,
                preserve_inflight_for_cleanup_retry,
                tmux_last_offset,
                watcher_owner_channel_id,
                bridge_relay_delegated_to_watcher,
                is_prompt_too_long,
                resume_failure_detected,
                recovery_retry,
                rx_disconnected,
                tmux_handed_off,
                bridge_output_owner,
                terminal_delivery_committed,
                terminal_session_reset_required,
                transcript_events,
                accumulated_input_tokens,
                accumulated_cache_create_tokens,
                accumulated_cache_read_tokens,
                accumulated_output_tokens,
                accumulated_memory_input_tokens,
                accumulated_memory_output_tokens,
                transport_error,
                api_friction_reports,
                cancelled,
                restart_followup_pending,
                bridge_skip_holder_owns_inflight,
                inflight_guard,
                inflight_state,
            },
        )
        .await;

        // completion_tx is sent automatically by CompletionGuard on drop
    }.instrument(bridge_span));
}
