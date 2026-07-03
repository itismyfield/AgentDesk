mod bridge_latency_spans;
mod cancel_finalize_policy;
mod chunk_compose;
mod completion_guard;
mod context_window;
#[cfg(unix)]
mod early_tui_completion;
mod finalize_epilogue;
mod followup_requeue;
mod headless_delivery;
mod memory_lifecycle;
mod output_lifecycle;
mod panel_lifecycle;
mod recall_feedback;
mod recovery_text;
mod retry_state;
mod single_message_footer;
mod skill_usage;
mod stale_resume;
mod status_panel;
mod streaming_edit_text;
mod task_notification_lifecycle;
mod terminal_controller_cutover;
mod terminal_delivery;
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
use response_delivery::{
    done_result_requires_full_terminal_replay, push_transcript_event,
    response_portion_after_offset, terminal_delivery_response_after_offset,
};

use super::gateway::TurnGateway;
use super::restart_report::{RestartCompletionReport, clear_restart_report, save_restart_report};
use super::*;
use crate::db::session_observability::{
    BackgroundChildSpawn, close_background_child_pg, insert_background_child_pg,
    mark_session_tool_use_pg,
};
use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::{
    RuntimeHandoff, RuntimeHandoffKind, StatusEvent, TaskNotificationKind,
};
use crate::services::memory::{
    CaptureRequest, TokenUsage, resolve_memory_role_id, resolve_memory_session_id,
};
use crate::services::observability::session_inventory::{
    format_child_inventory_progress, load_child_inventory_by_parent_key_pg,
};
use crate::services::provider::cancel_requested;
use output_lifecycle::{BridgeOutputOwner, classify_bridge_output_owner};
use panel_lifecycle::{
    child_progress_line, ensure_active_placeholder_card, first_request_line,
    refresh_session_panel_line_from_lifecycle, refresh_task_panel_line_from_dispatch,
};
use std::collections::VecDeque;

// Re-exports for pub(super) items used by sibling modules in the discord package
use bridge_latency_spans::BridgeLatencySpans;
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
pub(in crate::services::discord) use two_message_panel::{
    two_message_should_reanchor_panel_on_rollover, two_message_status_edit_generation_is_stale,
};
pub(super) use watcher_orphan_cleanup::{
    cleanup_or_preserve_watcher_orphan_spinner,
    should_delete_bridge_created_watcher_orphan_response,
};

/// #2452 H6 graduation: schedule the history-aware auto-retry via the
/// gateway's `_with_completion` variant, then release the
/// `RETRY_PENDING` dedup lockout AS SOON AS the gateway's completion
/// oneshot resolves. A 120s `tokio::time::timeout` safety net guarantees
/// the lockout cannot leak indefinitely even if the spawned scheduler
/// panics or wedges before sending on `completion_tx`.
///
/// The legacy 30s sleep inside `auto_retry_with_history` is preserved as
/// a back-compat fallback for callers that hit the trait's default
/// `_with_completion` impl (which sends on `completion_tx` immediately
/// after the inner `auto_retry_with_history` returns) — both paths
/// remove the same `channel_id` from the `RETRY_PENDING` set, so a
/// double-remove is a no-op.
fn spawn_retry_with_history_with_release(
    gateway: std::sync::Arc<dyn gateway::TurnGateway>,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    retry_text: String,
) {
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel::<()>();
    super::task_supervisor::spawn_observed("retry_with_history_dispatch", async move {
        gateway
            .schedule_retry_with_history_with_completion(
                channel_id,
                user_msg_id,
                &retry_text,
                completion_tx,
            )
            .await;
    });
    super::task_supervisor::spawn_observed("retry_with_history_release", async move {
        // 120s safety net: if completion_tx is dropped without a send
        // (panic, wedged future), the recv resolves with Err and we still
        // release. If 120s elapses with neither send nor drop, force
        // release so the lockout cannot leak forever.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(120), completion_rx).await;
        release_retry_pending(channel_id);
    });
}

// Re-export pub(crate) items
pub(crate) use tmux_runtime::tmux_runtime_paths;

// Items used by spawn_turn_bridge from submodules
use completion_guard::complete_work_dispatch_on_turn_end;
use context_window::{apply_context_token_update, persisted_context_tokens, resolve_done_response};
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
    should_submit_automatic_feedback_fallback, submit_pending_feedbacks,
    transcript_contains_explicit_memento_tool_call,
};
use retry_state::{
    clear_local_session_state, handle_gemini_retry_boundary, reset_session_for_auto_retry,
    sync_response_delivery_state,
};
use skill_usage::record_skill_usage_from_tool_use;
use stale_resume::{
    output_file_has_stale_resume_error_after_offset, stream_error_has_stale_resume_error,
    stream_error_requires_terminal_session_reset,
};
use status_panel::{
    bridge_epilogue_identity_guards_inflight_clear, migrate_separate_status_panel_to_footer,
    should_open_long_running_placeholder_controller,
    status_panel_completion_ready_after_terminal_body, status_panel_message_id_for_turn,
};
use terminal_delivery::{
    BridgeLeaseAcquire, bridge_delivery_lease_for_inflight, bridge_delivery_lease_key_for_inflight,
    bridge_epilogue_clears_inflight, bridge_epilogue_marks_watcher_delivered,
    bridge_epilogue_skip_save_is_identity_guarded, send_ordered_long_terminal_response,
    should_complete_work_dispatch_after_terminal_delivery,
    should_fail_dispatch_after_terminal_delivery, silent_turn_skip_marks_committed,
    terminal_delivery_should_send_new_chunks, tui_quiescence_timeout_requires_inflight_retry,
    turn_bridge_replace_outcome_committed, warn_preserved_uncommitted as td_warn,
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

use super::watcher_lifecycle_decision::should_resume_watcher_after_turn;
use crate::db::session_status::{AWAITING_BG, IDLE, TURN_ACTIVE};
use sqlx::Row;

#[cfg(unix)]
fn tmux_generation_file_mtime_ns(tmux_session_name: &str) -> i64 {
    super::tmux::read_generation_file_mtime_ns(tmux_session_name)
}

#[cfg(not(unix))]
fn tmux_generation_file_mtime_ns(_tmux_session_name: &str) -> i64 {
    0
}

fn sync_inflight_restart_mode_from_cancel(
    cancel_token: &crate::services::provider::CancelToken,
    inflight_state: &mut InflightTurnState,
) -> bool {
    let new_mode = cancel_token.restart_mode();
    if inflight_state.restart_mode == new_mode {
        return false;
    }
    match new_mode {
        Some(mode) => inflight_state.set_restart_mode(mode),
        None => inflight_state.clear_restart_mode(),
    }
    true
}

fn record_placeholder_live_event(
    shared: &SharedData,
    channel_id: ChannelId,
    event: Option<super::placeholder_live_events::RecentPlaceholderEvent>,
) {
    if (shared.ui.placeholder_live_events_enabled || shared.ui.status_panel_v2_enabled)
        && let Some(event) = event
    {
        shared
            .ui
            .placeholder_live_events
            .push_event(channel_id, event);
    }
}

fn record_status_panel_events(
    shared: &SharedData,
    channel_id: ChannelId,
    events: Vec<StatusEvent>,
) -> bool {
    if shared.ui.status_panel_v2_enabled && !events.is_empty() {
        shared
            .ui
            .placeholder_live_events
            .push_status_events(channel_id, events);
        true
    } else {
        false
    }
}

fn thinking_status_line() -> String {
    "💭 Thinking...".to_string()
}

fn redacted_thinking_transcript_event(_summary: Option<String>) -> SessionTranscriptEvent {
    SessionTranscriptEvent {
        kind: SessionTranscriptEventKind::Thinking,
        tool_name: None,
        summary: None,
        content: String::new(),
        status: Some("info".to_string()),
        is_error: false,
    }
}

#[cfg(test)]
mod thinking_redaction_tests {
    use super::*;

    // U-6 Policy clause 1 + clause 4: the transcript event we record for a
    // Thinking stream message must carry no raw model reasoning. Both
    // `summary` and `content` must be empty regardless of the input the
    // model sent, and the kind must be `Thinking` (so consumers can apply
    // the neutral marker policy in clause 2).
    #[test]
    fn redacted_thinking_event_drops_summary_and_keeps_content_blank() {
        let event = redacted_thinking_transcript_event(Some(
            "internal scratchpad reasoning that must not leak".to_string(),
        ));

        assert_eq!(event.kind, SessionTranscriptEventKind::Thinking);
        assert!(event.tool_name.is_none());
        assert!(
            event.summary.is_none(),
            "summary leaked: {:?}",
            event.summary
        );
        assert!(
            event.content.is_empty(),
            "content leaked: {:?}",
            event.content
        );
        assert_eq!(event.status.as_deref(), Some("info"));
        assert!(!event.is_error);
    }

    // Calling the redaction function with `None` summary keeps the same
    // invariants — defense in depth against future callers that might
    // attempt to pass through model text accidentally.
    #[test]
    fn redacted_thinking_event_with_none_summary_still_blank() {
        let event = redacted_thinking_transcript_event(None);

        assert!(event.summary.is_none());
        assert!(event.content.is_empty());
    }

    // U-6 Policy clause 2: the user-visible thinking marker is a single
    // neutral string with no model text, no timers, no token counts.
    // It must be a stable identifier that the relay can deduplicate on.
    #[test]
    fn thinking_status_line_is_neutral_single_marker() {
        let line = thinking_status_line();

        assert_eq!(line, "💭 Thinking...");
    }

    // U-6 Policy clause 2 (stability): repeated calls must return the
    // exact same marker string. The Thinking dispatch path uses this for
    // both `current_tool_line` replacement and dedupe — if it ever drifted
    // into a non-deterministic form (timestamp, counter, locale variant),
    // the relay could emit multiple markers per turn or fail to match the
    // previous one.
    #[test]
    fn thinking_status_line_is_stable_across_repeated_calls() {
        let baseline = thinking_status_line();
        for _ in 0..10 {
            assert_eq!(thinking_status_line(), baseline);
        }
    }
}

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

struct StreamMessageReceiverAdapter {
    rx: tokio::sync::mpsc::UnboundedReceiver<StreamMessage>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl StreamMessageReceiverAdapter {
    async fn recv(&mut self) -> Option<StreamMessage> {
        self.rx.recv().await
    }

    fn try_recv(&mut self) -> Result<StreamMessage, tokio::sync::mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for StreamMessageReceiverAdapter {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }
}

fn spawn_stream_message_receiver_adapter(
    rx: mpsc::Receiver<StreamMessage>,
) -> StreamMessageReceiverAdapter {
    let (tx, async_rx) = tokio::sync::mpsc::unbounded_channel();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_worker = stop.clone();
    tokio::task::spawn_blocking(move || {
        while !stop_worker.load(std::sync::atomic::Ordering::Acquire) {
            match rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(message) => {
                    if stop_worker.load(std::sync::atomic::Ordering::Acquire)
                        || tx.send(message).is_err()
                    {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    StreamMessageReceiverAdapter { rx: async_rx, stop }
}

fn turn_bridge_stream_wait_duration(
    done: bool,
    terminal_control_drain_until: Option<std::time::Instant>,
    now: std::time::Instant,
) -> std::time::Duration {
    if done {
        return terminal_control_drain_until
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or_else(|| std::time::Duration::from_millis(0));
    }
    std::time::Duration::from_secs(1)
}

#[cfg(test)]
mod ready_drain_unit_tests {
    use super::*;

    #[test]
    fn done_wait_uses_remaining_drain_window_as_safety_wake() {
        let now = std::time::Instant::now();
        assert_eq!(
            turn_bridge_stream_wait_duration(
                true,
                Some(now + std::time::Duration::from_millis(123)),
                now,
            ),
            std::time::Duration::from_millis(123)
        );
        assert_eq!(
            turn_bridge_stream_wait_duration(true, None, now),
            std::time::Duration::from_millis(0)
        );
        assert_eq!(
            turn_bridge_stream_wait_duration(false, None, now),
            std::time::Duration::from_secs(1)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_receiver_adapter_wakes_on_ready_frame() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut async_rx = spawn_stream_message_receiver_adapter(rx);
        tx.send(StreamMessage::TmuxReady {
            output_path: "/tmp/out.jsonl".to_string(),
            input_fifo_path: "/tmp/in.fifo".to_string(),
            tmux_session_name: "adk-test".to_string(),
            last_offset: 12,
        })
        .expect("send ready frame");

        let received = tokio::time::timeout(std::time::Duration::from_millis(50), async_rx.recv())
            .await
            .expect("ready frame should wake without a poll tick")
            .expect("adapter should forward ready frame");

        assert!(matches!(received, StreamMessage::TmuxReady { .. }));
    }
}

#[cfg(test)]
mod sentinel_overwrite_clamp_tests {
    use super::done_result_requires_full_terminal_replay;

    // #3419 R3: the existing offset-reset gate requires a >DISCORD_MSG_LIMIT
    // (8000-byte) authoritative replay, so a short sentinel NEVER trips it —
    // which is exactly why the SEPARATE clamp at the swap site is needed. This
    // pins that gap so a future refactor cannot silently make the clamp dead
    // (e.g. by assuming the replay gate already handles sentinels).
    #[test]
    fn replay_gate_does_not_reset_offset_for_short_sentinel() {
        let sentinel = "⚠ tool-only turn, no assistant text"; // ~40 bytes, < 8000
        assert!(sentinel.len() <= super::super::DISCORD_MSG_LIMIT);
        // streamed text this turn, prior offset > 0, sentinel == full_response:
        // the replay gate STILL returns false because of the length floor.
        assert!(!done_result_requires_full_terminal_replay(
            sentinel, sentinel, 900, true,
        ));
    }

    // #3419 R3 / codex MEDIUM: drive the REAL normalizer the bridge now calls
    // (`sync_response_delivery_state` → `normalized_response_sent_offset`), not a
    // re-implemented clamp, so a mutation that drops the clamp OR the char-boundary
    // walk-back is caught against actual production behaviour.
    #[test]
    fn sync_clamps_offset_within_replaced_body() {
        use crate::services::discord::InflightTurnState;
        use crate::services::provider::ProviderKind;

        let replaced = "⚠ tool-only turn, no assistant text".to_string();
        let mut offset = 900usize; // tracked the long pre-swap body
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            1,
            Some("adk-cc".to_string()),
            42,
            5001,
            5002,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc-1".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            10,
        );
        super::sync_response_delivery_state(&replaced, &mut offset, &mut state);
        // Clamped to len() (out-of-bounds prior offset) and char-boundary valid.
        assert_eq!(offset, replaced.len());
        assert!(replaced.is_char_boundary(offset));
        assert_eq!(state.response_sent_offset, offset);
        assert_eq!(state.full_response, replaced);
        // Mutation guard: the UNCLAMPED prior offset is out of bounds (the wedge).
        assert!(replaced.get(900..).is_none());
        assert!(replaced.get(offset..).is_some());
    }

    // #3419 R3 / codex MEDIUM (the char-boundary case the bare `.min(len)` missed):
    // a replacement BEGINNING with a multibyte sentinel (`⚠` = 3 bytes) with a
    // prior offset of 1. `1 < len()`, so `.min(len)` would leave it UNCHANGED at 1
    // — but byte 1 is INSIDE `⚠`, violating the char-boundary invariant and
    // panicking any later `full_response[offset..]` slice. The normalizer must walk
    // BACK to the nearest valid boundary (0).
    #[test]
    fn sync_normalizes_prior_offset_inside_leading_multibyte_char() {
        use crate::services::discord::InflightTurnState;
        use crate::services::provider::ProviderKind;

        let replaced = "⚠ tool-only turn, no assistant text".to_string();
        // Precondition: byte 1 is genuinely mid-multibyte (the bug surface).
        assert!(!replaced.is_char_boundary(1));
        let mut offset = 1usize; // prior valid offset that now lands mid-char
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            1,
            Some("adk-cc".to_string()),
            42,
            5001,
            5002,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc-1".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            10,
        );
        super::sync_response_delivery_state(&replaced, &mut offset, &mut state);
        // Normalized back to the leading boundary (0) — NOT left at the mid-char 1.
        assert_eq!(offset, 0);
        assert!(replaced.is_char_boundary(offset));
        assert_eq!(state.response_sent_offset, 0);
        // Mutation guard: a bare `.min(len)` (no walk-back) would leave offset 1,
        // which is NOT a char boundary and would panic on `&replaced[1..]`.
        assert!(replaced.get(1..).is_none());
        assert!(replaced.get(offset..).is_some());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WatcherHandoffClaimOutcome {
    None,
    ReusedExisting,
    Spawned,
}

#[cfg(test)]
mod streaming_edit_text_tests {
    use super::*;

    #[test]
    fn empty_response_notice_is_delivery_only_not_history_payload() {
        let full_response = String::new();
        let rendered =
            terminal_delivery_response_after_offset(&full_response, 0, Some("(No response)"));

        assert_eq!(rendered, "(No response)");
        assert!(full_response.is_empty());
    }
}

fn bridge_should_reclaim_relay_from_missing_watcher(
    watcher_owns_assistant_relay: bool,
    standby_relay_owns_output: bool,
    live_watcher_registered: bool,
) -> bool {
    watcher_owns_assistant_relay && !standby_relay_owns_output && !live_watcher_registered
}

#[cfg(test)]
mod bridge_busy_turn_handoff_tests {
    use super::*;
    use output_lifecycle::{BridgeOutputOwner, classify_bridge_output_owner};
    use watcher_handoff::{
        bridge_should_hand_off_busy_turn_to_watcher, genuinely_live_watcher_for_relay,
    };

    // Build a watcher handle with controllable liveness for the #3268 FIX 1
    // gate tests: `cancel` and the heartbeat age determine staleness.
    fn watcher_handle_with_liveness(
        tmux_session_name: &str,
        cancel: bool,
        heartbeat_ts_ms: i64,
    ) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: format!("/tmp/{tmux_session_name}.jsonl"),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(cancel)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(heartbeat_ts_ms)),
        }
    }

    // #3268 (Defect B) — the core regression. A NON-terminal turn whose early
    // TUI quiescence gate TIMED OUT (the pane is genuinely still busy on a
    // long-lived session) and that was NOT already delegated to the watcher,
    // with a LIVE watcher registered, MUST hand off to the watcher instead of
    // finalizing on the bridge. This is the exact condition that, when false in
    // production, let the bridge `submit_terminal(Complete)` strand the turn and
    // permanently stop relay.
    #[test]
    fn busy_timeout_with_live_watcher_hands_off() {
        assert!(
            bridge_should_hand_off_busy_turn_to_watcher(
                /* bridge_early_gate_timed_out */ true, /* terminal_error_path */ false,
                /* bridge_relay_delegated_to_watcher */ false,
                /* live_watcher_registered */ true,
            ),
            "gate timeout + non-terminal + not-yet-delegated + live watcher must hand off"
        );
    }

    // The handoff's relay-ownership promotion must route the rest of the turn
    // through the WatcherRelay branches — the bridge skips its own delivery and
    // (via `bridge_epilogue_marks_watcher_delivered`) does NOT mark the watcher
    // delivered, so the still-streaming output is NOT suppressed.
    #[test]
    fn handoff_promotes_ownership_to_watcher_relay_without_marking_delivered() {
        let handoff = bridge_should_hand_off_busy_turn_to_watcher(true, false, false, true);
        assert!(handoff);
        // After the promotion `bridge_relay_delegated_to_watcher == true`.
        let promoted_delegated = handoff;
        assert_eq!(
            classify_bridge_output_owner(/* standby */ false, promoted_delegated),
            Some(BridgeOutputOwner::WatcherRelay),
            "promoted ownership must classify as WatcherRelay so the bridge skips delivery"
        );
        assert!(
            !bridge_epilogue_marks_watcher_delivered(
                /* preserve_inflight_for_cleanup_retry */ false,
                promoted_delegated,
            ),
            "a handed-off (delegated) turn must NOT mark the watcher delivered — \
             marking it delivered is exactly what suppresses the still-streaming output"
        );
    }

    // A turn already delegated to the watcher needs no handoff (it is already
    // watcher-owned) — avoid a redundant second register/unpause.
    #[test]
    fn already_delegated_turn_does_not_re_hand_off() {
        assert!(
            !bridge_should_hand_off_busy_turn_to_watcher(true, false, true, true),
            "already-delegated turns are watcher-owned; no second handoff"
        );
    }

    // Terminal-error paths (cancelled / prompt_too_long / transport_error /
    // recovery_retry collapse into `terminal_error_path`) MUST still finalize on
    // the bridge as before — never hand off.
    #[test]
    fn terminal_error_paths_still_finalize_on_bridge() {
        assert!(
            !bridge_should_hand_off_busy_turn_to_watcher(true, true, false, true),
            "terminal-error turns finalize on the bridge, never hand off"
        );
    }

    // No gate timeout → the pane reported idle (or the gate did not apply); the
    // normal finalize path stands and there is nothing to hand off.
    #[test]
    fn quiesced_turn_does_not_hand_off() {
        assert!(
            !bridge_should_hand_off_busy_turn_to_watcher(false, false, false, true),
            "a quiesced (non-timed-out) turn finalizes normally"
        );
    }

    // No live watcher → there would be no authority to keep relaying or to
    // finalize on idle, so the bridge must NOT hand off (it owns the finalize).
    #[test]
    fn missing_live_watcher_does_not_hand_off() {
        assert!(
            !bridge_should_hand_off_busy_turn_to_watcher(true, false, false, false),
            "without a live watcher the bridge keeps finalize ownership"
        );
    }

    // #3268 FIX 1 (codex blocker): the handoff liveness gate must reject a STALE
    // watcher (heartbeat dead, not yet cancelled). Handing off to a stale handle
    // re-strands the turn — the bridge suppresses its own finalize while the
    // lingering handle has no real authority to finalize. A genuinely-live
    // watcher (recent heartbeat, not cancelled) is the ONLY one that may pass.
    #[test]
    fn handoff_liveness_gate_rejects_stale_watcher() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-claude-adk-cc-t1500000000000003268";
        let channel = ChannelId::new(1_500_000_000_000_003_268);
        // heartbeat_ts_ms = 1 → ancient → heartbeat_stale() == true, cancel=false.
        registry.insert(channel, watcher_handle_with_liveness(tmux, false, 1));
        assert!(
            !genuinely_live_watcher_for_relay(&registry, channel),
            "a heartbeat-stale watcher must NOT count as live for the handoff gate"
        );
        // The bridge therefore keeps finalize ownership (no handoff / no strand).
        assert!(
            !bridge_should_hand_off_busy_turn_to_watcher(
                true,
                false,
                false,
                genuinely_live_watcher_for_relay(&registry, channel),
            ),
            "a stale watcher on the timeout path must finalize on the bridge, not hand off"
        );
    }

    // A cancelled handle (sweeper set cancel=true, cleanup deliberately keeps the
    // handle) must also be rejected by the liveness gate.
    #[test]
    fn handoff_liveness_gate_rejects_cancelled_watcher() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-claude-adk-cc-t1500000000000003269";
        let channel = ChannelId::new(1_500_000_000_000_003_269);
        registry.insert(
            channel,
            watcher_handle_with_liveness(
                tmux,
                true,
                crate::services::discord::tmux_watcher_now_ms(),
            ),
        );
        assert!(
            !genuinely_live_watcher_for_relay(&registry, channel),
            "a cancelled watcher must NOT count as live for the handoff gate"
        );
    }

    // An absent handle (no live watcher at all) is rejected.
    #[test]
    fn handoff_liveness_gate_rejects_absent_watcher() {
        let registry = TmuxWatcherRegistry::new();
        let channel = ChannelId::new(1_500_000_000_000_003_270);
        assert!(
            !genuinely_live_watcher_for_relay(&registry, channel),
            "an absent watcher must NOT count as live for the handoff gate"
        );
    }

    // The positive case: a genuinely-live watcher (recent heartbeat, not
    // cancelled) DOES pass the liveness gate, so the timeout path hands off.
    #[test]
    fn handoff_liveness_gate_accepts_genuinely_live_watcher() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-claude-adk-cc-t1500000000000003271";
        let channel = ChannelId::new(1_500_000_000_000_003_271);
        registry.insert(
            channel,
            watcher_handle_with_liveness(
                tmux,
                false,
                crate::services::discord::tmux_watcher_now_ms(),
            ),
        );
        assert!(
            genuinely_live_watcher_for_relay(&registry, channel),
            "a present, non-cancelled, fresh-heartbeat watcher is genuinely live"
        );
        assert!(
            bridge_should_hand_off_busy_turn_to_watcher(
                true,
                false,
                false,
                genuinely_live_watcher_for_relay(&registry, channel),
            ),
            "a genuinely-live watcher on the timeout path must hand off as before"
        );
    }
}

fn active_turn_thread_channel_id(
    adk_session_name: Option<&str>,
    inflight_state: &InflightTurnState,
) -> Option<u64> {
    adk_session_name
        .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name)
        .or_else(|| {
            inflight_state
                .channel_name
                .as_deref()
                .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name)
        })
        .or(inflight_state.thread_id)
}

