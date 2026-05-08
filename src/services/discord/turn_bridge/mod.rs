mod completion_guard;
mod context_window;
mod memory_lifecycle;
mod output_lifecycle;
mod recall_feedback;
mod recovery_text;
mod retry_state;
mod skill_usage;
mod stale_resume;
mod terminal_delivery;
mod tmux_runtime;
mod turn_analytics;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests;

use super::gateway::TurnGateway;
use super::restart_report::{RestartCompletionReport, clear_restart_report, save_restart_report};
use super::*;
use crate::db::session_observability::{
    BackgroundChildSpawn, close_background_child_pg, insert_background_child_pg,
    mark_session_tool_use_pg,
};
use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::db::turns::{PersistTurnOwned, TurnTokenUsage};
use crate::services::agent_protocol::{StatusEvent, TaskNotificationKind};
use crate::services::memory::{
    CaptureRequest, SessionEndReason, TokenUsage, resolve_memory_role_id, resolve_memory_session_id,
};
use crate::services::observability::session_inventory::{
    format_child_inventory_progress, load_child_inventory_by_parent_key_pg,
};
use crate::services::provider::cancel_requested;
use output_lifecycle::{BridgeOutputOwner, classify_bridge_output_owner};
use std::collections::VecDeque;

// Re-exports for pub(super) items used by sibling modules in the discord package
pub(crate) use completion_guard::build_work_dispatch_completion_result;
pub(super) use completion_guard::{
    fail_dispatch_auth_expired, fail_dispatch_with_retry, guard_review_dispatch_completion,
    queue_dispatch_followup_with_handles, runtime_db_fallback_complete_with_result,
    streaming_final_complete_dispatch_with_result,
};
pub(super) use recovery_text::{
    auto_retry_with_history, build_session_retry_context_from_history, store_session_retry_context,
    take_session_retry_context_for_turn_with_audit,
};
pub(super) use stale_resume::result_event_has_stale_resume_error;
pub(crate) use tmux_runtime::TmuxCleanupPolicy;
pub(super) use tmux_runtime::bind_cancel_token_tmux_runtime;
pub(super) use tmux_runtime::cancel_active_token;
pub(super) use tmux_runtime::cancel_token_has_tmux_session;
pub(super) use tmux_runtime::handoff_interrupted_message;
pub(super) use tmux_runtime::stale_inflight_message;
pub(super) use tmux_runtime::stop_active_turn;

pub(super) fn classify_turn_finished_dispatch_kind(
    dispatch_context: Option<&str>,
    dispatch_type: Option<&str>,
) -> Option<&'static str> {
    let parsed =
        dispatch_context.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    if parsed
        .as_ref()
        .is_some_and(|value| json_has_bool_key(value, "auto_queue", true))
    {
        return Some("auto_queue");
    }
    match dispatch_type {
        Some("review-decision") => Some("review_decision"),
        _ => None,
    }
}

fn json_has_bool_key(value: &serde_json::Value, key: &str, expected: bool) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            map.get(key).and_then(|value| value.as_bool()) == Some(expected)
                || map
                    .values()
                    .any(|value| json_has_bool_key(value, key, expected))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| json_has_bool_key(value, key, expected)),
        _ => false,
    }
}

#[cfg(test)]
mod dispatch_kind_tests {
    use super::classify_turn_finished_dispatch_kind;

    #[test]
    fn marks_auto_queue_context() {
        assert_eq!(
            classify_turn_finished_dispatch_kind(
                Some(r#"{"auto_queue":true,"worktree_path":"/tmp/wt"}"#),
                Some("implementation"),
            ),
            Some("auto_queue")
        );
    }

    #[test]
    fn marks_nested_auto_queue_context_to_match_slo_sql_filter() {
        assert_eq!(
            classify_turn_finished_dispatch_kind(
                Some(r#"{"phase_gate":{"auto_queue":true}}"#),
                Some("implementation"),
            ),
            Some("auto_queue")
        );
    }

    #[test]
    fn marks_review_decision_type() {
        assert_eq!(
            classify_turn_finished_dispatch_kind(None, Some("review-decision")),
            Some("review_decision")
        );
    }

    #[test]
    fn auto_queue_takes_precedence_over_review_decision_type() {
        assert_eq!(
            classify_turn_finished_dispatch_kind(
                Some(r#"{"auto_queue":true}"#),
                Some("review-decision"),
            ),
            Some("auto_queue")
        );
    }

    #[test]
    fn keeps_interactive_unclassified() {
        assert_eq!(classify_turn_finished_dispatch_kind(None, None), None);
        assert_eq!(
            classify_turn_finished_dispatch_kind(Some(r#"{"auto_queue":false}"#), Some("review")),
            None
        );
    }
}

// Re-export pub(crate) items
pub(crate) use tmux_runtime::tmux_runtime_paths;

// Items used by spawn_turn_bridge from submodules
use completion_guard::complete_work_dispatch_on_turn_end;
use context_window::{apply_context_token_update, persisted_context_tokens, resolve_done_response};
use memory_lifecycle::{
    optional_metric_token_fields, plan_turn_end_memory, spawn_memory_capture_task,
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
use terminal_delivery::{
    should_complete_work_dispatch_after_terminal_delivery,
    should_fail_dispatch_after_terminal_delivery, turn_bridge_replace_outcome_committed,
};
use tmux_runtime::is_dcserver_restart_command;
use turn_analytics::{
    assert_response_sent_offset_progress, discord_turn_id, emit_turn_quality_event,
    record_turn_bridge_invariant, turn_duration_ms,
};

use super::watcher_lifecycle_decision::should_resume_watcher_after_turn;
use crate::db::session_status::{AWAITING_BG, AWAITING_USER, IDLE};
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

fn merge_task_notification_kind(
    current: Option<TaskNotificationKind>,
    new_kind: TaskNotificationKind,
) -> Option<TaskNotificationKind> {
    let priority = |kind: TaskNotificationKind| match kind {
        TaskNotificationKind::Subagent => 0,
        TaskNotificationKind::Background => 1,
        TaskNotificationKind::MonitorAutoTurn => 2,
    };

    match current {
        Some(existing) if priority(existing) >= priority(new_kind) => Some(existing),
        _ => Some(new_kind),
    }
}

fn release_task_notification_kind(
    current: Option<TaskNotificationKind>,
    closed_kind: TaskNotificationKind,
) -> Option<TaskNotificationKind> {
    match current {
        Some(existing) if existing == closed_kind => None,
        other => other,
    }
}

async fn close_next_tracked_background_child(
    pg_pool: Option<&sqlx::PgPool>,
    child_session_ids: &mut Vec<i64>,
    status: &str,
    reason: &str,
) {
    let Some(pg_pool) = pg_pool else {
        return;
    };
    if child_session_ids.is_empty() {
        return;
    }
    let child_session_id = child_session_ids.remove(0);
    match close_background_child_pg(pg_pool, child_session_id, status).await {
        Ok(_) => {}
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Failed to close background child session {child_session_id} after {reason}: {error}"
            );
        }
    }
}

fn first_request_line(user_text: &str) -> Option<String> {
    user_text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn monitor_handoff_tool_context(
    last_tool_name: Option<&str>,
    last_tool_summary: Option<&str>,
    current_tool_line: Option<&str>,
) -> (Option<String>, Option<String>) {
    let tool_summary = last_tool_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            current_tool_line
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
    let command_summary = last_tool_summary
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "…")
        .map(str::to_string);
    (tool_summary, command_summary)
}

async fn child_progress_line(
    pg_pool: Option<&sqlx::PgPool>,
    parent_session_key: Option<&str>,
) -> Option<String> {
    let (Some(pg_pool), Some(parent_session_key)) = (pg_pool, parent_session_key) else {
        return None;
    };
    match load_child_inventory_by_parent_key_pg(pg_pool, parent_session_key).await {
        Ok(summary) => format_child_inventory_progress(&summary, chrono::Utc::now()),
        Err(error) => {
            tracing::warn!(
                "Failed to load background child inventory for {}: {}",
                parent_session_key,
                error
            );
            None
        }
    }
}

async fn refresh_session_panel_line_from_lifecycle(
    shared: &SharedData,
    channel_id: ChannelId,
    turn_id: &str,
) -> bool {
    let Some(pg_pool) = shared.pg_pool.as_ref() else {
        return false;
    };
    let channel_id_text = channel_id.get().to_string();
    match crate::services::observability::turn_lifecycle::load_latest_session_lifecycle_event(
        pg_pool,
        &channel_id_text,
        turn_id,
    )
    .await
    {
        Ok(Some(event)) => shared
            .placeholder_live_events
            .set_session_panel_lifecycle_event(channel_id, &event.kind, &event.details_json),
        Ok(None) => false,
        Err(error) => {
            tracing::debug!(
                "[turn_bridge] failed to load session lifecycle line for turn {} in channel {}: {}",
                turn_id,
                channel_id,
                error
            );
            false
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskPanelDispatchMetadata {
    card_id: Option<String>,
    dispatch_type: Option<String>,
}

async fn load_task_panel_dispatch_metadata(
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
) -> Result<Option<TaskPanelDispatchMetadata>, String> {
    let Some(pg_pool) = pg_pool else {
        return Ok(None);
    };
    let row = sqlx::query(
        "SELECT kanban_card_id, dispatch_type
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pg_pool)
    .await
    .map_err(|error| format!("load task panel dispatch metadata for {dispatch_id}: {error}"))?;

    row.map(|row| {
        Ok::<TaskPanelDispatchMetadata, String>(TaskPanelDispatchMetadata {
            card_id: row
                .try_get("kanban_card_id")
                .map_err(|error| format!("decode task panel card_id for {dispatch_id}: {error}"))?,
            dispatch_type: row.try_get("dispatch_type").map_err(|error| {
                format!("decode task panel dispatch_type for {dispatch_id}: {error}")
            })?,
        })
    })
    .transpose()
}

async fn refresh_task_panel_line_from_dispatch(
    shared: &SharedData,
    channel_id: ChannelId,
    dispatch_id: &str,
) -> bool {
    let dispatch_id = dispatch_id.trim();
    if dispatch_id.is_empty() {
        return false;
    }
    let metadata =
        match load_task_panel_dispatch_metadata(shared.pg_pool.as_ref(), dispatch_id).await {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::debug!(
                    "[turn_bridge] failed to load task line for dispatch {} in channel {}: {}",
                    dispatch_id,
                    channel_id,
                    error
                );
                None
            }
        };
    shared.placeholder_live_events.set_task_panel_info(
        channel_id,
        dispatch_id,
        metadata.as_ref().and_then(|value| value.card_id.as_deref()),
        metadata
            .as_ref()
            .and_then(|value| value.dispatch_type.as_deref()),
    )
}

async fn close_all_tracked_background_children(
    pg_pool: Option<&sqlx::PgPool>,
    child_session_ids: &mut Vec<i64>,
    status: &str,
    reason: &str,
) {
    while !child_session_ids.is_empty() {
        close_next_tracked_background_child(pg_pool, child_session_ids, status, reason).await;
    }
}

fn task_notification_closes_background_child(kind: TaskNotificationKind, status: &str) -> bool {
    if !matches!(
        kind,
        TaskNotificationKind::Background | TaskNotificationKind::Subagent
    ) {
        return false;
    }
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "completed"
            | "done"
            | "finished"
            | "aborted"
            | "cancelled"
            | "canceled"
            | "failed"
            | "error"
    )
}

async fn ensure_active_placeholder_card<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    key: super::placeholder_controller::PlaceholderKey,
    input: super::placeholder_controller::PlaceholderActiveInput,
) -> super::placeholder_controller::PlaceholderControllerOutcome {
    if shared.placeholder_live_events_enabled
        && let Some(block) = shared.placeholder_live_events.render_block(key.channel_id)
    {
        return shared
            .placeholder_controller
            .ensure_active_with_live_events(gateway, key, input, block)
            .await;
    }
    shared
        .placeholder_controller
        .ensure_active(gateway, key, input)
        .await
}

fn record_placeholder_live_event(
    shared: &SharedData,
    channel_id: ChannelId,
    event: Option<super::placeholder_live_events::RecentPlaceholderEvent>,
) {
    if (shared.placeholder_live_events_enabled || shared.status_panel_v2_enabled)
        && let Some(event) = event
    {
        shared.placeholder_live_events.push_event(channel_id, event);
    }
}

fn record_status_panel_events(
    shared: &SharedData,
    channel_id: ChannelId,
    events: Vec<StatusEvent>,
) -> bool {
    if shared.status_panel_v2_enabled && !events.is_empty() {
        shared
            .placeholder_live_events
            .push_status_events(channel_id, events);
        true
    } else {
        false
    }
}

async fn complete_status_panel_v2<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    status_panel_msg_id: Option<MessageId>,
    provider: &ProviderKind,
    started_at_unix: i64,
    last_status_panel_text: &mut String,
    background: bool,
    source: &'static str,
) {
    if !shared.status_panel_v2_enabled {
        return;
    }
    let Some(status_msg_id) = status_panel_msg_id else {
        return;
    };
    shared
        .placeholder_live_events
        .push_status_event(channel_id, StatusEvent::TurnCompleted { background });
    let panel_text =
        shared
            .placeholder_live_events
            .render_status_panel(channel_id, provider, started_at_unix);
    if panel_text == *last_status_panel_text {
        return;
    }
    match gateway
        .edit_message(channel_id, status_msg_id, &panel_text)
        .await
    {
        Ok(()) => {
            *last_status_panel_text = panel_text;
        }
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] failed to finalize status-panel-v2 message {} in channel {} from {}: {}",
                status_msg_id,
                channel_id,
                source,
                error
            );
        }
    }
}