fn maybe_refresh_active_turn_activity_heartbeat(
    shared: &SharedData,
    provider: &ProviderKind,
    inflight_state: &InflightTurnState,
    adk_session_name: Option<&str>,
    last_heartbeat_at: &mut Option<std::time::Instant>,
) {
    maybe_refresh_active_turn_activity_heartbeat_at(
        shared,
        provider,
        inflight_state,
        adk_session_name,
        last_heartbeat_at,
        std::time::Instant::now(),
    );
}

#[cfg(unix)]
fn maybe_refresh_active_turn_activity_heartbeat_at(
    shared: &SharedData,
    provider: &ProviderKind,
    inflight_state: &InflightTurnState,
    adk_session_name: Option<&str>,
    last_heartbeat_at: &mut Option<std::time::Instant>,
    now: std::time::Instant,
) {
    if last_heartbeat_at.is_some_and(|last| {
        now.duration_since(last) < super::tmux::WATCHER_ACTIVITY_HEARTBEAT_INTERVAL
    }) {
        return;
    }

    let Some(tmux_session_name) = inflight_state.tmux_session_name.as_deref() else {
        return;
    };
    let thread_channel_id = active_turn_thread_channel_id(adk_session_name, inflight_state);

    let legacy_db = None::<&crate::db::Db>;

    if super::tmux::refresh_session_heartbeat_from_tmux_output(
        legacy_db,
        shared.pg_pool.as_ref(),
        &shared.token_hash,
        provider,
        tmux_session_name,
        thread_channel_id,
    ) {
        *last_heartbeat_at = Some(now);
    }
}

#[cfg(not(unix))]
fn maybe_refresh_active_turn_activity_heartbeat_at(
    _shared: &SharedData,
    _provider: &ProviderKind,
    _inflight_state: &InflightTurnState,
    _adk_session_name: Option<&str>,
    _last_heartbeat_at: &mut Option<std::time::Instant>,
    _now: std::time::Instant,
) {
}