fn should_open_long_running_placeholder_controller(status_panel_v2_enabled: bool) -> bool {
    !status_panel_v2_enabled
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

pub(super) struct TurnBridgeContext {
    pub(super) provider: ProviderKind,
    pub(super) gateway: Arc<dyn TurnGateway>,
    pub(super) channel_id: ChannelId,
    pub(super) user_msg_id: MessageId,
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
    pub(super) current_msg_id: MessageId,
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
    pub(super) inflight_state: InflightTurnState,
}

fn push_transcript_event(events: &mut Vec<SessionTranscriptEvent>, event: SessionTranscriptEvent) {
    let has_payload = !event.content.trim().is_empty()
        || event
            .summary
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        || event
            .tool_name
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
    if has_payload
        || matches!(
            event.kind,
            SessionTranscriptEventKind::Thinking
                | SessionTranscriptEventKind::Result
                | SessionTranscriptEventKind::Error
                | SessionTranscriptEventKind::Task
                | SessionTranscriptEventKind::System
        )
    {
        events.push(event);
    }
}

fn status_panel_message_id_for_turn(
    inflight_state: &mut InflightTurnState,
    reuse_status_panel_message: bool,
) -> Option<MessageId> {
    if !reuse_status_panel_message {
        inflight_state.status_message_id = None;
    }
    inflight_state.status_message_id.map(MessageId::new)
}

fn response_portion_after_offset(full_response: &str, response_sent_offset: usize) -> &str {
    full_response.get(response_sent_offset..).unwrap_or("")
}

fn should_delegate_bridge_relay_to_watcher(
    watcher_owns_assistant_relay: bool,
    watcher_relay_available_for_turn: bool,
    bridge_response_pending: bool,
    cancelled: bool,
    is_prompt_too_long: bool,
    transport_error: bool,
    recovery_retry: bool,
) -> bool {
    watcher_owns_assistant_relay
        && watcher_relay_available_for_turn
        && !bridge_response_pending
        && !cancelled
        && !is_prompt_too_long
        && !transport_error
        && !recovery_retry
}

fn live_watcher_registered_for_relay(shared: &SharedData, owner_channel_id: ChannelId) -> bool {
    shared
        .tmux_watchers
        .get(&owner_channel_id)
        .is_some_and(|watcher| !watcher.cancel.load(Ordering::Relaxed))
}

fn advance_tmux_relay_confirmed_end(
    shared: &SharedData,
    channel_id: ChannelId,
    confirmed_end_offset: Option<u64>,
    tmux_session_name: Option<&str>,
) {
    let Some(target_end) = confirmed_end_offset.filter(|offset| *offset > 0) else {
        return;
    };

    let relay_coord = shared.tmux_relay_coord(channel_id);

    // #1270 codex P2 (round 4): capture the `.generation` mtime BEFORE
    // attempting the CAS so the stored mtime is the one that was on disk
    // when we decided to label `target_end` as delivered. Reading after
    // the CAS opens a TOCTOU window where a fresh respawn writes a new
    // `.generation` between our advance and our marker store, then the
    // new mtime ends up paired with the OLD offset and the next
    // regression check mis-classifies the next fresh respawn as
    // same-wrapper rotation. There is still a residual race between this
    // read and any advance that happens earlier in the watcher pipeline
    // (the bytes labelled `target_end` were produced by some prior
    // wrapper, which may already have been replaced before we got here);
    // the fully race-free fix would carry the mtime from byte-read time
    // through the delivery pipeline, but that's a bigger refactor and
    // the typical timeline (cancel → multi-second wait → respawn) keeps
    // this read aligned with the wrapper that produced the bytes.
    let mtime_at_attempt = tmux_session_name
        .map(tmux_generation_file_mtime_ns)
        .filter(|m| *m != 0);

    let mut current = relay_coord
        .confirmed_end_offset
        .load(std::sync::atomic::Ordering::Acquire);
    let mut won_advance = false;

    while current < target_end {
        match relay_coord.confirmed_end_offset.compare_exchange(
            current,
            target_end,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => {
                won_advance = true;
                break;
            }
            Err(observed) => current = observed,
        }
    }

    // #964: observability timestamp — updated whenever the watermark advances
    // (including the CAS-loser path, since that still proves a peer completed
    // a relay) so `GET /api/channels/:id/watcher-state` can surface the most
    // recent relay activity without blocking on disk state.
    relay_coord.last_relay_ts_ms.store(
        chrono::Utc::now().timestamp_millis(),
        std::sync::atomic::Ordering::Release,
    );

    // Pair the pre-CAS mtime with the offset only when we actually won
    // the advance. Losers and no-ops leave the mtime baseline alone so
    // the legitimate winner's snapshot remains the one that labels the
    // watermark (PR #1271 round 3).
    if won_advance && let Some(mtime) = mtime_at_attempt {
        relay_coord
            .confirmed_end_generation_mtime_ns
            .store(mtime, std::sync::atomic::Ordering::Release);
    }

    let confirmed_end = relay_coord
        .confirmed_end_offset
        .load(std::sync::atomic::Ordering::Acquire);
    let confirmed_reached_target = confirmed_end >= target_end;
    crate::services::observability::record_invariant_check(
        confirmed_reached_target,
        crate::services::observability::InvariantViolation {
            provider: None,
            channel_id: Some(channel_id.get()),
            dispatch_id: None,
            session_key: None,
            turn_id: None,
            invariant: "tmux_confirmed_end_monotonic",
            code_location: "src/services/discord/turn_bridge/mod.rs:advance_tmux_relay_confirmed_end",
            message: "tmux relay confirmed_end_offset must reach the delivered output end",
            details: serde_json::json!({
                "target_end": target_end,
                "confirmed_end": confirmed_end,
            }),
        },
    );
    debug_assert!(
        confirmed_reached_target,
        "tmux relay confirmed_end_offset must reach target end"
    );
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

    if super::tmux::refresh_session_heartbeat_from_tmux_output(
        None::<&crate::db::Db>,
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

async fn enqueue_headless_delivery(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    owning_user_msg_id: MessageId,
    session_key: Option<&str>,
    delivery_bot: Option<&str>,
    content: &str,
) -> Result<(), String> {
    let target = format!("channel:{}", channel_id.get());
    let bot = delivery_bot
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("notify");

    let outbox_message = crate::services::message_outbox::OutboxMessage {
        target: &target,
        content,
        bot,
        source: "headless_turn",
        // Explicit reason_code keeps dedupe consistent across PG/SQLite.
        reason_code: Some("headless.delivery"),
        session_key,
    };
    if let Some(pool) = shared.pg_pool.as_ref() {
        match crate::services::message_outbox::enqueue_outbox_pg_returning_id(pool, outbox_message)
            .await
        {
            Ok(Some(outbox_id)) => {
                if let Some(session_key) =
                    session_key.map(str::trim).filter(|value| !value.is_empty())
                {
                    let thread_channel_id = channel_id.get().to_string();
                    let mut tx = match pool.begin().await {
                        Ok(tx) => tx,
                        Err(error) => {
                            return Err(format!(
                                "terminal delivery marker transaction begin failed for session {session_key}: {error}"
                            ));
                        }
                    };
                    if let Err(error) =
                        sqlx::query("SELECT pg_advisory_xact_lock(1752, hashtext($1))")
                            .bind(&thread_channel_id)
                            .execute(&mut *tx)
                            .await
                    {
                        let _ = tx.rollback().await;
                        return Err(format!(
                            "terminal delivery marker lock failed for session {session_key}: {error}"
                        ));
                    }

                    let active_user_message_id =
                        super::mailbox_snapshot(shared.as_ref(), channel_id)
                            .await
                            .active_user_message_id;
                    if let Some(active_user_message_id) = active_user_message_id
                        && active_user_message_id != owning_user_msg_id
                    {
                        tracing::warn!(
                            "[outbox] skipped terminal delivery marker {} for session {} because active turn message changed from {} to {}",
                            outbox_id,
                            session_key,
                            owning_user_msg_id.get(),
                            active_user_message_id.get()
                        );
                    } else if let Err(error) = sqlx::query(
                        "UPDATE sessions
                                SET active_turn_delivery_outbox_id = $1
                              WHERE session_key = $2
                                AND thread_channel_id = $3
                                AND status IN ('turn_active', 'working')",
                    )
                    .bind(outbox_id)
                    .bind(session_key)
                    .bind(&thread_channel_id)
                    .execute(&mut *tx)
                    .await
                    {
                        let _ = tx.rollback().await;
                        return Err(format!(
                            "terminal delivery marker write failed for session {session_key} row {outbox_id}: {error}"
                        ));
                    }
                    if let Err(error) = tx.commit().await {
                        return Err(format!(
                            "terminal delivery marker commit failed for session {session_key}: {error}"
                        ));
                    }
                }
                return Ok(());
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    "[outbox] postgres enqueue failed for terminal response on channel {}: {}",
                    channel_id,
                    error
                );
            }
        }
    }

    let notify_http = if let Some(registry) = shared.health_registry() {
        match super::health::resolve_bot_http(registry.as_ref(), "notify").await {
            Ok(http) => Some(http),
            Err((status, body)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ headless notify bot unavailable in channel {}: {} {} — falling back to provider bot",
                    channel_id,
                    status,
                    body
                );
                None
            }
        }
    } else {
        None
    };

    let http = notify_http
        .or_else(|| shared.cached_serenity_ctx.get().map(|ctx| ctx.http.clone()))
        .ok_or_else(|| {
            format!(
                "headless delivery unavailable for channel {}: no outbox storage or discord http",
                channel_id.get()
            )
        })?;
    send_long_message_raw(&http, channel_id, content, shared)
        .await
        .map_err(|error| format!("headless direct delivery failed: {error}"))?;
    Ok(())
}

fn total_model_input_tokens(
    input_tokens: u64,
    cache_create_tokens: u64,
    cache_read_tokens: u64,
) -> u64 {
    input_tokens
        .saturating_add(cache_create_tokens)
        .saturating_add(cache_read_tokens)
}

struct TurnAnalyticsSnapshot {
    output_path: Option<String>,
    output_start_offset: u64,
    output_end_offset: Option<u64>,
    fallback_session_id: Option<String>,
    fallback_token_usage: TurnTokenUsage,
    inflight_session_id: Option<String>,
}

impl TurnAnalyticsSnapshot {
    fn capture(
        inflight_state: &InflightTurnState,
        fallback_session_id: Option<&str>,
        fallback_token_usage: TurnTokenUsage,
    ) -> Self {
        Self {
            output_path: inflight_state.output_path.clone(),
            output_start_offset: inflight_state
                .turn_start_offset
                .unwrap_or(inflight_state.last_offset),
            output_end_offset: inflight_state
                .output_path
                .as_ref()
                .map(|_| inflight_state.last_offset),
            fallback_session_id: fallback_session_id.map(str::to_string),
            fallback_token_usage,
            inflight_session_id: inflight_state.session_id.clone(),
        }
    }
}

fn resolve_output_analytics_snapshot(
    snapshot: &TurnAnalyticsSnapshot,
) -> (Option<String>, TurnTokenUsage) {
    let (output_session_id, output_token_usage) = snapshot
        .output_path
        .as_deref()
        .map(|path| {
            crate::services::session_backend::extract_turn_analytics_from_output_range(
                path,
                snapshot.output_start_offset,
                snapshot.output_end_offset,
            )
        })
        .unwrap_or((None, None));

    (
        output_session_id
            .or_else(|| snapshot.fallback_session_id.clone())
            .or_else(|| snapshot.inflight_session_id.clone()),
        output_token_usage.unwrap_or(snapshot.fallback_token_usage),
    )
}

pub(super) fn persist_turn_analytics_row(
    sqlite: &crate::db::Db,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    role_binding: Option<&RoleBinding>,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    session_id: Option<&str>,
    inflight_state: &InflightTurnState,
    token_usage: TurnTokenUsage,
    duration_ms: i64,
) {
    persist_turn_analytics_row_with_handles(
        Some(sqlite),
        None,
        provider,
        channel_id,
        user_msg_id,
        role_binding,
        dispatch_id,
        session_key,
        session_id,
        inflight_state,
        token_usage,
        duration_ms,
    );
}

pub(super) fn persist_turn_analytics_row_with_handles(
    _db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    role_binding: Option<&RoleBinding>,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    session_id: Option<&str>,
    inflight_state: &InflightTurnState,
    token_usage: TurnTokenUsage,
    duration_ms: i64,
) {
    let thread_id = inflight_state
        .thread_id
        .map(|value| value.to_string())
        .or_else(|| {
            inflight_state
                .channel_name
                .as_deref()
                .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name)
                .map(|value| value.to_string())
        });
    let turn_id = discord_turn_id(provider, channel_id, user_msg_id, session_key);
    let session_key = session_key.map(str::to_string);
    let thread_title = inflight_state.thread_title.clone();
    let persisted_channel_id = inflight_state
        .logical_channel_id
        .unwrap_or(channel_id.get())
        .to_string();
    let agent_id = role_binding.map(|binding| binding.role_id.clone());
    let provider_name = provider.as_str().to_string();
    let dispatch_id = dispatch_id
        .map(str::to_string)
        .or_else(|| inflight_state.dispatch_id.clone());
    let started_at = inflight_state.started_at.clone();
    let analytics_snapshot =
        TurnAnalyticsSnapshot::capture(inflight_state, session_id, token_usage);
    let (resolved_session_id, resolved_token_usage) =
        resolve_output_analytics_snapshot(&analytics_snapshot);
    let entry = PersistTurnOwned {
        turn_id,
        session_key,
        thread_id,
        thread_title,
        channel_id: persisted_channel_id,
        agent_id,
        provider: Some(provider_name),
        session_id: resolved_session_id,
        dispatch_id,
        started_at: Some(started_at),
        finished_at: Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()),
        duration_ms: Some(duration_ms),
        token_usage: resolved_token_usage,
    };
    let pg_pool = pg_pool.cloned();
    let persist_pg = move |pg_pool: sqlx::PgPool, entry: PersistTurnOwned| async move {
        if let Err(error) = crate::db::turns::upsert_turn_owned_db(Some(&pg_pool), &entry).await {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ failed to persist turn analytics row: {error}");
        }
    };

    let Some(pg_pool) = pg_pool else {
        return;
    };
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        let _ = runtime.spawn(persist_pg(pg_pool, entry));
        return;
    }
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => {
            runtime.block_on(persist_pg(pg_pool, entry));
        }
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ failed to create runtime for turn analytics persistence: {error}"
            );
        }
    }
}

pub(super) fn spawn_turn_bridge(
    shared_owned: Arc<SharedData>,
    cancel_token: Arc<CancelToken>,
    rx: mpsc::Receiver<StreamMessage>,
    bridge: TurnBridgeContext,
) {
    use tracing::Instrument;
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
    );
    tokio::spawn(async move {
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let channel_id = bridge.channel_id;
        let provider = bridge.provider.clone();
        let gateway = bridge.gateway.clone();
        let user_msg_id = bridge.user_msg_id;
        let turn_id = discord_turn_id(
            &provider,
            bridge.channel_id,
            bridge.user_msg_id,
            bridge.adk_session_key.as_deref(),
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

        let mut full_response = bridge.full_response.clone();
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
        let mut watcher_owns_assistant_relay = bridge.inflight_state.watcher_owns_live_relay;
        let mut watcher_relay_available_for_turn = watcher_owns_assistant_relay
            && live_watcher_registered_for_relay(shared_owned.as_ref(), channel_id);
        // #1452 (Codex iter 3 P1): track whether THIS turn published a
        // mailbox-finalization debt onto the watcher handle. Without this
        // flag, the bridge's non-delegation `compare_exchange(true, false, ...)`
        // cannot tell apart "watcher consumed our debt" (skip finalize) from
        // "we never set debt at all" (must finalize). The flag flips to
        // true at the watcher-unpause site below; the non-delegation
        // finalization branch consults it to decide whether the
        // compare_exchange Err arm means "watcher beat us" or "no debt at
        // all".
        let mut bridge_published_finalize_owed_for_this_turn = false;
        // #1255 live-turn long-running tool placeholder card.
        //
        // `last_assistant_text_line` captures the last non-empty single-line
        // assistant prose emission so we can surface it as the placeholder
        // card's `요약` slot (the "⏳ CI 통과 신호 대기" use case from the
        // issue). It is reset on tool result / completion so a stale line
        // never leaks into the next tool placeholder.
        //
        // `long_running_placeholder_active` is `Some(...)` while a Monitor /
        // background-Bash call is mid-flight. It records the placeholder key
        // we are driving so the matching ToolResult / Done event can call
        // `controller.transition(Completed)`. The cancel / abort paths use
        // the same handle.
        let mut last_assistant_text_line: Option<String> = None;
        // Pair the active key with the input snapshot, close-trigger kind, and
        // an `ack_consumed` flag.
        //
        // Rollover uses the snapshot to retarget the controller onto the new
        // `current_msg_id`; the close-trigger distinguishes Monitor-style
        // ToolResult-closes from background-dispatch ack events; and
        // `ack_consumed` (codex round-6 P2 on #1308) prevents subsequent
        // unrelated ToolResults — for example a failing `Read`/`Grep` later
        // in the same turn — from closing a still-running background card.
        let mut long_running_placeholder_active: Option<(
            super::placeholder_controller::PlaceholderKey,
            super::placeholder_controller::PlaceholderActiveInput,
            super::formatting::LongRunningCloseTrigger,
            bool, // ack_consumed
        )> = None;
        let mut active_background_child_session_ids: Vec<i64> = Vec::new();
        if shared_owned.placeholder_live_events_enabled || shared_owned.status_panel_v2_enabled {
            shared_owned.placeholder_live_events.clear_channel(channel_id);
        }
        let mut transport_error = false;
        let mut api_friction_reports = Vec::new();
        let mut transcript_events = Vec::<SessionTranscriptEvent>::new();
        let mut resume_failure_detected = false;
        let mut terminal_session_reset_required = false;
        let mut recovery_retry = false;
        let mut last_adk_heartbeat = std::time::Instant::now();
        let mut pending_status_tool_results: VecDeque<String> = VecDeque::new();
        // codex round-8 P1 on PR #1308: while a long-running placeholder is
        // active, bump the inflight file's mtime so the sweeper sees the turn
        // as alive. Without this, a healthy 5+ minute background tool would
        // exceed `ABANDON_THRESHOLD_SECS` and the sweeper would cancel it.
        let mut last_inflight_long_run_heartbeat = std::time::Instant::now();
        const LIVE_LONG_RUN_HEARTBEAT_INTERVAL: std::time::Duration =
            std::time::Duration::from_secs(30);
        let mut last_activity_heartbeat_at: Option<std::time::Instant> = None;
        let mut current_msg_id = bridge.current_msg_id;
        let mut response_sent_offset = bridge.response_sent_offset;
        let mut tmux_last_offset = bridge.tmux_last_offset;
        let mut watcher_owner_channel_id = channel_id;
        let mut new_session_id = bridge.new_session_id.clone();
        let mut new_raw_provider_session_id: Option<String> = None;
        let defer_watcher_resume = bridge.defer_watcher_resume;
        let completion_tx = bridge.completion_tx;
        // Guard: ensure completion_tx fires even if the task panics or
        // exits early, preventing the parent from hanging on completion_rx.
        struct CompletionGuard(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for CompletionGuard {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let _completion_guard = CompletionGuard(completion_tx);

        // Guard: ensure inflight state file is cleaned up even if the task
        // panics or exits early.  On the normal path we defuse the guard
        // after the explicit clear_inflight_state() call.
        struct InflightCleanupGuard {
            provider: Option<ProviderKind>,
            channel_id: u64,
        }
        impl Drop for InflightCleanupGuard {
            fn drop(&mut self) {
                if let Some(ref provider) = self.provider {
                    clear_inflight_state(provider, self.channel_id);
                }
            }
        }
        let mut inflight_guard = InflightCleanupGuard {
            provider: Some(provider.clone()),
            channel_id: channel_id.get(),
        };

        let mut inflight_state = bridge.inflight_state.clone();
        let mut last_status_edit = tokio::time::Instant::now();
        let status_interval = super::status_update_interval();
        let mut last_session_panel_lifecycle_refresh =
            tokio::time::Instant::now() - status_interval;
        let mut status_panel_msg_id = status_panel_message_id_for_turn(
            &mut inflight_state,
            bridge.reuse_status_panel_message,
        );
        let mut last_status_panel_text = String::new();
        let mut status_panel_dirty = shared_owned.status_panel_v2_enabled;
        let mut last_status_panel_edit = tokio::time::Instant::now() - status_interval;
        let status_panel_started_at = chrono::Utc::now().timestamp();
        let turn_start = std::time::Instant::now();

        if shared_owned.status_panel_v2_enabled && status_panel_msg_id.is_none() {
            let response_placeholder = super::formatting::build_processing_status_block(SPINNER[0]);
            match gateway.send_message(channel_id, &response_placeholder).await {
                Ok(response_msg_id) => {
                    status_panel_msg_id = Some(current_msg_id);
                    inflight_state.status_message_id = Some(current_msg_id.get());
                    current_msg_id = response_msg_id;
                    last_edit_text = response_placeholder.to_string();
                    inflight_state.current_msg_id = current_msg_id.get();
                    inflight_state.current_msg_len = last_edit_text.len();
                    inflight_state.response_sent_offset = response_sent_offset;
                    inflight_state.full_response = full_response.clone();
                }
                Err(error) => {
                    tracing::warn!(
                        "[turn_bridge] failed to create status-panel-v2 response message in channel {}: {}",
                        channel_id,
                        error
                    );
                    status_panel_dirty = false;
                }
            }
        }

        if shared_owned.status_panel_v2_enabled
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

        let _ = save_inflight_state(&inflight_state);
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
                "user_msg_id": user_msg_id.get(),
                "request_owner_name": request_owner_name.as_str(),
            }),
        );

        while !done {
            let mut state_dirty = false;

            if cancel_requested(Some(cancel_token.as_ref())) {
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
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

            if cancel_requested(Some(cancel_token.as_ref())) {
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
                break;
            }

            loop {
                match rx.try_recv() {
                    Ok(msg) => match msg {
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
                            state_dirty = true;
                        }
                        StreamMessage::Text { content } => {
                            full_response.push_str(&content);
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
                        StreamMessage::ToolUse { name, input } => {
                            pending_status_tool_results.push_back(name.clone());
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
                            if None::<&crate::db::Db>.is_some() || shared_owned.pg_pool.is_some() {
                                match record_skill_usage_from_tool_use(
                                    None::<&crate::db::Db>,
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
                                super::placeholder_live_events::status_events_from_tool_use(
                                    &name, &input,
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
                                shared_owned.status_panel_v2_enabled,
                            )
                                && long_running_placeholder_active.is_none()
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
                                    let outcome = ensure_active_placeholder_card(
                                        shared_owned.as_ref(),
                                        gateway.as_ref(),
                                        key.clone(),
                                        input_payload.clone(),
                                    )
                                    .await;
                                    // codex round-2 P2: only commit the active
                                    // pointer when the controller actually
                                    // committed (or coalesced an existing
                                    // edit); otherwise the regular streaming
                                    // path stays in charge so the turn isn't
                                    // visually frozen on a transient edit
                                    // failure.
                                    use super::placeholder_controller::PlaceholderControllerOutcome::*;
                                    if matches!(outcome, Edited | Coalesced) {
                                        long_running_placeholder_active = Some((
                                            key,
                                            input_payload,
                                            close_trigger,
                                            false, // ack not yet consumed
                                        ));
                                        inflight_state
                                            .long_running_placeholder_active = true;
                                        state_dirty = true;
                                    }
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
                                let trimmed = full_response.trim_end();
                                full_response.truncate(trimmed.len());
                                full_response.push_str("\n\n");
                                inflight_state.full_response = full_response.clone();
                                state_dirty = true;
                            }
                        }
                        StreamMessage::ToolResult { content, is_error } => {
                            let status_tool_name = pending_status_tool_results.pop_front();
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
                                    shared_owned.as_ref(),
                                    channel_id,
                                    super::placeholder_live_events::RecentPlaceholderEvent::tool_error(
                                        &content,
                                    ),
                                );
                            }
                            status_panel_dirty |= record_status_panel_events(
                                shared_owned.as_ref(),
                                channel_id,
                                super::placeholder_live_events::status_events_from_tool_result(
                                    status_tool_name.as_deref(),
                                    is_error,
                                ),
                            );
                            // #1255: a long-running tool's ToolResult means the
                            // background card can transition to its terminal
                            // state.  We still keep the placeholder around for
                            // the rest of the turn so the user can see the
                            // status line; the controller's idempotent terminal
                            // transition keeps duplicate edits free.
                            // codex round-2 P1: only `Monitor`-style tools
                            // deliver their real completion via `ToolResult`.
                            // Background `Bash`/`Task`/`Agent` dispatches send
                            // back a job/task id ack on `ToolResult` and the
                            // actual work continues — terminating here would
                            // close the card while the background job is
                            // still running. Keep those open until `Done` /
                            // cancel.
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
                                    let outcome = shared_owned
                                        .placeholder_controller
                                        .transition(gateway.as_ref(), key.clone(), target)
                                        .await;
                                    // codex round-10 P2: only clear flag on
                                    // committed/already-terminal outcome.
                                    use super::placeholder_controller::PlaceholderControllerOutcome::*;
                                    if matches!(outcome, Edited | Coalesced | AlreadyTerminal) {
                                        inflight_state
                                            .long_running_placeholder_active = false;
                                        state_dirty = true;
                                    } else {
                                        // EditFailed — keep the placeholder
                                        // active so the next event/sweeper
                                        // can retry the terminal edit.
                                        long_running_placeholder_active =
                                            Some((key, snapshot, close_trigger, ack_consumed));
                                    }
                                } else {
                                    // Successful background dispatch ack OR a
                                    // later unrelated ToolResult — re-stash so
                                    // `Done`/cancel can close the card. Mark
                                    // the ack consumed so future is_error
                                    // results from other tools don't abort us.
                                    long_running_placeholder_active = Some((
                                        key,
                                        snapshot,
                                        close_trigger,
                                        true,
                                    ));
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
                        StreamMessage::TaskNotification {
                            summary,
                            status,
                            kind,
                            ..
                        } => {
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
                                super::placeholder_live_events::status_events_from_task_notification(
                                    kind.as_str(),
                                    &status,
                                    &summary,
                                ),
                            );
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
                                // Recovery reader requests the generic
                                // Discord-history auto-retry path when the
                                // resumed session dies before completion.
                                recovery_retry = true;
                            }
                            // #1255: turn finished while a long-running
                            // placeholder is still flagged as Active — close
                            // it now so the user does not stare at a stale
                            // 🔄 card forever. Idempotent if a prior
                            // ToolResult already fired Completed.
                            if let Some((key, snapshot, close_trigger, ack_consumed)) =
                                long_running_placeholder_active.take()
                            {
                                let target = if session_died_retry {
                                    super::placeholder_controller::PlaceholderLifecycle::Aborted
                                } else {
                                    super::placeholder_controller::PlaceholderLifecycle::Completed
                                };
                                let outcome = shared_owned
                                    .placeholder_controller
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
                            if let Some(resolved) = resolve_done_response(
                                &full_response,
                                &result,
                                any_tool_used,
                                has_post_tool_text,
                            ) {
                                full_response = resolved;
                                inflight_state.full_response = full_response.clone();
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
                            if shared_owned.status_panel_v2_enabled && has_context_token_data {
                                status_panel_dirty |= shared_owned
                                    .placeholder_live_events
                                    .set_context_panel_usage(
                                        channel_id,
                                        accumulated_input_tokens,
                                        accumulated_cache_create_tokens,
                                        accumulated_cache_read_tokens,
                                        context_window_tokens,
                                        context_compact_percent,
                                    );
                            }
                        }
                        StreamMessage::TmuxReady {
                            output_path,
                            input_fifo_path,
                            tmux_session_name,
                            last_offset,
                        } => {
                            tmux_handed_off = true;
                            tmux_last_offset = Some(last_offset);
                            inflight_state.tmux_session_name = Some(tmux_session_name.clone());
                            inflight_state.output_path = Some(output_path.clone());
                            inflight_state.input_fifo_path = Some(input_fifo_path);
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
                            let mailbox_finalize_owed =
                                Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let handle = TmuxWatcherHandle {
                                tmux_session_name: tmux_session_name.clone(),
                                paused: paused.clone(),
                                resume_offset: resume_offset.clone(),
                                cancel: cancel.clone(),
                                pause_epoch: pause_epoch.clone(),
                                turn_delivered: turn_delivered.clone(),
                                last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
                                mailbox_finalize_owed: mailbox_finalize_owed.clone(),
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
                            if watcher_claimed {
                                #[cfg(unix)]
                                {
                                    if let Some(ctx) = shared_owned.cached_serenity_ctx.get() {
                                        let http_bg = ctx.http.clone();
                                        let shared_bg = shared_owned.clone();
                                        inflight_state.watcher_owns_live_relay = true;
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
                                        tokio::spawn(super::tmux::tmux_output_watcher_with_restore(
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
                                            mailbox_finalize_owed,
                                            restored_turn,
                                        ));
                                        watcher_relay_available_for_turn = true;
                                        let _ = save_inflight_state(&inflight_state);
                                        watcher_ready_for_relay = true;
                                    } else {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ cached serenity context missing; tmux watcher not started for channel {}",
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
                                inflight_state.watcher_owns_live_relay = true;
                                watcher_owns_assistant_relay = true;
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
                                    // We treat "watcher is now responsible for relay"
                                    // as a superset of "bridge will delegate
                                    // finalization": the store is unconditional here,
                                    // and the bridge's later non-delegation paths
                                    // (cancelled/prompt_too_long/transport_error/
                                    // recovery_retry) revoke the debt with a
                                    // `store(false, Release)` before running their
                                    // own `mailbox_finish_turn`.
                                    watcher
                                        .mailbox_finalize_owed
                                        .store(true, Ordering::Release);
                                    bridge_published_finalize_owed_for_this_turn = true;
                                    // #1452 (Codex iter 3 P1): unpause must
                                    // use Release ordering so a watcher
                                    // observing `paused = false` is
                                    // guaranteed to also observe the
                                    // `mailbox_finalize_owed = true` store
                                    // above. With Relaxed ordering on a
                                    // weakly-ordered platform the two
                                    // stores can be reordered, letting the
                                    // watcher unpause, race to its terminal
                                    // swap, observe `false`, and skip
                                    // `mailbox_finish_turn` — recreating
                                    // the leak this change is meant to fix.
                                    watcher.paused.store(false, Ordering::Release);
                                }
                            }
                            state_dirty = true;
                        }
                        StreamMessage::ProcessReady {
                            output_path,
                            session_name,
                            last_offset,
                        } => {
                            // ProcessBackend completed first turn.
                            // No tmux watcher needed — process sessions are monitored
                            // inline via SessionProbe::process during read_output_file_until_result.
                            // Do NOT set tmux_handed_off: ProcessBackend has no watcher,
                            // so the handoff cleanup path would delete the placeholder
                            // with no one to send the final response.
                            tmux_last_offset = Some(last_offset);
                            inflight_state.tmux_session_name = Some(session_name);
                            inflight_state.output_path = Some(output_path);
                            inflight_state.last_offset = last_offset;
                            state_dirty = true;
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
                    },
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        rx_disconnected = true;
                        done = true;
                        break;
                    }
                }
            }

            if shared_owned.status_panel_v2_enabled
                && last_session_panel_lifecycle_refresh.elapsed() >= status_interval
            {
                last_session_panel_lifecycle_refresh = tokio::time::Instant::now();
                status_panel_dirty |= refresh_session_panel_line_from_lifecycle(
                    shared_owned.as_ref(),
                    channel_id,
                    turn_id.as_str(),
                )
                .await;
            }

            let indicator = SPINNER[spin_idx % SPINNER.len()];
            spin_idx += 1;

            if shared_owned.status_panel_v2_enabled
                && status_panel_dirty
                && last_status_panel_edit.elapsed() >= status_interval
                && let Some(status_msg_id) = status_panel_msg_id
            {
                let panel_text = shared_owned.placeholder_live_events.render_status_panel(
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

            if !watcher_owns_assistant_relay {
                loop {
                    let current_portion =
                        response_portion_after_offset(&full_response, response_sent_offset);
                    if done || current_portion.is_empty() {
                        break;
                    }

                    let indicator = SPINNER[spin_idx % SPINNER.len()];
                    let status_block = if shared_owned.status_panel_v2_enabled {
                        super::formatting::build_processing_status_block(indicator)
                    } else {
                        super::formatting::build_placeholder_status_block(
                            indicator,
                            prev_tool_status.as_deref(),
                            current_tool_line.as_deref(),
                            &full_response,
                        )
                    };
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
                                current_msg_id = next_msg_id;
                                last_edit_text = status_block;
                                last_status_edit = tokio::time::Instant::now() - status_interval;
                                inflight_state.current_msg_id = current_msg_id.get();
                                inflight_state.current_msg_len = last_edit_text.len();
                                inflight_state.response_sent_offset = response_sent_offset;
                                inflight_state.full_response = full_response.clone();
                                state_dirty = true;
                                // #1255 codex round-1 P2: rollover advanced
                                // `current_msg_id` past the message that owned the
                                // active long-running placeholder. The old message
                                // now holds delivered response content; retarget
                                // the controller onto the new message_id so the
                                // eventual terminal transition lands on the live
                                // card instead of overwriting that frozen chunk.
                                // codex round-2 P2: drop the active pointer if the
                                // retarget edit fails — otherwise we'd suppress
                                // streaming with no card visible.
                                // codex round-4 P2: detach the old key first so
                                // its `Active` controller entry doesn't linger as
                                // a non-evictable row in the cap-bounded map.
                                if let Some((old_key, snapshot, close_trigger, ack_consumed)) =
                                    long_running_placeholder_active.take()
                                {
                                    shared_owned.placeholder_controller.detach(&old_key);
                                    let new_key = super::placeholder_controller::PlaceholderKey {
                                        provider: provider.clone(),
                                        channel_id,
                                        message_id: current_msg_id,
                                    };
                                    let outcome = ensure_active_placeholder_card(
                                        shared_owned.as_ref(),
                                        gateway.as_ref(),
                                        new_key.clone(),
                                        snapshot.clone(),
                                    )
                                    .await;
                                    use super::placeholder_controller::PlaceholderControllerOutcome::*;
                                    if matches!(outcome, Edited | Coalesced) {
                                        long_running_placeholder_active = Some((
                                            new_key,
                                            snapshot,
                                            close_trigger,
                                            ack_consumed,
                                        ));
                                        // Flag is already true; refresh
                                        // updated_at-side bookkeeping by writing
                                        // through state_dirty.
                                        state_dirty = true;
                                    } else {
                                        // Retarget edit failed — drop the flag so
                                        // the regular streaming loop and sweeper
                                        // resume normal handling.
                                        inflight_state.long_running_placeholder_active = false;
                                        state_dirty = true;
                                    }
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

                let current_portion =
                    response_portion_after_offset(&full_response, response_sent_offset);
                let status_block = if shared_owned.status_panel_v2_enabled {
                    super::formatting::build_processing_status_block(indicator)
                } else {
                    super::formatting::build_placeholder_status_block(
                        indicator,
                        prev_tool_status.as_deref(),
                        current_tool_line.as_deref(),
                        &full_response,
                    )
                };
                let stable_display_text =
                    super::formatting::build_streaming_placeholder_text(current_portion, &status_block);

                // #1255 codex round-1 P2: while a long-running placeholder owns
                // `current_msg_id`, the controller is the sole writer. Skipping the
                // regular streaming edit prevents `stable_display_text` from
                // overwriting the `🔄 백그라운드 처리 중` card mid-flight.
                if stable_display_text != last_edit_text
                    && !done
                    && last_status_edit.elapsed() >= status_interval
                    && long_running_placeholder_active.is_none()
                {
                    let _ = gateway
                        .edit_message(channel_id, current_msg_id, &stable_display_text)
                        .await;
                    last_edit_text = stable_display_text;
                    last_status_edit = tokio::time::Instant::now();
                    inflight_state.current_msg_id = current_msg_id.get();
                    inflight_state.current_msg_len = last_edit_text.len();
                    inflight_state.response_sent_offset = response_sent_offset;
                    inflight_state.full_response = full_response.clone();
                    state_dirty = true;
                }
            }

            if shared_owned.placeholder_live_events_enabled
                && watcher_owns_assistant_relay
                && let Some((key, input, _, _)) = long_running_placeholder_active.as_ref()
                && let Some(block) = shared_owned.placeholder_live_events.render_block(channel_id)
            {
                let outcome = shared_owned
                    .placeholder_controller
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

            if state_dirty
                || inflight_state.current_tool_line != current_tool_line
                || inflight_state.last_tool_name != last_tool_name
                || inflight_state.last_tool_summary != last_tool_summary
                || inflight_state.prev_tool_status != prev_tool_status
            {
                inflight_state.current_tool_line = current_tool_line.clone();
                inflight_state.last_tool_name = last_tool_name.clone();
                inflight_state.last_tool_summary = last_tool_summary.clone();
                inflight_state.prev_tool_status = prev_tool_status.clone();
                let _ = save_inflight_state(&inflight_state);
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

        // codex round-9 P3 on PR #1308: drain any active long-running
        // placeholder on stream-error / receive-disconnect exits too. The
        // cancel branch already drives the controller to `Aborted`; here we
        // also need to handle `StreamMessage::Error` and `rx_disconnected`
        // exits so the controller does not leak an `Active` row and the
        // persisted `long_running_placeholder_active` flag does not survive
        // for the sweeper to abandon the card. Skip when `cancelled` (the
        // dedicated cancel block below handles it). For tmux-handoff exits
        // the dedicated handoff branch already calls `detach`, so we leave
        // those alone.
        if !cancelled
            && !(rx_disconnected && tmux_handed_off && full_response.is_empty())
        {
            if let Some((key, _, _, _)) = long_running_placeholder_active.take() {
                let target = if transport_error || rx_disconnected {
                    super::placeholder_controller::PlaceholderLifecycle::Aborted
                } else {
                    super::placeholder_controller::PlaceholderLifecycle::Completed
                };
                let outcome = shared_owned
                    .placeholder_controller
                    .transition(gateway.as_ref(), key, target)
                    .await;
                // codex round-10 P2: keep the persisted flag on EditFailed so
                // the sweeper can finalize the still-visible 🔄 card later.
                use super::placeholder_controller::PlaceholderControllerOutcome::*;
                if matches!(outcome, Edited | Coalesced | AlreadyTerminal) {
                    inflight_state.long_running_placeholder_active = false;
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
        // disconnect, or tmux handoff to a watcher that owns the rest of the
        // delivery). Promote it to its terminal ⚠ form and stash in
        // prev_tool_status so any follow-on placeholder edit (handoff embed,
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
        let bridge_relay_delegated_to_watcher = should_delegate_bridge_relay_to_watcher(
            watcher_owns_assistant_relay,
            watcher_relay_available_for_turn,
            !response_portion_after_offset(&full_response, response_sent_offset)
                .trim()
                .is_empty(),
            cancelled,
            is_prompt_too_long,
            transport_error,
            recovery_retry,
        );

        // Explicitly complete implementation/rework dispatches only after the
        // terminal Discord delivery commits. Completing here used to let
        // dispatch followups / auto-queue slot release race ahead of the final
        // message edit, so an archived/deleted thread could strand the turn
        // while the queue already advanced.
        let should_complete_work_dispatch_after_delivery = !cancelled
            && !is_prompt_too_long
            && !transport_error
            && !bridge_relay_delegated_to_watcher;
        let should_fail_dispatch_after_delivery = transport_error && !cancelled;

        let final_session_status = if cancelled || transport_error {
            IDLE
        } else if active_background_child_session_ids.is_empty() {
            AWAITING_USER
        } else {
            AWAITING_BG
        };

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
            .finalizing_turns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        shared_owned
            .global_finalizing
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let has_queued_turns = if bridge_relay_delegated_to_watcher {
            // #1452 (Codex P1): the actual `mailbox_finalize_owed.store(true,
            // Release)` happens EARLIER, at the watcher-unpause site in the
            // `TmuxReady` branch (~line 1980). Doing it there guarantees we
            // win any race with a fast watcher whose terminal `swap(false,
            // AcqRel)` could otherwise execute before this late delegation
            // decision and leave stale debt that would clear the next turn's
            // cancel_token.
            //
            // Here we only verify the invariant: a live watcher handle still
            // exists and its `mailbox_finalize_owed` is true. If either
            // condition fails, the watcher will not finalize and the channel
            // mailbox would leak its cancel_token; we surface the violation
            // via `record_turn_bridge_invariant`.
            let handoff_recorded = shared_owned
                .tmux_watchers
                .get(&watcher_owner_channel_id)
                .map(|watcher| {
                    watcher
                        .mailbox_finalize_owed
                        .load(std::sync::atomic::Ordering::Acquire)
                })
                .unwrap_or(false);
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
            false
        } else {
            // #1452 non-delegation path. The watcher-unpause site
            // optimistically publishes `mailbox_finalize_owed = true`, but
            // we are now finalizing on the bridge side instead (cancelled /
            // prompt_too_long / transport_error / recovery_retry, or the
            // watcher never ended up owning relay).
            //
            // Codex iter 3 P1: we must distinguish three outcomes:
            //   (a) THIS turn published the debt AND watcher has NOT yet
            //       consumed it → revoke (`true → false`) and run our own
            //       `mailbox_finish_turn`. Without revoke a future swap
            //       would mistakenly clear the next turn's cancel_token.
            //   (b) THIS turn published the debt AND watcher ALREADY
            //       consumed it (called `mailbox_finish_turn` itself) →
            //       SKIP our own finalization to avoid clearing a turn we
            //       no longer own (Codex P2 review iter 2).
            //   (c) THIS turn never published the debt at all (no
            //       `TmuxReady` reached, or watcher missing) → the
            //       handle's value is just whatever the previous turn
            //       left there. We MUST run our own `mailbox_finish_turn`
            //       — treating this as outcome (b) would leak the
            //       cancel_token (Codex iter 3 P1).
            //
            // `bridge_published_finalize_owed_for_this_turn` distinguishes
            // (a)/(b) from (c). `compare_exchange(true, false, AcqRel,
            // Acquire)` then distinguishes (a) from (b): Ok = revoked
            // unconsumed debt (a); Err = watcher beat us (b).
            let watcher_already_finalized = if bridge_published_finalize_owed_for_this_turn
                && let Some(watcher) =
                    shared_owned.tmux_watchers.get(&watcher_owner_channel_id)
            {
                matches!(
                    watcher.mailbox_finalize_owed.compare_exchange(
                        true,
                        false,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                    ),
                    Err(_)
                )
            } else {
                false
            };

            if watcher_already_finalized {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] watcher_finalized_before_bridge_revoke: skipping bridge mailbox_finish_turn for channel {} (#1452)",
                    channel_id.get()
                );
                false
            } else {
                let finish =
                    super::mailbox_finish_turn(&shared_owned, &provider, channel_id).await;
                record_turn_bridge_invariant(
                    finish.removed_token.is_some(),
                    &provider,
                    channel_id,
                    dispatch_id.as_deref(),
                    adk_session_key.as_deref(),
                    Some(turn_id.as_str()),
                    "mailbox_active_turn_matches_dispatch",
                    "src/services/discord/turn_bridge/mod.rs:mailbox_finish_turn",
                    "turn_bridge finalization expected exactly one active mailbox turn",
                    serde_json::json!({
                        "has_pending": finish.has_pending,
                        "mailbox_online": finish.mailbox_online,
                    }),
                );
                if let Some(removed_token) = finish.removed_token {
                    // Mark the token as cancelled so any lingering watchdog timer exits cleanly
                    // instead of mistakenly firing on a newer turn's token.
                    removed_token
                        .cancelled
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    shared_owned
                        .global_active
                        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
                // Clean up any pending watchdog deadline override for this channel
                super::clear_watchdog_deadline_override(channel_id.get()).await;
                // Clean up dispatch-thread parent mapping when the thread turn ends.
                // Iterate and remove entries whose thread matches this channel_id.
                shared_owned
                    .dispatch_thread_parents
                    .retain(|_, thread| *thread != channel_id);
                // Keep the override while queued turns remain so review/reused-thread routing
                // survives restart-preserve and same-runtime dequeue paths.
                if !finish.has_pending {
                    shared_owned.dispatch_role_overrides.remove(&channel_id);
                }
                finish.has_pending
            }
        };
        let mut preserve_inflight_for_cleanup_retry = false;
        let mut terminal_delivery_committed = false;
        let mut status_panel_terminal_committed = false;

        // Remove ⏳ only if NOT handing off to tmux watcher.
        // When tmux watcher is handling the response, it will do ⏳→✅ after delivery.
        let bridge_output_owner = classify_bridge_output_owner(
            rx_disconnected,
            tmux_handed_off,
            full_response.is_empty(),
            bridge_relay_delegated_to_watcher,
        );
        if !bridge_output_owner.skips_bridge_spinner_cleanup() {
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
            // Auto-retry with Discord history
            let gateway_c = gateway.clone();
            let retry_text = user_text_owned.clone();
            tokio::spawn(async move {
                gateway_c
                    .schedule_retry_with_history(channel_id, user_msg_id, &retry_text)
                    .await;
            });
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

        let bridge_output_owner_for_delivery = classify_bridge_output_owner(
            rx_disconnected,
            tmux_handed_off,
            full_response.is_empty(),
            bridge_relay_delegated_to_watcher,
        );

        if cancelled {
            close_all_tracked_background_children(
                shared_owned.pg_pool.as_ref(),
                &mut active_background_child_session_ids,
                "aborted",
                "cancel cleanup",
            )
            .await;
            // #1255: cancelled turn → drive any active long-running placeholder
            // into Aborted before the rest of the cleanup machinery runs. The
            // controller's idempotent terminal transition guarantees this is
            // safe even if the ToolResult event already fired Completed.
            if let Some((key, _, _, _)) = long_running_placeholder_active.take() {
                let _ = shared_owned
                    .placeholder_controller
                    .transition(
                        gateway.as_ref(),
                        key,
                        super::placeholder_controller::PlaceholderLifecycle::Aborted,
                    )
                    .await;
                inflight_state.long_running_placeholder_active = false;
                let _ = save_inflight_state(&inflight_state);
            }

            let cleanup_policy = match cancel_token.restart_mode() {
                Some(restart_mode) => TmuxCleanupPolicy::PreserveSessionAndInflight {
                    restart_mode,
                },
                None => TmuxCleanupPolicy::PreserveSession,
            };
            let cancel_source = cancel_token
                .cancel_source()
                .unwrap_or_else(|| "turn_bridge_cancelled".to_string());
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
                        tracing::warn!(
                            "[turn_bridge] failed to cancel dispatch {} during cancelled turn cleanup in postgres: {}",
                            dispatch_id,
                            error
                        );
                    }
                } else {
                    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
                    if let Some(db) = None::<&crate::db::Db> {
                        if let Ok(conn) = db.lock() {
                            if let Err(error) =
                                crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
                                    &conn,
                                    dispatch_id,
                                    Some(cancel_source.as_str()),
                                )
                            {
                                tracing::warn!(
                                    "[turn_bridge] failed to cancel dispatch {} during cancelled turn cleanup: {}",
                                    dispatch_id,
                                    error
                                );
                            }
                        } else {
                            tracing::warn!(
                                "[turn_bridge] failed to lock DB for cancelled turn cleanup on dispatch {}",
                                dispatch_id
                            );
                        }
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
                let formatted = if shared_owned.status_panel_v2_enabled {
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

            let replace_committed = turn_bridge_replace_outcome_committed(
                shared_owned.as_ref(),
                &provider,
                channel_id,
                current_msg_id,
                inflight_state.tmux_session_name.as_deref(),
                gateway
                    .replace_message_with_outcome(channel_id, current_msg_id, &terminal_response)
                    .await,
                "turn_bridge_cancelled_terminal_replace",
            );
            if replace_committed {
                status_panel_terminal_committed = true;
                advance_tmux_relay_confirmed_end(
                    shared_owned.as_ref(),
                    watcher_owner_channel_id,
                    tmux_last_offset,
                    inflight_state.tmux_session_name.as_deref(),
                );
            } else {
                preserve_inflight_for_cleanup_retry = true;
            }

            if preserved_restart_mode.is_none() {
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
            let replace_committed = turn_bridge_replace_outcome_committed(
                shared_owned.as_ref(),
                &provider,
                channel_id,
                current_msg_id,
                inflight_state.tmux_session_name.as_deref(),
                gateway
                    .replace_message_with_outcome(channel_id, current_msg_id, &full_response)
                    .await,
                "turn_bridge_prompt_too_long_replace",
            );
            if replace_committed {
                status_panel_terminal_committed = true;
                advance_tmux_relay_confirmed_end(
                    shared_owned.as_ref(),
                    watcher_owner_channel_id,
                    tmux_last_offset,
                    inflight_state.tmux_session_name.as_deref(),
                );
            } else {
                preserve_inflight_for_cleanup_retry = true;
            }

            gateway.add_reaction(channel_id, user_msg_id, '⚠').await;

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ⚠ Prompt too long (channel {})", channel_id);
        } else if bridge_output_owner_for_delivery == BridgeOutputOwner::LegacyTmuxHandoff {
            // Tmux watcher is handling response delivery — this is normal.
            // Don't delete placeholder — update it so the user sees the turn is still active.
            // The tmux watcher will replace this content when output arrives.
            //
            // #1114: information-rich monitor handoff placeholder. Uses
            // <t:UNIX:R> for client-side relative-time rendering (no server
            // refresh needed) and surfaces the last seen tool/command + the
            // handoff reason so the user knows what's still in flight.
            //
            // #1255: route through PlaceholderController so this edit
            // serializes against any concurrent live-turn Monitor placeholder
            // edit on the same message_id. If the live-turn placeholder
            // already reached a terminal state, the controller rejects this
            // re-activation and we fall through to the legacy direct-edit
            // path so the watcher still surfaces something to the user.
            let started_at_unix = chrono::Utc::now().timestamp()
                - i64::try_from(turn_start.elapsed().as_secs()).unwrap_or(0);
            let key = super::placeholder_controller::PlaceholderKey {
                provider: provider.clone(),
                channel_id,
                message_id: current_msg_id,
            };
            let (handoff_tool_summary, handoff_command_summary) = monitor_handoff_tool_context(
                last_tool_name.as_deref(),
                last_tool_summary.as_deref(),
                current_tool_line.as_deref(),
            );
            let controller_input = super::placeholder_controller::PlaceholderActiveInput {
                reason: super::formatting::MonitorHandoffReason::AsyncDispatch,
                started_at_unix,
                tool_summary: handoff_tool_summary.clone(),
                command_summary: handoff_command_summary.clone(),
                reason_detail: None,
                context_line: last_assistant_text_line.clone(),
                request_line: first_request_line(&user_text_owned),
                progress_line: child_progress_line(
                    shared_owned.pg_pool.as_ref(),
                    adk_session_key.as_deref(),
                )
                .await,
            };
            let controller_outcome = ensure_active_placeholder_card(
                shared_owned.as_ref(),
                gateway.as_ref(),
                key.clone(),
                controller_input,
            )
            .await;
            // Fall back to a direct edit only when the controller refused or
            // failed — `Edited`/`Coalesced` already cover the happy path.
            let handoff_edit: Result<(), String> = match controller_outcome {
                super::placeholder_controller::PlaceholderControllerOutcome::Edited
                | super::placeholder_controller::PlaceholderControllerOutcome::Coalesced => Ok(()),
                _ => {
                    let placeholder_text =
                        super::formatting::build_monitor_handoff_placeholder(
                            super::formatting::MonitorHandoffStatus::Active,
                            super::formatting::MonitorHandoffReason::AsyncDispatch,
                            started_at_unix,
                            handoff_tool_summary.as_deref(),
                            handoff_command_summary.as_deref(),
                        );
                    gateway
                        .edit_message(channel_id, current_msg_id, &placeholder_text)
                        .await
                }
            };
            let handoff_operation =
                super::placeholder_cleanup::PlaceholderCleanupOperation::EditHandoff;
            let handoff_outcome = match handoff_edit {
                Ok(_) => super::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
                Err(error) => super::placeholder_cleanup::PlaceholderCleanupOutcome::failed(error),
            };
            if let super::placeholder_cleanup::PlaceholderCleanupOutcome::Failed {
                class,
                detail,
            } = &handoff_outcome
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ placeholder cleanup {} failed ({}) for channel {} msg {}: {}",
                    handoff_operation.as_str(),
                    class.as_str(),
                    channel_id.get(),
                    current_msg_id.get(),
                    detail
                );
            }
            let handoff_committed = handoff_outcome.is_committed();
            shared_owned.placeholder_cleanup.record(
                super::placeholder_cleanup::PlaceholderCleanupRecord {
                    provider: provider.clone(),
                    channel_id,
                    message_id: current_msg_id,
                    tmux_session_name: inflight_state.tmux_session_name.clone(),
                    operation: handoff_operation,
                    outcome: handoff_outcome,
                    source: "turn_bridge_tmux_handoff",
                },
            );
            // codex round-5 P2 on PR #1308: after handoff, the watcher owns
            // the placeholder lifecycle through `placeholder_cleanup` and
            // direct edits — it never calls `transition`/`detach`. Drop the
            // controller's `Active` entry now so it does not survive as a
            // non-evictable row in the cap-bounded map.
            shared_owned.placeholder_controller.detach(&key);
            let ts = chrono::Local::now().format("%H:%M:%S");
            if handoff_committed {
                tracing::warn!(
                    "  [{ts}] ✓ tmux handoff complete, placeholder updated, watcher handles response (channel {})",
                    channel_id
                );
            } else {
                tracing::warn!(
                    "  [{ts}] ⚠ tmux handoff complete, but placeholder update failed; watcher still handles response (channel {})",
                    channel_id
                );
            }
        } else if bridge_output_owner_for_delivery == BridgeOutputOwner::WatcherRelay {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 tmux watcher owns assistant relay; bridge skipped direct response delivery (channel {})",
                channel_id
            );
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
                // Auto-retry with Discord history context
                let gateway_c = gateway.clone();
                let retry_text = user_text_owned.clone();
                tokio::spawn(async move {
                    gateway_c
                        .schedule_retry_with_history(channel_id, user_msg_id, &retry_text)
                        .await;
                });
                full_response = String::new(); // Suppress error message to user
            } else if full_response.is_empty() {
                // Fallback: try to extract response from tmux output file
                if let Some(ref path) = inflight_state.output_path {
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
                    let gateway_c = gateway.clone();
                    let retry_text = user_text_owned.clone();
                    tokio::spawn(async move {
                        gateway_c
                            .schedule_retry_with_history(channel_id, user_msg_id, &retry_text)
                            .await;
                    });
                    full_response = String::new();
                } else {
                    // Check for resume failure via other methods
                    let mut resume_failed = false;
                    let quick_exit = turn_start.elapsed().as_secs() < 10;
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
                        let gateway_c = gateway.clone();
                        let retry_text = user_text_owned.clone();
                        tokio::spawn(async move {
                            gateway_c
                                .schedule_retry_with_history(channel_id, user_msg_id, &retry_text)
                                .await;
                        });
                        full_response = String::new();
                    }
                    // Method 2: quick exit (<10s) + empty response + had a session_id to resume
                    if !resume_failed && quick_exit && rx_disconnected {
                        let attempted_resume = {
                            let data = shared_owned.core.lock().await;
                            data.sessions
                                .get(&channel_id)
                                .and_then(|s| s.session_id.as_ref())
                                .is_some()
                        };
                        if attempted_resume {
                            resume_failed = true;
                            resume_failure_detected = true;
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ Quick exit with empty response — auto-retrying with fresh session (channel {})",
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
                            let gateway_c = gateway.clone();
                            let retry_text = user_text_owned.clone();
                            tokio::spawn(async move {
                                gateway_c
                                    .schedule_retry_with_history(
                                        channel_id,
                                        user_msg_id,
                                        &retry_text,
                                    )
                                    .await;
                            });
                            full_response = String::new();
                        }
                    }
                    if !resume_failed {
                        if rx_disconnected {
                            full_response =
                                "(No response — 프로세스가 응답 없이 종료됨)".to_string();
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ Empty response: rx disconnected before any text \
                                 (channel {}, output_path={:?}, last_offset={})",
                                channel_id,
                                inflight_state.output_path,
                                inflight_state.last_offset
                            );
                        } else {
                            full_response = "(No response)".to_string();
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

            let mut delivery_response =
                response_portion_after_offset(&full_response, response_sent_offset).to_string();
            if let Some(warning) = review_dispatch_warning.as_deref() {
                let warning = warning.trim();
                if !warning.is_empty() {
                    if !delivery_response.trim().is_empty() {
                        delivery_response.push_str("\n\n");
                    }
                    delivery_response.push_str(warning);
                }
            }

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
                terminal_delivery_committed = true;
                advance_tmux_relay_confirmed_end(
                    shared_owned.as_ref(),
                    watcher_owner_channel_id,
                    tmux_last_offset,
                    inflight_state.tmux_session_name.as_deref(),
                );
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
                }
            } else {
                delivery_response = if shared_owned.status_panel_v2_enabled {
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
                    let replace_committed = turn_bridge_replace_outcome_committed(
                        shared_owned.as_ref(),
                        &provider,
                        channel_id,
                        current_msg_id,
                        inflight_state.tmux_session_name.as_deref(),
                        gateway
                            .replace_message_with_outcome(
                                channel_id,
                                current_msg_id,
                                &delivery_response,
                            )
                            .await,
                        "turn_bridge_terminal_replace",
                    );
                    if replace_committed {
                        advance_tmux_relay_confirmed_end(
                            shared_owned.as_ref(),
                            watcher_owner_channel_id,
                            tmux_last_offset,
                            inflight_state.tmux_session_name.as_deref(),
                        );
                        terminal_delivery_committed = true;
                    } else {
                        preserve_inflight_for_cleanup_retry = true;
                    }
                } else {
                    match enqueue_headless_delivery(
                        &shared_owned,
                        channel_id,
                        user_msg_id,
                        adk_session_key.as_deref(),
                        inflight_state.delivery_bot.as_deref(),
                        &delivery_response,
                    )
                    .await
                    {
                        Ok(()) => {
                            terminal_delivery_committed = true;
                        }
                        Err(error) => {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ headless delivery enqueue failed for channel {}: {}",
                                channel_id,
                                error
                            );
                        }
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
            if !preserve_inflight_for_cleanup_retry
                && !bridge_relay_delegated_to_watcher
                && let Some(watcher) = shared_owned.tmux_watchers.get(&watcher_owner_channel_id)
            {
                watcher.turn_delivered.store(true, Ordering::Relaxed);
            }

            if can_chain_locally
                && !preserve_inflight_for_cleanup_retry
                && !delivery_response.trim().is_empty()
            {
                gateway.add_reaction(channel_id, user_msg_id, '✅').await;
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ▶ Response sent");
            if let Ok(mut last) = shared_owned.last_turn_at.lock() {
                *last = Some(chrono::Local::now().to_rfc3339());
            }
            status_panel_terminal_committed =
                terminal_delivery_committed && !preserve_inflight_for_cleanup_retry;
        }

        if status_panel_terminal_committed {
            complete_status_panel_v2(
                shared_owned.as_ref(),
                gateway.as_ref(),
                channel_id,
                status_panel_msg_id,
                &provider,
                status_panel_started_at,
                &mut last_status_panel_text,
                false,
                "turn_terminal_delivery",
            )
            .await;
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

        let should_record_final_turn = !(is_prompt_too_long
            || resume_failure_detected
            || recovery_retry
            || (rx_disconnected && tmux_handed_off && full_response.is_empty())
            || bridge_relay_delegated_to_watcher)
            && !full_response.trim().is_empty();

        // Update in-memory session under lock.
        let mut should_persist_transcript = false;
        let mut should_analyze_recall_feedback = false;
        let mut should_spawn_memory_capture = false;
        let mut reflect_request = None;
        let mut session_end_reason = None;
        let mut clear_provider_session = false;
        let mut retry_context_to_store = None;
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
                    session_end_reason = memory_plan.session_end_reason;
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
                        if memory_plan.session_end_reason == Some(SessionEndReason::TurnCapReached)
                        {
                            retry_context_to_store =
                                build_session_retry_context_from_history(&session.history);
                        }
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

        if let Some(retry_context) = retry_context_to_store.as_deref()
            && let Err(err) = store_session_retry_context(
                None::<&crate::db::Db>,
                shared_owned.pg_pool.as_ref(),
                channel_id.get(),
                retry_context,
            )
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ failed to store retry context for channel {}: {}",
                channel_id.get(),
                err
            );
        }

        // Persist or clear provider session_id in DB so fresh-session transitions
        // survive dcserver restarts and idle cleanup.
        if clear_provider_session {
            if let Some(session_key) = adk_session_key.as_deref() {
                super::adk_session::clear_provider_session_id(session_key, shared_owned.api_port)
                    .await;
                if session_end_reason == Some(SessionEndReason::TurnCapReached) {
                    crate::services::termination_audit::record_termination_with_handles(
                        None::<&crate::db::Db>,
                        shared_owned.pg_pool.as_ref(),
                        session_key,
                        dispatch_id.as_deref(),
                        "turn_bridge",
                        "turn_cap_reached",
                        Some("provider session cleared after assistant turn cap"),
                        None,
                        None,
                        None,
                    );
                }
            }
        } else if let (Some(session_key), Some(persisted_sid)) =
            (adk_session_key.as_deref(), session_id_to_persist.as_deref())
        {
            super::adk_session::save_provider_session_id(
                session_key,
                persisted_sid,
                new_raw_provider_session_id.as_deref(),
                &provider,
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
        } else if bridge_relay_delegated_to_watcher {
            "watcher_relay"
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
            "completed" | "tmux_handoff" | "watcher_relay"
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

        if None::<&crate::db::Db>.is_some() || shared_owned.pg_pool.is_some() {
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

        let mut background_memory_task = None;
        if let Some(reflect_request) = reflect_request {
            background_memory_task = Some(spawn_memory_reflect_task(
                channel_id,
                capture_memory_settings.clone(),
                reflect_request,
            ));
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
            background_memory_task = Some(spawn_memory_capture_task(
                channel_id,
                capture_memory_settings,
                capture_request,
            ));
        }

        if let Some(memory_task) = background_memory_task {
            match tokio::time::timeout(std::time::Duration::from_secs(30), memory_task).await {
                Ok(Ok(result)) => {
                    accumulated_memory_input_tokens = accumulated_memory_input_tokens
                        .saturating_add(result.token_usage.input_tokens);
                    accumulated_memory_output_tokens = accumulated_memory_output_tokens
                        .saturating_add(result.token_usage.output_tokens);
                }
                Ok(Err(err)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] [memory] background task join failed for channel {}: {}",
                        channel_id.get(),
                        err
                    );
                }
                Err(_) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] [memory] background task timed out after 30s for channel {} — skipping token accounting",
                        channel_id.get(),
                    );
                }
            }
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
        } else if preserve_inflight_for_cleanup_retry || bridge_relay_delegated_to_watcher {
            let _ = save_inflight_state(&inflight_state);
            inflight_guard.provider.take();
            if bridge_relay_delegated_to_watcher {
                crate::services::observability::emit_inflight_lifecycle_event(
                    provider.as_str(),
                    channel_id.get(),
                    dispatch_id.as_deref(),
                    adk_session_key.as_deref(),
                    Some(turn_id.as_str()),
                    "delegated_to_watcher",
                    serde_json::json!({
                        "preserve_inflight_for_cleanup_retry": preserve_inflight_for_cleanup_retry,
                        "full_response_len": inflight_state.full_response.len(),
                        "response_sent_offset": inflight_state.response_sent_offset,
                        "watcher_owns_live_relay": inflight_state.watcher_owns_live_relay,
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
            clear_inflight_state(&provider, channel_id.get());
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

        // Finalization complete — decrement counters
        shared_owned
            .finalizing_turns
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        shared_owned
            .global_finalizing
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        // Note: deferred restart exit is handled by the 5-second poll loop in mod.rs,
        // which saves pending queues before calling check_deferred_restart.
        // Calling it here would risk exiting before other providers save their queues.

        if has_queued_turns {
            // Drain mode: if restart is pending, don't start new turns from queue.
            // The queued messages will be saved to disk and processed after restart.
            if preserve_inflight_for_cleanup_retry {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ QUEUE-GUARD: preserving queued command(s) for channel {} until placeholder cleanup retry commits",
                    channel_id
                );
            } else if shared_owned
                .restart_pending
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏸ DRAIN: skipping queued turn dequeue for channel {} (restart pending)",
                    channel_id
                );
            } else if let Some(bot_owner_provider) = gateway.bot_owner_provider() {
                if let Err(reason) = gateway.validate_live_routing(channel_id).await {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⚠ QUEUE-GUARD: preserving queued command(s) for channel {} (reason={})",
                        channel_id,
                        reason
                    );
                } else {
                    let next_intervention = super::mailbox_take_next_soft_intervention(
                        &shared_owned,
                        &bot_owner_provider,
                        channel_id,
                    )
                    .await;

                    if let Some((intervention, has_more_queued_turns)) = next_intervention {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!("  [{ts}] 📋 Processing next queued command");
                        if let Err(e) = gateway
                            .dispatch_queued_turn(
                                channel_id,
                                &intervention,
                                &request_owner_name,
                                has_more_queued_turns,
                            )
                            .await
                        {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!("  [{ts}]   ⚠ queued command failed: {e}");
                            super::mailbox_requeue_intervention_front(
                                &shared_owned,
                                &bot_owner_provider,
                                channel_id,
                                intervention,
                            )
                            .await;
                        }
                    }
                }
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 📦 preserving queued command(s): missing live Discord context — scheduling deferred drain"
                );
                if let Some(offset) = tmux_last_offset
                    && let Some(watcher) = shared_owned.tmux_watchers.get(&watcher_owner_channel_id)
                {
                    if let Ok(mut guard) = watcher.resume_offset.lock() {
                        *guard = Some(offset);
                    }
                    watcher.paused.store(false, Ordering::Relaxed);
                }
                super::schedule_deferred_idle_queue_kickoff(
                    shared_owned.clone(),
                    provider.clone(),
                    channel_id,
                    "turn bridge queued backlog",
                );
            }
        }

        // completion_tx is sent automatically by CompletionGuard on drop
    }.instrument(bridge_span));
}

/// codex P2 followup (#1670): unit-level coverage of the
/// "preserve task_notification_kind while other children remain" rule.
/// This mirrors the production logic at the
/// `task_notification_closes_background_child` arm in
/// `StreamMessage::TaskNotification` (search: `codex P2 followup`).
///
/// The existing exhaustive-status coverage lives in
/// `tests::task_notification_kind_resets_after_terminal_status` but that
/// module is feature-gated on `legacy-sqlite-tests`; this lighter copy is
/// added in the non-gated mod so the regression is observable in normal
/// `cargo test` runs.
#[cfg(test)]
mod task_notification_kind_lifecycle_tests {
    use super::{
        TaskNotificationKind, merge_task_notification_kind, release_task_notification_kind,
        task_notification_closes_background_child,
    };

    #[test]
    fn kind_persists_while_other_children_remain() {
        let mut child_ids: Vec<i64> = vec![100, 101];
        let mut kind: Option<TaskNotificationKind> = None;
        kind = merge_task_notification_kind(kind, TaskNotificationKind::Subagent);

        let new_kind = TaskNotificationKind::Subagent;
        let status = "completed";
        kind = merge_task_notification_kind(kind, new_kind);
        if task_notification_closes_background_child(new_kind, status) {
            // Mirror the single-pop behavior of
            // `close_next_tracked_background_child` at the call site.
            let _ = child_ids.remove(0);
            if child_ids.is_empty() {
                kind = release_task_notification_kind(kind, new_kind);
            }
        }

        assert_eq!(child_ids, vec![101]);
        assert_eq!(
            kind,
            Some(TaskNotificationKind::Subagent),
            "#1670 P2: kind must persist while other tracked children remain"
        );

        // Second terminal closes the queue, kind releases.
        let new_kind = TaskNotificationKind::Subagent;
        let status = "completed";
        kind = merge_task_notification_kind(kind, new_kind);
        if task_notification_closes_background_child(new_kind, status) {
            let _ = child_ids.remove(0);
            if child_ids.is_empty() {
                kind = release_task_notification_kind(kind, new_kind);
            }
        }
        assert!(child_ids.is_empty());
        assert_eq!(
            kind, None,
            "#1670 P2: kind must release once the last child closes"
        );
    }

    #[test]
    fn kind_releases_immediately_when_queue_was_already_singleton() {
        let mut child_ids: Vec<i64> = vec![42];
        let mut kind: Option<TaskNotificationKind> = Some(TaskNotificationKind::Background);

        let new_kind = TaskNotificationKind::Background;
        let status = "aborted";
        kind = merge_task_notification_kind(kind, new_kind);
        if task_notification_closes_background_child(new_kind, status) {
            let _ = child_ids.remove(0);
            if child_ids.is_empty() {
                kind = release_task_notification_kind(kind, new_kind);
            }
        }

        assert!(child_ids.is_empty());
        assert_eq!(
            kind, None,
            "#1670 P2: with a single tracked child, terminal status must release the kind"
        );
    }

    #[test]
    fn non_terminal_status_keeps_kind_and_does_not_pop() {
        let mut child_ids: Vec<i64> = vec![1, 2];
        let mut kind: Option<TaskNotificationKind> = None;
        kind = merge_task_notification_kind(kind, TaskNotificationKind::Subagent);

        let new_kind = TaskNotificationKind::Subagent;
        let status = "started";
        kind = merge_task_notification_kind(kind, new_kind);
        if task_notification_closes_background_child(new_kind, status) {
            let _ = child_ids.remove(0);
            if child_ids.is_empty() {
                kind = release_task_notification_kind(kind, new_kind);
            }
        }

        assert_eq!(child_ids, vec![1, 2], "non-terminal must not pop");
        assert_eq!(
            kind,
            Some(TaskNotificationKind::Subagent),
            "non-terminal must keep kind"
        );
    }

    #[test]
    fn lower_priority_child_close_preserves_higher_priority_kind() {
        let mut child_ids: Vec<i64> = vec![7];
        let mut kind: Option<TaskNotificationKind> = Some(TaskNotificationKind::MonitorAutoTurn);

        let new_kind = TaskNotificationKind::Subagent;
        kind = merge_task_notification_kind(kind, new_kind);
        if task_notification_closes_background_child(new_kind, "completed") {
            let _ = child_ids.remove(0);
            if child_ids.is_empty() {
                kind = release_task_notification_kind(kind, new_kind);
            }
        }

        assert!(child_ids.is_empty());
        assert_eq!(
            kind,
            Some(TaskNotificationKind::MonitorAutoTurn),
            "#1683: lower-priority child terminal notification must not clear a higher-priority active kind"
        );
    }
}

#[cfg(test)]
mod status_panel_v2_rework_tests {
    use super::{
        InflightTurnState, MessageId, ProviderKind,
        should_open_long_running_placeholder_controller, status_panel_message_id_for_turn,
    };

    #[test]
    fn status_panel_v2_disables_long_running_placeholder_controller() {
        assert!(!should_open_long_running_placeholder_controller(true));
        assert!(should_open_long_running_placeholder_controller(false));
    }

    fn test_inflight_state() -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            1,
            Some("adk-cdx-test".to_string()),
            2,
            3,
            4,
            "test turn".to_string(),
            None,
            None,
            None,
            None,
            0,
        )
    }

    #[test]
    fn fresh_turn_discards_stale_status_panel_message_id() {
        let mut state = test_inflight_state();
        state.status_message_id = Some(99);

        let status_panel_msg_id = status_panel_message_id_for_turn(&mut state, false);

        assert_eq!(status_panel_msg_id, None);
        assert_eq!(state.status_message_id, None);
    }

    #[test]
    fn resume_turn_preserves_status_panel_message_id() {
        let mut state = test_inflight_state();
        state.status_message_id = Some(99);

        let status_panel_msg_id = status_panel_message_id_for_turn(&mut state, true);

        assert_eq!(status_panel_msg_id, Some(MessageId::new(99)));
        assert_eq!(state.status_message_id, Some(99));
    }
}