#[allow(clippy::too_many_arguments)]
fn handle_watcher_runtime_handoff(
    shared_owned: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: &mut InflightTurnState,
    runtime_kind: RuntimeHandoffKind,
    output_path: String,
    input_fifo_path: Option<String>,
    tmux_session_name: String,
    last_offset: u64,
    tmux_last_offset: &mut Option<u64>,
    watcher_owner_channel_id: &mut ChannelId,
    standby_relay_owns_output: &mut bool,
    watcher_relay_available_for_turn: &mut bool,
    watcher_handoff_claim_outcome: &mut WatcherHandoffClaimOutcome,
    tmux_handed_off: &mut bool,
    watcher_owns_assistant_relay: &mut bool,
    state_dirty: &mut bool,
    done: bool,
    terminal_control_drain_until: &mut Option<std::time::Instant>,
) {
    *tmux_last_offset = Some(last_offset);
    inflight_state.runtime_kind = Some(runtime_kind);
    inflight_state.tmux_session_name = Some(tmux_session_name.clone());
    inflight_state.output_path = Some(output_path.clone());
    let mut fifo_path = input_fifo_path.filter(|path| !path.is_empty());
    // #2235 one-release compat window: ClaudeTui rows must still ship a
    // populated `input_fifo_path` so a rollback to an old binary can satisfy
    // its FIFO-required recovery branch. Synthesize from the canonical
    // per-session tmux path when the caller didn't supply one.
    if matches!(runtime_kind, RuntimeHandoffKind::ClaudeTui) && fifo_path.is_none() {
        let (_, synthesized_fifo) = tmux_runtime_paths(&tmux_session_name);
        if !synthesized_fifo.is_empty() {
            fifo_path = Some(synthesized_fifo);
        }
    }
    inflight_state.input_fifo_path = fifo_path;
    inflight_state.last_offset = last_offset;
    *state_dirty |= inflight_state.set_watcher_owner_channel_id(watcher_owner_channel_id.get());
    // #2235 NOTE: we deliberately do NOT durably save the row here.
    // `watcher_owns_live_relay` is still `false` at this point and only flips
    // to `true` after the watcher is successfully claimed and spawned (the
    // leader-branch path below). A save before that flag is set would leak a
    // v8 row with the new handoff shape alongside `watcher_owns_live_relay =
    // false`, which on restart would make the restored watcher yield to a
    // phantom bridge owner (codex adversarial review on #2235). The
    // existing branch-specific saves at the post-flag flip points plus the
    // centralized `state_dirty` flush already cover the durable-stamp
    // guarantee for watcher-owned RuntimeReady paths.
    //
    // #2263: the standby branch is INTENTIONALLY not covered by this
    // invariant — see the in-branch comment near the
    // `*standby_relay_owns_output = true` assignment for why the flag
    // stays `false` on standby and the trade-off vs duplicate delivery.

    // #226: Atomic claim via try_claim_watcher
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
    let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let last_heartbeat_ts_ms = Arc::new(std::sync::atomic::AtomicI64::new(
        super::tmux_watcher_now_ms(),
    ));
    let handle = TmuxWatcherHandle {
        tmux_session_name: tmux_session_name.clone(),
        output_path: output_path.clone(),
        paused: paused.clone(),
        resume_offset: resume_offset.clone(),
        cancel: cancel.clone(),
        pause_epoch: pause_epoch.clone(),
        turn_delivered: turn_delivered.clone(),
        last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
    };
    #[cfg(unix)]
    let (watcher_claimed, watcher_claim_replaced_existing) = {
        let claim = super::tmux::claim_or_reuse_watcher(
            &shared_owned.tmux_watchers,
            channel_id,
            handle,
            provider,
            "turn_bridge_runtime_ready",
        );
        *watcher_owner_channel_id = claim.owner_channel_id();
        *state_dirty |= inflight_state.set_watcher_owner_channel_id(watcher_owner_channel_id.get());
        (claim.should_spawn(), claim.replaced_existing())
    };
    #[cfg(not(unix))]
    let (watcher_claimed, watcher_claim_replaced_existing) = {
        let _ = handle;
        (false, false)
    };
    #[cfg(unix)]
    let mut watcher_ready_for_relay = !watcher_claimed;
    #[cfg(not(unix))]
    let mut watcher_ready_for_relay = false;
    *watcher_handoff_claim_outcome = if watcher_claimed {
        WatcherHandoffClaimOutcome::Spawned
    } else {
        WatcherHandoffClaimOutcome::ReusedExisting
    };
    let _ = watcher_claim_replaced_existing;
    if watcher_claimed {
        #[cfg(unix)]
        {
            let on_standby = shared_owned.http.cached_serenity_ctx.get().is_none();
            if on_standby {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ standby relay: skipping tmux watcher spawn for channel {}; spawning JSONL→Discord standby_relay",
                    channel_id
                );
                let _ = shared_owned.tmux_watchers.remove(watcher_owner_channel_id);
                if let Some(http_for_standby) = shared_owned.serenity_http_or_token_fallback() {
                    let placeholder_msg_id_opt = if inflight_state.current_msg_id == 0 {
                        None
                    } else {
                        Some(serenity::MessageId::new(inflight_state.current_msg_id))
                    };
                    let output_path_for_standby = output_path.clone();
                    let turn_binding_for_standby =
                        super::standby_relay::StandbyRelayTurnBinding::from_state(&inflight_state);
                    let cancel_for_standby = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let shared_for_standby = shared_owned.clone();
                    let provider_for_standby = provider.clone();
                    super::task_supervisor::spawn_observed(
                        "turn_bridge_standby_relay",
                        super::standby_relay::run_standby_relay(
                            http_for_standby,
                            channel_id,
                            placeholder_msg_id_opt,
                            output_path_for_standby,
                            turn_binding_for_standby,
                            last_offset,
                            cancel_for_standby,
                            shared_for_standby,
                            provider_for_standby,
                            // #2448: bumped from 900s (15min) heuristic stop
                            // signal to a 1800s (30min) safety backstop. The
                            // authoritative exit signal is now
                            // `InflightSignal::Completed`, broadcast by
                            // `CompletionGuard` on bridge drop.
                            std::time::Duration::from_secs(1800),
                        ),
                    );
                    *standby_relay_owns_output = true;
                    inflight_state
                        .set_relay_owner_kind(super::inflight::RelayOwnerKind::StandbyRelay);
                    // #2263: intentionally leave `watcher_owns_live_relay = false`
                    // on the standby branch.
                    //
                    // The flag's downstream contract in
                    // `tmux::watcher_should_yield_to_inflight_state` is
                    // narrowly "the restored TMUX WATCHER itself owns
                    // delivery for this turn — do not yield". The standby
                    // branch never spawns a watcher (the briefly-claimed
                    // slot was just removed at line ~1477); the
                    // `standby_relay` task is a separate, non-persisted
                    // delivery owner whose ownership is NOT representable
                    // by this single boolean.
                    //
                    // Setting the flag to `true` here would over-claim
                    // ownership for any watcher restored against this
                    // state on a different node (or after failover) — it
                    // would short-circuit the yield gate and let a
                    // restored watcher deliver concurrently with a still-
                    // alive standby_relay, producing duplicate Discord
                    // posts (codex adversarial review on #2263).
                    //
                    // The cost of keeping it `false` is the phantom-
                    // bridge yield window: on restart, a restored watcher
                    // whose tmux offset overlaps `turn_start_offset` will
                    // yield to a bridge owner that died with the original
                    // standby process and will suppress relay for the
                    // overlapping batch. The inflight row is then cleared
                    // by the `INFLIGHT_STALENESS_THRESHOLD_SECS` (300s)
                    // staleness path in `classify_inflight_diagnostic_state`
                    // (router/message_handler.rs) and the recovery-engine
                    // sweep, after which a follow-up user turn proceeds
                    // normally. The completed standby_relay response that
                    // landed before the crash is preserved on Discord (it
                    // was posted before the process died); the failure
                    // mode is the user-visible stall on the FOLLOW-UP
                    // turn until staleness sweep, NOT a dropped response.
                    //
                    // #2376 records `relay_owner_kind = standby_relay` so a
                    // restored watcher can yield for every live batch, not
                    // only batches that overlap the original turn_start_offset.
                    // A future owner-lease timestamp can distinguish
                    // dead-standby from live-standby and remove the phantom
                    // yield window entirely.
                    //
                    // Per-turn in-process state is still correctly tracked
                    // by `standby_relay_owns_output = true` above; that
                    // local flag is what gates the bridge's terminal
                    // delivery suppression for the current turn.
                    let _ = save_inflight_state(inflight_state);
                } else {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ standby relay skipped: no Http source for channel {}",
                        channel_id
                    );
                }
            } else if let Some(http_bg) = shared_owned.serenity_http_or_token_fallback() {
                let shared_bg = shared_owned.clone();
                inflight_state.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
                let restored_turn = super::tmux::restored_watcher_turn_from_inflight(
                    inflight_state,
                    &tmux_session_name,
                    true,
                );
                if let Ok(mut guard) = resume_offset.lock() {
                    *guard = Some(last_offset);
                }
                turn_delivered.store(false, std::sync::atomic::Ordering::Relaxed);
                if watcher_claim_replaced_existing {
                    shared_owned.record_tmux_watcher_reconnect(channel_id);
                }
                super::task_supervisor::spawn_observed_tmux_watcher(
                    "turn_bridge_tmux_output_watcher_with_restore",
                    shared_bg.clone(),
                    tmux_session_name.clone(),
                    cancel.clone(),
                    super::tmux::tmux_output_watcher_with_restore(
                        channel_id,
                        http_bg,
                        shared_bg,
                        output_path,
                        tmux_session_name,
                        last_offset,
                        cancel,
                        paused,
                        resume_offset,
                        pause_epoch,
                        turn_delivered,
                        last_heartbeat_ts_ms,
                        restored_turn,
                    ),
                );
                *watcher_relay_available_for_turn = true;
                let _ = save_inflight_state(inflight_state);
                watcher_ready_for_relay = true;
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ no Http source (neither cached_serenity_ctx nor cached_bot_token); tmux watcher not started for channel {}",
                    channel_id
                );
                if let Some((_, handle)) =
                    shared_owned.tmux_watchers.remove(watcher_owner_channel_id)
                {
                    handle
                        .cancel
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
    if watcher_ready_for_relay {
        *tmux_handed_off = true;
        inflight_state.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
        *watcher_owns_assistant_relay = true;
        let _ = save_inflight_state(inflight_state);
        if let Some(watcher) = shared_owned.tmux_watchers.get(watcher_owner_channel_id) {
            *watcher_relay_available_for_turn = true;
            if let Ok(mut guard) = watcher.resume_offset.lock() {
                *guard = Some(last_offset);
            }
            watcher
                .turn_delivered
                .store(false, std::sync::atomic::Ordering::Relaxed);
            // #3016 phase 2: register the turn with the single-authority
            // finalizer BEFORE unpausing the watcher. Message arrival order in
            // the actor replaces the deleted Release/AcqRel ordering: the
            // ledger now knows the turn exists (with the watcher as relay
            // owner) before the watcher can submit its terminal. The ledger is
            // the authority that superseded the legacy `mailbox_finalize_owed`
            // flag (removed in #3016 phase-5b2) and the CAS revoke deleted from
            // the bridge finalize branches below.
            shared_owned.turn_finalizer.register_start(
                super::turn_finalizer::TurnKey::new(
                    channel_id,
                    inflight_state.effective_finalizer_turn_id(),
                    shared_owned.restart.current_generation,
                ),
                provider.clone(),
                super::inflight::RelayOwnerKind::Watcher,
                // #3016 phase-5a: prime the reconcile cache at register time.
                shared_owned,
            );
            watcher
                .paused
                .store(false, std::sync::atomic::Ordering::Release);
        }
    }
    *state_dirty = true;
    if done {
        *terminal_control_drain_until = None;
    }
}

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
    let bridge_session_key = bridge.adk_session_key.clone();
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
        session_key = tracing::field::debug(bridge_session_key.as_deref()),
        turn_id = %bridge_turn_id,
    );
    super::task_supervisor::spawn_observed("discord_turn_bridge", async move {
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
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
        // #2451 H5 graduation: `StreamMessage::Init { session_id, .. }` is
        // the explicit provider handshake — it lands as soon as the
        // provider has a live session bound to the new turn. We use its
        // arrival as the authoritative "resume succeeded" witness so the
        // empty-response classification no longer has to guess from
        // `turn_start.elapsed() < 10s`. The elapsed-time heuristic is
        // retained only as a 30s safety backstop.
        let mut session_handshake_seen = false;
        // #2451: snapshot whether the channel had a prior provider
        // session_id at turn-start time. The previous logic re-read
        // `shared.core.sessions` at empty-response classification time,
        // which races with `reset_session_for_auto_retry` and produces
        // false negatives ("session was already cleared, so we never
        // attempted resume"). Capturing this once at the top closes the
        // race.
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
        const LIVE_LONG_RUN_HEARTBEAT_INTERVAL: std::time::Duration =
            std::time::Duration::from_secs(30);
        let mut last_activity_heartbeat_at: Option<std::time::Instant> = None;
        let mut terminal_control_ready_observed = false;
        let mut terminal_control_drain_until: Option<std::time::Instant> = None;
        let mut bridge_created_response_placeholder_msg_id: Option<MessageId> = None;
        // A recovery turn with no anchored placeholder (current_msg_id == 0,
        // e.g. a TUI-direct turn) reaches the bridge with `None`. The bridge
        // streams into a concrete placeholder, so create a fresh one now; if
        // creation fails we fall back to the channel and the first streaming
        // edit re-creates it. This keeps the working `current_msg_id` a real
        // `MessageId` for the ~30 downstream relay sites without panicking on
        // `MessageId::new(0)`.
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
                            channel = channel_id.get(),
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
        let completion_tx = bridge.completion_tx;
        // Guard: ensure completion_tx fires even if the task panics or
        // exits early, preventing the parent from hanging on completion_rx.
        //
        // #2448: also publish an explicit `InflightSignal::Completed`
        // broadcast on drop so any per-turn relay tasks (currently the
        // standby JSONL relay) can exit immediately instead of polling
        // against a wall-clock deadline. The broadcast send is best-effort
        // — if no subscriber is registered, `send` returns Err and we
        // ignore it.
        struct CompletionGuard {
            tx: Option<tokio::sync::oneshot::Sender<()>>,
            broadcaster: tokio::sync::broadcast::Sender<super::inflight::InflightSignal>,
            channel_id: ChannelId,
        }
        impl Drop for CompletionGuard {
            fn drop(&mut self) {
                if let Some(tx) = self.tx.take() {
                    let _ = tx.send(());
                }
                let _ = self
                    .broadcaster
                    .send(super::inflight::InflightSignal::Completed {
                        channel_id: self.channel_id.get(),
                    });
            }
        }
        let _completion_guard = CompletionGuard {
            tx: completion_tx,
            broadcaster: shared_owned.inflight_signals.clone(),
            channel_id,
        };

        // Guard: ensure inflight state file is cleaned up even if the task
        // panics or exits early.  On the normal path we defuse the guard
        // after the explicit clear_inflight_state() call.
        //
        // #3161 (codex P2): the Drop runs on ANY abnormal exit (panic / early
        // return after the mailbox release but before the explicit defuse). A
        // plain unconditional `clear_inflight_state` here is identity-blind and
        // can delete a row this turn does NOT own — e.g. a NEWER turn already
        // re-wrote the channel's inflight after this turn released the mailbox.
        // The guard now carries THIS turn's `user_msg_id` and routes the
        // abnormal-path clear through the identity-aware guarded clears, so it
        // only removes the row when the on-disk identity still matches THIS
        // turn (non-zero) or is a genuine zero-id-owned row (zero). A newer
        // owner yields `UserMsgMismatch` and is preserved.
        struct InflightCleanupGuard {
            provider: Option<ProviderKind>,
            channel_id: u64,
            user_msg_id: u64,
            token_hash: String,
        }
        impl Drop for InflightCleanupGuard {
            fn drop(&mut self) {
                if let Some(ref provider) = self.provider {
                    // #3859: this Drop runs on ANY abnormal exit (panic /
                    // early-return) while the turn may still own a live
                    // "🔄 처리 중" placeholder. Route through the abandon-request
                    // helper — identical ownership guards to the plain guarded
                    // clear, but it durably records the placeholder for the
                    // placeholder sweeper to finalize to "중단됨" BEFORE deleting
                    // the row (which still frees the channel immediately).
                    if self.user_msg_id != 0 {
                        super::inflight::request_inflight_abandon_if_matches(
                            provider,
                            self.channel_id,
                            self.user_msg_id,
                            &self.token_hash,
                        );
                    } else {
                        super::inflight::request_inflight_abandon_if_matches_zero_owned(
                            provider,
                            self.channel_id,
                            &self.token_hash,
                        );
                    }
                }
            }
        }
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
        let mut last_session_panel_lifecycle_refresh =
            tokio::time::Instant::now() - status_interval;
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
                if sync_inflight_restart_mode_from_cancel(
                    cancel_token.as_ref(),
                    &mut inflight_state,
                ) {
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
            || terminal_control_drain_until
                .is_some_and(|deadline| std::time::Instant::now() < deadline)
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
                        StreamMessage::RetryBoundary => {
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
                                    &mut inflight_state,
                                )
                            {
                                transcript_events.clear();
                                state_dirty = true;
                            }
                        }
                        StreamMessage::Init {
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
                        StreamMessage::Text { content } => {
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
                                continue;
                            }
                            streamed_assistant_text_this_turn = true;
                            // #3608: normalize the chunk boundary so a tool-use
                            // `\n\n` separator + a chunk that itself begins with
                            // blank lines does not accumulate into `\n\n\n\n`.
                            chunk_compose::append_streamed_text_chunk(&mut full_response, &content);
                            if (watcher_owns_assistant_relay
                                && watcher_relay_available_for_turn)
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
                            super::formatting::preserve_previous_tool_status(
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
                        StreamMessage::Thinking { summary } => {
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
                                .map(super::formatting::finalize_in_progress_tool_status);
                            super::formatting::preserve_previous_tool_status(
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
                        StreamMessage::ToolUse {
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
                                super::placeholder_live_events::RecentPlaceholderEvent::tool_use(
                                    &name, &input,
                                ),
                            );
                            status_panel_dirty |= record_status_panel_events(
                                shared_owned.as_ref(),
                                channel_id,
                                super::placeholder_live_events::status_events_from_tool_use_with_id(
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
                                .map(super::formatting::finalize_in_progress_tool_status);
                            super::formatting::preserve_previous_tool_status(
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
                            let long_running_tool =
                                if !is_dcserver_restart_command(&input) {
                                    super::formatting::classify_long_running_tool(&name, &input)
                                } else {
                                    None
                                };
                            if matches!(
                                long_running_tool,
                                Some((
                                    _,
                                    super::formatting::LongRunningCloseTrigger::BackgroundDispatch,
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
                            )
                                && long_running_placeholder_active.is_none()
                                && pending_long_running_open_after_state_save.is_none()
                                && pending_long_running_retarget_after_state_save.is_none()
                            {
                                if let Some((reason, close_trigger, reason_detail)) =
                                    long_running_tool
                                {
                                    let started_at_unix = chrono::Utc::now().timestamp();
                                    let key =
                                        super::placeholder_controller::PlaceholderKey {
                                            provider: provider.clone(),
                                            channel_id,
                                            message_id: current_msg_id,
                                        };
                                    let input_payload =
                                        super::placeholder_controller::PlaceholderActiveInput {
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
                            if !restart_followup_pending && is_dcserver_restart_command(&input) {
                                let mut report = RestartCompletionReport::new(
                                    provider.clone(),
                                    channel_id.get(),
                                    "pending",
                                    format!(
                                        "dcserver restart requested by `{}`; 새 프로세스가 후속 보고를 이어받을 예정입니다.",
                                        request_owner_name
                                    ),
                                );
                                report.current_msg_id = Some(current_msg_id.get());
                                report.channel_name = adk_session_name.clone();
                                if save_restart_report(&report).is_ok() {
                                    restart_followup_pending = true;
                                    inflight_state.set_restart_mode(
                                        crate::services::discord::InflightRestartMode::DrainRestart,
                                    );
                                    let handoff_text = "♻️ dcserver 재시작 중...\n\n재시작 후 현재 turn은 자동 새 턴으로 이어가지 않고, 상태만 다시 확인합니다.";
                                    let _ = gateway
                                        .edit_message(channel_id, current_msg_id, handoff_text)
                                        .await;
                                    last_edit_text = handoff_text.to_string();
                                    inflight_state.current_msg_id = current_msg_id.get();
                                    inflight_state.current_msg_len = handoff_text.len();
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
                        StreamMessage::ToolResult {
                            content,
                            is_error,
                            tool_use_id,
                        } => {
                            // #3084: resolve the originating tool by id when the
                            // result carries one (pairing a delayed subagent
                            // result to its own ToolUse); otherwise fall back to
                            // FIFO order for id-less backends.
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
                            // #1084: flag oversize tool outputs + record metrics.
                            // Never mutates `content` — the agent and transcript
                            // still see the raw output; only a warn log + counters
                            // fire when thresholds are exceeded.
                            let _ = crate::services::tool_output_guard::observe(
                                last_tool_name.as_deref(),
                                is_error,
                                &content,
                            );
                            if is_error {
                                record_placeholder_live_event(
                                    &shared_owned,
                                    channel_id,
                                    super::placeholder_live_events::RecentPlaceholderEvent::tool_error(
                                        &content,
                                    ),
                                );
                            }
                            status_panel_dirty |= record_status_panel_events(
                                shared_owned.as_ref(),
                                channel_id,
                                super::placeholder_live_events::status_events_from_tool_result_with_id(
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
                                    super::formatting::LongRunningCloseTrigger::MonitorLike
                                );
                                // codex round-6 P2: only the FIRST ToolResult
                                // after the background ToolUse can be its
                                // dispatch ack. Subsequent ToolResults belong
                                // to other foreground tools and must not close
                                // the still-running background card. Once the
                                // ack is consumed, re-stash unconditionally
                                // until `Done`/cancel.
                                let is_dispatch_ack =
                                    !monitor_like && !ack_consumed;
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
                                    let target = if is_error {
                                        super::placeholder_controller::PlaceholderLifecycle::Aborted
                                    } else {
                                        super::placeholder_controller::PlaceholderLifecycle::Completed
                                    };
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
                                    } else {
                                        let outcome = shared_owned
                                            .ui.placeholder_controller
                                            .transition(gateway.as_ref(), key.clone(), target)
                                            .await;
                                        // codex round-10 P2: only clear flag on
                                        // committed/already-terminal outcome.
                                        use super::placeholder_controller::PlaceholderControllerOutcome::*;
                                        if matches!(outcome, Edited | Coalesced | AlreadyTerminal)
                                        {
                                            inflight_state.long_running_placeholder_active = false;
                                            state_dirty = true;
                                        } else {
                                            // EditFailed — keep the placeholder
                                            // active so the next event/sweeper
                                            // can retry the terminal edit.
                                            long_running_placeholder_active =
                                                Some((key, snapshot, close_trigger, ack_consumed));
                                        }
                                    }
                                } else {
                                    // Successful background dispatch ack OR a
                                    // later unrelated ToolResult — re-stash so
                                    // `Done`/cancel can close the card. Mark
                                    // the ack consumed so future is_error
                                    // results from other tools don't abort us.
                                    let active_key_for_pending_update = key.clone();
                                    long_running_placeholder_active = Some((
                                        key,
                                        snapshot,
                                        close_trigger,
                                        true,
                                    ));
                                    if let Some((
                                        pending_key,
                                        _,
                                        _,
                                        pending_ack_consumed,
                                        _,
                                    )) = pending_long_running_retarget_after_state_save.as_mut()
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
                                    super::formatting::LongRunningCloseTrigger::MonitorLike
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
                                super::formatting::preserve_previous_tool_status(
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
                        StreamMessage::TaskNotification { tool_use_id, summary, status, kind, .. } => {
                            inflight_state.task_notification_kind =
                                merge_task_notification_kind(inflight_state.task_notification_kind, kind);
                            state_dirty = true;
                            record_placeholder_live_event(
                                shared_owned.as_ref(),
                                channel_id,
                                super::placeholder_live_events::RecentPlaceholderEvent::task_notification(
                                    kind.as_str(),
                                    &status,
                                    &summary,
                                ),
                            );
                            status_panel_dirty |= record_status_panel_events(
                                shared_owned.as_ref(),
                                channel_id,
                                super::placeholder_live_events::status_events_from_task_notification_with_tool_use_id(
                                    kind.as_str(),
                                    &status,
                                    &summary,
                                    tool_use_id.as_deref(),
                                ),
                            );
                            if single_message_panel_footer_mode {
                                let indicator =
                                    super::single_message_panel::single_message_panel_spinner_frame(
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
                        StreamMessage::Done {
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
                                let _ = save_inflight_state(&inflight_state);
                            }
                            // #1255: turn finished while a long-running placeholder
                            // is still Active — close it now so the user does not
                            // stare at a stale 🔄 card. Idempotent if a prior
                            // ToolResult already fired Completed.
                            if let Some((key, snapshot, close_trigger, ack_consumed)) =
                                long_running_placeholder_active.take()
                            {
                                let target = if session_died_retry {
                                    super::placeholder_controller::PlaceholderLifecycle::Aborted
                                } else {
                                    super::placeholder_controller::PlaceholderLifecycle::Completed
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
                                        .ui.placeholder_controller
                                        .transition(gateway.as_ref(), key.clone(), target)
                                        .await;
                                    // codex round-10/11 P2/P3: on `EditFailed`,
                                    // re-stash the tuple so subsequent
                                    // streaming/sweeper paths can retry the
                                    // terminal edit. Only clear the persisted
                                    // flag on a committed (or already-terminal)
                                    // transition.
                                    use super::placeholder_controller::PlaceholderControllerOutcome::*;
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
                                    &mut inflight_state,
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
                                    channel = channel_id.get(),
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
                            // #2449 H4 graduation: only arm the 250ms drain
                            // window when handoff is genuinely ambiguous.
                            // If a runtime handoff has already been observed
                            // (`tmux_handed_off` flipped or
                            // `inflight_state.runtime_kind` stamped), the
                            // ownership question is already settled and any
                            // further drain just delays bridge exit by up to
                            // 250ms. The drain remains armed for
                            // warm-followup providers that emit `Done` before
                            // their handoff frame — in those cases the
                            // handoff arm clears the deadline to `None` as
                            // soon as the frame lands. Do not use persisted
                            // `runtime_kind` as this signal: fresh managed
                            // turns can be pre-stamped before the control
                            // frame arrives.
                            if !terminal_control_ready_observed {
                                terminal_control_drain_until = Some(
                                    std::time::Instant::now()
                                        + std::time::Duration::from_millis(250),
                                );
                            }
                        }
                        StreamMessage::Error {
                            message, stderr, ..
                        } => {
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
                                    &mut inflight_state,
                                );
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    "  [{ts}] ⚠ Clearing stored provider session after terminal {} session failure (channel {})",
                                    provider.as_str(),
                                    channel_id,
                                );
                            }
                            inflight_state.full_response = full_response.clone();
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
                        StreamMessage::StatusUpdate {
                            input_tokens,
                            cache_create_tokens,
                            cache_read_tokens,
                            output_tokens,
                            ..
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
                                    .ui.placeholder_live_events
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
                        StreamMessage::StatusEvents { events } => {
                            status_panel_dirty |= record_status_panel_events(
                                shared_owned.as_ref(),
                                channel_id,
                                events,
                            );
                        }
                        StreamMessage::TmuxReady {
                            output_path,
                            input_fifo_path,
                            tmux_session_name,
                            last_offset,
                        } => {
                            terminal_control_ready_observed = true;
                            tmux_last_offset = Some(last_offset);
                            inflight_state.runtime_kind =
                                Some(RuntimeHandoffKind::LegacyTmuxWrapper);
                            inflight_state.tmux_session_name = Some(tmux_session_name.clone());
                            inflight_state.output_path = Some(output_path.clone());
                            inflight_state.input_fifo_path =
                                Some(input_fifo_path).filter(|path| !path.is_empty());
                            inflight_state.last_offset = last_offset;

                            // #226: Atomic claim via try_claim_watcher
                            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let paused = Arc::new(std::sync::atomic::AtomicBool::new(true));
                            let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
                            let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(1));
                            let turn_delivered =
                                Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let last_heartbeat_ts_ms =
                                Arc::new(std::sync::atomic::AtomicI64::new(
                                    super::tmux_watcher_now_ms(),
                                ));
                            let handle = TmuxWatcherHandle {
                                tmux_session_name: tmux_session_name.clone(),
                                output_path: output_path.clone(),
                                paused: paused.clone(),
                                resume_offset: resume_offset.clone(),
                                cancel: cancel.clone(),
                                pause_epoch: pause_epoch.clone(),
                                turn_delivered: turn_delivered.clone(),
                                last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
                            };
                            #[cfg(unix)]
                            let (watcher_claimed, watcher_claim_replaced_existing) = {
                                // #1135: Reuse a live watcher for the same
                                // tmux session; replace only stale or
                                // different-session incumbents.
                                let claim = super::tmux::claim_or_reuse_watcher(
                                    &shared_owned.tmux_watchers,
                                    channel_id,
                                    handle,
                                    &provider,
                                    "turn_bridge_tmux_ready",
                                );
                                watcher_owner_channel_id = claim.owner_channel_id();
                                let _ = inflight_state.set_watcher_owner_channel_id(watcher_owner_channel_id.get());
                                (claim.should_spawn(), claim.replaced_existing())
                            };
                            #[cfg(not(unix))]
                            let (watcher_claimed, watcher_claim_replaced_existing) = {
                                let _ = handle;
                                (false, false)
                            };
                            #[cfg(unix)]
                            let mut watcher_ready_for_relay = !watcher_claimed;
                            #[cfg(not(unix))]
                            let mut watcher_ready_for_relay = false;
                            watcher_handoff_claim_outcome = if watcher_claimed {
                                WatcherHandoffClaimOutcome::Spawned
                            } else {
                                WatcherHandoffClaimOutcome::ReusedExisting
                            };
                            if watcher_claimed {
                                #[cfg(unix)]
                                {
                                    // Phase 5.3 of intake-node-routing
                                    // (issue #2011): on cluster-standby nodes
                                    // (no Discord gateway lease, no
                                    // `cached_serenity_ctx`), bypass the tmux
                                    // watcher entirely — its internal state
                                    // machine has multiple gateway-coupled
                                    // assumptions that prevent the relay step
                                    // from firing on standby (verified
                                    // 2026-05-10). Instead, leave
                                    // `watcher_relay_available_for_turn=false`
                                    // so the bridge delivers the response
                                    // itself via
                                    // `gateway.replace_message_with_outcome`
                                    // after the producer's `Done` event
                                    // populates `delivery_response`. The
                                    // bridge's REST gateway path already uses
                                    // `serenity_http_or_token_fallback()`
                                    // (Phase 5.2) so the post lands on Discord
                                    // even without the gateway runtime.
                                    //
                                    // Leader path is unchanged: when
                                    // `cached_serenity_ctx` is set, spawn the
                                    // watcher as before so streaming partial
                                    // output continues to work.
                                    let on_standby = shared_owned.http.cached_serenity_ctx.get().is_none();
                                    if on_standby {
                                        // Phase 5.3 of intake-node-routing (issue #2011):
                                        // skip the watcher entirely on standby and
                                        // spawn the standalone JSONL → Discord relay
                                        // task instead. The watcher's leader-only
                                        // state machine prevents its relay step from
                                        // firing on standby nodes; bypassing it
                                        // sidesteps an entire class of
                                        // gateway-coupling bugs.
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::info!(
                                            "  [{ts}] ⏭ standby relay: skipping tmux watcher spawn for channel {}; spawning JSONL→Discord standby_relay",
                                            channel_id
                                        );
                                        // Drop the registered watcher slot so a
                                        // subsequent turn does not falsely reuse
                                        // a "live" watcher that we never spawned.
                                        // Do NOT call `cancel.store(true)` on the
                                        // returned handle: the inner cancel Arc
                                        // is shared with the local `cancel` and
                                        // would pre-cancel the standby_relay we
                                        // are about to spawn (Codex P1 review on
                                        // PR #2012). The cancel Arc is otherwise
                                        // unused on this branch since no watcher
                                        // task ever reads it.
                                        let _ = shared_owned
                                            .tmux_watchers
                                            .remove(&watcher_owner_channel_id);
                                        if let Some(http_for_standby) =
                                            shared_owned.serenity_http_or_token_fallback()
                                        {
                                            let placeholder_msg_id_opt =
                                                if inflight_state.current_msg_id == 0 {
                                                    None
                                                } else {
                                                    Some(serenity::MessageId::new(
                                                        inflight_state.current_msg_id,
                                                    ))
                                            };
                                            let output_path_for_standby = output_path.clone();
                                            let turn_binding_for_standby = super::standby_relay::StandbyRelayTurnBinding::from_state(
                                                &inflight_state,
                                            );
                                            // Use a fresh cancel Arc, independent
                                            // from the watcher's `cancel` (which
                                            // is shared via `handle.cancel`).
                                            let cancel_for_standby = Arc::new(
                                                std::sync::atomic::AtomicBool::new(false),
                                            );
                                            let shared_for_standby = shared_owned.clone();
                                            let provider_for_standby = provider.clone();
                                            super::task_supervisor::spawn_observed(
                                                "turn_bridge_runtime_standby_relay",
                                                super::standby_relay::run_standby_relay(
                                                http_for_standby,
                                                channel_id,
                                                placeholder_msg_id_opt,
                                                output_path_for_standby,
                                                turn_binding_for_standby,
                                                last_offset,
                                                cancel_for_standby,
                                                shared_for_standby,
                                                provider_for_standby,
                                                // #2448: see TmuxReady branch
                                                // — timeout demoted to safety
                                                // backstop after broadcast
                                                // exit signal landed.
                                                std::time::Duration::from_secs(1800),
                                                ),
                                            );
                                            standby_relay_owns_output = true;
                                            inflight_state.set_relay_owner_kind(
                                                super::inflight::RelayOwnerKind::StandbyRelay,
                                            );
                                            // #2263: see the helper-fn
                                            // `handle_watcher_runtime_handoff`
                                            // standby branch — intentionally
                                            // leave `watcher_owns_live_relay = false`
                                            // because the standby_relay task
                                            // is not a tmux watcher, and the
                                            // yield-gate flag would over-claim
                                            // ownership for a watcher restored
                                            // by a different node, risking
                                            // duplicate Discord delivery.
                                            // Per-turn delivery ownership is
                                            // tracked both locally by
                                            // `standby_relay_owns_output` and
                                            // durably by `relay_owner_kind`.
                                            let _ = save_inflight_state(&inflight_state);
                                        } else {
                                            let ts = chrono::Local::now().format("%H:%M:%S");
                                            tracing::warn!(
                                                "  [{ts}] ⚠ standby relay skipped: no Http source for channel {}",
                                                channel_id
                                            );
                                        }
                                        // Leave watcher_relay_available_for_turn=false
                                        // and watcher_ready_for_relay=false so the
                                        // bridge does NOT delegate to a non-existent
                                        // watcher. The standby_relay task delivers
                                        // the response independently.
                                    } else if let Some(http_bg) = shared_owned.serenity_http_or_token_fallback() {
                                        let shared_bg = shared_owned.clone();
                                        inflight_state.set_relay_owner_kind(
                                            super::inflight::RelayOwnerKind::Watcher,
                                        );
                                        let restored_turn =
                                            super::tmux::restored_watcher_turn_from_inflight(
                                                &inflight_state,
                                                &tmux_session_name,
                                                true,
                                            );
                                        if let Ok(mut guard) = resume_offset.lock() {
                                            *guard = Some(last_offset);
                                        }
                                        turn_delivered.store(false, Ordering::Relaxed);
                                        if watcher_claim_replaced_existing {
                                            shared_owned.record_tmux_watcher_reconnect(channel_id);
                                        }
                                        super::task_supervisor::spawn_observed_tmux_watcher(
                                            "turn_bridge_runtime_tmux_output_watcher_with_restore",
                                            shared_bg.clone(),
                                            tmux_session_name.clone(),
                                            cancel.clone(),
                                            super::tmux::tmux_output_watcher_with_restore(
                                            channel_id,
                                            http_bg,
                                            shared_bg,
                                            output_path,
                                            tmux_session_name,
                                            last_offset,
                                            cancel,
                                            paused,
                                            resume_offset,
                                            pause_epoch,
                                            turn_delivered,
                                            last_heartbeat_ts_ms,
                                            restored_turn,
                                            ),
                                        );
                                        watcher_relay_available_for_turn = true;
                                        let _ = save_inflight_state(&inflight_state);
                                        watcher_ready_for_relay = true;
                                    } else {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ no Http source (neither cached_serenity_ctx nor cached_bot_token); tmux watcher not started for channel {}",
                                            channel_id
                                        );
                                        if let Some((_, handle)) =
                                            shared_owned
                                                .tmux_watchers
                                                .remove(&watcher_owner_channel_id)
                                        {
                                            handle.cancel.store(true, Ordering::Relaxed);
                                        }
                                    }
                                }
                            }
                            if watcher_ready_for_relay {
                                tmux_handed_off = true;
                                inflight_state.set_relay_owner_kind(
                                    super::inflight::RelayOwnerKind::Watcher,
                                );
                                watcher_owns_assistant_relay = true;
                                let _ = save_inflight_state(&inflight_state);
                                if let Some(watcher) =
                                    shared_owned.tmux_watchers.get(&watcher_owner_channel_id)
                                {
                                    watcher_relay_available_for_turn = true;
                                    if let Ok(mut guard) = watcher.resume_offset.lock() {
                                        *guard = Some(last_offset);
                                    }
                                    watcher.turn_delivered.store(false, Ordering::Relaxed);
                                    // #1452 (Codex P1): publish the mailbox-finalization
                                    // debt BEFORE unpausing the watcher.
                                    //
                                    // The watcher's terminal `swap(false, AcqRel)` runs
                                    // as soon as it sees a Done event; if we delayed
                                    // the store until the bridge's later delegation
                                    // decision (line 2419+), the watcher could swap
                                    // first, observe `false`, skip `mailbox_finish_turn`,
                                    // and the bridge's late `store(true)` would leave
                                    // stale debt that either keeps `cancel_token`
                                    // permanently set OR is consumed by a future
                                    // watcher event for the WRONG turn.
                                    //
                                    // #3016 phase-5b2: the legacy
                                    // `mailbox_finalize_owed` store that used to
                                    // publish "bridge will delegate finalization"
                                    // here is removed; the `register_start` below
                                    // (RelayOwnerKind::Watcher) is the ledger
                                    // authority that replaced it.
                                    // #3016 phase 3: register the turn with the
                                    // single-authority finalizer BEFORE
                                    // unpausing the watcher — same as the
                                    // `handle_watcher_runtime_handoff` helper.
                                    // This legacy `StreamMessage::TmuxReady`
                                    // handoff does NOT go through that helper, so
                                    // without this the watcher terminal would
                                    // have no Watcher-owned ledger entry — and
                                    // a busy-pane gate-timeout would finalize
                                    // immediately instead of arming the
                                    // deadline-backstop. Registering here with
                                    // the same finalizer id makes it defer.
                                    shared_owned.turn_finalizer.register_start(
                                        super::turn_finalizer::TurnKey::new(
                                            channel_id,
                                            inflight_state.effective_finalizer_turn_id(),
                                            shared_owned.restart.current_generation,
                                        ),
                                        provider.clone(),
                                        super::inflight::RelayOwnerKind::Watcher,
                                        // #3016 phase-5a: prime the reconcile cache
                                        // at register time.
                                        &shared_owned,
                                    );
                                    // #1452 (Codex iter 3 P1) / #3016 phase-5b2:
                                    // unpause uses Release ordering so a watcher
                                    // observing `paused = false` is guaranteed to
                                    // also observe the prior writes — the
                                    // `register_start` (RelayOwnerKind::Watcher)
                                    // ledger entry that now drives the
                                    // gate-timeout defer. With Relaxed ordering on
                                    // a weakly-ordered platform the writes could
                                    // be reordered, letting the watcher unpause
                                    // and submit a terminal before the ledger
                                    // knows the turn exists.
                                    watcher.paused.store(false, Ordering::Release);
                                }
                            }
                            state_dirty = true;
                            if done {
                                terminal_control_drain_until = None;
                            }
                        }
                        StreamMessage::RuntimeReady { handoff } => {
                            terminal_control_ready_observed = true;
                            match handoff {
                                RuntimeHandoff::LegacyTmuxWrapper {
                                    output_path,
                                    input_fifo_path,
                                    tmux_session_name,
                                    last_offset,
                                } => {
                                handle_watcher_runtime_handoff(
                                    &shared_owned,
                                    &provider,
                                    channel_id,
                                    &mut inflight_state,
                                    RuntimeHandoffKind::LegacyTmuxWrapper,
                                    output_path,
                                    Some(input_fifo_path),
                                    tmux_session_name,
                                    last_offset,
                                    &mut tmux_last_offset,
                                    &mut watcher_owner_channel_id,
                                    &mut standby_relay_owns_output,
                                    &mut watcher_relay_available_for_turn,
                                    &mut watcher_handoff_claim_outcome,
                                    &mut tmux_handed_off,
                                    &mut watcher_owns_assistant_relay,
                                    &mut state_dirty,
                                    done,
                                    &mut terminal_control_drain_until,
                                );
                            }
                            RuntimeHandoff::ClaudeTui {
                                transcript_path,
                                tmux_session_name,
                                last_offset,
                            } => {
                                handle_watcher_runtime_handoff(
                                    &shared_owned,
                                    &provider,
                                    channel_id,
                                    &mut inflight_state,
                                    RuntimeHandoffKind::ClaudeTui,
                                    transcript_path,
                                    None,
                                    tmux_session_name,
                                    last_offset,
                                    &mut tmux_last_offset,
                                    &mut watcher_owner_channel_id,
                                    &mut standby_relay_owns_output,
                                    &mut watcher_relay_available_for_turn,
                                    &mut watcher_handoff_claim_outcome,
                                    &mut tmux_handed_off,
                                    &mut watcher_owns_assistant_relay,
                                    &mut state_dirty,
                                    done,
                                    &mut terminal_control_drain_until,
                                );
                            }
                            RuntimeHandoff::CodexTui {
                                rollout_path,
                                thread_id,
                                tmux_session_name,
                                last_offset,
                            } => {
                                if let Some(thread_id) = thread_id {
                                    inflight_state.session_id = Some(thread_id);
                                }
                                handle_watcher_runtime_handoff(
                                    &shared_owned,
                                    &provider,
                                    channel_id,
                                    &mut inflight_state,
                                    RuntimeHandoffKind::CodexTui,
                                    rollout_path,
                                    None,
                                    tmux_session_name,
                                    last_offset,
                                    &mut tmux_last_offset,
                                    &mut watcher_owner_channel_id,
                                    &mut standby_relay_owns_output,
                                    &mut watcher_relay_available_for_turn,
                                    &mut watcher_handoff_claim_outcome,
                                    &mut tmux_handed_off,
                                    &mut watcher_owns_assistant_relay,
                                    &mut state_dirty,
                                    done,
                                    &mut terminal_control_drain_until,
                                );
                            }
                            RuntimeHandoff::ProcessBackend {
                                output_path,
                                session_name,
                                last_offset,
                            } => {
                                tmux_last_offset = Some(last_offset);
                                inflight_state.runtime_kind =
                                    Some(RuntimeHandoffKind::ProcessBackend);
                                inflight_state.tmux_session_name = Some(session_name);
                                inflight_state.output_path = Some(output_path);
                                inflight_state.input_fifo_path = None;
                                inflight_state.last_offset = last_offset;
                                state_dirty = true;
                                // #2235: see CodexTui arm — durable stamp of
                                // runtime_kind across a bridge-crash window.
                                let _ = save_inflight_state(&inflight_state);
                                if done {
                                    terminal_control_drain_until = None;
                                }
                                }
                            RuntimeHandoff::ClaudeEAdapter {
                                output_path,
                                session_name,
                                last_offset,
                            } => {
                                // Phase 1 of the claude-e rollout (see
                                // `docs/claude-e-rollout/`). The adapter
                                // is a per-turn PTY spawn — no tmux pane
                                // backs it, so `tmux_session_name` must
                                // stay `None` to satisfy the
                                // `inflight_tmux_one_to_one` invariant
                                // when a channel switches between TUI
                                // and claude-e. `session_name` is the
                                // logical adapter id (Claude session uuid
                                // or `claude-e-{pid}`); it does not map
                                // to a tmux pane and is intentionally
                                // not stamped here.
                                let _ = session_name;
                                tmux_last_offset = Some(last_offset);
                                inflight_state.runtime_kind =
                                    Some(RuntimeHandoffKind::ClaudeEAdapter);
                                inflight_state.tmux_session_name = None;
                                inflight_state.output_path = Some(output_path);
                                inflight_state.input_fifo_path = None;
                                inflight_state.last_offset = last_offset;
                                state_dirty = true;
                                let _ = save_inflight_state(&inflight_state);
                                if done {
                                    terminal_control_drain_until = None;
                                }
                            }
                            }
                        }
                        StreamMessage::ProcessReady {
                            output_path,
                            session_name,
                            last_offset,
                        } => {
                            terminal_control_ready_observed = true;
                            // ProcessBackend completed first turn.
                            // No tmux watcher needed — process sessions are monitored
                            // inline via SessionProbe::process during read_output_file_until_result.
                            // Do NOT set tmux_handed_off: ProcessBackend has no watcher,
                            // so the handoff cleanup path would delete the placeholder
                            // with no one to send the final response.
                            tmux_last_offset = Some(last_offset);
                            inflight_state.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
                            inflight_state.tmux_session_name = Some(session_name);
                            inflight_state.output_path = Some(output_path);
                            inflight_state.input_fifo_path = None;
                            inflight_state.last_offset = last_offset;
                            state_dirty = true;
                            // #2235: persist runtime_kind stamp immediately —
                            // ProcessBackend has no watcher so we want the
                            // on-disk row to reflect the new backend before
                            // any potential bridge crash.
                            let _ = save_inflight_state(&inflight_state);
                            if done {
                                terminal_control_drain_until = None;
                            }
                        }
                        StreamMessage::OutputOffset { offset } => {
                            tmux_last_offset = Some(offset);
                            inflight_state.last_offset = offset;
                            maybe_refresh_active_turn_activity_heartbeat(
                                shared_owned.as_ref(),
                                &provider,
                                &inflight_state,
                                adk_session_name.as_deref(),
                                &mut last_activity_heartbeat_at,
                            );
                            state_dirty = true;
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

            if shared_owned.ui.status_panel_v2_enabled
                && last_session_panel_lifecycle_refresh.elapsed() >= status_interval
            {
                last_session_panel_lifecycle_refresh = tokio::time::Instant::now();
                status_panel_dirty |= refresh_session_panel_line_from_lifecycle(
                    shared_owned.as_ref(),
                    channel_id,
                    turn_id.as_str(),
                    inflight_state.tmux_session_name.as_deref(),
                    &provider, // #3983 item4: one-shot session banner render
                )
                .await;
            }

            let indicator = SPINNER[spin_idx % SPINNER.len()];
            spin_idx += 1;

            // #3813 Phase 2: hold the status-panel / footer edit off the shared
            // rate lane while the opening answer is pending so the #4006 fast lane
            // wins it. `status_panel_dirty` stays set → renders next interval. See
            // status_panel_edit_defer_for_first_answer for the #3477 guard.
            let defer_status_panel_for_first_answer = status_panel_edit_defer_for_first_answer(
                first_answer_relayed,
                !response_portion_after_offset(&full_response, response_sent_offset).is_empty(),
            );

            if shared_owned.ui.status_panel_v2_enabled
                && bridge_status_panel_dirty_should_edit_separate_panel(
                    status_panel_dirty,
                    single_message_panel_footer_mode,
                )
                && !defer_status_panel_for_first_answer
                && last_status_panel_edit.elapsed() >= status_interval
                && let Some(status_msg_id) = status_panel_msg_id
            {
                let panel_text = shared_owned.ui.placeholder_live_events.render_status_panel(
                    channel_id,
                    &provider,
                    status_panel_started_at,
                );
                if panel_text != last_status_panel_text {
                    match gateway
                        .edit_message(channel_id, status_msg_id, &panel_text)
                        .await
                    {
                        Ok(()) => {
                            last_status_panel_text = panel_text;
                            last_status_panel_edit = tokio::time::Instant::now();
                            inflight_state.status_message_id = Some(status_msg_id.get());
                            state_dirty = true;
                        }
                        Err(error) => {
                            tracing::warn!(
                                "[turn_bridge] failed to edit status-panel-v2 message {} in channel {}: {}",
                                status_msg_id,
                                channel_id,
                                error
                            );
                        }
                    }
                }
                status_panel_dirty = false;
            }
            if single_message_panel_footer_mode
                && status_panel_dirty
                && !defer_status_panel_for_first_answer
                && last_status_panel_edit.elapsed() >= status_interval
            {
                refresh_bridge_footer(
                    shared_owned.as_ref(),
                    channel_id,
                    footer_owner,
                    indicator,
                )
                .await;
                last_status_panel_edit = tokio::time::Instant::now();
                status_panel_dirty = false;
            }
            if !watcher_owns_assistant_relay && !standby_relay_owns_output {
                // #3805 P2 (PR-D): track whether an answer rollover created a fresh
                // tail message this interval, so the two-message status panel is
                // re-anchored BELOW it exactly once (not on quiet intervals).
                let mut rolled_over_this_interval = false;
                loop {
                    let current_portion =
                        response_portion_after_offset(&full_response, response_sent_offset);
                    // #3813 AC#1 tail: mark first-output pre-rollover (first_output<=first_relay).
                    bridge_spans.mark_first_output(!current_portion.is_empty());
                    if done || current_portion.is_empty() {
                        break;
                    }

                    let indicator = SPINNER[spin_idx % SPINNER.len()];
                    let status_block = build_bridge_single_message_panel_status_block(
                        shared_owned.as_ref(),
                        channel_id,
                        &provider,
                        status_panel_started_at,
                        indicator,
                        prev_tool_status.as_deref(),
                        current_tool_line.as_deref(),
                        &full_response,
                    );
                    if bridge_streaming_rollover_should_skip(current_portion) {
                        break;
                    }
                    let Some(plan) =
                        super::formatting::plan_streaming_rollover(current_portion, &status_block)
                    else {
                        break;
                    };

                    match gateway
                        .edit_message(channel_id, current_msg_id, &plan.frozen_chunk)
                        .await
                    {
                        Ok(()) => match gateway.send_message(channel_id, &status_block).await {
                            Ok(next_msg_id) => {
                                let next_response_sent_offset =
                                    response_sent_offset + plan.split_at;
                                assert_response_sent_offset_progress(
                                    &provider,
                                    channel_id,
                                    dispatch_id.as_deref(),
                                    adk_session_key.as_deref(),
                                    &turn_id,
                                    response_sent_offset,
                                    next_response_sent_offset,
                                    &full_response,
                                    "src/services/discord/turn_bridge/mod.rs:rollover_response_sent_offset",
                                );
                                response_sent_offset = next_response_sent_offset;
                                streaming_rollover_frozen_msg_ids.push(current_msg_id);
                                current_msg_id = next_msg_id;
                                rolled_over_this_interval = true;
                                last_edit_text = status_block;
                                last_status_edit = tokio::time::Instant::now() - status_interval;
                                inflight_state.current_msg_id = current_msg_id.get();
                                inflight_state.current_msg_len = last_edit_text.len();
                                inflight_state.response_sent_offset = response_sent_offset;
                                inflight_state.full_response = full_response.clone();
                                state_dirty = true;
                                // #3813 AC#1 tail: rollover send = bridge first relay.
                                bridge_spans.mark_first_relay(true);
                                if let Some((_, _, _, _, pending_new_key)) =
                                    pending_long_running_retarget_after_state_save.as_mut()
                                {
                                    *pending_new_key =
                                        super::placeholder_controller::PlaceholderKey {
                                            provider: provider.clone(),
                                            channel_id,
                                            message_id: current_msg_id,
                                        };
                                }
                                if let Some((pending_key, _, _, _)) =
                                    pending_long_running_open_after_state_save.as_mut()
                                {
                                    pending_key.message_id = current_msg_id;
                                }
                                // #1255: rollover retargets the controller to the
                                // new message and detaches the old key first.
                                if let Some((old_key, snapshot, close_trigger, ack_consumed)) =
                                    long_running_placeholder_active.as_ref()
                                {
                                    let new_key = super::placeholder_controller::PlaceholderKey {
                                        provider: provider.clone(),
                                        channel_id,
                                        message_id: current_msg_id,
                                    };
                                    pending_long_running_retarget_after_state_save =
                                        Some((
                                            old_key.clone(),
                                            snapshot.clone(),
                                            *close_trigger,
                                            *ack_consumed,
                                            new_key,
                                        ));
                                    state_dirty = true;
                                }
                            }
                            Err(error) => {
                                tracing::warn!(
                                    "[discord] failed to create rollover placeholder in channel {}: {}",
                                    channel_id,
                                    error
                                );
                                let _ = gateway
                                    .edit_message(channel_id, current_msg_id, &plan.display_snapshot)
                                    .await;
                                last_edit_text = plan.display_snapshot;
                                break;
                            }
                        },
                        Err(error) => {
                            tracing::warn!(
                                "[discord] failed to freeze rollover chunk for message {} in channel {}: {}",
                                current_msg_id,
                                channel_id,
                                error
                            );
                            break;
                        }
                    }
                }

                // #3805 P2 (PR-D): after a mid-turn answer rollover the live status
                // panel is now stranded ABOVE the new tail answer chunk. Under the
                // two-message flag, re-anchor it BELOW the new answer (send new,
                // retire old, bump the generation epoch) so it stays pinned to the
                // latest chunk. Gate is OFF-inert → the rollover path is
                // byte-identical when the flag is off.
                if rolled_over_this_interval
                    && two_message_panel::two_message_should_reanchor_panel_on_rollover(
                        shared_owned.ui.two_message_panel_enabled,
                        status_panel_msg_id.is_some(),
                    )
                {
                    let panel_text = shared_owned.ui.placeholder_live_events.render_status_panel(
                        channel_id,
                        &provider,
                        status_panel_started_at,
                    );
                    let reanchored =
                        two_message_panel::reanchor_bridge_two_message_status_panel_below_answer(
                            gateway.as_ref(),
                            shared_owned.as_ref(),
                            channel_id,
                            &provider,
                            &panel_text,
                            current_msg_id,
                            &mut status_panel_msg_id,
                            &mut inflight_state,
                            &mut status_panel_generation,
                            &mut last_status_panel_text,
                        )
                        .await;
                    if reanchored {
                        state_dirty = true;
                    }
                }

                let current_portion =
                    response_portion_after_offset(&full_response, response_sent_offset);
                let status_block = build_bridge_single_message_panel_status_block(
                    shared_owned.as_ref(),
                    channel_id,
                    &provider,
                    status_panel_started_at,
                    indicator,
                    prev_tool_status.as_deref(),
                    current_tool_line.as_deref(),
                    &full_response,
                );
                let stable_display_text = build_turn_bridge_streaming_edit_text(
                    shared_owned.ui.status_panel_v2_enabled,
                    current_portion,
                    &status_block,
                    &provider,
                );

                if super::single_message_panel::streaming_footer_text_changed(
                    single_message_panel_footer_mode,
                    &last_edit_text,
                    &stable_display_text,
                )
                    && !done
                    && bridge_streaming_edit_gate_open(
                        last_status_edit.elapsed() >= status_interval,
                        first_answer_relayed,
                        current_portion.is_empty(),
                    )
                    && long_running_placeholder_active.is_none()
                    && pending_long_running_open_after_state_save.is_none()
                    && pending_long_running_retarget_after_state_save.is_none()
                {
                    let edit_ok = TurnGateway::edit_message(
                        gateway.as_ref(),
                        channel_id,
                        current_msg_id,
                        &stable_display_text,
                    )
                    .await
                    .is_ok();
                    last_status_edit = tokio::time::Instant::now();
                    if edit_ok {
                        first_answer_relayed |= !current_portion.is_empty();
                        // #3813 AC#1 tail: first bridge-owned relay delivered.
                        bridge_spans.mark_first_relay(!current_portion.is_empty());
                        last_edit_text = stable_display_text;
                        inflight_state.current_msg_id = current_msg_id.get();
                        inflight_state.current_msg_len = last_edit_text.len();
                        inflight_state.response_sent_offset = response_sent_offset;
                        inflight_state.full_response = full_response.clone();
                        state_dirty = true;
                    }
                }
            }

            if shared_owned.ui.placeholder_live_events_enabled
                && watcher_owns_assistant_relay
                && let Some((key, input, _, _)) = long_running_placeholder_active.as_ref()
                && let Some(block) = shared_owned.ui.placeholder_live_events.render_block(channel_id)
            {
                let outcome = shared_owned
                    .ui.placeholder_controller
                    .ensure_active_with_live_events(
                        gateway.as_ref(),
                        key.clone(),
                        input.clone(),
                        block,
                    )
                    .await;
                if matches!(
                    outcome,
                    super::placeholder_controller::PlaceholderControllerOutcome::Edited
                ) {
                    state_dirty = true;
                }
            }

            if bridge_should_reclaim_relay_from_missing_watcher(
                watcher_owns_assistant_relay,
                standby_relay_owns_output,
                live_watcher_registered_for_relay(shared_owned.as_ref(), watcher_owner_channel_id),
            ) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ turn_bridge reclaiming assistant relay for channel {} after watcher disappeared",
                    channel_id.get()
                );
                watcher_owns_assistant_relay = false;
                watcher_relay_available_for_turn = false;
                inflight_state.set_relay_owner_kind(super::inflight::RelayOwnerKind::None);
                state_dirty = true;
            }

            if state_dirty
                || pending_long_running_open_after_state_save.is_some()
                || pending_long_running_retarget_after_state_save.is_some()
                || inflight_state.current_tool_line != current_tool_line
                || inflight_state.last_tool_name != last_tool_name
                || inflight_state.last_tool_summary != last_tool_summary
                || inflight_state.prev_tool_status != prev_tool_status
            {
                inflight_state.current_tool_line = current_tool_line.clone();
                inflight_state.last_tool_name = last_tool_name.clone();
                inflight_state.last_tool_summary = last_tool_summary.clone();
                inflight_state.prev_tool_status = prev_tool_status.clone();
                match save_inflight_state(&inflight_state) {
                    Ok(()) => {
                        if let Some((key, snapshot, close_trigger, ack_consumed)) =
                            pending_long_running_open_after_state_save.take()
                        {
                            if key.message_id == current_msg_id
                                && long_running_placeholder_active.is_none()
                            {
                                let outcome = ensure_active_placeholder_card(
                                    shared_owned.as_ref(),
                                    gateway.as_ref(),
                                    key.clone(),
                                    snapshot.clone(),
                                )
                                .await;
                                use super::placeholder_controller::PlaceholderControllerOutcome::*;
                                if matches!(outcome, Edited | Coalesced) {
                                    long_running_placeholder_active =
                                        Some((key, snapshot, close_trigger, ack_consumed));
                                } else {
                                    inflight_state.long_running_placeholder_active = false;
                                    if let Err(error) = save_inflight_state(&inflight_state) {
                                        tracing::warn!(
                                            "[turn_bridge] failed to persist long-running placeholder open failure in channel {}: {}",
                                            channel_id,
                                            error
                                        );
                                    }
                                }
                            } else {
                                inflight_state.long_running_placeholder_active = false;
                                if let Err(error) = save_inflight_state(&inflight_state) {
                                    tracing::warn!(
                                        "[turn_bridge] failed to persist stale long-running placeholder open drop in channel {}: {}",
                                        channel_id,
                                        error
                                    );
                                }
                            }
                        }
                        if let Some((
                            old_key,
                            snapshot,
                            close_trigger,
                            ack_consumed,
                            new_key,
                        )) = pending_long_running_retarget_after_state_save.take()
                        {
                            let active_still_matches_old_key = long_running_placeholder_active
                                .as_ref()
                                .is_some_and(|(active_key, _, _, _)| *active_key == old_key);
                            if active_still_matches_old_key {
                                shared_owned.ui.placeholder_controller.detach(&old_key);
                                let outcome = ensure_active_placeholder_card(
                                    shared_owned.as_ref(),
                                    gateway.as_ref(),
                                    new_key.clone(),
                                    snapshot.clone(),
                                )
                                .await;
                                use super::placeholder_controller::PlaceholderControllerOutcome::*;
                                if matches!(outcome, Edited | Coalesced) {
                                    long_running_placeholder_active =
                                        Some((new_key, snapshot, close_trigger, ack_consumed));
                                } else {
                                    // Retarget edit failed — drop the flag so the
                                    // regular streaming loop and sweeper resume
                                    // normal handling.
                                    long_running_placeholder_active = None;
                                    inflight_state.long_running_placeholder_active = false;
                                    if let Err(error) = save_inflight_state(&inflight_state) {
                                        tracing::warn!(
                                            "[turn_bridge] failed to persist long-running placeholder retarget failure in channel {}: {}",
                                            channel_id,
                                            error
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            "[turn_bridge] failed to persist inflight state before moving placeholder pin in channel {}: {}",
                            channel_id,
                            error
                        );
                    }
                }
            }

            if last_adk_heartbeat.elapsed() >= std::time::Duration::from_secs(30) {
                post_adk_session_status(
                    adk_session_key.as_deref(),
                    adk_session_name.as_deref(),
                    Some(provider.as_str()),
                    "working",
                    &provider,
                    adk_session_info.as_deref(),
                    None,
                    adk_cwd.as_deref(),
                    dispatch_id.as_deref(),
                    adk_session_name.as_deref().and_then(
                        crate::services::discord::adk_session::parse_thread_channel_id_from_name,
                    ),
                    Some(channel_id),
                    role_binding
                        .as_ref()
                        .map(|binding| binding.role_id.as_str()),
                    shared_owned.api_port,
                )
                .await;
                last_adk_heartbeat = std::time::Instant::now();
            }

            // codex round-8 P1: keep `placeholder_sweeper` from abandoning a
            // healthy long-running tool wait by bumping inflight mtime every
            // 30s while a placeholder is owned. If the turn dies, this loop
            // stops firing → mtime stops advancing → sweeper can abandon
            // normally past `ABANDON_THRESHOLD_SECS`.
            if long_running_placeholder_active.is_some()
                && last_inflight_long_run_heartbeat.elapsed()
                    >= LIVE_LONG_RUN_HEARTBEAT_INTERVAL
            {
                inflight_state.updated_at = chrono::Utc::now().to_rfc3339();
                let _ = save_inflight_state(&inflight_state);
                last_inflight_long_run_heartbeat = std::time::Instant::now();
            }
        }

        // #3813 AC#1 tail: emit bridge-side latency spans once at loop exit
        // (observation-only; self-suppresses when no bridge relay happened).
        bridge_spans.log(channel_id.get(), provider.as_str());

        // codex round-9 P3 on PR #1308: drain any active long-running
        // placeholder on stream-error / receive-disconnect exits too. The
        // cancel branch already drives the controller to `Aborted`; here we
        // also need to handle `StreamMessage::Error` and `rx_disconnected`
        // exits so the controller does not leak an `Active` row and the
        // persisted `long_running_placeholder_active` flag does not survive
        // for the sweeper to abandon the card. Skip when `cancelled` (the
        // dedicated cancel block below handles it) or when a relay owner will
        // continue the visible output lifecycle.
        let relay_owns_output_at_stream_end = standby_relay_owns_output
            || (rx_disconnected && tmux_handed_off && full_response.is_empty());
        if pending_long_running_open_after_state_save.take().is_some() {
            inflight_state.long_running_placeholder_active = false;
            let _ = save_inflight_state(&inflight_state);
        }
        if !cancelled && relay_owns_output_at_stream_end {
            let relay_owned_pending_retarget_matches_active =
                if let Some((active_key, _, _, _)) = long_running_placeholder_active.as_ref() {
                    pending_long_running_retarget_after_state_save
                        .as_ref()
                        .is_some_and(|(pending_key, _, _, _, _)| pending_key == active_key)
                } else {
                    false
                };
            if relay_owned_pending_retarget_matches_active
                && let Some((key, _, _, _)) = long_running_placeholder_active.take()
            {
                let _ = pending_long_running_retarget_after_state_save.take();
                shared_owned.ui.placeholder_controller.detach(&key);
                inflight_state.long_running_placeholder_active = false;
                let _ = save_inflight_state(&inflight_state);
            }
        }
        if !cancelled && !relay_owns_output_at_stream_end {
            if let Some((key, _, _, _)) = long_running_placeholder_active.take() {
                let target = if transport_error || rx_disconnected {
                    super::placeholder_controller::PlaceholderLifecycle::Aborted
                } else {
                    super::placeholder_controller::PlaceholderLifecycle::Completed
                };
                let pending_retarget_matches_key = pending_long_running_retarget_after_state_save
                    .as_ref()
                    .is_some_and(|(pending_key, _, _, _, _)| *pending_key == key);
                if pending_retarget_matches_key {
                    let _ = pending_long_running_retarget_after_state_save.take();
                    shared_owned.ui.placeholder_controller.detach(&key);
                    inflight_state.long_running_placeholder_active = false;
                } else {
                    let outcome = shared_owned
                        .ui.placeholder_controller
                        .transition(gateway.as_ref(), key, target)
                        .await;
                    // codex round-10 P2: keep the persisted flag on EditFailed so
                    // the sweeper can finalize the still-visible 🔄 card later.
                    use super::placeholder_controller::PlaceholderControllerOutcome::*;
                    if matches!(outcome, Edited | Coalesced | AlreadyTerminal) {
                        inflight_state.long_running_placeholder_active = false;
                    }
                }
                let _ = save_inflight_state(&inflight_state);
            }
            if transport_error || rx_disconnected {
                close_all_tracked_background_children(
                    shared_owned.pg_pool.as_ref(),
                    &mut active_background_child_session_ids,
                    "aborted",
                    "turn loop exit",
                )
                .await;
            }
        }

        // #1113 stream-end finalization: the main turn loop has exited, which
        // means we won't receive any more StreamMessage events for this turn.
        // If `current_tool_line` still carries the running ⚙ marker, the
        // ToolResult never landed (process exit, parser error, transport
        // disconnect, or relay ownership transfer for the rest of the
        // delivery). Promote it to its terminal ⚠ form and stash in
        // prev_tool_status so any follow-on placeholder edit (recovery notice,
        // recovery notice, terminal response) reflects the orphaned state.
        if let Some(running) = current_tool_line.as_deref() {
            if running.starts_with("⚙") {
                let finalized = super::formatting::finalize_in_progress_tool_status(running);
                super::formatting::preserve_previous_tool_status(
                    &mut prev_tool_status,
                    Some(finalized.as_str()),
                    None,
                );
                current_tool_line = Some(finalized);
                inflight_state.current_tool_line = current_tool_line.clone();
                inflight_state.prev_tool_status = prev_tool_status.clone();
                let _ = save_inflight_state(&inflight_state);
            }
        }

        let extracted_api_friction =
            crate::services::api_friction::extract_api_friction_reports(&full_response);
        if !extracted_api_friction.reports.is_empty() {
            api_friction_reports.extend(extracted_api_friction.reports);
            full_response = extracted_api_friction.cleaned_response;
            inflight_state.full_response = full_response.clone();
        }
        for error in extracted_api_friction.parse_errors {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ invalid API_FRICTION marker: {error}");
        }

        let claude_tui_followup_pre_submit_requeue_candidate = {
            let base = crate::services::claude::claude_tui_followup_requeue_enabled()
                && bridge_claude_tui_followup_requeue_prompt_error(
                    &provider,
                    inflight_state.runtime_kind,
                    &full_response,
                );
            // #3885 (reworked): a follow-up pre-submit readiness timeout normally
            // requeues the inflight ("prompt never reached the pane → safe to
            // retry"). The dup risk is re-injecting an input that ALREADY landed:
            // when the SAME input is the turn the pane is streaming (or just
            // completed), that turn already delivers the response, so a requeue
            // produces duplicate prose. The first cut gated on a channel-scoped
            // busy probe, which (a) DROPPED a genuinely-unsubmitted follow-up that
            // happened to sit behind a DIFFERENT streaming turn, and (b) missed
            // the already-completed same-input case (idle pane). Gate instead on
            // INPUT CORRELATION: suppress ONLY when the recorded prompt anchor for
            // this pane resolves to THIS inflight's user_msg_id (the relay records
            // the submitted prompt's anchor as the synthetic inflight's
            // user_msg_id; a non-consuming peek leaves it for the watcher). A
            // different / absent anchor means the follow-up is genuinely
            // unsubmitted, so it STILL requeues — the deferred idle-queue kickoff
            // is itself gated on pane-busy, so a follow-up behind a different
            // streaming turn is DEFERRED (preserved in the mailbox), not dropped.
            let same_input_occupies_pane = base
                && claude_tui_followup_same_input_occupies_pane(
                    inflight_state
                        .tmux_session_name
                        .as_deref()
                        .and_then(|tmux_session_name| {
                            crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                                provider.as_str(),
                                tmux_session_name,
                                channel_id.get(),
                            )
                        })
                        .map(|anchor| anchor.message_id),
                    inflight_state.user_msg_id,
                );
            claude_tui_followup_requeue_streaming_aware(base, same_input_occupies_pane)
        };
        if claude_tui_followup_pre_submit_requeue_candidate {
            full_response = CLAUDE_TUI_FOLLOWUP_REQUEUE_DELIVERY_NOTICE.to_string();
            inflight_state.full_response = full_response.clone();
        }

        let is_prompt_too_long = full_response.contains("__prompt too long__");
        let review_dispatch_warning = if !cancelled && !is_prompt_too_long {
            guard_review_dispatch_completion(
                shared_owned.api_port,
                dispatch_id.as_deref(),
                &full_response,
                provider.as_str(),
            )
            .await
        } else {
            None
        };
        if review_dispatch_warning.is_some() {
            crate::services::observability::emit_guard_fired(
                provider.as_str(),
                channel_id.get(),
                dispatch_id.as_deref(),
                adk_session_key.as_deref(),
                Some(turn_id.as_str()),
                "review_dispatch_pending",
            );
        }
        let terminal_error_path =
            cancelled || is_prompt_too_long || transport_error || recovery_retry;
        // A bridge rebuilt from durable state must honor the row's existing
        // relay owner. The pending-response guard below only applies to
        // in-process handoffs where the bridge may already own unsent bytes.
        let recovered_watcher_owns_output =
            matches!(initial_relay_owner_kind, super::inflight::RelayOwnerKind::Watcher)
                && watcher_owns_assistant_relay
                && watcher_relay_available_for_turn
                && !terminal_error_path;
        let response_unsent =
            response_portion_after_offset(&full_response, response_sent_offset);
        let response_pending_trimmed_empty = response_unsent.trim().is_empty();
        // #3268: `mut` so the post-gate self-healing handoff (below) can promote it.
        let mut bridge_relay_delegated_to_watcher = recovered_watcher_owns_output
            || should_delegate_bridge_relay_to_watcher(
                watcher_owns_assistant_relay,
                watcher_relay_available_for_turn,
                !response_pending_trimmed_empty,
                cancelled,
                is_prompt_too_long,
                transport_error,
                recovery_retry,
            );
        // #3268: `mut` so the post-gate self-healing handoff can promote it to
        // `WatcherRelay` once the gate confirms the pane is still busy.
        let mut bridge_output_owner = classify_bridge_output_owner(
            standby_relay_owns_output
                && !cancelled
                && !is_prompt_too_long
                && !transport_error
                && !recovery_retry,
            bridge_relay_delegated_to_watcher,
        );
        // #3281: empty-terminal-response visibility (owner-`None` kind/payload
        // preserved verbatim; adds the delegated-watcher quadrant) — see
        // `watcher_handoff::emit_bridge_empty_terminal_response_visibility`.
        watcher_handoff::emit_bridge_empty_terminal_response_visibility(
            shared_owned.as_ref(),
            watcher_owner_channel_id,
            bridge_output_owner,
            terminal_error_path,
            &provider,
            channel_id,
            dispatch_id.as_deref(),
            adk_session_key.as_deref(),
            turn_id.as_str(),
            current_msg_id.get(),
            response_pending_trimmed_empty,
            watcher_owns_assistant_relay,
            watcher_relay_available_for_turn,
            standby_relay_owns_output,
            rx_disconnected,
            tmux_handed_off,
            response_sent_offset,
            full_response.len(),
            tmux_last_offset,
            inflight_state.turn_start_offset,
        );

        // Explicitly complete implementation/rework dispatches only after the
        // terminal Discord delivery commits. Completing here used to let
        // dispatch followups / auto-queue slot release race ahead of the final
        // message edit, so an archived/deleted thread could strand the turn
        // while the queue already advanced.
        // #3268: `mut` so the post-gate self-healing handoff can clear it — a
        // handed-off turn is NOT done on the bridge side.
        let mut should_complete_work_dispatch_after_delivery = !cancelled
            && !is_prompt_too_long
            && !transport_error
            && bridge_output_owner.is_none();
        let should_fail_dispatch_after_delivery = transport_error && !cancelled;

        let final_session_status = if active_background_child_session_ids.is_empty() {
            IDLE
        } else {
            AWAITING_BG
        };

        // Keep the session visibly active while Discord terminal delivery and
        // status-panel finalization are still pending. Publishing idle here lets
        // observers race ahead of the final response/status edit.
        post_adk_session_status(
            adk_session_key.as_deref(),
            adk_session_name.as_deref(),
            Some(provider.as_str()),
            TURN_ACTIVE,
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

        let can_chain_locally = gateway.can_chain_locally();
        // Mark this turn as finalizing — deferred restart must wait until we finish
        // sending the Discord response and cleaning up state.
        shared_owned
            .restart.finalizing_turns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        shared_owned
            .restart.global_finalizing
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // #2293/#2780 — hoist the TUI completion gate BEFORE the visible
        // completion/status cleanup so we can still suppress `응답 완료` when
        // the pane has not quiesced. Do NOT use this gate as a mailbox
        // correctness primitive: the bounded 3s probe is best-effort
        // observability, and blocking `mailbox_finish_turn` here strands
        // later user messages behind a stale active turn. The hosted-TUI
        // pre-submit guard below is the correctness barrier that prevents
        // follow-up input from being injected into a still-busy pane.
        // #3038: the early TUI completion gate (eligibility filter + bounded
        // quiescence probe + timed-out warning) is extracted verbatim to
        // `early_tui_completion.rs`. The two outputs are consumed later, so the
        // `#[cfg]` `let` declarations stay here (preserving the exact unix /
        // non-unix split) and the helper returns the computed values; behavior
        // is byte-identical (see the module doc for the seam-fix note).
        #[cfg(unix)]
        let bridge_early_gate_timed_out;
        #[cfg(not(unix))]
        let bridge_early_gate_timed_out = false;
        #[cfg(unix)]
        let bridge_tui_gate_outcome_early: Option<super::tmux::TuiCompletionGateOutcome>;
        #[cfg(unix)]
        {
            let (outcome_early, gate_timed_out) =
                early_tui_completion::run_early_tui_completion_gate(
                    cancelled,
                    is_prompt_too_long,
                    transport_error,
                    recovery_retry,
                    &inflight_state,
                    &provider,
                    channel_id,
                )
                .await;
            bridge_tui_gate_outcome_early = outcome_early;
            bridge_early_gate_timed_out = gate_timed_out;
        }
        // #3268 (Defect B): on (gate timeout + non-terminal + genuinely-live
        // watcher) hand the busy turn back to the watcher — see `watcher_handoff`.
        #[cfg(unix)]
        watcher_handoff::maybe_hand_off_busy_turn_to_watcher(
            &shared_owned,
            bridge_early_gate_timed_out,
            terminal_error_path,
            watcher_owner_channel_id,
            channel_id,
            &provider,
            dispatch_id.as_deref(),
            adk_session_key.as_deref(),
            turn_id.as_str(),
            current_msg_id.get(),
            can_chain_locally,
            tmux_last_offset,
            response_unsent,
            &mut inflight_state,
            &mut bridge_relay_delegated_to_watcher,
            &mut bridge_output_owner,
            &mut should_complete_work_dispatch_after_delivery,
        );
        let has_queued_turns = if bridge_relay_delegated_to_watcher {
            // #1452 (Codex P1): the actual `mailbox_finalize_owed.store(true,
            // Release)` happens EARLIER, at the watcher-unpause site in the
            // `TmuxReady` branch (~line 1980). Doing it there guarantees we
            // win any race with a fast watcher whose terminal `swap(false,
            // AcqRel)` could otherwise execute before this late delegation
            // decision and leave stale debt that would clear the next turn's
            // cancel_token.
            //
            // Here we only verify the invariant: the single-authority finalizer
            // ledger still holds a live (non-`Finalized`) watcher-owned Pending
            // entry for this turn's channel/generation. If it does not, the
            // watcher will not finalize and the channel mailbox would leak its
            // cancel_token; we surface the violation via
            // `record_turn_bridge_invariant`.
            //
            // #3016 phase-5b1: this replaces the legacy
            // `mailbox_finalize_owed.load()` consumer with the ledger query. The
            // two are equivalent because `owed = true` is set atomically with the
            // `register_start(.., RelayOwnerKind::Watcher)` at the watcher-unpause
            // sites (~4196, ~6129) keyed by `TurnKey::new(channel_id, _,
            // current_generation)` — i.e. `owed ⟺ a live watcher Pending entry`.
            // The query keys on the SAME `channel_id` + `current_generation`
            // `register_start` used (the flag field/producers are removed in
            // phase-5b2).
            let handoff_recorded = shared_owned
                .turn_finalizer
                .has_live_watcher_pending(channel_id, shared_owned.restart.current_generation)
                .await;
            record_turn_bridge_invariant(
                handoff_recorded,
                &provider,
                channel_id,
                dispatch_id.as_deref(),
                adk_session_key.as_deref(),
                Some(turn_id.as_str()),
                "bridge_handoff_finds_watcher_handle",
                "src/services/discord/turn_bridge/mod.rs:bridge_relay_delegated_to_watcher",
                "bridge delegation expected to find a live watcher handle holding finalization debt",
                serde_json::json!({
                    "watcher_owner_channel_id": watcher_owner_channel_id.get(),
                }),
            );
            if handoff_recorded {
                false
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    provider = %provider.as_str(),
                    channel = channel_id.get(),
                    watcher_owner_channel = watcher_owner_channel_id.get(),
                    tui_gate_timed_out = bridge_early_gate_timed_out,
                    "  [{ts}] ⚠ bridge watcher handoff missing finalizer; bridge is releasing mailbox to avoid stranded queued turns"
                );
                // #3016 phase 2: route through the single-authority finalizer.
                // It owns the exact sequence this branch ran inline (mailbox
                // cancel_token release + `mark_completion_cleanup`/`cancelled`
                // + counter decrement gated on `removed_token.is_some()` +
                // watchdog override clear + dispatch_thread_parents retain +
                // voice drain + dispatch_role_overrides cleanup). The ledger
                // gate makes a transitional double-finalize with a still-direct
                // watcher (phase 3 routes it) a harmless idempotent no-op.
                let outcome = shared_owned
                    .turn_finalizer
                    .submit_terminal(
                        super::turn_finalizer::TurnKey::new(
                            channel_id,
                            inflight_state.effective_finalizer_turn_id(),
                            shared_owned.restart.current_generation,
                        ),
                        provider.clone(),
                        if cancelled {
                            super::turn_finalizer::TerminalEvent::Cancel
                        } else {
                            super::turn_finalizer::TerminalEvent::Complete
                        },
                        super::turn_finalizer::FinalizeContext::bridge(),
                        shared_owned.clone(),
                    )
                    .await;
                // `finalize_owned` is the real invariant: SOME authority (this
                // bridge submission OR whoever already finalized — the ledger
                // guarantees exactly-once) owns the finalize for this turn. All
                // three outcomes satisfy it, so none is a violation.
                // `this_finalized` records only whether THIS submission was the
                // one finalizer; an `AlreadyFinalized` (someone else won, e.g. a
                // sweeper/watcher race) is the normal/info path and must NOT
                // emit a false `[invariant]` ERROR.
                let (this_finalized, finalize_owned, has_pending_after_voice) = match outcome {
                    super::turn_finalizer::FinalizeOutcome::Finalized {
                        removed_token,
                        has_pending,
                        ..
                    } => (removed_token.is_some(), true, has_pending),
                    super::turn_finalizer::FinalizeOutcome::AlreadyFinalized
                    | super::turn_finalizer::FinalizeOutcome::Deferred => {
                        // Something else finalized first (rare sweeper race —
                        // the watcher handle is gone in this branch). This
                        // branch always drained deferred voice work before
                        // #3016, so drain it here too rather than leaking
                        // deferred voice prompts.
                        let voice_deferred_enqueued = shared_owned
                            .voice_barge_in
                            .drain_deferred_after_turn(&shared_owned, &provider, channel_id)
                            .await;
                        (false, true, voice_deferred_enqueued)
                    }
                };
                let _ = this_finalized;
                record_turn_bridge_invariant(
                    finalize_owned,
                    &provider,
                    channel_id,
                    dispatch_id.as_deref(),
                    adk_session_key.as_deref(),
                    Some(turn_id.as_str()),
                    "mailbox_active_turn_recovered_after_missing_watcher_handoff",
                    "src/services/discord/turn_bridge/mod.rs:bridge_relay_delegated_to_watcher",
                    "missing watcher handoff finalizer must leave the turn finalized by some authority",
                    serde_json::json!({
                        "this_finalized": this_finalized,
                        "has_pending": has_pending_after_voice,
                        "watcher_owner_channel_id": watcher_owner_channel_id.get(),
                    }),
                );
                has_pending_after_voice
            }
        } else {
            // #1452 non-delegation path: we are finalizing on the bridge side
            // (cancelled / prompt_too_long / transport_error / recovery_retry,
            // or the watcher never ended up owning relay).
            //
            // #3016 phase 2/3: the single-authority finalizer's per-turn
            // ledger phase gate ARBITRATES exactly-once — whoever submits
            // the terminal first finalizes; the loser receives
            // `AlreadyFinalized` and does nothing.
            //
            // #3016 phase-5b2: the legacy `mailbox_finalize_owed` flag has been
            // removed entirely, so the CAS true→false revoke that used to run
            // here (gated on `bridge_published_finalize_owed_for_this_turn`) is
            // gone. The ledger's exactly-once gate already guards the
            // cross-turn hazard the CAS protected against — a watcher surviving
            // into the NEXT turn no longer has a flag to swap.
            if bridge_early_gate_timed_out {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    provider = %provider.as_str(),
                    channel = channel_id.get(),
                    "  [{ts}] ⚠ #2293/#2780: bridge releasing mailbox despite TUI quiescence timeout; follow-up pre-submit gate will requeue if pane is still busy"
                );
            }
            let outcome = shared_owned
                .turn_finalizer
                .submit_terminal(
                    super::turn_finalizer::TurnKey::new(
                        channel_id,
                        inflight_state.effective_finalizer_turn_id(),
                        shared_owned.restart.current_generation,
                    ),
                    provider.clone(),
                    if cancelled {
                        super::turn_finalizer::TerminalEvent::Cancel
                    } else {
                        super::turn_finalizer::TerminalEvent::Complete
                    },
                    super::turn_finalizer::FinalizeContext::bridge(),
                    shared_owned.clone(),
                )
                .await;
            // `finalize_owned` is the actual invariant: SOMEONE (this bridge
            // submission OR the watcher that won the race OR the deadline-armed
            // backstop for a deferred gate-timeout) owns the one finalize for
            // this turn. The single-authority ledger guarantees that, so all
            // three outcomes are the normal/info path. `this_finalized` only
            // records whether THIS submission was the one that finalized — it
            // must NOT be conflated with an invariant violation, otherwise a
            // watcher-won `AlreadyFinalized` (e.g. the watcher consumed
            // `mailbox_finalize_owed` before the bridge revoked it) emits a
            // false `[invariant]` ERROR for completely normal exactly-once
            // behaviour — the pre-#3016 code treated that as an info path.
            let (this_finalized, finalize_owned, has_pending_after_voice) = match outcome {
                super::turn_finalizer::FinalizeOutcome::Finalized {
                    removed_token,
                    has_pending,
                    ..
                } => (removed_token.is_some(), true, has_pending),
                super::turn_finalizer::FinalizeOutcome::AlreadyFinalized
                | super::turn_finalizer::FinalizeOutcome::Deferred => {
                    // The watcher won the finalize race (or the gate-timeout
                    // deferred), so the bridge's `do_finalize` did NOT run and
                    // the watcher's finalize context does not drain voice. The
                    // pre-#3016 `watcher_already_finalized` branch still drained
                    // deferred voice barge-in work here, so we must too —
                    // otherwise prompts deferred during the turn stay stuck.
                    let voice_deferred_enqueued = shared_owned
                        .voice_barge_in
                        .drain_deferred_after_turn(&shared_owned, &provider, channel_id)
                        .await;
                    (false, true, voice_deferred_enqueued)
                }
            };
            let _ = this_finalized;
            record_turn_bridge_invariant(
                finalize_owned,
                &provider,
                channel_id,
                dispatch_id.as_deref(),
                adk_session_key.as_deref(),
                Some(turn_id.as_str()),
                "mailbox_active_turn_matches_dispatch",
                "src/services/discord/turn_bridge/mod.rs:mailbox_finish_turn",
                "turn_bridge finalization expected the turn to be finalized by some authority",
                serde_json::json!({
                    "this_finalized": this_finalized,
                    "has_pending": has_pending_after_voice,
                }),
            );
            has_pending_after_voice
        };
        let mut preserve_inflight_for_cleanup_retry = false;
        // #3041 P1-2 (codex P1-2 R3): set ONLY on a delivery-lease `Skip`, where
        // the live HOLDER (the watcher) — a different actor sharing the same
        // per-channel `DeliveryLeaseCell` — owns this turn's delivery AND its
        // inflight lifecycle (the watcher CLEARS inflight on its own success).
        // Distinct from the bridge-owned `preserve_inflight_for_cleanup_retry`
        // sites (EditFailed, PG-cancel-fail, replace-not-committed, send/enqueue
        // failure, TUI quiescence timeout) where the BRIDGE still owns the row
        // and its epilogue `save_inflight_state` is load-bearing. On a Skip the
        // bridge must NOT blindly rewrite inflight: the holder may have already
        // cleared it on success, and a blind re-save would resurrect a STALE
        // inflight row for an already-delivered turn (recovery sees it as
        // delivered and returns without clearing → permanent stale leak). The
        // epilogue's save is therefore made IDENTITY-GUARDED on a Skip
        // (`save_inflight_state_if_matches_identity`): it only rewrites if the
        // on-disk row STILL matches this turn's identity, so a watcher-clear
        // (file gone) or a newer turn (identity mismatch) no-ops instead of
        // resurrecting. When the holder FAILS (does not clear), the row is still
        // present + matching, so the bridge refreshes it and retry survives.
        let mut bridge_skip_holder_owns_inflight = false;
        let (mut terminal_delivery_committed, mut terminal_body_visible) = (false, false);
        let mut status_panel_terminal_committed = false;
        let mut completion_footer_terminal_text: Option<String> = None;
        // #2161 (Codex round-2 H1): hoisted into the outer scope so the
        // bridge can run the TUI completion gate BEFORE dispatch completion
        // and reuse the same outcome for the visible status-panel emit
        // below. Default is "emit" for paths that don't reach the gate
        // (e.g. cancelled, prompt_too_long, transport_error) and for
        // non-unix targets where the tmux module is configured out.
        #[allow(unused_mut)]
        let mut bridge_should_emit_completion = true;

        // Remove ⏳ only if the bridge still owns output delivery.
        // Relay owners commit their own visible lifecycle.
        if !bridge_output_owner
            .map(|owner| owner.skips_bridge_spinner_cleanup())
            .unwrap_or(false)
            && let Some(user_msg_id) = user_msg_id
        {
            gateway.remove_reaction(channel_id, user_msg_id, '⏳').await;
        }

        // Recovery auto-retry: session died during restart recovery
        if recovery_retry {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ↻ Recovery session died — triggering auto-retry with history (channel {})",
                channel_id
            );
            reset_session_for_auto_retry(
                &shared_owned,
                channel_id,
                &cancel_token,
                adk_session_key.as_deref(),
                &mut new_session_id,
                &mut new_raw_provider_session_id,
                &mut inflight_state,
                "recovery session died",
            )
            .await;
            // #2452 H6: schedule the auto-retry via the explicit
            // completion path so the dedup lockout is released as soon
            // as scheduling resolves (≤ 120s safety net inside helper).
            // A recovery turn with no anchored user message (user_msg_id == 0)
            // has no message to retry-with-history against, so skip scheduling.
            if let Some(user_msg_id) = user_msg_id {
                spawn_retry_with_history_with_release(
                    gateway.clone(),
                    channel_id,
                    user_msg_id,
                    user_text_owned.clone(),
                );
            }
            // Replace placeholder with recovery notice (don't delete — avoids visual gap)
            let _ = gateway
                .edit_message(
                    channel_id,
                    current_msg_id,
                    "↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다.",
                )
                .await;
            full_response = String::new();
        }

        if cancelled {
            close_all_tracked_background_children(
                shared_owned.pg_pool.as_ref(),
                &mut active_background_child_session_ids,
                "aborted",
                "cancel cleanup",
            )
            .await;
            if pending_long_running_open_after_state_save.take().is_some() {
                inflight_state.long_running_placeholder_active = false;
                let _ = save_inflight_state(&inflight_state);
            }
            // #1255: cancelled turn → drive any active long-running placeholder
            // into Aborted before the rest of the cleanup machinery runs. The
            // controller's idempotent terminal transition guarantees this is
            // safe even if the ToolResult event already fired Completed.
            // #2289 (Codex review): mirror the Done/stream-end outcome handling
            // — only clear the persisted flag when the transition actually
            // committed (Edited / Coalesced / AlreadyTerminal). On
            // `EditFailed`, leave the controller entry Active and the
            // persisted flag set so a later retry path or the placeholder
            // sweeper can finish the teardown. Without this, dropping a
            // raced `Done` here would silently leak a non-evictable Active
            // controller row and clobber the sweeper's repair signal.
            if let Some((key, snapshot, close_trigger, ack_consumed)) =
                long_running_placeholder_active.take()
            {
                let pending_retarget_matches_key = pending_long_running_retarget_after_state_save
                    .as_ref()
                    .is_some_and(|(pending_key, _, _, _, _)| *pending_key == key);
                if pending_retarget_matches_key {
                    let _ = pending_long_running_retarget_after_state_save.take();
                    shared_owned.ui.placeholder_controller.detach(&key);
                    inflight_state.long_running_placeholder_active = false;
                    let _ = save_inflight_state(&inflight_state);
                } else {
                    let outcome = shared_owned
                        .ui.placeholder_controller
                        .transition(
                            gateway.as_ref(),
                            key.clone(),
                            super::placeholder_controller::PlaceholderLifecycle::Aborted,
                        )
                        .await;
                    use super::placeholder_controller::PlaceholderControllerOutcome::*;
                    if matches!(outcome, Edited | Coalesced | AlreadyTerminal) {
                        inflight_state.long_running_placeholder_active = false;
                        let _ = save_inflight_state(&inflight_state);
                    } else {
                        // EditFailed (or any non-committed outcome): leave the
                        // persisted flag set AND preserve the inflight file for
                        // cleanup retry. The placeholder sweeper relies on the
                        // inflight file existing to discover the stuck row; if
                        // we let the normal cancel cleanup delete it, the
                        // sweeper would lose its repair signal and the
                        // controller entry would stay Active forever. Force
                        // the inflight to be preserved so the next sweeper
                        // pass can finish the teardown.
                        let _ = (key, snapshot, close_trigger, ack_consumed);
                        let _ = save_inflight_state(&inflight_state);
                        preserve_inflight_for_cleanup_retry = true;
                    }
                }
            }

            let cleanup_policy = match cancel_token.restart_mode() {
                Some(restart_mode) => TmuxCleanupPolicy::PreserveSessionAndInflight {
                    restart_mode,
                },
                None => TmuxCleanupPolicy::PreserveSession,
            };
            // #3169 (death #3): the `None`-cancel_source fallback uses the
            // shared `ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON` sentinel so the
            // tmux_runtime SIGINT guard recognises this anonymous internal
            // teardown and suppresses claude's session-killing teardown SIGINT.
            let cancel_source = cancel_token.cancel_source().unwrap_or_else(|| {
                tmux_runtime::ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON.to_string()
            });
            stop_active_turn(
                &provider,
                &cancel_token,
                cleanup_policy,
                &cancel_source,
            )
            .await;

            if let Some(dispatch_id) = dispatch_id.as_deref() {
                if let Some(pg_pool) = shared_owned.pg_pool.as_ref() {
                    if let Err(error) = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
                        pg_pool,
                        dispatch_id,
                        Some(cancel_source.as_str()),
                    )
                    .await
                    {
                        // #2044 F8: when the PG cancel fails, the
                        // dispatch row stays in its previous (possibly
                        // "running") state while our local cleanup
                        // proceeds. Without setting
                        // `preserve_inflight_for_cleanup_retry`, the
                        // inflight file would also be deleted, so a
                        // subsequent dispatch_followup could re-use
                        // the dispatch id thinking the turn is still
                        // healthy — producing a dispatch-state ↔
                        // inflight-state inconsistency. Set the retry
                        // flag so the next cleanup pass re-attempts
                        // the cancel, and emit a structured tracing
                        // event so ops can alarm on
                        // `dispatch_cancel_pg_failed`.
                        tracing::warn!(
                            event = "dispatch_cancel_pg_failed",
                            dispatch_id = %dispatch_id,
                            channel_id = channel_id.get(),
                            cancel_source = %cancel_source,
                            error = %error,
                            "[turn_bridge] failed to cancel dispatch in postgres; preserving inflight for cleanup retry",
                        );
                        preserve_inflight_for_cleanup_retry = true;
                    }
                }
            }

            let preserved_restart_mode = cancel_token.restart_mode();
            let remaining_response =
                response_portion_after_offset(&full_response, response_sent_offset);
            let terminal_response = if let Some(restart_mode) = preserved_restart_mode {
                handoff_interrupted_message(restart_mode, remaining_response)
            } else if remaining_response.trim().is_empty() {
                "[Stopped]".to_string()
            } else {
                let formatted = if shared_owned.ui.status_panel_v2_enabled {
                    super::formatting::format_for_discord_with_status_panel(
                        remaining_response,
                        &provider,
                    )
                } else {
                    super::formatting::format_for_discord_with_provider(
                        remaining_response,
                        &provider,
                    )
                };
                format!("{}\n\n[Stopped]", formatted)
            };

            // #3041 P1-2 (site 1 — cancel/stop terminal replace): acquire the
            // shared delivery lease BEFORE delivering the `[Stopped]` body; a B2
            // Skip means the holder owns this turn/range → do NOT deliver+advance.
            // (codex P1-a) lease on `watcher_owner_channel_id` — the cell + TurnKey
            // channel the WATCHER uses (a reused watcher can own a channel != this
            // bridge's `channel_id`), so the two CONTEND on one cell (single-holder
            // B2) instead of both delivering = duplicate.
            let stop_lease_acquire = bridge_delivery_lease_for_inflight(
                shared_owned.as_ref(),
                watcher_owner_channel_id,
                shared_owned.restart.current_generation,
                &inflight_state,
                tmux_last_offset,
            );
            if matches!(stop_lease_acquire, BridgeLeaseAcquire::Skip) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    channel = channel_id.get(),
                    "  [{ts}] 🌉 #3041 B2: delivery lease held by another holder — bridge skipped duplicate cancel/stop terminal replace (channel {})",
                    channel_id
                );
                // #3041 P1-2 (codex P1-c): a B2 Skip means the holder (the watcher)
                // owns this range — the bridge is a TRUE no-op on completion
                // side-effects. PRESERVE inflight so the epilogue does NOT clear it
                // (~9017) / mark `watcher.turn_delivered` (~8356); a still-retry-able
                // turn is re-delivered by ACK-poll if the holder ultimately fails.
                // (codex P1-2 R3) the holder owns the inflight lifecycle → make the
                // epilogue save identity-guarded so it never resurrects a cleared row.
                preserve_inflight_for_cleanup_retry = true;
                bridge_skip_holder_owns_inflight = true;
            } else {
                let stop_lease = match stop_lease_acquire {
                    BridgeLeaseAcquire::Held(lease) => Some(lease),
                    _ => None,
                };
                let replace_committed = turn_bridge_replace_outcome_committed(
                    shared_owned.as_ref(),
                    &provider,
                    channel_id,
                    current_msg_id,
                    inflight_state.tmux_session_name.as_deref(),
                    gateway
                        .replace_message_with_outcome(channel_id, current_msg_id, &terminal_response)
                        .await,
                    dispatch_id.as_deref(),
                    adk_session_key.as_deref(),
                    Some(turn_id.as_str()),
                    "turn_bridge_cancelled_terminal_replace",
                );
                if replace_committed {
                    status_panel_terminal_committed = true;
                }
                // B6: the ONLY confirmed_end advance is via a successful lease
                // commit. `Held` → commit (Delivered advances, NotDelivered not).
                // `NoRange` has NO new bytes → no advance outside a lease (codex
                // P1-b: a degenerate equal-nonzero range must not advance).
                if let Some(lease) = stop_lease {
                    let lease_range = lease.range();
                    let outcome = if replace_committed {
                        crate::services::discord::LeaseOutcome::Delivered
                    } else {
                        crate::services::discord::LeaseOutcome::NotDelivered
                    };
                    let committed = lease.commit_and_advance(
                        shared_owned.as_ref(),
                        watcher_owner_channel_id,
                        inflight_state.tmux_session_name.as_deref(),
                        outcome,
                    );
                    if replace_committed && committed {
                        super::outbound::delivery_record::shadow_mirror_delivered_frontier(
                            shared_owned.as_ref(),
                            &provider,
                            watcher_owner_channel_id,
                            lease_range,
                            true,
                            Some(current_msg_id.get()),
                            Some(channel_id.get()),
                        );
                    }
                }
                if !replace_committed {
                    preserve_inflight_for_cleanup_retry = true;
                }
            }

            if preserved_restart_mode.is_none()
                && let Some(user_msg_id) = user_msg_id
            {
                gateway.add_reaction(channel_id, user_msg_id, '🛑').await;
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ■ Stopped");
        } else if is_prompt_too_long {
            let mention = gateway.requester_mention().unwrap_or_default();
            full_response = format!(
                "{} ⚠️ 프롬프트가 너무 깁니다. 대화 컨텍스트가 모델 한도를 초과했습니다.\n\n\
                 다음 메시지를 보내면 자동으로 새 턴이 시작됩니다.\n\
                 컨텍스트를 줄이려면 `/compact` 또는 `/clear`를 사용해 주세요.",
                mention
            );
            // #3041 P1-2 (site 2 — prompt-too-long terminal replace): same lease
            // routing as site 1 — acquire before replace; B2-skip if held. (codex
            // P1-a) lease on `watcher_owner_channel_id` (shared cell + TurnKey).
            let plt_lease_acquire = bridge_delivery_lease_for_inflight(
                shared_owned.as_ref(),
                watcher_owner_channel_id,
                shared_owned.restart.current_generation,
                &inflight_state,
                tmux_last_offset,
            );
            if matches!(plt_lease_acquire, BridgeLeaseAcquire::Skip) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    channel = channel_id.get(),
                    "  [{ts}] 🌉 #3041 B2: delivery lease held by another holder — bridge skipped duplicate prompt-too-long terminal replace (channel {})",
                    channel_id
                );
                // #3041 P1-2 (codex P1-c): preserve retry on a B2 Skip — holder owns
                // delivery; do NOT clear inflight / mark the watcher delivered.
                // (codex P1-2 R3) holder owns the inflight lifecycle on a Skip.
                preserve_inflight_for_cleanup_retry = true;
                bridge_skip_holder_owns_inflight = true;
            } else {
                let plt_lease = match plt_lease_acquire {
                    BridgeLeaseAcquire::Held(lease) => Some(lease),
                    _ => None,
                };
                let replace_committed = turn_bridge_replace_outcome_committed(
                    shared_owned.as_ref(),
                    &provider,
                    channel_id,
                    current_msg_id,
                    inflight_state.tmux_session_name.as_deref(),
                    gateway
                        .replace_message_with_outcome(channel_id, current_msg_id, &full_response)
                        .await,
                    dispatch_id.as_deref(),
                    adk_session_key.as_deref(),
                    Some(turn_id.as_str()),
                    "turn_bridge_prompt_too_long_replace",
                );
                if replace_committed {
                    status_panel_terminal_committed = true;
                }
                // B6 (codex P1-b): advance ONLY via a successful lease commit.
                // NoRange has no new bytes → no advance outside the lease.
                if let Some(lease) = plt_lease {
                    let lease_range = lease.range();
                    let outcome = if replace_committed {
                        crate::services::discord::LeaseOutcome::Delivered
                    } else {
                        crate::services::discord::LeaseOutcome::NotDelivered
                    };
                    let committed = lease.commit_and_advance(
                        shared_owned.as_ref(),
                        watcher_owner_channel_id,
                        inflight_state.tmux_session_name.as_deref(),
                        outcome,
                    );
                    if replace_committed && committed {
                        super::outbound::delivery_record::shadow_mirror_delivered_frontier(
                            shared_owned.as_ref(),
                            &provider,
                            watcher_owner_channel_id,
                            lease_range,
                            true,
                            Some(current_msg_id.get()),
                            Some(channel_id.get()),
                        );
                    }
                }
                if !replace_committed {
                    preserve_inflight_for_cleanup_retry = true;
                }
            }

            if let Some(user_msg_id) = user_msg_id {
                gateway.add_reaction(channel_id, user_msg_id, '⚠').await;
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ⚠ Prompt too long (channel {})", channel_id);
        } else if let Some(owner) = bridge_output_owner {
            let ts = chrono::Local::now().format("%H:%M:%S");
            match owner {
                BridgeOutputOwner::WatcherRelay => {
                    tracing::info!(
                        "  [{ts}] 👁 tmux watcher owns assistant relay; bridge skipped direct response delivery (channel {})",
                        channel_id
                    );
                    if should_delete_bridge_created_watcher_orphan_response(
                        shared_owned.ui.status_panel_v2_enabled,
                        watcher_handoff_claim_outcome,
                        bridge_created_response_placeholder_msg_id,
                        current_msg_id,
                    ) {
                        // #3607: terminal-anchor guard + durable delete
                        // observability live in the sibling so the hot file only
                        // dispatches. The guard skips deleting a committed
                        // terminal anchor (the accident this fixes); a genuine
                        // non-terminal orphan is deleted, recorded, and retried.
                        cleanup_or_preserve_watcher_orphan_spinner(
                            shared_owned.clone(),
                            &provider,
                            gateway.clone(),
                            channel_id,
                            current_msg_id,
                            &inflight_state,
                        )
                        .await;
                    }
                }
                BridgeOutputOwner::StandbyRelay => tracing::info!(
                    "  [{ts}] 👁 standby relay owns assistant relay; bridge skipped direct response delivery (channel {})",
                    channel_id
                ),
            }
        } else {
            // Check for stale resume failure BEFORE any other response handling.
            // This path is driven by explicit error/result events, not assistant text.
            if resume_failure_detected {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ Resume failed (error in response), clearing session_id (channel {})",
                    channel_id
                );
                reset_session_for_auto_retry(
                    &shared_owned,
                    channel_id,
                    &cancel_token,
                    adk_session_key.as_deref(),
                    &mut new_session_id,
                    &mut new_raw_provider_session_id,
                    &mut inflight_state,
                    "resume failed in response output",
                )
                .await;
                // #2452 H6: explicit completion path — see helper docs.
                // Skip retry-with-history when the recovery turn has no anchored
                // user message (user_msg_id == 0).
                if let Some(user_msg_id) = user_msg_id {
                    spawn_retry_with_history_with_release(
                        gateway.clone(),
                        channel_id,
                        user_msg_id,
                        user_text_owned.clone(),
                    );
                }
                full_response = String::new(); // Suppress error message to user
            } else if full_response.is_empty() {
                // #2451 H5 graduation: the authoritative resume-failure
                // witness is the absence of `StreamMessage::Init` after a
                // turn that attempted resume. `attempted_resume` is the
                // turn-start snapshot of the provider session_id (taken
                // before any reset_session_for_auto_retry side effect),
                // and `session_handshake_seen` is flipped inside the
                // `Init` handler. The old `quick_exit < 10s` test is kept
                // as a 30s safety backstop for providers whose `Init`
                // emission is unreliable (e.g. gemini may not emit Init
                // on resume success).
                let attempted_resume = had_prior_session_id_at_turn_start;
                let resume_likely_failed_by_handshake =
                    attempted_resume && !session_handshake_seen && rx_disconnected;
                // Backstop only — wider threshold to keep false positives
                // away from healthy fast turns.
                let quick_exit_backstop = turn_start.elapsed().as_secs() < 30;
                let quick_empty_resume =
                    resume_likely_failed_by_handshake || (quick_exit_backstop && rx_disconnected && attempted_resume);
                // Fallback: try to extract response from tmux output file
                if quick_empty_resume {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⏭ Skipping output file recovery after quick empty resume exit (channel {})",
                        channel_id
                    );
                } else if let Some(ref path) = inflight_state.output_path {
                    let recovered = super::recovery::extract_response_from_output_pub(
                        path,
                        inflight_state.last_offset,
                    );
                    if !recovered.trim().is_empty() {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ↻ Recovered {} chars from output file for channel {}",
                            recovered.len(),
                            channel_id
                        );
                        full_response = recovered;
                    }
                }

                // Check for stale resume failure in recovered output
                let stale_resume_in_output = inflight_state
                    .output_path
                    .as_deref()
                    .map(|path| {
                        output_file_has_stale_resume_error_after_offset(
                            path,
                            inflight_state.last_offset,
                        )
                    })
                    .unwrap_or(false);
                if stale_resume_in_output {
                    resume_failure_detected = true;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Resume failed (stale session_id in recovered output), auto-retrying (channel {})",
                        channel_id
                    );
                    reset_session_for_auto_retry(
                        &shared_owned,
                        channel_id,
                        &cancel_token,
                        adk_session_key.as_deref(),
                        &mut new_session_id,
                        &mut new_raw_provider_session_id,
                        &mut inflight_state,
                        "stale session_id in recovered output",
                    )
                    .await;
                    // #2452 H6: explicit completion path — see helper docs.
                    if let Some(user_msg_id) = user_msg_id {
                        spawn_retry_with_history_with_release(
                            gateway.clone(),
                            channel_id,
                            user_msg_id,
                            user_text_owned.clone(),
                        );
                    }
                    full_response = String::new();
                } else {
                    // Check for resume failure via other methods
                    let mut resume_failed = false;
                    // Method 1: check tmux output file
                    if let Some(ref path) = inflight_state.output_path
                        && output_file_has_stale_resume_error_after_offset(
                            path,
                            inflight_state.last_offset,
                        )
                    {
                        resume_failed = true;
                        resume_failure_detected = true;
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ Resume failed (stale session_id in output file), auto-retrying (channel {})",
                            channel_id
                        );
                        reset_session_for_auto_retry(
                            &shared_owned,
                            channel_id,
                            &cancel_token,
                            adk_session_key.as_deref(),
                            &mut new_session_id,
                            &mut new_raw_provider_session_id,
                            &mut inflight_state,
                            "stale session_id in output file",
                        )
                        .await;
                        // #2452 H6: explicit completion path — see helper.
                        if let Some(user_msg_id) = user_msg_id {
                            spawn_retry_with_history_with_release(
                                gateway.clone(),
                                channel_id,
                                user_msg_id,
                                user_text_owned.clone(),
                            );
                        }
                        full_response = String::new();
                    }
                    // #2451 H5 Method 2: authoritative resume-failure
                    // classification via the explicit `Init` handshake
                    // witness. The legacy `quick_exit < 10s` test now
                    // serves only as the 30s safety backstop above. If
                    // `attempted_resume` was true AND we never saw `Init`
                    // AND rx disconnected, the provider almost certainly
                    // failed to bind the prior session_id. The original
                    // `core.sessions` re-fetch is replaced by the
                    // turn-start snapshot so the recheck cannot race a
                    // prior reset_session_for_auto_retry.
                    if !resume_failed
                        && rx_disconnected
                        && attempted_resume
                        && (!session_handshake_seen || quick_exit_backstop)
                    {
                        {
                            resume_failed = true;
                            resume_failure_detected = true;
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ Empty response with no Init handshake (session_handshake_seen={}, elapsed={}s) — auto-retrying with fresh session (channel {})",
                                session_handshake_seen,
                                turn_start.elapsed().as_secs(),
                                channel_id
                            );
                            reset_session_for_auto_retry(
                                &shared_owned,
                                channel_id,
                                &cancel_token,
                                adk_session_key.as_deref(),
                                &mut new_session_id,
                                &mut new_raw_provider_session_id,
                                &mut inflight_state,
                                "quick exit with empty response",
                            )
                            .await;
                            // #2452 H6: explicit completion path.
                            if let Some(user_msg_id) = user_msg_id {
                                spawn_retry_with_history_with_release(
                                    gateway.clone(),
                                    channel_id,
                                    user_msg_id,
                                    user_text_owned.clone(),
                                );
                            }
                            full_response = String::new();
                        }
                    }
                    if !resume_failed {
                        if rx_disconnected {
                            terminal_empty_response_notice =
                                Some("(No response — 프로세스가 응답 없이 종료됨)".to_string());
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ Empty response: rx disconnected before any text \
                                 (channel {}, output_path={:?}, last_offset={})",
                                channel_id,
                                inflight_state.output_path,
                                inflight_state.last_offset
                            );
                        } else {
                            terminal_empty_response_notice = Some("(No response)".to_string());
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ Empty response: done without text (channel {})",
                                channel_id
                            );
                        }
                    }
                }
            }

            let late_api_friction =
                crate::services::api_friction::extract_api_friction_reports(&full_response);
            if !late_api_friction.reports.is_empty() {
                api_friction_reports.extend(late_api_friction.reports);
                full_response = late_api_friction.cleaned_response;
                sync_response_delivery_state(
                    &full_response,
                    &mut response_sent_offset,
                    &mut inflight_state,
                );
            }
            for error in late_api_friction.parse_errors {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!("  [{ts}] ⚠ invalid API_FRICTION marker: {error}");
            }

            let mut delivery_response = terminal_delivery_response_after_offset(
                &full_response,
                response_sent_offset,
                terminal_empty_response_notice.as_deref(),
            );
            if let Some(warning) = review_dispatch_warning.as_deref() {
                let warning = warning.trim();
                if !warning.is_empty() {
                    if !delivery_response.trim().is_empty() {
                        delivery_response.push_str("\n\n");
                    }
                    delivery_response.push_str(warning);
                }
            }
            let spoken_delivery_response = delivery_response.clone();

            // Headless silent trigger (metadata.silent=true): suppress assistant
            // text delivery entirely. Lifecycle/error/cancel notifications still
            // flow through their own paths.
            if inflight_state.silent_turn {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🤫 turn_bridge: silent_turn suppressed terminal delivery for channel {} ({} chars)",
                    channel_id,
                    delivery_response.len()
                );
                terminal_body_visible = true;
                // #3041 P1-2 (site 3 — silent_turn suppression): no Discord post,
                // but the offset STILL advances so the suppressed range is marked
                // consumed (not re-delivered by recovery). Per B6 the advance flows
                // through a lease commit: acquire→commit(Delivered)→release (the
                // bridge OWNS this range; instantaneous "send" → heartbeat formality).
                // (codex P1-a) lease on `watcher_owner_channel_id` (shared cell +
                // TurnKey channel as the watcher). (codex P1-c)
                // `terminal_delivery_committed` is set ONLY when THIS actor resolves
                // the range (`Held`→commit / `NoRange`); on `Skip` the watcher owns
                // delivery → NO-OP on completion side-effects, leave the turn retry-able.
                let lease_acquire = bridge_delivery_lease_for_inflight(
                    shared_owned.as_ref(),
                    watcher_owner_channel_id,
                    shared_owned.restart.current_generation,
                    &inflight_state,
                    tmux_last_offset,
                );
                // (codex P1-c) one source of truth for "does this acquire outcome
                // mark the silent turn committed": Skip → false (holder owns it,
                // stay retry-able), Held/NoRange → true.
                terminal_delivery_committed =
                    silent_turn_skip_marks_committed(&lease_acquire);
                match lease_acquire {
                    BridgeLeaseAcquire::Held(lease) => {
                        lease.commit_and_advance(
                            shared_owned.as_ref(),
                            watcher_owner_channel_id,
                            inflight_state.tmux_session_name.as_deref(),
                            crate::services::discord::LeaseOutcome::Delivered,
                        );
                    }
                    BridgeLeaseAcquire::Skip => {
                        // B2-skip: the watcher holds the live lease and owns this
                        // range's delivery (codex P1-c). `terminal_delivery_committed
                        // = false` alone is NOT enough — the epilogue still marks
                        // `watcher.turn_delivered` (~8356) and CLEARS inflight (~9017)
                        // unless `preserve_inflight_for_cleanup_retry` is set; set it
                        // so a Skip is a TRUE no-op and the holder's eventual
                        // NotDelivered/Unknown stays re-deliverable.
                        preserve_inflight_for_cleanup_retry = true;
                        // codex P1-2 R3: holder owns the inflight lifecycle on a
                        // Skip — identity-guard the epilogue save (no resurrect).
                        bridge_skip_holder_owns_inflight = true;
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            channel = channel_id.get(),
                            "  [{ts}] 🌉 #3041 B2: delivery lease held by another holder — bridge silent_turn skipped offset advance, left turn retry-able (channel {})",
                            channel_id
                        );
                    }
                    BridgeLeaseAcquire::NoRange => {
                        // No offset to advance (zero/inverted range): the suppression
                        // resolves the (empty) range. B6 holds (no advance).
                    }
                }
            } else if delivery_response.trim().is_empty() {
                if gateway
                    .edit_message(
                        channel_id,
                        current_msg_id,
                        "↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다.",
                    )
                    .await
                    .is_ok()
                {
                    terminal_delivery_committed = true;
                    terminal_body_visible = true;
                }
            } else {
                delivery_response = if shared_owned.ui.status_panel_v2_enabled {
                    super::formatting::format_for_discord_with_status_panel(
                        &delivery_response,
                        &provider,
                    )
                } else {
                    super::formatting::format_for_discord_with_provider(
                        &delivery_response,
                        &provider,
                    )
                };
                if can_chain_locally {
                    if terminal_delivery_should_send_new_chunks(
                        can_chain_locally,
                        &delivery_response,
                    ) {
                        let bridge_start = inflight_state.turn_start_offset.unwrap_or(0);
                        let bridge_end = tmux_last_offset.unwrap_or(0);
                        if terminal_controller_cutover::bridge_long_chunks_cutover_decision(
                            can_chain_locally,
                            &delivery_response,
                            bridge_end > bridge_start,
                            true,
                        ) {
                            let bridge_turn = super::turn_finalizer::TurnKey::new(
                                watcher_owner_channel_id,
                                inflight_state.user_msg_id,
                                shared_owned.restart.current_generation,
                            );
                            let bridge_lease_key = bridge_delivery_lease_key_for_inflight(
                                watcher_owner_channel_id,
                                shared_owned.restart.current_generation,
                                &inflight_state,
                            );
                            let cell = shared_owned.delivery_lease(watcher_owner_channel_id);
                            terminal_controller_cutover::apply_bridge_long_chunks_controller(
                                gateway.as_ref(),
                                shared_owned.as_ref(),
                                &provider,
                                channel_id,
                                watcher_owner_channel_id,
                                inflight_state.tmux_session_name.as_deref(),
                                &cell,
                                &shared_owned.ui.placeholder_controller,
                                current_msg_id,
                                &delivery_response,
                                full_response.len(),
                                bridge_turn,
                                bridge_start,
                                bridge_end,
                                single_message_panel_footer_mode,
                                dispatch_id.as_deref(),
                                adk_session_key.as_deref(),
                                Some(turn_id.as_str()),
                                Some(bridge_lease_key.clone()),
                                terminal_controller_cutover::BridgeLongChunksLocals {
                                    terminal_delivery_committed: &mut terminal_delivery_committed,
                                    terminal_body_visible: &mut terminal_body_visible,
                                    completion_footer_terminal_text:
                                        &mut completion_footer_terminal_text,
                                    preserve_inflight_for_cleanup_retry:
                                        &mut preserve_inflight_for_cleanup_retry,
                                    bridge_skip_holder_owns_inflight:
                                        &mut bridge_skip_holder_owns_inflight,
                                    response_sent_offset: &mut response_sent_offset,
                                    inflight_response_sent_offset:
                                        &mut inflight_state.response_sent_offset,
                                },
                            )
                            .await;
                        } else {
                            let lease_acquire = bridge_delivery_lease_for_inflight(
                                shared_owned.as_ref(),
                                watcher_owner_channel_id,
                                shared_owned.restart.current_generation,
                                &inflight_state,
                                tmux_last_offset,
                            );
                            terminal_controller_cutover::apply_bridge_long_chunks_legacy(
                                lease_acquire,
                                gateway.as_ref(),
                                shared_owned.as_ref(),
                                &provider,
                                channel_id,
                                watcher_owner_channel_id,
                                inflight_state.tmux_session_name.as_deref(),
                                current_msg_id,
                                &delivery_response,
                                full_response.len(),
                                single_message_panel_footer_mode,
                                dispatch_id.as_deref(),
                                adk_session_key.as_deref(),
                                Some(turn_id.as_str()),
                                terminal_controller_cutover::BridgeLongChunksLocals {
                                    terminal_delivery_committed: &mut terminal_delivery_committed,
                                    terminal_body_visible: &mut terminal_body_visible,
                                    completion_footer_terminal_text:
                                        &mut completion_footer_terminal_text,
                                    preserve_inflight_for_cleanup_retry:
                                        &mut preserve_inflight_for_cleanup_retry,
                                    bridge_skip_holder_owns_inflight:
                                        &mut bridge_skip_holder_owns_inflight,
                                    response_sent_offset: &mut response_sent_offset,
                                    inflight_response_sent_offset:
                                        &mut inflight_state.response_sent_offset,
                                },
                            )
                            .await;
                        }
                    } else {
                        // #3089 A5/#3998 S1-f2: route structurally eligible
                        // short-replace through the controller
                        // (`terminal_controller_cutover`).
                        let bridge_start = inflight_state.turn_start_offset.unwrap_or(0);
                        let ordered_range = tmux_last_offset.is_some_and(|e| e > bridge_start);
                        let cutover_short_replace =
                            terminal_controller_cutover::bridge_short_replace_cutover_decision(
                                can_chain_locally,
                                &delivery_response,
                                ordered_range,
                                true,
                        );
                        if cutover_short_replace {
                            let bridge_turn = super::turn_finalizer::TurnKey::new(
                                watcher_owner_channel_id,
                                inflight_state.user_msg_id,
                                shared_owned.restart.current_generation,
                            );
                            let bridge_lease_key = bridge_delivery_lease_key_for_inflight(
                                watcher_owner_channel_id,
                                shared_owned.restart.current_generation,
                                &inflight_state,
                            );
                            let cell = shared_owned.delivery_lease(watcher_owner_channel_id);
                            terminal_controller_cutover::apply_bridge_short_replace_controller(
                                gateway.as_ref(),
                                shared_owned.as_ref(),
                                &provider,
                                channel_id,
                                watcher_owner_channel_id,
                                inflight_state.tmux_session_name.as_deref(),
                                &cell,
                                &shared_owned.ui.placeholder_controller,
                                current_msg_id,
                                &delivery_response,
                                full_response.len(),
                                bridge_turn,
                                bridge_start,
                                tmux_last_offset.unwrap_or(0),
                                single_message_panel_footer_mode,
                                dispatch_id.as_deref(),
                                adk_session_key.as_deref(),
                                Some(turn_id.as_str()),
                                Some(bridge_lease_key.clone()),
                                terminal_controller_cutover::BridgeShortReplaceLocals {
                                    terminal_delivery_committed: &mut terminal_delivery_committed,
                                    terminal_body_visible: &mut terminal_body_visible,
                                    completion_footer_terminal_text:
                                        &mut completion_footer_terminal_text,
                                    preserve_inflight_for_cleanup_retry:
                                        &mut preserve_inflight_for_cleanup_retry,
                                    bridge_skip_holder_owns_inflight:
                                        &mut bridge_skip_holder_owns_inflight,
                                    inflight_response_sent_offset:
                                        &mut inflight_state.response_sent_offset,
                                },
                            )
                            .await;
                        } else {
                            // #3041 P1-2 (site 5 — normal bridge terminal replace):
                            // acquire the shared delivery lease on
                            // `watcher_owner_channel_id` BEFORE delivering so the
                            // watcher and bridge serialize. On B2 Skip the holder owns
                            // this range/turn, so do NOT deliver+advance.
                            let lease_acquire = match terminal_controller_cutover::bridge_terminal_lease_range(
                                Some((bridge_start, tmux_last_offset.unwrap_or(0))),
                                cutover_short_replace,
                            ) {
                                Some(_) => bridge_delivery_lease_for_inflight(
                                    shared_owned.as_ref(),
                                    watcher_owner_channel_id,
                                    shared_owned.restart.current_generation,
                                    &inflight_state,
                                    tmux_last_offset,
                                ),
                                None => BridgeLeaseAcquire::NoRange,
                            };
                            if matches!(lease_acquire, BridgeLeaseAcquire::Skip) {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    channel = channel_id.get(),
                                    "  [{ts}] 🌉 #3041 B2: delivery lease held by another holder — bridge skipped duplicate terminal replace (channel {})",
                                    channel_id
                                );
                                // #3041 P1-2 (codex P1-c): preserve retry on a B2
                                // Skip — holder owns delivery; do NOT clear inflight /
                                // mark the watcher delivered. (codex P1-2 R3)
                                // identity-guard the save.
                                preserve_inflight_for_cleanup_retry = true;
                                bridge_skip_holder_owns_inflight = true;
                            } else {
                                // `Held(lease)` commits through the lease; `NoRange`
                                // delivers without a lease and without offset advance.
                                let lease = match lease_acquire {
                                    BridgeLeaseAcquire::Held(lease) => Some(lease),
                                    _ => None,
                                };
                                {
                                    let replace_outcome = gateway
                                        .replace_message_with_outcome(
                                            channel_id,
                                            current_msg_id,
                                            &delivery_response,
                                        )
                                        .await;
                                    // #2860: delivered if the placeholder was edited OR a
                                    // fallback posted the full delivery_response as a fresh
                                    // message (edit non-committed); record it delivered so
                                    // stall-watchdog recovery does not re-deliver this turn.
                                    let fallback_delivered = matches!(
                                        &replace_outcome,
                                        Ok(super::formatting::ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. })
                                    );
                                    let replace_committed = turn_bridge_replace_outcome_committed(
                                        shared_owned.as_ref(),
                                        &provider,
                                        channel_id,
                                        current_msg_id,
                                        inflight_state.tmux_session_name.as_deref(),
                                        replace_outcome,
                                        dispatch_id.as_deref(),
                                        adk_session_key.as_deref(),
                                        Some(turn_id.as_str()),
                                        "turn_bridge_terminal_replace",
                                    );
                                    // #3041 P1-2 / B6: confirmed_end advance flows ONLY
                                    // through the lease commit — `Delivered` on a committed
                                    // replace, `NotDelivered` otherwise.
                                    let outcome = if let Some(lease) = lease {
                                        let lease_range = lease.range();
                                        let outcome = if replace_committed {
                                            crate::services::discord::LeaseOutcome::Delivered
                                        } else {
                                            crate::services::discord::LeaseOutcome::NotDelivered
                                        };
                                        let committed = lease.commit_and_advance(
                                            shared_owned.as_ref(),
                                            watcher_owner_channel_id,
                                            inflight_state.tmux_session_name.as_deref(),
                                            outcome,
                                        );
                                        // #3630: mirror the delivered frontier like the
                                        // cutover/long-chunk paths so a post-restart
                                        // no-inflight watcher dedups it instead of
                                        // re-relaying a duplicate.
                                        if replace_committed && committed {
                                            super::outbound::delivery_record::shadow_mirror_delivered_frontier(
                                                shared_owned.as_ref(),
                                                &provider,
                                                watcher_owner_channel_id,
                                                lease_range,
                                                true,
                                                Some(current_msg_id.get()),
                                                Some(channel_id.get()),
                                            );
                                        }
                                        replace_committed
                                    } else {
                                        // NoRange: no new bytes, so deliver without a lease
                                        // and without advancing.
                                        replace_committed
                                    };
                                    if outcome {
                                        terminal_delivery_committed = true;
                                        terminal_body_visible = true;
                                        if single_message_panel_footer_mode {
                                            completion_footer_terminal_text =
                                                Some(delivery_response.clone());
                                        }
                                    } else {
                                        preserve_inflight_for_cleanup_retry = true;
                                        if fallback_delivered {
                                            // The fallback carried the whole response; persist
                                            // that offset so recovery never treats it as
                                            // never-delivered.
                                            inflight_state.response_sent_offset =
                                                full_response.len();
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    match enqueue_headless_delivery(
                        &shared_owned,
                        channel_id,
                        user_msg_id,
                        adk_session_key.as_deref(),
                        inflight_state.delivery_bot.as_deref(),
                        &delivery_response,
                        Some(cancel_token.as_ref()),
                    )
                    .await
                    {
                        Ok(()) => {
                            cleanup_headless_streaming_placeholder_after_delivery(
                                shared_owned.as_ref(),
                                channel_id,
                                current_msg_id,
                                status_panel_msg_id,
                                &last_edit_text,
                                &provider,
                            )
                            .await;
                            terminal_delivery_committed = true;
                            terminal_body_visible = true;
                        }
                        Err(error) => {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ headless delivery enqueue failed for channel {}: {} — preserving inflight for retry (full_response not yet delivered)",
                                channel_id,
                                error
                            );
                            // Symmetric with the can_chain_locally failure arm: the answer was NOT delivered (enqueue failed) → do NOT let finalization clear inflight (it is the only persisted full_response). Preserving routes disposition through save_inflight_state so recovery can re-deliver.
                            preserve_inflight_for_cleanup_retry = true;
                        }
                    }
                }
            }

            if terminal_delivery_committed {
                response_sent_offset = full_response.len();
                inflight_state.response_sent_offset = response_sent_offset;
                inflight_state.terminal_delivery_committed = true;
                inflight_state.full_response = full_response.clone();
                if let Err(error) = save_inflight_state(&inflight_state) {
                    tracing::warn!(
                        provider = %provider.as_str(),
                        channel = channel_id.get(),
                        error = %error,
                        "turn bridge failed to mirror committed terminal delivery before cleanup"
                    );
                }
                for frozen_msg_id in terminal_full_replay_cleanup_msg_ids.drain(..) {
                    // #5413/#3607: current_msg_id is the terminal answer and is
                    // already excluded here, so no terminal-anchor guard is
                    // needed — every drained id is a non-terminal streamed prefix.
                    if frozen_msg_id == current_msg_id {
                        continue;
                    }
                    let (replay_outcome, replay_detail) =
                        match gateway.delete_message(channel_id, frozen_msg_id).await {
                            Ok(()) => {
                                tracing::info!(
                                    target: "agentdesk::codex_rollout_handoff",
                                    provider = %provider.as_str(),
                                    channel = channel_id.get(),
                                    message_id = frozen_msg_id.get(),
                                    "turn_bridge removed streamed rollover prefix after full terminal replay"
                                );
                                ("committed", None)
                            }
                            Err(error) => {
                                tracing::warn!(
                                    target: "agentdesk::codex_rollout_handoff",
                                    provider = %provider.as_str(),
                                    channel = channel_id.get(),
                                    message_id = frozen_msg_id.get(),
                                    error = %error,
                                    "turn_bridge failed to remove streamed rollover prefix after full terminal replay"
                                );
                                ("failed", Some(error))
                            }
                        };
                    crate::services::observability::emit_relay_delete(
                        provider.as_str(),
                        channel_id.get(),
                        frozen_msg_id.get(),
                        adk_session_key.as_deref(),
                        Some(turn_id.as_str()),
                        "full_terminal_replay_prefix",
                        "delete_nonterminal",
                        replay_outcome,
                        replay_detail.as_deref(),
                    );
                }
                // #2236: look up the typed handoff marker stamped at dispatch
                // time so multi-agent setups with overlapping background
                // channels can be disambiguated by the stored agent_id. The
                // marker is consumed inside voice_background_completion_target
                // below; passing the stored agent_id (cloned, not taken) here
                // keeps both lookups consistent for this single dispatch.
                //
                // #2274: if the in-memory marker is absent (dcserver
                // restarted mid-turn, or rehydration has not yet completed)
                // fall back to the durable PG row for the agent_id lookup.
                // A recovery turn with no anchored user message (user_msg_id ==
                // 0) is never a voice turn — voice turns carry a synthetic,
                // non-zero voice message id — so the voice-handoff completion
                // routing (all keyed on the user message id) does not apply.
                if let Some(user_msg_id) = user_msg_id {
                    let pg_pool_for_handoff = shared_owned.pg_pool.as_ref();
                    let in_memory_handoff_agent_id =
                        crate::voice::announce_meta::global_store()
                            .get_handoff(user_msg_id)
                            .and_then(|meta| meta.agent_id);
                    let stored_handoff_agent_id = match in_memory_handoff_agent_id {
                        Some(agent_id) => Some(agent_id),
                        None => match pg_pool_for_handoff {
                            Some(pool) => crate::voice::announce_meta::load_handoff_durable(
                                pool,
                                user_msg_id,
                            )
                            .await
                            .ok()
                            .flatten()
                            .and_then(|meta| meta.agent_id),
                            None => None,
                        },
                    };
                    let mapped_voice_channel_id = shared_owned
                        .voice_barge_in
                        .voice_channel_for_background(channel_id, stored_handoff_agent_id.as_deref())
                        .await;
                    if let Some(voice_channel_id) = voice_background_completion_target(
                        mapped_voice_channel_id,
                        dispatch_id.as_deref(),
                        user_msg_id,
                        Some(&turn_id),
                        &user_text_owned,
                        channel_id,
                        pg_pool_for_handoff,
                    )
                    .await
                    {
                        if !inflight_state.silent_turn {
                            let voice_barge_in = shared_owned.voice_barge_in.clone();
                            let shared_for_voice = shared_owned.clone();
                            let summary_source = spoken_delivery_response.clone();
                            let background_channel_id = channel_id;
                            let failed = cancelled
                                || is_prompt_too_long
                                || transport_error
                                || recovery_retry
                                || resume_failure_detected;
                            super::task_supervisor::spawn_observed("voice_background_completion_summary", async move {
                                voice_barge_in
                                    .speak_voice_background_completion_summary(
                                        &shared_for_voice,
                                        voice_channel_id,
                                        background_channel_id,
                                        &summary_source,
                                        failed,
                                    )
                                    .await;
                                voice_barge_in.publish_progress_for_playback(
                                    background_channel_id,
                                    Some(voice_channel_id),
                                    "agent:done",
                                );
                            });
                        } else {
                            shared_owned.voice_barge_in.publish_progress_for_playback(
                                channel_id,
                                Some(voice_channel_id),
                                "agent:done",
                            );
                        }
                    } else if inflight_state.source == crate::dispatch::Source::Voice {
                        if !inflight_state.silent_turn {
                            shared_owned
                                .voice_barge_in
                                .spawn_spoken_result_playback(
                                    &shared_owned,
                                    channel_id,
                                    &spoken_delivery_response,
                                )
                                .await;
                        }
                        shared_owned
                            .voice_barge_in
                            .publish_progress(channel_id, "agent:done");
                    }
                }
            }

            // #2161 (Codex round-2 H1): run the TUI completion gate BEFORE
            // dispatch completion / queue drain so a still-busy ClaudeTui
            // pane cannot advance lifecycle state ahead of pane quiescence.
            // The same outcome is reused by the status-panel emit below.
            // The gate lives in the `tmux` module (`#[cfg(unix)]`); on
            // non-unix targets we skip it and emit completion as normal.
            //
            // #2293 — when the early gate (~line 4254, hoisted ABOVE the
            // mailbox release) already ran for this turn, reuse its outcome
            // here instead of polling tmux a second time. The early
            // result is the authoritative one because it was sampled
            // closer to the moment the mailbox would have been released;
            // re-polling here would create a window where the pane could
            // settle into idle AFTER we already deferred mailbox cleanup,
            // producing inconsistent dispatch / status semantics across
            // the two gate sites.
            #[cfg(unix)]
            {
                let tui_transport_error_skip_gate = claude_tui_followup_pre_submit_requeue_candidate
                    || (transport_error
                        && bridge_tui_transport_error_should_skip_quiescence(
                        &provider,
                        inflight_state.runtime_kind,
                        &full_response,
                        ));
                let bridge_gate_outcome = if terminal_delivery_committed
                    && !preserve_inflight_for_cleanup_retry
                    && !tui_transport_error_skip_gate
                {
                    if let Some(outcome) = bridge_tui_gate_outcome_early {
                        outcome
                    } else if let Some(tmux_session_name) =
                        inflight_state.tmux_session_name.as_deref()
                    {
                        super::tmux::run_tui_completion_gate(
                            &provider,
                            channel_id,
                            tmux_session_name,
                            inflight_state.task_notification_kind,
                        )
                        .await
                    } else {
                        super::tmux::TuiCompletionGateOutcome::NotGated
                    }
                } else {
                    if terminal_delivery_committed && tui_transport_error_skip_gate {
                        tracing::info!(
                            provider = %provider.as_str(),
                            channel = channel_id.get(),
                            runtime_kind = ?inflight_state.runtime_kind,
                            "TUI transport error was already delivered; skipping quiescence gate so inflight cleanup can complete"
                        );
                    }
                    if claude_tui_followup_pre_submit_requeue_candidate {
                        followup_requeue::requeue_claude_tui_followup_pre_submit_timeout(
                            &shared_owned,
                            &provider,
                            channel_id,
                            &inflight_state,
                            dispatch_id.as_deref(),
                            adk_session_key.as_deref(),
                            turn_id.as_str(),
                        )
                        .await;
                    }
                    super::tmux::TuiCompletionGateOutcome::NotGated
                };

                bridge_should_emit_completion = bridge_gate_outcome.should_emit_completion();

                // On TimedOut we preserve the inflight + suppress dispatch
                // completion so queued turns do not drain into a busy pane. The
                // next watcher pass / placeholder sweeper reconciles when the
                // pane finally reports idle.
                if matches!(
                    bridge_gate_outcome,
                    super::tmux::TuiCompletionGateOutcome::TimedOut
                ) {
                    if tui_quiescence_timeout_requires_inflight_retry(terminal_delivery_committed) {
                        preserve_inflight_for_cleanup_retry = true;
                    } else {
                        let terminal_ui_status_msg_id = normalize_status_panel_message_id(
                            inflight_state.status_message_id.map(MessageId::new),
                        );
                        if super::terminal_ui_obligation::should_record_terminal_ui_obligation(
                                terminal_delivery_committed,
                                true,
                                terminal_ui_status_msg_id.is_some(),
                            )
                            && let Some(status_msg_id) = terminal_ui_status_msg_id
                        {
                            super::terminal_ui_obligation::record_terminal_ui_obligation_pending_status(
                                shared_owned.as_ref(),
                                &provider,
                                channel_id,
                                status_msg_id,
                                status_panel_started_at,
                                &inflight_state,
                            )
                            .await;
                        }
                        tracing::warn!(
                            provider = %provider.as_str(),
                            channel = channel_id.get(),
                            "TUI completion quiescence timed out after terminal delivery committed; suppressing visible completion only and continuing inflight cleanup"
                        );
                    }
                }
            }

            if should_complete_work_dispatch_after_terminal_delivery(
                should_complete_work_dispatch_after_delivery,
                terminal_delivery_committed,
                preserve_inflight_for_cleanup_retry,
                resume_failure_detected,
                recovery_retry,
                &full_response,
            ) {
                complete_work_dispatch_on_turn_end(
                    &shared_owned,
                    dispatch_id.as_deref(),
                    adk_cwd.as_deref(),
                    Some(&full_response),
                )
                .await;
            } else if should_fail_dispatch_after_terminal_delivery(
                should_fail_dispatch_after_delivery,
                terminal_delivery_committed,
                preserve_inflight_for_cleanup_retry,
            ) {
                // Transport error — fail the dispatch only after the terminal
                // error response is deliverable, so auto-queue does not advance
                // ahead of visible turn completion.
                fail_dispatch_with_retry(
                    shared_owned.api_port,
                    dispatch_id.as_deref(),
                    &full_response,
                )
                .await;
            }

            // Signal the watcher that this turn's response was already delivered.
            // Prevents the watcher from relaying the same response when it resumes.
            // #3041 P1-2 (codex P1-c): a B2 Skip set
            // `preserve_inflight_for_cleanup_retry = true`, so this gate (encoded in
            // `bridge_epilogue_marks_watcher_delivered`) does NOT mark the watcher
            // delivered — the bridge never delivered the range; the holder owns it.
            if bridge_epilogue_marks_watcher_delivered(
                preserve_inflight_for_cleanup_retry,
                bridge_relay_delegated_to_watcher,
            ) && let Some(watcher) =
                shared_owned.tmux_watchers.get(&watcher_owner_channel_id)
            {
                watcher.turn_delivered.store(true, Ordering::Release);
            }

            if can_chain_locally
                && !preserve_inflight_for_cleanup_retry
                && !delivery_response.trim().is_empty()
                && let Some(user_msg_id) = user_msg_id
            {
                gateway.add_reaction(channel_id, user_msg_id, '✅').await;
            }

            td_warn(terminal_delivery_committed, preserve_inflight_for_cleanup_retry, channel_id);
            status_panel_terminal_committed = status_panel_completion_ready_after_terminal_body(
                terminal_delivery_committed,
                terminal_body_visible,
                preserve_inflight_for_cleanup_retry,
            );
            if status_panel_terminal_committed {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ▶ Response sent");
                if let Ok(mut last) = shared_owned.last_turn_at.lock() {
                    *last = Some(chrono::Local::now().to_rfc3339());
                }
            }
        }

        let mut status_panel_completion_committed = true;
        if status_panel_terminal_committed
            && bridge_should_emit_completion
            && (single_message_panel_footer_mode
                || bridge_should_complete_separate_status_panel(shared_owned.ui.status_panel_v2_enabled))
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
                    shared_owned.ui.placeholder_live_events.set_context_panel_usage(
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
                super::single_message_panel::single_message_panel_spinner_frame(spin_idx);
            status_panel_completion_committed =
                complete_bridge_terminal_footer_or_status_panel(
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
                adk_session_name.as_deref().and_then(
                    crate::services::discord::adk_session::parse_thread_channel_id_from_name,
                ),
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
                channel = channel_id.get(),
                "turn bridge withheld final idle status because status-panel completion edit did not commit"
            );
        }

        if !bridge_relay_delegated_to_watcher
            && should_resume_watcher_after_turn(
            defer_watcher_resume,
            has_queued_turns,
            can_chain_locally,
        ) && let Some(offset) = tmux_last_offset
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
                    if memory_plan.persist_transcript {
                        session.history.push(HistoryItem {
                            item_type: HistoryType::User,
                            content: user_text_owned.clone(),
                        });
                        session.history.push(HistoryItem {
                            item_type: HistoryType::Assistant,
                            content: full_response.clone(),
                        });
                        should_persist_transcript = true;
                    }
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
                    if memory_plan.clear_provider_session {
                        session.clear_provider_session();
                    } else if let Some(sid) = new_session_id.as_ref() {
                        session.restore_provider_session(Some(sid.clone()));
                    }
                    should_spawn_memory_capture = memory_plan.spawn_capture;
                    should_analyze_recall_feedback = memory_plan.analyze_recall_feedback;
                    session.session_id.clone()
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
                super::adk_session::clear_provider_session_id(session_key, shared_owned.api_port)
                    .await;
            }
        } else if let (Some(session_key), Some(persisted_sid)) =
            (adk_session_key.as_deref(), session_id_to_persist.as_deref())
        {
            super::adk_session::save_provider_session_id(
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
        let mut voluntary_feedback_reminder_injected = false;
        if let Some(analysis) = recall_feedback_analysis.as_ref()
            && let Some(reminder) = build_voluntary_feedback_reminder(analysis)
        {
            push_transcript_event(&mut transcript_events, reminder_transcript_event(reminder));
            voluntary_feedback_reminder_injected = true;
            recall_feedback_analysis = Some(analyze_recall_feedback_turn(&transcript_events));
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

        if should_persist_transcript
            && (None::<&crate::db::Db>.is_some() || shared_owned.pg_pool.is_some())
        {
            let channel_id_text = channel_id.get().to_string();
            if let Err(e) = crate::db::session_transcripts::persist_turn_db(
                None::<&crate::db::Db>,
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

        if (None::<&crate::db::Db>.is_some() || shared_owned.pg_pool.is_some())
            && !api_friction_reports.is_empty()
        {
            match crate::services::api_friction::record_api_friction_reports(
                None::<&crate::db::Db>,
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
        if (None::<&crate::db::Db>.is_some() || shared_owned.pg_pool.is_some())
            && let Some(user_msg_id) = user_msg_id
        {
            persist_turn_analytics_row_with_handles(
                None::<&crate::db::Db>,
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

        let mut auto_feedback_count = 0usize;
        if let Some(analysis) = recall_feedback_analysis.as_ref()
            && should_submit_automatic_feedback_fallback(
                analysis,
                voluntary_feedback_reminder_injected,
            )
        {
            let submit_result = match tokio::time::timeout(
                std::time::Duration::from_secs(20),
                submit_pending_feedbacks(
                    &capture_memory_settings,
                    session_id_to_persist.as_deref(),
                    analysis.pending_feedbacks.clone(),
                ),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] [memory] submit_pending_feedbacks timed out after 20s for channel {}",
                        channel_id.get(),
                    );
                    Default::default()
                }
            };
            auto_feedback_count = submit_result.submitted_count;
            accumulated_memory_input_tokens = accumulated_memory_input_tokens
                .saturating_add(submit_result.token_usage.input_tokens);
            accumulated_memory_output_tokens = accumulated_memory_output_tokens
                .saturating_add(submit_result.token_usage.output_tokens);
            for error in submit_result.errors {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!("  [{ts}] ⚠ failed to auto-submit recall tool_feedback: {error}");
            }
        }

        if let (Some(db), Some(analysis)) =
            (None::<&crate::db::Db>, recall_feedback_analysis.as_ref())
            && analysis.recall_count > 0
        {
            let stat = crate::db::memento_feedback_stats::MementoFeedbackTurnStat {
                turn_id: turn_id.clone(),
                stat_date: chrono::Local::now().format("%Y-%m-%d").to_string(),
                agent_id: memory_role_id.clone(),
                provider: provider.as_str().to_string(),
                recall_count: i64::try_from(analysis.recall_count).unwrap_or(i64::MAX),
                manual_tool_feedback_count: i64::try_from(analysis.manual_feedback_count)
                    .unwrap_or(i64::MAX),
                manual_covered_recall_count: i64::try_from(analysis.manual_covered_recall_count)
                    .unwrap_or(i64::MAX),
                auto_tool_feedback_count: i64::try_from(auto_feedback_count).unwrap_or(i64::MAX),
                covered_recall_count: i64::try_from(
                    analysis.covered_recall_count_after(auto_feedback_count),
                )
                .unwrap_or(i64::MAX),
            };
            if let Err(error) = crate::db::memento_feedback_stats::upsert_turn_stat(db, &stat) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!("  [{ts}] ⚠ failed to persist memento feedback stats: {error}");
            }
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
                handle: spawn_memory_capture_task(
                    channel_id,
                    capture_memory_settings,
                    capture_request,
                ),
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
            super::metrics::record_turn(&super::metrics::TurnMetric {
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
            let _ = save_inflight_state(&inflight_state);
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
            // survives. Every other preserve site (bridge-owned cleanup retry) and
            // the delegated-owner path keep the blind save: there is no competing
            // holder, so the bridge's save is authoritative.
            let identity_guarded_skip_save =
                bridge_epilogue_skip_save_is_identity_guarded(bridge_skip_holder_owns_inflight);
            if identity_guarded_skip_save {
                let expected_identity =
                    crate::services::discord::inflight::InflightTurnIdentity::from_state(
                        &inflight_state,
                    );
                let guarded_outcome =
                    crate::services::discord::inflight::save_inflight_state_if_matches_identity(
                        &inflight_state,
                        &expected_identity,
                        inflight_state.turn_start_offset,
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
                let _ = save_inflight_state(&inflight_state);
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
                use super::inflight::GuardedClearOutcome;
                match super::inflight::clear_inflight_state_if_matches(
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
                            channel = channel_id.get(),
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
                use super::inflight::GuardedClearOutcome;
                match super::inflight::clear_inflight_state_if_matches_zero_owned(
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
                            channel = channel_id.get(),
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
        super::mailbox_clear_recovery_marker(&shared_owned, channel_id).await;

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
    }.instrument(bridge_span));
}
