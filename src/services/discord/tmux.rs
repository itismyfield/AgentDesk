use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};

use libsql_rusqlite::OptionalExtension;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::message_outbox::{
    OutboxMessage, enqueue_lifecycle_notification_best_effort, enqueue_outbox_best_effort,
};
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};
use crate::services::session_backend::{
    StreamLineState, classify_task_notification_kind, observe_stream_context,
};
use crate::services::tmux_diagnostics::{
    build_tmux_death_diagnostic, read_tmux_exit_reason, record_tmux_exit_reason,
    tmux_exit_reason_is_normal_completion, tmux_session_exists, tmux_session_has_live_pane,
};

use super::formatting::{
    build_streaming_placeholder_text, format_tool_input, plan_streaming_rollover,
    replace_long_message_raw, send_long_message_raw, truncate_str,
};
use super::settings::{
    channel_supports_provider, load_last_remote_profile, load_last_session_path,
    resolve_role_binding, validate_bot_channel_routing_with_provider_channel,
};
use super::tmux_error_detect::{
    detect_provider_overload_message, is_auth_error_message, is_prompt_too_long_message,
};
use super::tmux_overload_retry::{
    PROVIDER_OVERLOAD_MAX_RETRIES, ProviderOverloadDecision, clear_provider_overload_retry_state,
    record_provider_overload_retry, schedule_provider_overload_retry,
};
use super::tmux_restart_handoff::{
    resolve_dispatched_thread_dispatch_from_db, resume_aborted_restart_turn,
};
use super::{SharedData, TmuxWatcherHandle, rate_limit_wait};
const READY_FOR_INPUT_IDLE_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
pub(super) const WATCHER_ACTIVITY_HEARTBEAT_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);
const READY_FOR_INPUT_STUCK_LABEL: &str = "stuck_at_ready";
const READY_FOR_INPUT_STUCK_REASON: &str = "agent ended at Ready for input without commit/push";
const SUPPRESSED_INTERNAL_LABEL: &str = "(자동으로 처리된 내부 작업이라 여기서 멈췄어요)";
const SUPPRESSED_RESTART_LABEL: &str =
    "(서버가 재시작되면서 답변이 중간에 멈췄어요 — 필요하시면 다시 질문해 주세요)";
const MISSING_INFLIGHT_REATTACH_GRACE_ATTEMPTS: usize = 3;
const MISSING_INFLIGHT_REATTACH_GRACE_DELAY: tokio::time::Duration =
    tokio::time::Duration::from_millis(200);
const RECENT_WATCHER_REATTACH_OFFSET_CAPACITY: usize = 32;
const RECENT_WATCHER_REATTACH_OFFSET_TTL: std::time::Duration =
    std::time::Duration::from_secs(15 * 60);
const MONITOR_AUTO_TURN_REASON_CODE: &str = "lifecycle.monitor_auto_turn";
const MONITOR_AUTO_TURN_DEFERRED_REASON_CODE: &str = "lifecycle.monitor_auto_turn.deferred";

#[derive(Debug, Clone)]
struct RecentWatcherReattachOffset {
    channel_id: ChannelId,
    tmux_session_name: String,
    offset: u64,
    recorded_at: std::time::Instant,
}

static RECENT_WATCHER_REATTACH_OFFSETS: LazyLock<Mutex<VecDeque<RecentWatcherReattachOffset>>> =
    LazyLock::new(|| {
        Mutex::new(VecDeque::with_capacity(
            RECENT_WATCHER_REATTACH_OFFSET_CAPACITY,
        ))
    });

fn recent_watcher_reattach_offsets()
-> std::sync::MutexGuard<'static, VecDeque<RecentWatcherReattachOffset>> {
    match RECENT_WATCHER_REATTACH_OFFSETS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn prune_recent_watcher_reattach_offsets(
    offsets: &mut VecDeque<RecentWatcherReattachOffset>,
    now: std::time::Instant,
) {
    offsets.retain(|entry| {
        now.saturating_duration_since(entry.recorded_at) <= RECENT_WATCHER_REATTACH_OFFSET_TTL
    });
}

fn record_recent_watcher_reattach_offset(
    channel_id: ChannelId,
    tmux_session_name: &str,
    offset: u64,
) {
    let now = std::time::Instant::now();
    let mut offsets = recent_watcher_reattach_offsets();
    prune_recent_watcher_reattach_offsets(&mut offsets, now);
    while offsets.len() >= RECENT_WATCHER_REATTACH_OFFSET_CAPACITY {
        offsets.pop_front();
    }
    offsets.push_back(RecentWatcherReattachOffset {
        channel_id,
        tmux_session_name: tmux_session_name.to_string(),
        offset,
        recorded_at: now,
    });
}

fn matching_recent_watcher_reattach_offset(
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
) -> Option<RecentWatcherReattachOffset> {
    let now = std::time::Instant::now();
    let mut offsets = recent_watcher_reattach_offsets();
    prune_recent_watcher_reattach_offsets(&mut offsets, now);
    offsets
        .iter()
        .rev()
        .find(|entry| {
            entry.channel_id == channel_id
                && entry.tmux_session_name == tmux_session_name
                && entry.offset == data_start_offset
        })
        .cloned()
}

#[cfg(test)]
fn clear_recent_watcher_reattach_offsets_for_tests() {
    recent_watcher_reattach_offsets().clear();
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct WatcherLineOutcome {
    pub found_result: bool,
    pub is_prompt_too_long: bool,
    pub is_auth_error: bool,
    pub auth_error_message: Option<String>,
    pub is_provider_overloaded: bool,
    pub provider_overload_message: Option<String>,
    pub stale_resume_detected: bool,
    pub auto_compacted: bool,
    pub task_notification_kind: Option<TaskNotificationKind>,
}

#[derive(Debug, Clone)]
pub(super) struct RestoredWatcherTurn {
    current_msg_id: MessageId,
    response_sent_offset: usize,
    full_response: String,
    last_edit_text: String,
    task_notification_kind: Option<TaskNotificationKind>,
    finish_mailbox_on_completion: bool,
}

#[derive(Debug)]
struct WatcherStreamSeed {
    placeholder_msg_id: Option<MessageId>,
    response_sent_offset: usize,
    full_response: String,
    last_edit_text: String,
    task_notification_kind: Option<TaskNotificationKind>,
    finish_mailbox_on_completion: bool,
}

fn normalize_response_sent_offset(full_response: &str, response_sent_offset: usize) -> usize {
    let mut offset = response_sent_offset.min(full_response.len());
    while offset > 0 && !full_response.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn record_watcher_invariant(
    condition: bool,
    provider: Option<&ProviderKind>,
    channel_id: ChannelId,
    invariant: &'static str,
    code_location: &'static str,
    message: &'static str,
    details: serde_json::Value,
) -> bool {
    crate::services::observability::record_invariant_check(
        condition,
        crate::services::observability::InvariantViolation {
            provider: provider.map(ProviderKind::as_str),
            channel_id: Some(channel_id.get()),
            dispatch_id: None,
            session_key: None,
            turn_id: None,
            invariant,
            code_location,
            message,
            details,
        },
    )
}

pub(super) fn restored_watcher_turn_from_inflight(
    state: &super::inflight::InflightTurnState,
    tmux_session_name: &str,
    finish_mailbox_on_completion: bool,
) -> Option<RestoredWatcherTurn> {
    if state.rebind_origin
        || state.current_msg_id == 0
        || state
            .tmux_session_name
            .as_deref()
            .is_some_and(|name| name != tmux_session_name)
    {
        return None;
    }

    let response_sent_offset =
        normalize_response_sent_offset(&state.full_response, state.response_sent_offset);
    Some(RestoredWatcherTurn {
        current_msg_id: MessageId::new(state.current_msg_id),
        response_sent_offset,
        full_response: state.full_response.clone(),
        last_edit_text: reconstructed_inflight_placeholder_body(state),
        task_notification_kind: state.task_notification_kind,
        finish_mailbox_on_completion,
    })
}

fn watcher_stream_seed(restored_turn: Option<RestoredWatcherTurn>) -> WatcherStreamSeed {
    match restored_turn {
        Some(restored) => WatcherStreamSeed {
            placeholder_msg_id: Some(restored.current_msg_id),
            response_sent_offset: restored.response_sent_offset,
            full_response: restored.full_response,
            last_edit_text: restored.last_edit_text,
            task_notification_kind: restored.task_notification_kind,
            finish_mailbox_on_completion: restored.finish_mailbox_on_completion,
        },
        None => WatcherStreamSeed {
            placeholder_msg_id: None,
            response_sent_offset: 0,
            full_response: String::new(),
            last_edit_text: String::new(),
            task_notification_kind: None,
            finish_mailbox_on_completion: false,
        },
    }
}

fn lifecycle_reason_code_for_tmux_exit(reason: &str) -> &'static str {
    let lower = reason.to_ascii_lowercase();
    if tmux_exit_reason_is_normal_completion(reason) {
        "lifecycle.normal_completion"
    } else if lower.contains("force-kill")
        || lower.contains("deadlock")
        || lower.contains("prompt too long")
        || lower.contains("auth")
    {
        "lifecycle.force_kill"
    } else if lower.contains("idle") || lower.contains("turn cap") || lower.contains("cleanup") {
        "lifecycle.auto_cleanup"
    } else {
        "lifecycle.tmux_terminated"
    }
}

fn tmux_death_is_normal_completion(reason: Option<&str>, _diagnostic: Option<&str>) -> bool {
    reason.is_some_and(tmux_exit_reason_is_normal_completion)
}

fn stream_line_state_token_usage(state: &StreamLineState) -> Option<TurnTokenUsage> {
    let usage = TurnTokenUsage {
        input_tokens: state.accum_input_tokens,
        cache_create_tokens: state.accum_cache_create_tokens,
        cache_read_tokens: state.accum_cache_read_tokens,
        output_tokens: state.accum_output_tokens,
    };
    (usage.input_tokens > 0
        || usage.cache_create_tokens > 0
        || usage.cache_read_tokens > 0
        || usage.output_tokens > 0)
        .then_some(usage)
}

fn watcher_ready_for_input_turn_completed(
    tracker: &mut crate::services::provider::ReadyForInputIdleTracker,
    data_start_offset: u64,
    current_offset: u64,
    ready_for_input: bool,
    post_work_observed: bool,
    now: std::time::Instant,
) -> crate::services::provider::ReadyForInputIdleState {
    tracker.observe_idle_state(
        current_offset > data_start_offset,
        ready_for_input,
        post_work_observed,
        now,
    )
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalRelayDecision {
    should_direct_send: bool,
    should_tag_monitor_origin: bool,
    should_enqueue_notify_outbox: bool,
    suppressed: bool,
}

fn terminal_relay_decision(
    has_assistant_response: bool,
    task_notification_kind: Option<TaskNotificationKind>,
) -> TerminalRelayDecision {
    match task_notification_kind {
        Some(TaskNotificationKind::MonitorAutoTurn) => TerminalRelayDecision {
            should_direct_send: has_assistant_response,
            should_tag_monitor_origin: has_assistant_response,
            should_enqueue_notify_outbox: false,
            suppressed: !has_assistant_response,
        },
        Some(TaskNotificationKind::Background) => TerminalRelayDecision {
            // Background task_notification marks that a background event (Monitor
            // completion, task_complete, etc.) fired during the turn. The response
            // after that event is user-facing content and must reach Discord.
            // Historical behavior suppressed the whole terminal relay, which
            // caused #1044 A→C: user messages streamed after the tag were lost.
            should_direct_send: has_assistant_response,
            should_tag_monitor_origin: false,
            should_enqueue_notify_outbox: false,
            suppressed: !has_assistant_response,
        },
        Some(TaskNotificationKind::Subagent) => TerminalRelayDecision {
            // Subagent turn = internal sub-agent reporting to parent. Not routed
            // to the user-facing channel.
            should_direct_send: false,
            should_tag_monitor_origin: false,
            should_enqueue_notify_outbox: false,
            suppressed: true,
        },
        None => TerminalRelayDecision {
            should_direct_send: has_assistant_response,
            should_tag_monitor_origin: false,
            should_enqueue_notify_outbox: false,
            suppressed: false,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SuppressedPlaceholderAction {
    None,
    Delete,
    Edit(String),
}

fn is_spinner_prefix_char(ch: char) -> bool {
    matches!(
        ch,
        '⠏' | '⠋' | '⠙' | '⠹' | '⠸' | '⠼' | '⠴' | '⠦' | '⠧' | '⠇'
    )
}

fn is_inprogress_indicator_line(line: &str) -> bool {
    line.trim_start()
        .chars()
        .next()
        .is_some_and(is_spinner_prefix_char)
}

fn strip_inprogress_indicators(body: &str) -> String {
    let mut lines: Vec<&str> = body
        .lines()
        .filter(|line| !is_inprogress_indicator_line(line))
        .collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn rewrite_placeholder_as_terminal_suppressed(text: &str, label: &'static str) -> String {
    let cleaned = strip_inprogress_indicators(text);
    let trimmed = cleaned.trim_end();
    if trimmed.ends_with(label) {
        return trimmed.to_string();
    }
    if trimmed.is_empty() {
        return label.to_string();
    }

    let suffix = format!("\n\n{label}");
    let max_base_len = super::DISCORD_MSG_LIMIT.saturating_sub(suffix.len());
    let base = if trimmed.len() > max_base_len {
        truncate_str(trimmed, max_base_len)
    } else {
        trimmed.to_string()
    };
    format!("{base}{suffix}")
}

fn reconstructed_inflight_placeholder_body(state: &super::inflight::InflightTurnState) -> String {
    let current_portion = state
        .full_response
        .get(state.response_sent_offset..)
        .unwrap_or("");
    let status_block = super::formatting::build_placeholder_status_block(
        "⠼",
        state.prev_tool_status.as_deref(),
        state.current_tool_line.as_deref(),
        &state.full_response,
    );
    build_streaming_placeholder_text(current_portion, &status_block)
}

fn orphan_suppressed_placeholder_action(
    state: &super::inflight::InflightTurnState,
    has_active_turn: bool,
    tmux_session_name: &str,
) -> SuppressedPlaceholderAction {
    if has_active_turn
        || state.rebind_origin
        || state.response_sent_offset == 0
        || state.current_msg_id == 0
        || state.tmux_session_name.as_deref() != Some(tmux_session_name)
    {
        return SuppressedPlaceholderAction::None;
    }

    let body = reconstructed_inflight_placeholder_body(state);
    SuppressedPlaceholderAction::Edit(rewrite_placeholder_as_terminal_suppressed(
        &body,
        SUPPRESSED_RESTART_LABEL,
    ))
}

/// Unified entry point for every placeholder-suppression decision.
///
/// Three production sites produced identical edit/delete/log scaffolding before
/// #1055 (bridge-guard duplicate relay at `tmux_output_watcher_with_restore`,
/// task-notification terminal suppress at the same function, and
/// `reconcile_orphan_suppressed_placeholder_for_restored_watcher`). The
/// `decide_placeholder_suppression` + `apply_placeholder_suppression` pair
/// replaces those copies so a future placeholder-suppression regression can be
/// fixed in exactly one location. See also `Shared Agent Rules` — DRY 강제.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaceholderSuppressOrigin {
    OrphanRestartHandoff,
    ActiveBridgeTurnGuard,
    TaskNotificationTerminal,
}

impl PlaceholderSuppressOrigin {
    fn log_scope(self) -> &'static str {
        match self {
            Self::OrphanRestartHandoff => "orphan suppressed placeholder reconcile",
            Self::ActiveBridgeTurnGuard => "active bridge suppressed placeholder",
            Self::TaskNotificationTerminal => "suppressed placeholder",
        }
    }
}

struct PlaceholderSuppressContext<'a> {
    origin: PlaceholderSuppressOrigin,
    placeholder_msg_id: Option<serenity::MessageId>,
    response_sent_offset: usize,
    last_edit_text: &'a str,
    inflight_state: Option<&'a super::inflight::InflightTurnState>,
    has_active_turn: bool,
    tmux_session_name: &'a str,
    task_notification_kind: Option<TaskNotificationKind>,
    reattach_offset_match: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlaceholderSuppressDecision {
    None,
    /// Keep the user-facing body already streamed to the placeholder; strip
    /// the in-progress spinner/status-block suffix so the message is not
    /// frozen mid-stream. `reason` feeds the observability log only.
    /// `cleaned_body` is the stripped body that should be written back.
    Preserve {
        reason: &'static str,
        cleaned_body: String,
    },
    Edit(String),
    Delete,
}

fn strip_placeholder_indicators_for_preserve(text: &str) -> String {
    strip_inprogress_indicators(text).trim_end().to_string()
}

fn decide_placeholder_suppression(
    ctx: &PlaceholderSuppressContext<'_>,
) -> PlaceholderSuppressDecision {
    match ctx.origin {
        PlaceholderSuppressOrigin::OrphanRestartHandoff => {
            let Some(state) = ctx.inflight_state else {
                return PlaceholderSuppressDecision::None;
            };
            match orphan_suppressed_placeholder_action(
                state,
                ctx.has_active_turn,
                ctx.tmux_session_name,
            ) {
                SuppressedPlaceholderAction::None => PlaceholderSuppressDecision::None,
                SuppressedPlaceholderAction::Delete => PlaceholderSuppressDecision::Delete,
                SuppressedPlaceholderAction::Edit(content) => {
                    PlaceholderSuppressDecision::Edit(content)
                }
            }
        }
        PlaceholderSuppressOrigin::ActiveBridgeTurnGuard => {
            if ctx.reattach_offset_match {
                return PlaceholderSuppressDecision::Preserve {
                    reason: "reattach-offset-match",
                    cleaned_body: strip_placeholder_indicators_for_preserve(ctx.last_edit_text),
                };
            }
            match suppressed_placeholder_action(
                ctx.placeholder_msg_id.is_some(),
                ctx.response_sent_offset,
                ctx.last_edit_text,
            ) {
                SuppressedPlaceholderAction::None => PlaceholderSuppressDecision::None,
                SuppressedPlaceholderAction::Delete => PlaceholderSuppressDecision::Delete,
                SuppressedPlaceholderAction::Edit(content) => {
                    PlaceholderSuppressDecision::Edit(content)
                }
            }
        }
        PlaceholderSuppressOrigin::TaskNotificationTerminal => {
            let preserves_body = matches!(
                ctx.task_notification_kind,
                Some(TaskNotificationKind::Background | TaskNotificationKind::Subagent)
            );
            match suppressed_placeholder_action(
                ctx.placeholder_msg_id.is_some(),
                ctx.response_sent_offset,
                ctx.last_edit_text,
            ) {
                SuppressedPlaceholderAction::None => PlaceholderSuppressDecision::None,
                SuppressedPlaceholderAction::Delete => PlaceholderSuppressDecision::Delete,
                SuppressedPlaceholderAction::Edit(_) if preserves_body => {
                    PlaceholderSuppressDecision::Preserve {
                        reason: "background-or-subagent-kind",
                        cleaned_body: strip_placeholder_indicators_for_preserve(ctx.last_edit_text),
                    }
                }
                SuppressedPlaceholderAction::Edit(content) => {
                    PlaceholderSuppressDecision::Edit(content)
                }
            }
        }
    }
}

async fn apply_placeholder_suppression(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    placeholder_msg_id: Option<serenity::MessageId>,
    origin: PlaceholderSuppressOrigin,
    decision: PlaceholderSuppressDecision,
    detail: Option<&str>,
) {
    match decision {
        PlaceholderSuppressDecision::None => {}
        PlaceholderSuppressDecision::Preserve {
            reason,
            cleaned_body,
        } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let detail_suffix = detail.map(|d| format!(" — {d}")).unwrap_or_default();
            tracing::info!(
                "  [{ts}] 👁 {} preserved placeholder ({reason}){detail_suffix}",
                origin.log_scope()
            );
            if let Some(msg_id) = placeholder_msg_id {
                if cleaned_body.is_empty() {
                    let _ = channel_id.delete_message(http, msg_id).await;
                } else {
                    rate_limit_wait(shared, channel_id).await;
                    if let Err(error) = channel_id
                        .edit_message(
                            http,
                            msg_id,
                            serenity::EditMessage::new().content(&cleaned_body),
                        )
                        .await
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ {} preserve finalize edit failed for channel {} msg {}: {}",
                            origin.log_scope(),
                            channel_id.get(),
                            msg_id.get(),
                            error
                        );
                    }
                }
            }
        }
        PlaceholderSuppressDecision::Delete => {
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id.delete_message(http, msg_id).await;
            }
        }
        PlaceholderSuppressDecision::Edit(content) => {
            if let Some(msg_id) = placeholder_msg_id {
                rate_limit_wait(shared, channel_id).await;
                if let Err(error) = channel_id
                    .edit_message(http, msg_id, serenity::EditMessage::new().content(&content))
                    .await
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ {} final edit failed for channel {} msg {}: {}",
                        origin.log_scope(),
                        channel_id.get(),
                        msg_id.get(),
                        error
                    );
                }
            }
        }
    }
}

fn suppressed_placeholder_action(
    has_placeholder: bool,
    response_sent_offset: usize,
    last_edit_text: &str,
) -> SuppressedPlaceholderAction {
    if !has_placeholder {
        return SuppressedPlaceholderAction::None;
    }

    let placeholder_was_exposed = response_sent_offset > 0 || !last_edit_text.trim().is_empty();
    if placeholder_was_exposed {
        SuppressedPlaceholderAction::Edit(rewrite_placeholder_as_terminal_suppressed(
            last_edit_text,
            SUPPRESSED_INTERNAL_LABEL,
        ))
    } else {
        SuppressedPlaceholderAction::Delete
    }
}

fn monitor_auto_turn_label(tmux_session_name: &str) -> String {
    parse_provider_and_channel_from_tmux_name(tmux_session_name)
        .map(|(_, channel_name)| channel_name)
        .filter(|channel_name| !channel_name.trim().is_empty())
        .unwrap_or_else(|| tmux_session_name.to_string())
}

fn monitor_auto_turn_session_key(channel_id: ChannelId, data_start_offset: u64) -> String {
    format!(
        "monitor_auto_turn:ch:{}:off:{}",
        channel_id.get(),
        data_start_offset
    )
}

fn monitor_auto_turn_completion_notice(label: &str, event_count: usize) -> String {
    format!("🔔 Monitor 완료: {label} (events={event_count})")
}

fn enqueue_monitor_auto_turn_suppressed_notification(
    pg_pool: Option<&sqlx::PgPool>,
    db: Option<&crate::db::Db>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
    event_count: usize,
) -> bool {
    let target = format!("channel:{}", channel_id.get());
    let session_key = monitor_auto_turn_session_key(channel_id, data_start_offset);
    let label = monitor_auto_turn_label(tmux_session_name);
    let content = monitor_auto_turn_completion_notice(&label, event_count);
    enqueue_lifecycle_notification_best_effort(
        db,
        pg_pool,
        target.as_str(),
        Some(session_key.as_str()),
        MONITOR_AUTO_TURN_REASON_CODE,
        content.as_str(),
    )
}

fn enqueue_monitor_auto_turn_deferred_notification(
    pg_pool: Option<&sqlx::PgPool>,
    db: Option<&crate::db::Db>,
    channel_id: ChannelId,
    data_start_offset: u64,
) -> bool {
    let target = format!("channel:{}", channel_id.get());
    let session_key = format!(
        "monitor_auto_turn_deferred:ch:{}:off:{}",
        channel_id.get(),
        data_start_offset
    );
    enqueue_lifecycle_notification_best_effort(
        db,
        pg_pool,
        target.as_str(),
        Some(session_key.as_str()),
        MONITOR_AUTO_TURN_DEFERRED_REASON_CODE,
        "🔔 Monitor 트리거 유예 (유저 턴 종료 후 처리)",
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MonitorAutoTurnStart {
    acquired: bool,
    deferred: bool,
}

async fn start_monitor_auto_turn_when_available(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    data_start_offset: u64,
    cancel: &std::sync::atomic::AtomicBool,
) -> MonitorAutoTurnStart {
    let mut deferred = false;
    let synthetic_message_id = MessageId::new(data_start_offset.max(1));

    loop {
        if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
            return MonitorAutoTurnStart {
                acquired: false,
                deferred,
            };
        }

        let token = Arc::new(crate::services::provider::CancelToken::new());
        let started = super::mailbox_try_start_turn(
            shared,
            channel_id,
            token,
            UserId::new(1),
            synthetic_message_id,
        )
        .await;
        if started {
            shared
                .global_active
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            shared
                .turn_start_times
                .insert(channel_id, std::time::Instant::now());
            return MonitorAutoTurnStart {
                acquired: true,
                deferred,
            };
        }

        if !deferred {
            deferred = true;
            let _ = enqueue_monitor_auto_turn_deferred_notification(
                shared.pg_pool.as_ref(),
                sqlite_runtime_db(shared),
                channel_id,
                data_start_offset,
            );
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🔔 Monitor auto-turn deferred until active user turn completes (channel {}, provider={})",
                channel_id.get(),
                provider.as_str()
            );
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }
}

async fn finish_monitor_auto_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) {
    let finish = super::mailbox_finish_turn(shared, provider, channel_id).await;
    if let Some(token) = finish.removed_token {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .global_active
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
    shared.turn_start_times.remove(&channel_id);
    if let Ok(mut last) = shared.last_turn_at.lock() {
        *last = Some(chrono::Local::now().to_rfc3339());
    }
    if finish.has_pending {
        super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            "monitor auto-turn completed with queued backlog",
        );
    }
}

async fn finish_monitor_auto_turn_if_claimed(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    claimed: &mut bool,
    finished: &mut bool,
) {
    if *claimed {
        finish_monitor_auto_turn(shared, provider, channel_id).await;
        *claimed = false;
        *finished = true;
    }
}

fn ensure_monitor_auto_turn_inflight(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    input_fifo_path: &str,
    session_id: Option<&str>,
    turn_start_offset: u64,
    last_offset: u64,
) {
    if super::inflight::load_inflight_state(provider, channel_id.get()).is_some() {
        return;
    }

    let channel_name = parse_provider_and_channel_from_tmux_name(tmux_session_name)
        .map(|(_, channel_name)| channel_name);
    let mut synthetic = super::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        channel_name,
        0,
        0,
        0,
        "Monitor auto-turn".to_string(),
        session_id.map(str::to_string),
        Some(tmux_session_name.to_string()),
        Some(output_path.to_string()),
        Some(input_fifo_path.to_string()),
        last_offset,
    );
    synthetic.turn_start_offset = Some(turn_start_offset);
    synthetic.rebind_origin = true;

    match super::inflight::save_inflight_state_create_new(&synthetic) {
        Ok(()) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Registered synthetic inflight for monitor auto-turn in channel {}",
                channel_id.get()
            );
        }
        Err(super::inflight::CreateNewInflightError::AlreadyExists) => {}
        Err(super::inflight::CreateNewInflightError::Internal(error)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Failed to register synthetic monitor inflight for channel {}: {}",
                channel_id.get(),
                error
            );
        }
    }
}

/// #826 P1 #2 (option b): Decide which of the two offset watermarks
/// (`last_relayed_offset`, `last_enqueued_offset`) a watcher tick should
/// advance after attempting to deliver a terminal response.
///
///  - `last_relayed_offset` is the canonical "Discord has durably received
///    this byte range" watermark. It must advance ONLY on confirmed
///    foreground delivery (direct send or placeholder replace succeeded), or
///    on the notify-path fallback that reached Discord.
///  - `last_enqueued_offset` is the "outbox row committed" watermark. It
///    advances when the notify-bot outbox insert succeeded — the outbox
///    worker owns delivery + retry from there. Prevents re-enqueue of the
///    same range on the next tick without conflating staging with delivery.
///
/// Both watermarks advance in lock-step on genuine delivery so a later
/// dedupe check (which takes their max) sees a single unified floor.
///
/// Pure function extracted for regression-test coverage of the offset-commit
/// gate; the runtime version lives inline in the watcher loop because it is
/// intertwined with other relay bookkeeping.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct OffsetAdvanceDecision {
    pub advance_relayed: bool,
    pub advance_enqueued: bool,
}

#[inline]
pub(super) fn notify_path_offset_advance_decision(
    has_current_response: bool,
    enqueue_succeeded: bool,
    direct_send_delivered: bool,
) -> OffsetAdvanceDecision {
    if direct_send_delivered {
        // Confirmed foreground delivery. Lift both watermarks.
        return OffsetAdvanceDecision {
            advance_relayed: true,
            advance_enqueued: true,
        };
    }
    if enqueue_succeeded {
        // Staged on the outbox — advance the enqueue watermark to dedupe the
        // next tick, but leave the canonical relayed watermark alone.
        return OffsetAdvanceDecision {
            advance_relayed: false,
            advance_enqueued: true,
        };
    }
    if !has_current_response {
        // Empty turn — advance both in lock-step (the original single-offset
        // behaviour) so the watcher doesn't spin on this range.
        return OffsetAdvanceDecision {
            advance_relayed: true,
            advance_enqueued: true,
        };
    }
    // Nothing delivered, nothing staged — leave BOTH watermarks untouched so
    // the next tick can try again.
    OffsetAdvanceDecision::default()
}

/// #826: Build the dedupe session_key for a background-trigger outbox row.
/// Includes the tmux output offset and a short content hash so distinct
/// completions land as separate rows (different offsets ⇒ different keys)
/// while a retry of the exact same range within the dedupe window (same
/// offset + identical content) collapses into one. The resulting key is
/// compact (≤~64 chars) and safe to use as a dedupe column.
///
/// Pure function so the #897 counter-model review P1 (dedupe reason_code
/// AND session_key must BOTH be present for the lifecycle dedupe to arm)
/// has a testable contract.
#[inline]
pub(super) fn build_bg_trigger_session_key(
    channel_id: u64,
    data_start_offset: u64,
    content: &str,
) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!(
        "bg_trigger:ch:{channel_id}:off:{data_start_offset}:h:{:016x}",
        hasher.finish()
    )
}

/// #826: Enqueue a background-trigger turn's terminal response on the
/// notify-bot outbox so it reaches the channel without going through the
/// command bot. The notify-bot is dropped at the intake gate, which is what
/// keeps the auto-trigger path from feeding back into a new turn.
///
/// **Storage backend** (#897 counter-model re-review round 2 Medium):
/// matches `turn_bridge::enqueue_headless_delivery`'s priority —
/// `pg_pool` first when available (primary production storage), falling
/// back to the SQLite `Db` when only the legacy backend is wired in.
/// Without this, a PG-backed runtime would reach the old SQLite-only
/// code path with `Db::None` and silently fall back to direct-send,
/// bypassing the new dedupe / failure-reconcile behaviour entirely.
///
/// **Dedupe** (#897 round 1 P1 #3): both `reason_code` and `session_key`
/// are set so the lifecycle-notification dedupe in
/// `message_outbox::enqueue` can arm. `session_key` encodes
/// `channel_id + data_start_offset + content hash`, so:
///   * Distinct background completions in the same channel produce distinct
///     session_keys (different offsets or different content) → each lands
///     as its own outbox row.
///   * A duplicate retry of the exact same tmux range within the dedupe TTL
///     (same offset, identical content) collapses into the single existing
///     row, which guards against the watcher re-enqueuing while the outbox
///     worker is still delivering.
///   * The dedupe lookup filters out `status='failed'` rows, so a permanently
///     failed prior attempt is NOT allowed to suppress a fresh re-stage.
///
/// The PG path currently does INSERT without a per-tick dedupe query (the
/// SQLite-only `enqueue` helper lives in `message_outbox.rs`; porting it
/// to a shared sqlx/rusqlite interface is tracked separately). Same-row
/// dedupe on the PG side is still achievable via a `UNIQUE(reason_code,
/// session_key, status) WHERE status != 'failed'` partial index, but
/// that's a schema change outside this PR's scope. Follow-up tracked in
/// #898-family.
///
/// Returns `false` only when BOTH backends are unavailable or their
/// insert fails — the caller falls back to a direct command-bot send in
/// that case so the message is never silently lost.
pub(super) async fn enqueue_background_trigger_response_to_notify_outbox(
    pg_pool: Option<&sqlx::PgPool>,
    db: Option<&crate::db::Db>,
    channel_id: ChannelId,
    content: &str,
    data_start_offset: u64,
) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return true;
    }
    let target = format!("channel:{}", channel_id.get());
    let session_key = build_bg_trigger_session_key(channel_id.get(), data_start_offset, content);

    // #897 round-3 High: when `pg_pool` is configured, the outbox worker
    // drains PG EXCLUSIVELY. Writing a row to SQLite as a "fallback" would
    // silently black-hole the message because no worker polls it in that
    // mode. On PG insert failure we return `false` so the caller falls
    // back to a DIRECT Discord send (the only path that guarantees
    // delivery in PG mode) rather than papering over the failure with an
    // undeliverable SQLite row. Mirrors
    // `turn_bridge::enqueue_headless_delivery` which also refuses to fall
    // back to SQLite when PG is configured.
    if let Some(pool) = pg_pool {
        return match sqlx::query(
            "INSERT INTO message_outbox
             (target, content, bot, source, reason_code, session_key)
             VALUES ($1, $2, 'notify', 'system', 'bg_trigger.auto_turn', $3)",
        )
        .bind(target.as_str())
        .bind(content)
        .bind(session_key.as_str())
        .execute(pool)
        .await
        {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(
                    "background-trigger postgres outbox insert failed for channel {}: {}",
                    channel_id,
                    error
                );
                false
            }
        };
    }

    // PG is not configured — use the SQLite outbox (legacy / test setups).
    let Some(db) = db else {
        return false;
    };
    // Call `enqueue` directly (instead of `enqueue_with_db`) so we can
    // distinguish a dedupe-skip (`Ok(false)` — an EARLIER retry already wrote
    // the row, so this call is conceptually successful) from a genuine SQL
    // error (`Err(_)` — caller must fall back to direct send). The legacy
    // `enqueue_with_db` collapses both into `false`, which would spuriously
    // trigger the direct-send fallback on a dedupe and write the same
    // message twice.
    match db.separate_conn() {
        Ok(conn) => {
            match crate::services::message_outbox::enqueue(
                &conn,
                crate::services::message_outbox::OutboxMessage {
                    target: target.as_str(),
                    content,
                    bot: "notify",
                    source: "system",
                    reason_code: Some("bg_trigger.auto_turn"),
                    session_key: Some(session_key.as_str()),
                },
            ) {
                Ok(_inserted_or_deduped) => true,
                Err(error) => {
                    tracing::warn!(
                        "background-trigger outbox enqueue failed for channel {}: {}",
                        channel_id,
                        error
                    );
                    false
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                "background-trigger outbox connection failed for channel {}: {}",
                channel_id,
                error
            );
            false
        }
    }
}

/// #897 counter-model review P1 #2: Find permanently-failed notify-bot
/// outbox rows that originated from this watcher's background-trigger
/// enqueues, extract the tmux offsets that caused them, and delete the
/// rows so they don't accumulate. Returns the MINIMUM observed
/// `data_start_offset` encoded in `session_key`, which the caller uses to
/// roll `last_enqueued_offset` back and re-stage the same tmux range on
/// the next watcher tick.
///
/// **Storage backend** (#897 round 2 Medium): prefers `pg_pool` when
/// available, falling back to the SQLite `Db` — mirrors the enqueue
/// path's ordering so a PG-backed runtime actually reconciles its own
/// failed rows instead of silently skipping when `Db::None`.
///
/// Why this is safe to re-stage:
/// * `message_outbox::enqueue`'s lifecycle dedupe filters out rows where
///   `status='failed'`, so re-inserting at the same session_key produces a
///   fresh pending row rather than collapsing into the dead one.
/// * We delete the failed rows here so they don't pollute `SELECT *`
///   queries or eat unbounded table space.
///
/// Without this reconciliation a single transient notify-bot or Discord
/// failure permanently suppresses re-enqueue for the remainder of the
/// watcher's lifetime — the exact P1 gap the counter-model reviewer
/// flagged. See PR #897.
async fn reconcile_failed_bg_trigger_enqueues_for_channel(
    pg_pool: Option<&sqlx::PgPool>,
    db: Option<&crate::db::Db>,
    channel_id: ChannelId,
) -> Option<u64> {
    let target = format!("channel:{}", channel_id.get());

    // #897 round-3 High: when `pg_pool` is configured it is the ONLY
    // authoritative store. Consulting SQLite as a "fallback" on PG
    // failure or on an empty PG result would surface rows from a legacy
    // test/dev database that the outbox worker never produced, and worse
    // could delete rows written by a prior run. On PG error we surface
    // `None` so the next poll retries; there is no data-safe fallback.
    if let Some(pool) = pg_pool {
        let rows_res = sqlx::query_as::<_, (i64, Option<String>)>(
            "SELECT id, session_key FROM message_outbox
             WHERE target = $1
               AND bot = 'notify'
               AND source = 'system'
               AND reason_code = 'bg_trigger.auto_turn'
               AND status = 'failed'",
        )
        .bind(target.as_str())
        .fetch_all(pool)
        .await;

        return match rows_res {
            Ok(rows) if !rows.is_empty() => {
                let mut min_offset: Option<u64> = None;
                for (_, session_key) in &rows {
                    if let Some(offset) = session_key
                        .as_deref()
                        .and_then(parse_bg_trigger_offset_from_session_key)
                    {
                        min_offset = Some(min_offset.map(|m| m.min(offset)).unwrap_or(offset));
                    }
                }
                for (id, _) in &rows {
                    if let Err(error) = sqlx::query("DELETE FROM message_outbox WHERE id = $1")
                        .bind(id)
                        .execute(pool)
                        .await
                    {
                        tracing::warn!(
                            "failed to delete reconciled bg_trigger row {}: {}",
                            id,
                            error
                        );
                    }
                }
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ♻ reconciled {} failed bg_trigger outbox row(s) for channel {} (min offset: {:?}) [pg]",
                    rows.len(),
                    channel_id,
                    min_offset,
                );
                min_offset
            }
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(
                    "postgres bg_trigger reconcile query failed for channel {}: {}",
                    channel_id,
                    error
                );
                None
            }
        };
    }

    // PG is not configured — use the SQLite outbox (legacy/test setups).
    let db = db?;
    let conn = db.separate_conn().ok()?;

    let rows: Vec<(i64, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, COALESCE(session_key, '') FROM message_outbox
                 WHERE target = ?1
                   AND bot = 'notify'
                   AND source = 'system'
                   AND reason_code = 'bg_trigger.auto_turn'
                   AND status = 'failed'",
            )
            .ok()?;
        stmt.query_map(libsql_rusqlite::params![target.as_str()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .ok()?
        .filter_map(|r| r.ok())
        .collect()
    };
    if rows.is_empty() {
        return None;
    }

    let mut min_offset: Option<u64> = None;
    for (_, session_key) in &rows {
        if let Some(offset) = parse_bg_trigger_offset_from_session_key(session_key) {
            min_offset = Some(min_offset.map(|m| m.min(offset)).unwrap_or(offset));
        }
    }

    for (id, _) in &rows {
        let _ = conn.execute(
            "DELETE FROM message_outbox WHERE id = ?1",
            libsql_rusqlite::params![id],
        );
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ♻ reconciled {} failed bg_trigger outbox row(s) for channel {} (min offset: {:?}) [sqlite]",
        rows.len(),
        channel_id,
        min_offset,
    );

    min_offset
}

/// Pure helper: extract the `data_start_offset` encoded in a
/// background-trigger `session_key`. Format produced by
/// `build_bg_trigger_session_key` is `bg_trigger:ch:{id}:off:{offset}:h:{hash16}`.
/// Returns `None` for malformed keys so the caller can safely ignore
/// outbox rows whose session_key no longer conforms to the expected shape
/// (e.g. future schema changes or hand-written operator rows).
#[inline]
pub(super) fn parse_bg_trigger_offset_from_session_key(session_key: &str) -> Option<u64> {
    let after_off = session_key.split(":off:").nth(1)?;
    let off_str = after_off.split(':').next()?;
    off_str.parse::<u64>().ok()
}

/// Pure helper for the watermark-rollback policy (#897 P1 #2). Given the
/// watcher's current `last_enqueued_offset` and the minimum offset from a
/// reconciled outbox failure, return the new watermark that allows
/// re-emission of the failed range on the next watcher tick while
/// preserving progress past other, unaffected ranges.
///
/// Rules:
/// 1. `None → None`: nothing staged, nothing to roll back.
/// 2. Current ≤ reconciled: the watermark is already at or below the
///    failed offset, so the next visit will naturally re-emit that range.
/// 3. Current > reconciled: pull back to `reconciled.saturating_sub(1)` so
///    the dedupe floor `max(relayed, enqueued)` permits
///    `data_start_offset < prev_offset` evaluation at the exact failed
///    offset. Using `saturating_sub` guards against reconciled=0.
#[inline]
pub(super) fn rollback_enqueued_offset_for_reconciled_failures(
    last_enqueued_offset: Option<u64>,
    reconciled_min_offset: u64,
) -> Option<u64> {
    match last_enqueued_offset {
        None => None,
        Some(current) if current <= reconciled_min_offset => Some(current),
        Some(_) => Some(reconciled_min_offset.saturating_sub(1)),
    }
}

fn watcher_should_yield_to_active_bridge_turn(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
    current_offset: u64,
) -> bool {
    let state = super::inflight::load_inflight_state(provider, channel_id.get());
    watcher_should_yield_to_inflight_state(
        state.as_ref(),
        tmux_session_name,
        data_start_offset,
        current_offset,
    )
}

fn watcher_should_yield_to_inflight_state(
    state: Option<&super::inflight::InflightTurnState>,
    tmux_session_name: &str,
    data_start_offset: u64,
    current_offset: u64,
) -> bool {
    let Some(state) = state else {
        return false;
    };

    if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return false;
    }

    let turn_start_offset = state.turn_start_offset.unwrap_or(state.last_offset);
    data_start_offset <= turn_start_offset && turn_start_offset < current_offset
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeadSessionCleanupPlan {
    preserve_tmux_session: bool,
    report_idle_status: bool,
}

fn dead_session_cleanup_plan(dispatch_protected: bool) -> DeadSessionCleanupPlan {
    DeadSessionCleanupPlan {
        preserve_tmux_session: dispatch_protected,
        report_idle_status: true,
    }
}

fn extract_result_error_text(value: &serde_json::Value) -> String {
    let errors = value
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::trim))
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    if !errors.trim().is_empty() {
        errors
    } else {
        value
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    }
}

fn load_restored_session_cwd(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    session_keys: &[String],
) -> Option<String> {
    if let Some(pg_pool) = pg_pool {
        let session_keys = session_keys.to_vec();
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                for session_key in session_keys {
                    let path = sqlx::query_scalar::<_, String>(
                        "SELECT cwd FROM sessions WHERE session_key = $1 LIMIT 1",
                    )
                    .bind(&session_key)
                    .fetch_optional(&pool)
                    .await
                    .map_err(|error| format!("load tmux restore cwd {session_key}: {error}"))?;
                    if let Some(path) =
                        path.filter(|path| !path.is_empty() && std::path::Path::new(path).is_dir())
                    {
                        return Ok(Some(path));
                    }
                }
                Ok(None)
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    let conn = db?.read_conn().ok()?;
    session_keys.iter().find_map(|session_key| {
        conn.query_row(
            "SELECT cwd FROM sessions WHERE session_key = ?1",
            [session_key.as_str()],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|path| !path.is_empty() && std::path::Path::new(path).is_dir())
    })
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

fn inflight_duration_ms(started_at: Option<&str>) -> Option<i64> {
    let started_at = started_at?.trim();
    if started_at.is_empty() {
        return None;
    }
    let parsed = chrono::NaiveDateTime::parse_from_str(started_at, "%Y-%m-%d %H:%M:%S").ok()?;
    let elapsed = chrono::Local::now().naive_local() - parsed;
    Some(elapsed.num_milliseconds().max(0))
}

fn load_restored_provider_session_id(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
) -> Option<String> {
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_keys =
        super::adk_session::build_session_key_candidates(token_hash, provider, &tmux_name);

    if let Some(pg_pool) = pg_pool {
        let session_keys = session_keys.clone();
        let provider_name = provider.as_str().to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                for session_key in session_keys {
                    let session_id = sqlx::query_scalar::<_, Option<String>>(
                        "SELECT claude_session_id
                         FROM sessions
                         WHERE session_key = $1 AND provider = $2
                         LIMIT 1",
                    )
                    .bind(&session_key)
                    .bind(&provider_name)
                    .fetch_optional(&pool)
                    .await
                    .map_err(|error| format!("load tmux provider session {session_key}: {error}"))?
                    .flatten();
                    if let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty())
                    {
                        return Ok(Some(session_id));
                    }
                }
                Ok(None)
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    db.and_then(|db| {
        db.read_conn().ok().and_then(|conn| {
            session_keys.iter().find_map(|session_key| {
                conn.query_row(
                    "SELECT claude_session_id FROM sessions WHERE session_key = ?1 AND provider = ?2",
                    [session_key.as_str(), provider.as_str()],
                    |row| row.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten()
                .filter(|session_id| !session_id.is_empty())
            })
        })
    })
}

fn recovery_handled_channel_key(channel_id: u64) -> String {
    format!("recovery_handled_channel:{channel_id}")
}

fn sqlite_runtime_db(shared: &SharedData) -> Option<&crate::db::Db> {
    if shared.pg_pool.is_some() {
        None
    } else {
        shared.sqlite.as_ref()
    }
}

fn watcher_has_post_work_ready_evidence(
    full_response: &str,
    tool_state: &WatcherToolState,
    task_notification_kind: Option<TaskNotificationKind>,
) -> bool {
    !full_response.trim().is_empty() || tool_state.any_tool_used || task_notification_kind.is_some()
}

fn normalize_human_alert_target(channel: &str) -> Option<String> {
    let channel = channel.trim();
    if channel.is_empty() {
        return None;
    }
    Some(if channel.starts_with("channel:") {
        channel.to_string()
    } else {
        format!("channel:{channel}")
    })
}

fn load_human_alert_target(shared: &SharedData) -> Option<String> {
    if let Some(pool) = shared.pg_pool.as_ref() {
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            |pool| async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT value FROM kv_meta WHERE key = 'kanban_human_alert_channel_id'",
                )
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres human alert target: {error}"))
            },
            |message| message,
        )
        .ok()
        .flatten()
        .and_then(|channel| normalize_human_alert_target(&channel));
    }

    sqlite_runtime_db(shared)
        .and_then(|db| db.read_conn().ok())
        .and_then(|conn| {
            conn.query_row(
                "SELECT value FROM kv_meta WHERE key = 'kanban_human_alert_channel_id'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
        })
        .and_then(|channel| normalize_human_alert_target(&channel))
}

fn merge_card_label_metadata(existing_metadata: Option<&str>, label: &str) -> String {
    let mut metadata = existing_metadata
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();

    let mut labels = metadata
        .get("labels")
        .and_then(|value| value.as_str())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !labels.iter().any(|existing| existing == label) {
        labels.push(label.to_string());
    }
    metadata.insert(
        "labels".to_string(),
        serde_json::Value::String(labels.join(",")),
    );

    serde_json::Value::Object(metadata).to_string()
}

fn update_card_ready_failure_marker_sqlite(
    db: &crate::db::Db,
    card_id: &str,
    reason: &str,
) -> Result<bool, String> {
    let conn = db
        .separate_conn()
        .map_err(|error| format!("open sqlite card metadata DB: {error}"))?;
    let existing_metadata = conn
        .query_row(
            "SELECT metadata FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|error| format!("load sqlite card metadata for {card_id}: {error}"))?
        .flatten();
    let metadata_json =
        merge_card_label_metadata(existing_metadata.as_deref(), READY_FOR_INPUT_STUCK_LABEL);
    let updated = conn
        .execute(
            "UPDATE kanban_cards
             SET metadata = ?1,
                 blocked_reason = ?2,
                 updated_at = datetime('now')
             WHERE id = ?3",
            libsql_rusqlite::params![metadata_json, reason, card_id],
        )
        .map_err(|error| format!("update sqlite ready marker for {card_id}: {error}"))?;
    Ok(updated > 0)
}

async fn update_card_ready_failure_marker_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    reason: &str,
) -> Result<bool, String> {
    let existing_metadata = sqlx::query_scalar::<_, Option<String>>(
        "SELECT metadata::text FROM kanban_cards WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card metadata for {card_id}: {error}"))?
    .flatten();
    let metadata_json =
        merge_card_label_metadata(existing_metadata.as_deref(), READY_FOR_INPUT_STUCK_LABEL);
    let updated = sqlx::query(
        "UPDATE kanban_cards
         SET metadata = $1::jsonb,
             blocked_reason = $2,
             updated_at = NOW()
         WHERE id = $3",
    )
    .bind(metadata_json)
    .bind(reason)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres ready marker for {card_id}: {error}"))?
    .rows_affected();
    Ok(updated > 0)
}

fn load_dispatch_card_id(shared: &SharedData, dispatch_id: &str) -> Option<String> {
    if let Some(pool) = shared.pg_pool.as_ref() {
        let dispatch_id = dispatch_id.to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |pool| async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT kanban_card_id FROM task_dispatches WHERE id = $1",
                )
                .bind(dispatch_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres dispatch card id: {error}"))
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    sqlite_runtime_db(shared)
        .and_then(|db| db.read_conn().ok())
        .and_then(|conn| {
            conn.query_row(
                "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
        })
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct ReadyForInputFailureResult {
    pub dispatch_failed: bool,
    pub card_id: Option<String>,
    pub card_marked: bool,
    pub human_alert_sent: bool,
}

pub(super) async fn fail_dispatch_for_ready_for_input_stall(
    shared: &Arc<SharedData>,
    dispatch_id: &str,
    tmux_session_name: &str,
) -> Result<ReadyForInputFailureResult, String> {
    let payload = serde_json::json!({
        "reason": READY_FOR_INPUT_STUCK_REASON,
        "failure_kind": READY_FOR_INPUT_STUCK_LABEL,
        "tmux_session_name": tmux_session_name,
    });
    let changed = crate::dispatch::set_dispatch_status_with_backends(
        sqlite_runtime_db(shared.as_ref()),
        shared.pg_pool.as_ref(),
        dispatch_id,
        "failed",
        Some(&payload),
        "tmux_ready_for_input_stuck",
        Some(&["pending", "dispatched"]),
        false,
    )
    .map_err(|error| format!("mark dispatch {dispatch_id} failed for ready stall: {error}"))?;

    let card_id = load_dispatch_card_id(shared.as_ref(), dispatch_id);
    let mut card_marked = false;
    if let Some(card_id_ref) = card_id.as_deref() {
        card_marked = if let Some(pool) = shared.pg_pool.as_ref() {
            update_card_ready_failure_marker_pg(pool, card_id_ref, READY_FOR_INPUT_STUCK_REASON)
                .await?
        } else if let Some(db) = sqlite_runtime_db(shared.as_ref()) {
            update_card_ready_failure_marker_sqlite(db, card_id_ref, READY_FOR_INPUT_STUCK_REASON)?
        } else {
            false
        };
    }

    let human_alert_sent = if changed > 0 {
        load_human_alert_target(shared.as_ref()).is_some_and(|target| {
            let card_label = card_id.as_deref().unwrap_or("-");
            let content = format!(
                "자동큐 safety-net 발동: dispatch {dispatch_id} / card {card_label} / session {tmux_session_name} / {READY_FOR_INPUT_STUCK_REASON}"
            );
            enqueue_lifecycle_notification_best_effort(
                sqlite_runtime_db(shared.as_ref()),
                shared.pg_pool.as_ref(),
                &target,
                Some(dispatch_id),
                "dispatch.stuck_at_ready",
                &content,
            )
        })
    } else {
        false
    };

    Ok(ReadyForInputFailureResult {
        dispatch_failed: changed > 0,
        card_id,
        card_marked,
        human_alert_sent,
    })
}

pub(super) fn recovery_handled_channel_exists(shared: &SharedData, channel_id: u64) -> bool {
    let key = recovery_handled_channel_key(channel_id);

    if let Ok(value) = super::internal_api::get_kv_value(&key) {
        return value.is_some();
    }

    if let Some(pg_pool) = shared.pg_pool.as_ref() {
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(
                         SELECT 1
                         FROM kv_meta
                         WHERE key = $1
                           AND (expires_at IS NULL OR expires_at > NOW())
                     )",
                )
                .bind(&key)
                .fetch_one(&pool)
                .await
                .map_err(|error| format!("load recovery handled marker {key}: {error}"))
            },
            |message| message,
        )
        .unwrap_or(false);
    }

    shared
        .sqlite
        .as_ref()
        .and_then(|db| {
            db.lock().ok().and_then(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) > 0 FROM kv_meta WHERE key = ?1",
                    [key],
                    |row| row.get::<_, bool>(0),
                )
                .ok()
            })
        })
        .unwrap_or(false)
}

pub(super) async fn store_recovery_handled_channels(shared: &SharedData, channel_ids: &[u64]) {
    if channel_ids.is_empty() {
        return;
    }

    let marker_value = chrono::Utc::now().timestamp().to_string();
    let mut stored_via_internal_api = true;
    for channel_id in channel_ids {
        let key = recovery_handled_channel_key(*channel_id);
        if let Err(error) = super::internal_api::set_kv_value(&key, &marker_value) {
            tracing::debug!(
                "recovery handled marker fallback for {key}: direct runtime API unavailable: {error}"
            );
            stored_via_internal_api = false;
            break;
        }
    }
    if stored_via_internal_api {
        return;
    }

    if let Some(pg_pool) = shared.pg_pool.as_ref() {
        match pg_pool.begin().await {
            Ok(mut tx) => {
                for channel_id in channel_ids {
                    let key = recovery_handled_channel_key(*channel_id);
                    if let Err(error) = sqlx::query(
                        "INSERT INTO kv_meta (key, value, expires_at)
                         VALUES ($1, $2, NULL)
                         ON CONFLICT (key) DO UPDATE
                         SET value = EXCLUDED.value,
                             expires_at = EXCLUDED.expires_at",
                    )
                    .bind(&key)
                    .bind(&marker_value)
                    .execute(&mut *tx)
                    .await
                    {
                        tracing::warn!(
                            "failed to persist recovery handled marker {key} in postgres: {error}"
                        );
                        return;
                    }
                }
                if let Err(error) = tx.commit().await {
                    tracing::warn!("failed to commit recovery handled marker tx: {error}");
                }
            }
            Err(error) => {
                tracing::warn!("failed to begin recovery handled marker tx: {error}");
            }
        }
        return;
    }

    if let Some(db) = shared.sqlite.as_ref()
        && let Ok(conn) = db.lock()
    {
        for channel_id in channel_ids {
            let key = recovery_handled_channel_key(*channel_id);
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                libsql_rusqlite::params![key, chrono::Utc::now().timestamp().to_string()],
            )
            .ok();
        }
    }
}

pub(super) async fn clear_recovery_handled_channels(shared: &SharedData) {
    if let Err(error) = super::internal_api::clear_kv_prefix("recovery_handled_channel:") {
        tracing::debug!(
            "recovery handled marker clear fallback: direct runtime API unavailable: {error}"
        );
    } else {
        return;
    }

    if let Some(pg_pool) = shared.pg_pool.as_ref() {
        if let Err(error) =
            sqlx::query("DELETE FROM kv_meta WHERE key LIKE 'recovery_handled_channel:%'")
                .execute(pg_pool)
                .await
        {
            tracing::warn!("failed to clear recovery handled markers in postgres: {error}");
        }
        return;
    }

    if let Some(db) = shared.sqlite.as_ref()
        && let Ok(conn) = db.lock()
    {
        conn.execute(
            "DELETE FROM kv_meta WHERE key LIKE 'recovery_handled_channel:%'",
            [],
        )
        .ok();
    }
}

// Tmux watcher output is activity, but reusing hook_session here would also
// overwrite status/tokens defaults. Touch only last_heartbeat instead.
pub(super) fn refresh_session_heartbeat_from_tmux_output(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    tmux_session_name: &str,
    thread_channel_id: Option<u64>,
) -> bool {
    let session_keys =
        super::adk_session::build_session_key_candidates(token_hash, provider, tmux_session_name);

    if let Some(pg_pool) = pg_pool {
        let provider_name = provider.as_str().to_string();
        let thread_channel_id = thread_channel_id.map(|value| value.to_string());
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                let updated = sqlx::query(
                    "UPDATE sessions
                     SET last_heartbeat = NOW()
                     WHERE session_key = $1 OR session_key = $2",
                )
                .bind(&session_keys[0])
                .bind(&session_keys[1])
                .execute(&pool)
                .await
                .map_err(|error| format!("refresh pg watcher heartbeat by session key: {error}"))?
                .rows_affected();
                if updated > 0 {
                    return Ok(true);
                }

                let Some(thread_channel_id) = thread_channel_id else {
                    return Ok(false);
                };
                let updated = sqlx::query(
                    "UPDATE sessions
                     SET last_heartbeat = NOW()
                     WHERE provider = $1
                       AND thread_channel_id = $2
                       AND status IN ('idle', 'working')",
                )
                .bind(&provider_name)
                .bind(&thread_channel_id)
                .execute(&pool)
                .await
                .map_err(|error| {
                    format!("refresh pg watcher heartbeat by thread channel: {error}")
                })?
                .rows_affected();
                Ok(updated > 0)
            },
            |message| message,
        )
        .unwrap_or(false);
    }

    let Some(db) = db else {
        return false;
    };
    let Ok(conn) = db.lock() else {
        return false;
    };
    let updated = conn
        .execute(
            "UPDATE sessions
             SET last_heartbeat = datetime('now')
             WHERE session_key = ?1 OR session_key = ?2",
            [session_keys[0].as_str(), session_keys[1].as_str()],
        )
        .unwrap_or(0);
    if updated > 0 {
        return true;
    }

    thread_channel_id.is_some_and(|thread_channel_id| {
        let thread_channel_id = thread_channel_id.to_string();
        conn.execute(
            "UPDATE sessions
             SET last_heartbeat = datetime('now')
             WHERE provider = ?1
               AND thread_channel_id = ?2
               AND status IN ('idle', 'working')",
            [provider.as_str(), thread_channel_id.as_str()],
        )
        .unwrap_or(0)
            > 0
    })
}

fn maybe_refresh_watcher_activity_heartbeat(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    tmux_session_name: &str,
    thread_channel_id: Option<u64>,
    last_heartbeat_at: &mut Option<std::time::Instant>,
) {
    let now = std::time::Instant::now();
    if last_heartbeat_at
        .is_some_and(|last| now.duration_since(last) < WATCHER_ACTIVITY_HEARTBEAT_INTERVAL)
    {
        return;
    }

    if refresh_session_heartbeat_from_tmux_output(
        db,
        pg_pool,
        token_hash,
        provider,
        tmux_session_name,
        thread_channel_id,
    ) {
        *last_heartbeat_at = Some(now);
    }
}

async fn clear_provider_session_for_retry(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    fallback_session_id: Option<&str>,
) {
    let stale_sid = {
        let mut data = shared.core.lock().await;
        let old = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.session_id.clone())
            .or_else(|| fallback_session_id.map(ToString::to_string));
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
        }
        old
    };

    let session_key = format!(
        "{}:{}",
        crate::services::platform::hostname_short(),
        tmux_session_name
    );
    super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;

    if let Some(sid) = stale_sid {
        let _ = super::internal_api::clear_stale_session_id(&sid).await;
    }
}

async fn resolve_watcher_dispatch_id(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    inflight_state: Option<&super::inflight::InflightTurnState>,
) -> Option<String> {
    inflight_state
        .and_then(|state| state.dispatch_id.clone())
        .or_else(|| {
            inflight_state.and_then(|state| super::adk_session::parse_dispatch_id(&state.user_text))
        })
        .or(super::adk_session::lookup_pending_dispatch_for_thread(
            shared.api_port,
            channel_id.get(),
        )
        .await)
        .or_else(|| {
            resolve_dispatched_thread_dispatch_from_db(
                shared.sqlite.as_ref(),
                shared.pg_pool.as_ref(),
                channel_id.get(),
            )
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MissingInflightFallbackPlan {
    warn: bool,
    trigger_reattach: bool,
}

fn missing_inflight_fallback_plan(
    inflight_missing: bool,
    dispatch_resolved: bool,
    terminal_output_committed: bool,
) -> MissingInflightFallbackPlan {
    MissingInflightFallbackPlan {
        warn: inflight_missing,
        trigger_reattach: inflight_missing && !dispatch_resolved && terminal_output_committed,
    }
}

async fn wait_for_reacquired_turn_bridge_inflight_state(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    attempts: usize,
    delay: tokio::time::Duration,
) -> bool {
    for attempt in 0..=attempts {
        if super::inflight::load_inflight_state(provider, channel_id.get()).is_some_and(|state| {
            !state.rebind_origin && state.tmux_session_name.as_deref() == Some(tmux_session_name)
        }) {
            return true;
        }

        if attempt < attempts {
            tokio::time::sleep(delay).await;
        }
    }

    false
}

fn trigger_missing_inflight_reattach(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ watcher: DB dispatch fallback unresolved for channel {} — triggering explicit reattach to {}",
        channel_id.get(),
        tmux_session_name
    );

    if !tmux_session_has_live_pane(tmux_session_name) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: explicit reattach skipped for channel {} — tmux session is not live ({})",
            channel_id.get(),
            tmux_session_name
        );
        return;
    }

    let (output_path, input_fifo_path) = super::turn_bridge::tmux_runtime_paths(tmux_session_name);
    let initial_offset = std::fs::metadata(&output_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let channel_name =
        parse_provider_and_channel_from_tmux_name(tmux_session_name).map(|(_, name)| name);

    let mut state = super::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        channel_name.clone(),
        0,
        0,
        0,
        "watcher missing-inflight reattach".to_string(),
        None,
        Some(tmux_session_name.to_string()),
        Some(output_path.clone()),
        Some(input_fifo_path),
        initial_offset,
    );
    state.rebind_origin = true;

    match super::inflight::save_inflight_state_create_new(&state) {
        Ok(()) => {}
        Err(super::inflight::CreateNewInflightError::AlreadyExists) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: explicit reattach skipped for channel {} — inflight state already exists",
                channel_id.get()
            );
            return;
        }
        Err(super::inflight::CreateNewInflightError::Internal(error)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::error!(
                "  [{ts}] ❌ watcher: explicit reattach failed for channel {} / {} after DB fallback miss: {}",
                channel_id.get(),
                tmux_session_name,
                error
            );
            return;
        }
    }

    if let Ok(mut data) = shared.core.try_lock() {
        let session = data
            .sessions
            .entry(channel_id)
            .or_insert_with(|| super::DiscordSession {
                session_id: None,
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel_id.get()),
                channel_name: channel_name.clone(),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: super::runtime_store::load_generation(),
                assistant_turns: 0,
            });
        session.channel_id = Some(channel_id.get());
        session.last_active = tokio::time::Instant::now();
        if session.channel_name.is_none() {
            session.channel_name = channel_name.clone();
        }
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: explicit reattach could not refresh in-memory session for channel {} because core state was busy",
            channel_id.get()
        );
    }

    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
    let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = TmuxWatcherHandle {
        paused: paused.clone(),
        resume_offset: resume_offset.clone(),
        cancel: cancel.clone(),
        pause_epoch: pause_epoch.clone(),
        turn_delivered: turn_delivered.clone(),
    };
    let fresh = claim_or_replace_watcher(
        &shared.tmux_watchers,
        channel_id,
        handle,
        provider,
        "watcher_missing_inflight_fallback",
    );
    record_recent_watcher_reattach_offset(channel_id, tmux_session_name, initial_offset);
    tokio::spawn(tmux_output_watcher(
        channel_id,
        http.clone(),
        shared.clone(),
        output_path,
        tmux_session_name.to_string(),
        initial_offset,
        cancel,
        paused,
        resume_offset,
        pause_epoch,
        turn_delivered,
    ));

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ↻ watcher: reattach triggered for channel {} (tmux={}, offset={}, replaced={})",
        channel_id.get(),
        tmux_session_name,
        initial_offset,
        !fresh
    );
}

/// #226: Atomically claim a channel for watcher creation using DashMap::entry().
/// Returns true if the claim succeeded (caller should spawn the watcher).
/// Returns false if a watcher already exists (caller should skip).
pub(super) fn try_claim_watcher(
    watchers: &dashmap::DashMap<ChannelId, TmuxWatcherHandle>,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
) -> bool {
    use dashmap::mapref::entry::Entry;
    let claimed = match watchers.entry(channel_id) {
        Entry::Occupied(_) => false,
        Entry::Vacant(entry) => {
            entry.insert(handle);
            true
        }
    };
    let slot_present = watchers.contains_key(&channel_id);
    record_watcher_invariant(
        slot_present,
        None,
        channel_id,
        "watcher_one_per_channel",
        "src/services/discord/tmux.rs:try_claim_watcher",
        "watcher claim must leave a single channel-owned watcher slot",
        serde_json::json!({
            "claimed": claimed,
            "watcher_slots": watchers.len(),
        }),
    );
    debug_assert!(
        slot_present,
        "watcher claim must leave a channel-owned watcher slot"
    );
    claimed
}

/// #243: Claim a channel for watcher creation, cancelling any existing watcher.
/// Unlike try_claim_watcher (which skips if occupied), this always succeeds:
/// if a watcher already exists, it is cancelled and replaced.
/// Returns true if a fresh slot was created, false if an existing watcher was replaced.
pub(super) fn claim_or_replace_watcher(
    watchers: &dashmap::DashMap<ChannelId, TmuxWatcherHandle>,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    source: &str,
) -> bool {
    use dashmap::mapref::entry::Entry;
    let fresh = match watchers.entry(channel_id) {
        Entry::Occupied(mut entry) => {
            // Cancel the existing watcher — it will exit on its next loop iteration
            // and skip DashMap removal (since cancel is set).
            entry
                .get()
                .cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let stale_cancelled = entry
                .get()
                .cancel
                .load(std::sync::atomic::Ordering::Relaxed);
            record_watcher_invariant(
                stale_cancelled,
                Some(provider),
                channel_id,
                "watcher_replacement_cancels_stale",
                "src/services/discord/tmux.rs:claim_or_replace_watcher",
                "replacing a watcher must cancel the stale watcher before installing the new handle",
                serde_json::json!({
                    "source": source,
                }),
            );
            debug_assert!(
                stale_cancelled,
                "stale watcher must be cancelled before replacement"
            );
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ♻ watcher replaced for channel {} — cancelled stale watcher",
                channel_id
            );
            entry.insert(handle);
            crate::services::observability::emit_watcher_replaced(
                provider.as_str(),
                channel_id.get(),
                source,
            );
            false
        }
        Entry::Vacant(entry) => {
            entry.insert(handle);
            true
        }
    };
    let slot_present = watchers.contains_key(&channel_id);
    record_watcher_invariant(
        slot_present,
        Some(provider),
        channel_id,
        "watcher_one_per_channel",
        "src/services/discord/tmux.rs:claim_or_replace_watcher",
        "watcher replacement must leave exactly one channel-owned watcher slot",
        serde_json::json!({
            "fresh": fresh,
            "source": source,
            "watcher_slots": watchers.len(),
        }),
    );
    debug_assert!(
        slot_present,
        "watcher replacement must leave a channel-owned watcher slot"
    );
    fresh
}

use crate::services::tmux_common::{current_tmux_owner_marker, tmux_owner_path};

pub(super) fn session_belongs_to_current_runtime(
    session_name: &str,
    current_owner_marker: &str,
) -> bool {
    std::fs::read_to_string(tmux_owner_path(session_name))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| value == current_owner_marker)
        .unwrap_or(false)
}

async fn reconcile_orphan_suppressed_placeholder_for_restored_watcher(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
) {
    let has_active_turn = shared.mailbox(channel_id).has_active_turn().await;
    let Some(state) = super::inflight::load_inflight_state(provider, channel_id.get()) else {
        return;
    };
    let ctx = PlaceholderSuppressContext {
        origin: PlaceholderSuppressOrigin::OrphanRestartHandoff,
        placeholder_msg_id: Some(MessageId::new(state.current_msg_id)),
        response_sent_offset: state.response_sent_offset,
        last_edit_text: "",
        inflight_state: Some(&state),
        has_active_turn,
        tmux_session_name,
        task_notification_kind: None,
        reattach_offset_match: false,
    };
    let decision = decide_placeholder_suppression(&ctx);
    let is_edit = matches!(decision, PlaceholderSuppressDecision::Edit(_));
    let msg_id = MessageId::new(state.current_msg_id);
    apply_placeholder_suppression(
        http,
        channel_id,
        shared,
        ctx.placeholder_msg_id,
        ctx.origin,
        decision,
        None,
    )
    .await;
    if is_edit {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ✓ reconciled orphan suppressed placeholder for channel {} msg {}",
            channel_id.get(),
            msg_id.get()
        );
    }
}

fn persist_watcher_stream_progress(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_msg_id: Option<MessageId>,
    full_response: &str,
    response_sent_offset: usize,
    current_tool_line: Option<&str>,
    prev_tool_status: Option<&str>,
    task_notification_kind: Option<TaskNotificationKind>,
) {
    let Some(mut inflight) = super::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return;
    };
    if inflight.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return;
    }

    if let Some(msg_id) = current_msg_id {
        inflight.current_msg_id = msg_id.get();
    }
    let normalized_response_sent_offset =
        normalize_response_sent_offset(full_response, response_sent_offset);
    let monotonic_offset = normalized_response_sent_offset >= inflight.response_sent_offset;
    record_watcher_invariant(
        monotonic_offset,
        Some(provider),
        channel_id,
        "response_sent_offset_monotonic",
        "src/services/discord/tmux.rs:persist_watcher_stream_progress",
        "watcher response_sent_offset must not move backwards",
        serde_json::json!({
            "previous": inflight.response_sent_offset,
            "next": normalized_response_sent_offset,
            "tmux_session_name": tmux_session_name,
        }),
    );
    debug_assert!(
        monotonic_offset,
        "watcher response_sent_offset must not move backwards"
    );
    let offset_in_bounds = normalized_response_sent_offset <= full_response.len()
        && full_response.is_char_boundary(normalized_response_sent_offset);
    record_watcher_invariant(
        offset_in_bounds,
        Some(provider),
        channel_id,
        "response_sent_offset_in_bounds",
        "src/services/discord/tmux.rs:persist_watcher_stream_progress",
        "watcher response_sent_offset must stay on a full_response boundary",
        serde_json::json!({
            "next": normalized_response_sent_offset,
            "full_response_len": full_response.len(),
            "tmux_session_name": tmux_session_name,
        }),
    );
    debug_assert!(
        offset_in_bounds,
        "watcher response_sent_offset must stay on a full_response boundary"
    );
    inflight.full_response = full_response.to_string();
    inflight.response_sent_offset = normalized_response_sent_offset;
    inflight.current_tool_line = current_tool_line.map(str::to_string);
    inflight.prev_tool_status = prev_tool_status.map(str::to_string);
    if task_notification_kind.is_some() {
        inflight.task_notification_kind = task_notification_kind;
    }
    let _ = super::inflight::save_inflight_state(&inflight);
}

async fn finish_restored_watcher_active_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    finish_mailbox_on_completion: bool,
    stop_source: &'static str,
) {
    if !finish_mailbox_on_completion {
        return;
    }

    let finish = super::mailbox_finish_turn(shared, provider, channel_id).await;
    if let Some(token) = finish.removed_token {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    super::clear_watchdog_deadline_override(channel_id.get()).await;
    shared
        .dispatch_thread_parents
        .retain(|_, thread| *thread != channel_id);
    if !finish.has_pending {
        shared.dispatch_role_overrides.remove(&channel_id);
    }
    if finish.mailbox_online && finish.has_pending {
        super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            stop_source,
        );
    }
}

/// Background watcher that continuously tails a tmux output file.
/// When Claude produces output from terminal input (not Discord), relay it to Discord.
pub(super) async fn tmux_output_watcher(
    channel_id: ChannelId,
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    output_path: String,
    tmux_session_name: String,
    initial_offset: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    turn_delivered: Arc<std::sync::atomic::AtomicBool>,
) {
    tmux_output_watcher_with_restore(
        channel_id,
        http,
        shared,
        output_path,
        tmux_session_name,
        initial_offset,
        cancel,
        paused,
        resume_offset,
        pause_epoch,
        turn_delivered,
        None,
    )
    .await;
}

/// Background watcher variant used by restart recovery to continue editing an
/// existing streaming placeholder instead of creating a new one.
pub(super) async fn tmux_output_watcher_with_restore(
    channel_id: ChannelId,
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    output_path: String,
    tmux_session_name: String,
    initial_offset: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    turn_delivered: Arc<std::sync::atomic::AtomicBool>,
    restored_turn: Option<RestoredWatcherTurn>,
) {
    use std::io::{Read, Seek, SeekFrom};

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 👁 tmux watcher started for #{tmux_session_name} at offset {initial_offset}"
    );

    let (watcher_provider, watcher_channel_name) =
        parse_provider_and_channel_from_tmux_name(&tmux_session_name).unwrap_or((
            crate::services::provider::ProviderKind::Claude,
            String::new(),
        ));
    let watcher_thread_channel_id =
        super::adk_session::parse_thread_channel_id_from_name(&watcher_channel_name);
    let mut current_offset = initial_offset;
    let input_fifo_path = super::turn_bridge::tmux_runtime_paths(&tmux_session_name).1;
    let mut prompt_too_long_killed = false;
    let mut turn_result_relayed = false;
    let mut last_activity_heartbeat_at: Option<std::time::Instant> = None;
    let mut restored_turn = restored_turn;
    // Guard against duplicate relay: track the offset from which the last relay was sent.
    // If the outer loop circles back and current_offset hasn't advanced past this point,
    // the relay is suppressed.
    // Initialize from persisted inflight state so replacement watcher instances skip
    // already-delivered output (fixes double-reply on stale watcher replacement).
    let mut last_relayed_offset: Option<u64> = {
        if let Some((pk, _)) = parse_provider_and_channel_from_tmux_name(&tmux_session_name) {
            super::inflight::load_inflight_state(&pk, channel_id.get())
                .and_then(|s| s.last_watcher_relayed_offset)
        } else {
            None
        }
    };
    // Rolling-size-cap rotation state. The watcher loop spins predictably
    // (~500ms sleeps) so a mod-N gate on an iteration counter gives a
    // regular-ish cadence for the size check without hitting the fs every
    // spin. See issue #892.
    let mut rotation_tick: u32 = 0;
    const ROTATION_CHECK_EVERY: u32 = 60; // ~30s at 500ms base cadence

    loop {
        // Always consume resume_offset first — the turn bridge may have set it
        // between the previous paused check and now, so reading it here prevents
        // the watcher from using a stale current_offset after unpausing.
        if let Some(new_offset) = resume_offset.lock().ok().and_then(|mut g| g.take()) {
            current_offset = new_offset;
            // If the bridge already delivered the previous turn, treat this resume
            // point as already consumed once so the watcher doesn't re-relay the
            // same batch after unpausing.
            last_relayed_offset = if turn_delivered.load(Ordering::Relaxed) {
                Some(new_offset)
            } else {
                None
            };
            // Clear turn_delivered after preserving the duplicate-relay guard so
            // future turns beyond this resume point can be relayed normally.
            turn_delivered.store(false, Ordering::Relaxed);
        }

        // Check cancel or global shutdown (both exit quietly, no "session ended" message)
        if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
            break;
        }

        // If paused (Discord handler is processing its own turn), wait
        if paused.load(Ordering::Relaxed) {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            continue;
        }

        // Periodic size-cap rotation for the session jsonl. Running this off
        // the watcher loop keeps the wrapper child process simple while
        // still enforcing a 20 MB soft cap (see issue #892).
        rotation_tick = rotation_tick.wrapping_add(1);

        if rotation_tick % ROTATION_CHECK_EVERY == 0 {
            let path = output_path.clone();
            let session = tmux_session_name.clone();
            let prev_offset = current_offset;
            let rotation = tokio::task::spawn_blocking(move || {
                crate::services::tmux_common::truncate_jsonl_head_safe(
                    &path,
                    crate::services::tmux_common::JSONL_SIZE_CAP_BYTES,
                    crate::services::tmux_common::JSONL_TARGET_KEEP_BYTES,
                )
                .map_err(|e| e.to_string())
            })
            .await
            .unwrap_or_else(|e| Err(format!("join error: {e}")));
            match rotation {
                Ok(Some(new_size)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ✂ rotated jsonl for {} — new size {} bytes (was beyond cap)",
                        session,
                        new_size
                    );
                    // File was rewritten from the head: reset reader offset
                    // so the watcher doesn't seek past the new EOF. Also
                    // reset the duplicate-relay guard.
                    if prev_offset > new_size {
                        current_offset = new_size;
                        last_relayed_offset = Some(new_size);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!("  [{ts}] ⚠ jsonl rotation failed for {}: {}", session, e);
                }
            }
        }

        // Snapshot pause epoch — if this changes later, a Discord turn claimed this data
        let epoch_snapshot = pause_epoch.load(Ordering::Relaxed);

        // Check if tmux session is still alive (with timeout to prevent
        // blocking thread pool exhaustion if tmux hangs)
        let alive = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking({
                let name = tmux_session_name.clone();
                move || tmux_session_has_live_pane(&name)
            }),
        )
        .await
        .unwrap_or(Ok(false))
        .unwrap_or(false);

        if !alive {
            // Re-check shutdown/cancel — SIGTERM handler may have set the flag
            // between the top-of-loop check and here
            if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                );
                break;
            }
            // Extra grace: wait briefly and re-check, since SIGTERM handler is async
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                );
                break;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            let diagnostic = build_tmux_death_diagnostic(&tmux_session_name, Some(&output_path));
            if let Some(diag) = diagnostic.as_deref() {
                tracing::info!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping ({diag})"
                );
            } else {
                tracing::info!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping"
                );
            }
            let reason_short = read_tmux_exit_reason(&tmux_session_name);
            let is_normal_completion =
                tmux_death_is_normal_completion(reason_short.as_deref(), diagnostic.as_deref());
            // Notify: tmux session termination with reason
            if !is_normal_completion {
                let reason_short_text = reason_short.as_deref().unwrap_or("unknown");
                let is_force_kill = reason_short_text.contains("force-kill");
                if !is_force_kill {
                    // Strip timestamp prefix if present (format: "[YYYY-MM-DD HH:MM:SS] reason")
                    let reason_text = reason_short_text
                        .strip_prefix('[')
                        .and_then(|s| s.find("] ").map(|i| &s[i + 2..]))
                        .unwrap_or(reason_short_text);
                    let reason_truncated: String = reason_text.chars().take(100).collect();
                    let session_key = super::adk_session::build_adk_session_key(
                        &shared,
                        channel_id,
                        &watcher_provider,
                    )
                    .await
                    .unwrap_or_else(|| {
                        format!(
                            "{}:{}",
                            crate::services::platform::hostname_short(),
                            tmux_session_name
                        )
                    });
                    enqueue_lifecycle_notification_best_effort(
                        sqlite_runtime_db(shared.as_ref()),
                        shared.pg_pool.as_ref(),
                        &format!("channel:{}", channel_id.get()),
                        Some(session_key.as_str()),
                        lifecycle_reason_code_for_tmux_exit(reason_text),
                        &format!("🔴 세션 종료: {reason_truncated}"),
                    );
                }
            } else {
                tracing::info!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended after normal completion, skipping lifecycle notification"
                );
            }
            if !prompt_too_long_killed && !turn_result_relayed {
                // Suppress warning for normal dispatch completion — not an error
                let suppress_restart = is_normal_completion
                    || reason_short
                        .as_deref()
                        .is_some_and(tmux_exit_reason_is_normal_completion);
                if !suppress_restart {
                    let _ = resume_aborted_restart_turn(
                        channel_id,
                        &http,
                        &shared,
                        &tmux_session_name,
                        &output_path,
                    )
                    .await;
                }
            }
            break;
        }

        // Try to read new data from output file
        let read_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking({
                let path = output_path.clone();
                let offset = current_offset;
                move || -> Result<(Vec<u8>, u64), String> {
                    let mut file =
                        std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
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
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                continue;
            }
        };

        if data.is_empty() {
            // No new data, sleep and retry
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            continue;
        }

        // We got new data while not paused — this means terminal input triggered a response
        let data_start_offset = current_offset; // offset where this read batch started
        current_offset = new_offset;
        maybe_refresh_watcher_activity_heartbeat(
            shared.sqlite.as_ref(),
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            &watcher_provider,
            &tmux_session_name,
            watcher_thread_channel_id,
            &mut last_activity_heartbeat_at,
        );

        // Collect the full turn: keep reading until we see a "result" event
        let mut all_data = String::from_utf8_lossy(&data).to_string();
        let mut state = StreamLineState::new();
        let stream_seed = watcher_stream_seed(restored_turn.take());
        let mut full_response = stream_seed.full_response;
        let mut tool_state = WatcherToolState::new();

        // Create a placeholder message for real-time status display
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut spin_idx: usize = 0;
        let mut placeholder_msg_id: Option<serenity::MessageId> = stream_seed.placeholder_msg_id;
        let mut last_edit_text = stream_seed.last_edit_text;
        let mut response_sent_offset = stream_seed.response_sent_offset;
        let finish_mailbox_on_completion = stream_seed.finish_mailbox_on_completion;
        let mut monitor_auto_turn_claimed = false;
        let mut monitor_auto_turn_deferred = false;
        let mut monitor_auto_turn_finished = false;

        // Process any complete lines we already have
        let initial_outcome = process_watcher_lines(
            &mut all_data,
            &mut state,
            &mut full_response,
            &mut tool_state,
        );
        let mut found_result = initial_outcome.found_result;
        let mut is_prompt_too_long = initial_outcome.is_prompt_too_long;
        let mut is_auth_error = initial_outcome.is_auth_error;
        let mut auth_error_message = initial_outcome.auth_error_message;
        let mut is_provider_overloaded = initial_outcome.is_provider_overloaded;
        let mut provider_overload_message = initial_outcome.provider_overload_message;
        let mut stale_resume_detected = initial_outcome.stale_resume_detected;
        let mut task_notification_kind = stream_seed.task_notification_kind;
        if let Some(kind) = initial_outcome.task_notification_kind {
            task_notification_kind = merge_task_notification_kind(task_notification_kind, kind);
        }
        if matches!(
            task_notification_kind,
            Some(TaskNotificationKind::MonitorAutoTurn)
        ) {
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
            if !start.acquired {
                continue;
            }
            ensure_monitor_auto_turn_inflight(
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                &output_path,
                &input_fifo_path,
                state.last_session_id.as_deref(),
                data_start_offset,
                current_offset,
            );
        }

        // Keep reading until result or timeout
        // Check if a Discord turn claimed this data since our epoch snapshot
        let epoch_changed = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let mut was_paused = paused.load(Ordering::Relaxed) || epoch_changed;
        if was_paused && !monitor_auto_turn_deferred {
            // A Discord turn took over — discard what we read
            continue;
        }
        if !found_result {
            let turn_start = tokio::time::Instant::now();
            let turn_timeout = super::turn_watchdog_timeout();
            let mut last_status_update = tokio::time::Instant::now();
            let mut ready_for_input_tracker =
                crate::services::provider::ReadyForInputIdleTracker::default();
            let mut last_ready_probe_at: Option<std::time::Instant> = None;
            let mut ready_for_input_failure_notice: Option<String> = None;

            while !found_result && turn_start.elapsed() < turn_timeout {
                if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
                    break;
                }
                if paused.load(Ordering::Relaxed) {
                    was_paused = true;
                    break;
                }

                let read_more = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    tokio::task::spawn_blocking({
                        let path = output_path.clone();
                        let offset = current_offset;
                        move || -> Result<(Vec<u8>, u64), String> {
                            let mut file =
                                std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
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

                match read_more {
                    Ok(Ok(Ok((chunk, off)))) if !chunk.is_empty() => {
                        current_offset = off;
                        maybe_refresh_watcher_activity_heartbeat(
                            shared.sqlite.as_ref(),
                            shared.pg_pool.as_ref(),
                            &shared.token_hash,
                            &watcher_provider,
                            &tmux_session_name,
                            watcher_thread_channel_id,
                            &mut last_activity_heartbeat_at,
                        );
                        ready_for_input_tracker.record_output();
                        all_data.push_str(&String::from_utf8_lossy(&chunk));
                        let outcome = process_watcher_lines(
                            &mut all_data,
                            &mut state,
                            &mut full_response,
                            &mut tool_state,
                        );
                        found_result = found_result || outcome.found_result;
                        is_prompt_too_long = is_prompt_too_long || outcome.is_prompt_too_long;
                        is_auth_error = is_auth_error || outcome.is_auth_error;
                        if auth_error_message.is_none() {
                            auth_error_message = outcome.auth_error_message;
                        }
                        is_provider_overloaded =
                            is_provider_overloaded || outcome.is_provider_overloaded;
                        stale_resume_detected =
                            stale_resume_detected || outcome.stale_resume_detected;
                        if let Some(kind) = outcome.task_notification_kind {
                            task_notification_kind =
                                merge_task_notification_kind(task_notification_kind, kind);
                        }
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
                                if !start.acquired {
                                    was_paused = true;
                                    break;
                                }
                            }
                            ensure_monitor_auto_turn_inflight(
                                &watcher_provider,
                                channel_id,
                                &tmux_session_name,
                                &output_path,
                                &input_fifo_path,
                                state.last_session_id.as_deref(),
                                data_start_offset,
                                current_offset,
                            );
                        }
                        if provider_overload_message.is_none() {
                            provider_overload_message = outcome.provider_overload_message;
                        }
                        // Notify when auto-compaction is detected in output
                        if outcome.auto_compacted {
                            let target = format!("channel:{}", channel_id.get());
                            let _ = enqueue_outbox_best_effort(
                                shared.pg_pool.as_ref(),
                                sqlite_runtime_db(shared.as_ref()),
                                OutboxMessage {
                                    target: target.as_str(),
                                    content: "🗜️ 자동 컨텍스트 압축 감지",
                                    bot: "notify",
                                    source: "system",
                                    reason_code: None,
                                    session_key: None,
                                },
                            )
                            .await;
                        }
                    }
                    Ok(Ok(Ok((_, off)))) => {
                        current_offset = off;
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        let now = std::time::Instant::now();
                        let should_probe_ready = last_ready_probe_at
                            .map(|last| {
                                now.duration_since(last) >= READY_FOR_INPUT_IDLE_PROBE_INTERVAL
                            })
                            .unwrap_or(true);
                        if should_probe_ready {
                            last_ready_probe_at = Some(now);
                            let ready_for_input = tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                tokio::task::spawn_blocking({
                                    let name = tmux_session_name.clone();
                                    move || {
                                        crate::services::provider::tmux_session_ready_for_input(
                                            &name,
                                        )
                                    }
                                }),
                            )
                            .await
                            .unwrap_or(Ok(false))
                            .unwrap_or(false);
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
                                }
                                crate::services::provider::ReadyForInputIdleState::PostWorkIdleTimeout => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    let dispatch_id = resolve_dispatched_thread_dispatch_from_db(
                                        shared.sqlite.as_ref(),
                                        shared.pg_pool.as_ref(),
                                        watcher_thread_channel_id.unwrap_or_else(|| channel_id.get()),
                                    )
                                    .or_else(|| {
                                        super::inflight::load_inflight_state(
                                            &watcher_provider,
                                            channel_id.get(),
                                        )
                                        .and_then(|state| state.dispatch_id)
                                    });
                                    if let Some(dispatch_id) = dispatch_id {
                                        match fail_dispatch_for_ready_for_input_stall(
                                            &shared,
                                            &dispatch_id,
                                            &tmux_session_name,
                                        )
                                        .await
                                        {
                                            Ok(result) => {
                                                tracing::warn!(
                                                    "  [{ts}] ⚠ watcher marked post-work Ready-for-input stall as failed for {} / dispatch {} (card={:?}, card_marked={}, human_alert_sent={})",
                                                    tmux_session_name,
                                                    dispatch_id,
                                                    result.card_id,
                                                    result.card_marked,
                                                    result.human_alert_sent
                                                );
                                                if let Some(state) = super::inflight::load_inflight_state(
                                                    &watcher_provider,
                                                    channel_id.get(),
                                                )
                                                .filter(|state| !state.rebind_origin)
                                                {
                                                    let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                                                    super::formatting::remove_reaction_raw(
                                                        &http,
                                                        channel_id,
                                                        user_msg_id,
                                                        '⏳',
                                                    )
                                                    .await;
                                                    super::formatting::add_reaction_raw(
                                                        &http,
                                                        channel_id,
                                                        user_msg_id,
                                                        '⚠',
                                                    )
                                                    .await;
                                                }
                                                super::inflight::clear_inflight_state(
                                                    &watcher_provider,
                                                    channel_id.get(),
                                                );
                                                ready_for_input_failure_notice = Some(format!(
                                                    "⚠️ 작업 후 `Ready for input` 상태에서 멈춰 dispatch를 실패 처리했습니다.\n사유: {READY_FOR_INPUT_STUCK_REASON}"
                                                ));
                                            }
                                            Err(error) => {
                                                tracing::warn!(
                                                    "  [{ts}] ⚠ watcher failed to persist Ready-for-input stall failure for {} / dispatch {}: {}",
                                                    tmux_session_name,
                                                    dispatch_id,
                                                    error
                                                );
                                                ready_for_input_failure_notice = Some(format!(
                                                    "⚠️ 작업 후 `Ready for input` 상태에서 멈췄지만 dispatch 실패 처리를 저장하지 못했습니다.\n사유: {}",
                                                    truncate_str(&error, 300)
                                                ));
                                            }
                                        }
                                    } else {
                                        tracing::warn!(
                                            "  [{ts}] ⚠ watcher detected post-work Ready-for-input stall for {} but could not resolve a dispatched task",
                                            tmux_session_name
                                        );
                                        ready_for_input_failure_notice = Some(
                                            "⚠️ 작업 후 `Ready for input` 상태에서 멈췄지만 연결된 dispatch를 찾지 못해 자동 실패 처리하지 못했습니다.".to_string(),
                                        );
                                    }
                                    full_response.clear();
                                    found_result = true;
                                }
                            }
                        }
                    }
                    _ => {
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    }
                }

                // Check for stale session error during streaming — abort relay immediately.
                // Only structured error/result events can trip this flag.
                if stale_resume_detected {
                    break;
                }

                // Update Discord placeholder at configurable interval
                if last_status_update.elapsed() >= super::status_update_interval() {
                    last_status_update = tokio::time::Instant::now();
                    let indicator = SPINNER[spin_idx % SPINNER.len()];
                    spin_idx += 1;

                    loop {
                        let current_portion =
                            full_response.get(response_sent_offset..).unwrap_or("");
                        if current_portion.is_empty() {
                            break;
                        }

                        let status_block = super::formatting::build_placeholder_status_block(
                            indicator,
                            tool_state.prev_tool_status.as_deref(),
                            tool_state.current_tool_line.as_deref(),
                            &full_response,
                        );
                        let Some(msg_id) = placeholder_msg_id else {
                            break;
                        };
                        let Some(plan) = plan_streaming_rollover(current_portion, &status_block)
                        else {
                            break;
                        };

                        rate_limit_wait(&shared, channel_id).await;
                        match channel_id
                            .edit_message(
                                &http,
                                msg_id,
                                serenity::EditMessage::new().content(&plan.frozen_chunk),
                            )
                            .await
                        {
                            Ok(_) => {
                                rate_limit_wait(&shared, channel_id).await;
                                match channel_id
                                    .send_message(
                                        &http,
                                        serenity::CreateMessage::new().content(&status_block),
                                    )
                                    .await
                                {
                                    Ok(message) => {
                                        placeholder_msg_id = Some(message.id);
                                        response_sent_offset += plan.split_at;
                                        last_edit_text = status_block;
                                        persist_watcher_stream_progress(
                                            &watcher_provider,
                                            channel_id,
                                            &tmux_session_name,
                                            placeholder_msg_id,
                                            &full_response,
                                            response_sent_offset,
                                            tool_state.current_tool_line.as_deref(),
                                            tool_state.prev_tool_status.as_deref(),
                                            task_notification_kind,
                                        );
                                    }
                                    Err(error) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ tmux rollover placeholder send failed in channel {}: {}",
                                            channel_id.get(),
                                            error
                                        );
                                        rate_limit_wait(&shared, channel_id).await;
                                        let _ = channel_id
                                            .edit_message(
                                                &http,
                                                msg_id,
                                                serenity::EditMessage::new()
                                                    .content(&plan.display_snapshot),
                                            )
                                            .await;
                                        last_edit_text = plan.display_snapshot;
                                        break;
                                    }
                                }
                            }
                            Err(error) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    "  [{ts}] ⚠ tmux rollover freeze failed for msg {} in channel {}: {}",
                                    msg_id.get(),
                                    channel_id.get(),
                                    error
                                );
                                break;
                            }
                        }
                    }

                    let status_block = super::formatting::build_placeholder_status_block(
                        indicator,
                        tool_state.prev_tool_status.as_deref(),
                        tool_state.current_tool_line.as_deref(),
                        &full_response,
                    );
                    let current_portion = full_response.get(response_sent_offset..).unwrap_or("");
                    let display_text =
                        build_streaming_placeholder_text(current_portion, &status_block);

                    if display_text != last_edit_text {
                        match placeholder_msg_id {
                            Some(msg_id) => {
                                // Edit existing placeholder
                                rate_limit_wait(&shared, channel_id).await;
                                let _ = channel_id
                                    .edit_message(
                                        &http,
                                        msg_id,
                                        serenity::EditMessage::new().content(&display_text),
                                    )
                                    .await;
                            }
                            None => {
                                // Create new placeholder
                                if let Ok(msg) = channel_id.say(&http, &display_text).await {
                                    placeholder_msg_id = Some(msg.id);
                                }
                            }
                        }
                        last_edit_text = display_text;
                        persist_watcher_stream_progress(
                            &watcher_provider,
                            channel_id,
                            &tmux_session_name,
                            placeholder_msg_id,
                            &full_response,
                            response_sent_offset,
                            tool_state.current_tool_line.as_deref(),
                            tool_state.prev_tool_status.as_deref(),
                            task_notification_kind,
                        );
                    }
                }
            }

            if let Some(notice) = ready_for_input_failure_notice {
                match placeholder_msg_id {
                    Some(msg_id) => {
                        rate_limit_wait(&shared, channel_id).await;
                        let _ = channel_id
                            .edit_message(
                                &http,
                                msg_id,
                                serenity::EditMessage::new().content(&notice),
                            )
                            .await;
                    }
                    None => {
                        let _ = channel_id.say(&http, &notice).await;
                    }
                }
                clear_provider_overload_retry_state(channel_id);
                continue;
            }
        }

        // If paused was set while we were reading (even if already unpaused), discard partial data.
        // Also check epoch: if it changed, a Discord turn claimed this data even if paused is now false.
        let paused_now = paused.load(Ordering::Relaxed);
        let epoch_changed_now = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let deferred_monitor_ready =
            monitor_auto_turn_claimed && monitor_auto_turn_deferred && !paused_now;
        if (was_paused || paused_now || epoch_changed_now) && !deferred_monitor_ready {
            // Clean up placeholder if we created one
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id.delete_message(&http, msg_id).await;
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        // Handle prompt-too-long: kill session so next message creates a fresh one
        if is_prompt_too_long {
            clear_provider_overload_retry_state(channel_id);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Prompt too long detected in watcher for {tmux_session_name}, killing session"
            );
            prompt_too_long_killed = true;

            let sess = tmux_session_name.clone();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "prompt_too_long",
                        Some("watcher cleanup: prompt too long"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: prompt too long");
                    crate::services::platform::tmux::kill_session_with_reason(
                        &sess,
                        "watcher cleanup: prompt too long",
                    );
                }),
            )
            .await;

            let notice = "⚠️ 컨텍스트 한도 초과로 세션을 초기화했습니다. 다음 메시지부터 새 세션으로 처리됩니다.";
            match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    let _ = channel_id
                        .edit_message(&http, msg_id, serenity::EditMessage::new().content(notice))
                        .await;
                }
                None => {
                    let _ = channel_id.say(&http, notice).await;
                }
            }
            // Don't break — let the watcher exit naturally when session-alive check fails
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        // Handle auth error: kill session and notify user to re-authenticate
        if is_auth_error {
            clear_provider_overload_retry_state(channel_id);
            let inflight_state =
                super::inflight::load_inflight_state(&watcher_provider, channel_id.get());
            let fallback_session_id = inflight_state
                .as_ref()
                .and_then(|state| state.session_id.as_deref());
            let dispatch_id =
                resolve_watcher_dispatch_id(&shared, channel_id, inflight_state.as_ref()).await;
            let auth_detail = auth_error_message
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("authentication expired");
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Auth error detected in watcher for {tmux_session_name}: {}",
                truncate_str(auth_detail, 300)
            );
            prompt_too_long_killed = true; // reuse flag to suppress duplicate "session ended" message

            clear_provider_session_for_retry(
                &shared,
                channel_id,
                &tmux_session_name,
                fallback_session_id,
            )
            .await;

            let sess = tmux_session_name.clone();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "auth_error",
                        Some("watcher cleanup: authentication failed"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: authentication failed");
                    crate::services::platform::tmux::kill_session_with_reason(
                        &sess,
                        "watcher cleanup: authentication failed",
                    );
                }),
            )
            .await;

            let notice = format!(
                "⚠️ 인증이 만료되어 현재 dispatch를 실패 처리했습니다. 세션을 종료합니다.\n관리자가 CLI에서 재인증(`/login`)을 완료한 후 다시 디스패치해주세요.\n\n사유: {}",
                truncate_str(auth_detail, 300)
            );
            match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    let _ = channel_id
                        .edit_message(&http, msg_id, serenity::EditMessage::new().content(&notice))
                        .await;
                }
                None => {
                    let _ = channel_id.say(&http, &notice).await;
                }
            }
            // #897 round-3 Medium: skip reaction work for `rebind_origin`
            // inflights — their `user_msg_id=0` identifies no real Discord
            // message so issuing reactions against it just produces API
            // errors. The synthetic state was created by
            // `/api/inflight/rebind` to adopt a live tmux session.
            if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin) {
                let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                super::formatting::remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
                super::formatting::add_reaction_raw(&http, channel_id, user_msg_id, '⚠').await;
            }
            super::inflight::clear_inflight_state(&watcher_provider, channel_id.get());
            let failure_text = format!(
                "authentication expired; re-authentication required: {}",
                truncate_str(auth_detail, 300)
            );
            super::turn_bridge::fail_dispatch_with_retry(
                shared.api_port,
                dispatch_id.as_deref(),
                &failure_text,
            )
            .await;
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        if is_provider_overloaded {
            let overload_message = provider_overload_message
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("provider overload detected");
            let inflight_state =
                super::inflight::load_inflight_state(&watcher_provider, channel_id.get());
            let retry_text = inflight_state
                .as_ref()
                .map(|state| state.user_text.clone())
                .filter(|text| !text.trim().is_empty());
            let fallback_session_id = inflight_state
                .as_ref()
                .and_then(|state| state.session_id.as_deref());
            let dispatch_id =
                resolve_watcher_dispatch_id(&shared, channel_id, inflight_state.as_ref()).await;

            let decision = retry_text
                .as_deref()
                .map(|text| record_provider_overload_retry(channel_id, text))
                .unwrap_or(ProviderOverloadDecision::Exhausted);
            let retry_notice = match &decision {
                ProviderOverloadDecision::Retry { attempt, delay, .. } => format!(
                    "⚠️ 모델 capacity 상태를 감지해 세션을 정리했습니다. {}분 후 자동 재시도합니다. ({}/{})",
                    delay.as_secs() / 60,
                    attempt,
                    PROVIDER_OVERLOAD_MAX_RETRIES
                ),
                ProviderOverloadDecision::Exhausted => format!(
                    "⚠️ 모델 capacity 상태가 계속되어 자동 재시도를 중단했습니다. 잠시 후 다시 시도해 주세요.\n\n사유: {}",
                    truncate_str(overload_message, 300)
                ),
            };

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Provider overload detected in watcher for {}: {}",
                tmux_session_name,
                overload_message
            );
            prompt_too_long_killed = true;

            clear_provider_session_for_retry(
                &shared,
                channel_id,
                &tmux_session_name,
                fallback_session_id,
            )
            .await;

            let sess = tmux_session_name.clone();
            let termination_reason = match &decision {
                ProviderOverloadDecision::Retry { .. } => "provider_overload_retry",
                ProviderOverloadDecision::Exhausted => "provider_overload_exhausted",
            };
            let termination_detail = format!("watcher cleanup: {overload_message}");
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        termination_reason,
                        Some(&termination_detail),
                        None,
                    );
                    record_tmux_exit_reason(&sess, &termination_detail);
                    crate::services::platform::tmux::kill_session_with_reason(
                        &sess,
                        &termination_detail,
                    );
                }),
            )
            .await;

            match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    let _ = channel_id
                        .edit_message(
                            &http,
                            msg_id,
                            serenity::EditMessage::new().content(&retry_notice),
                        )
                        .await;
                }
                None => {
                    let _ = channel_id.say(&http, &retry_notice).await;
                }
            }

            // #897 round-3 Medium: skip reaction + retry scheduling for
            // `rebind_origin` inflights — they have no real user message
            // to react against and no real user text to re-prompt.
            if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin) {
                let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                super::formatting::remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
                if matches!(&decision, ProviderOverloadDecision::Exhausted) {
                    super::formatting::add_reaction_raw(&http, channel_id, user_msg_id, '⚠').await;
                }
            }
            super::inflight::clear_inflight_state(&watcher_provider, channel_id.get());

            match decision {
                ProviderOverloadDecision::Retry {
                    attempt,
                    delay,
                    fingerprint,
                } => {
                    if let Some(retry_text) = retry_text {
                        if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin) {
                            schedule_provider_overload_retry(
                                shared.clone(),
                                http.clone(),
                                watcher_provider.clone(),
                                channel_id,
                                serenity::MessageId::new(state.user_msg_id),
                                retry_text,
                                attempt,
                                delay,
                                fingerprint,
                            );
                        } else {
                            clear_provider_overload_retry_state(channel_id);
                        }
                    } else {
                        clear_provider_overload_retry_state(channel_id);
                    }
                }
                ProviderOverloadDecision::Exhausted => {
                    let failure_text = format!(
                        "provider overloaded after {} auto-retries: {}",
                        PROVIDER_OVERLOAD_MAX_RETRIES,
                        truncate_str(overload_message, 300)
                    );
                    super::turn_bridge::fail_dispatch_with_retry(
                        shared.api_port,
                        dispatch_id.as_deref(),
                        &failure_text,
                    )
                    .await;
                }
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        // Final guard: re-check epoch and turn_delivered right before relay.
        // Closes the race window where a Discord turn starts between the epoch check
        // above (line 277) and this relay — the turn_bridge may have already delivered
        // the same response to its own placeholder.
        let paused_now = paused.load(Ordering::Relaxed);
        let epoch_changed_now = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let turn_delivered_now = turn_delivered.load(Ordering::Relaxed);
        let deferred_monitor_ready =
            monitor_auto_turn_claimed && monitor_auto_turn_deferred && !paused_now;
        if (paused_now || epoch_changed_now || turn_delivered_now) && !deferred_monitor_ready {
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id.delete_message(&http, msg_id).await;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Late epoch/delivered guard: suppressed duplicate relay for {}",
                tmux_session_name
            );
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        if watcher_should_yield_to_active_bridge_turn(
            &watcher_provider,
            channel_id,
            &tmux_session_name,
            data_start_offset,
            current_offset,
        ) {
            let matched_reattach = matching_recent_watcher_reattach_offset(
                channel_id,
                &tmux_session_name,
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
                placeholder_msg_id,
                response_sent_offset,
                last_edit_text: &last_edit_text,
                inflight_state: None,
                has_active_turn: false,
                tmux_session_name: &tmux_session_name,
                task_notification_kind: None,
                reattach_offset_match: matched_reattach.is_some(),
            };
            apply_placeholder_suppression(
                &http,
                channel_id,
                &shared,
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
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        // Duplicate-relay guard: if we already relayed from this same data
        // range, suppress. Use strict `<` so output starting exactly at the
        // previous boundary is treated as the next turn rather than a re-read.
        if let Some(prev_offset) = last_relayed_offset {
            if data_start_offset < prev_offset {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 👁 Duplicate relay guard: suppressed re-relay for {} (data_start={}, last_relayed={:?})",
                    tmux_session_name,
                    data_start_offset,
                    last_relayed_offset,
                );
                if let Some(msg_id) = placeholder_msg_id {
                    let _ = channel_id.delete_message(&http, msg_id).await;
                }
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                )
                .await;
                continue;
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
                super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;
            }
            if let Some(ref sid) = stale_sid {
                let _ = super::internal_api::clear_stale_session_id(sid).await;
            }
            crate::services::termination_audit::record_termination_for_tmux(
                &tmux_session_name,
                None,
                "tmux_watcher",
                "stale_resume_retry",
                Some("stale session resume detected — forcing fresh session before auto-retry"),
                None,
            );
            record_tmux_exit_reason(
                &tmux_session_name,
                "stale session resume detected — forcing fresh session before auto-retry",
            );
            crate::services::platform::tmux::kill_session_with_reason(
                &tmux_session_name,
                "stale session resume detected — forcing fresh session before auto-retry",
            );
            // Replace placeholder with recovery notice (don't delete — avoids visual gap)
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id
                    .edit_message(
                        &http,
                        msg_id,
                        serenity::EditMessage::new()
                            .content("↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다."),
                    )
                    .await;
            }
            // Auto-retry: persist Discord history for LLM injection, then queue the
            // original user message as an internal follow-up instead of self-routing
            // through /api/send announce.
            //
            // #897 round-4 Medium: a `rebind_origin` inflight has no real
            // user message or text to retry with (`user_msg_id=0`,
            // user_text="/api/inflight/rebind"), so auto-retry would
            // enqueue a garbage internal follow-up. Skip the retry; the
            // operator is expected to re-invoke `/api/inflight/rebind`
            // once the tmux session is healthy again.
            match super::inflight::load_inflight_state(&watcher_provider, channel_id.get()) {
                Some(state) if state.rebind_origin => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Watcher auto-retry skipped for channel {} — rebind_origin inflight has no user message to retry",
                        channel_id
                    );
                }
                Some(state) => {
                    super::turn_bridge::auto_retry_with_history(
                        &http,
                        &shared,
                        &watcher_provider,
                        channel_id,
                        serenity::MessageId::new(state.user_msg_id),
                        &state.user_text,
                    )
                    .await;
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
            full_response = String::new();
        }

        let has_assistant_response = !full_response.trim().is_empty();
        let current_response = full_response.get(response_sent_offset..).unwrap_or("");
        let has_current_response = !current_response.trim().is_empty();

        // Cross-watcher relay coordination (root-cause fix for duplicate
        // terminal-response emission observed when `claim_or_replace_watcher`
        // replaces a watcher mid-flight and both the outgoing and incoming
        // instance pass their per-instance dedupe guards for the same tmux
        // range). `TmuxRelayCoord` is shared across all watcher instances for
        // the channel (survives handle replacement), so the two atomics below
        // serialize concurrent emissions and carry the confirmed-delivery
        // watermark between instances without touching disk.
        // The local `last_relayed_offset` guard above handles the
        // single-instance case; this coord layer closes the multi-instance
        // duplicate-relay hole.
        let relay_coord = shared.tmux_relay_coord(channel_id);
        let confirmed_end_pre_claim = relay_coord
            .confirmed_end_offset
            .load(std::sync::atomic::Ordering::Acquire);
        // Strict `<` preserves the same "exact boundary = new turn" semantic
        // the local dedupe above uses (see comment at line ~2125 about the
        // `task_notification` auto-trigger writing its tmux output starting
        // at the previous turn's end offset).
        if confirmed_end_pre_claim != 0 && data_start_offset < confirmed_end_pre_claim {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Cross-watcher dedupe: skipped relay for {} (data_start={}, confirmed_end={})",
                tmux_session_name,
                data_start_offset,
                confirmed_end_pre_claim
            );
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id.delete_message(&http, msg_id).await;
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }
        // CAS the emission slot. `0` = free; any non-zero value = a watcher
        // is mid-emission with that start offset. `.max(1)` guarantees the
        // stored value is non-zero even when `data_start_offset == 0`.
        let slot_claim_token = data_start_offset.max(1);
        if relay_coord
            .relay_slot
            .compare_exchange(
                0,
                slot_claim_token,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Cross-watcher serialization: slot busy, skipped relay for {} (data_start={})",
                tmux_session_name,
                data_start_offset
            );
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id.delete_message(&http, msg_id).await;
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }
        // Re-check confirmed_end inside the slot in case another watcher
        // advanced it between our first load and the CAS above.
        let confirmed_end_in_slot = relay_coord
            .confirmed_end_offset
            .load(std::sync::atomic::Ordering::Acquire);
        if confirmed_end_in_slot != 0 && data_start_offset < confirmed_end_in_slot {
            relay_coord
                .relay_slot
                .store(0, std::sync::atomic::Ordering::Release);
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id.delete_message(&http, msg_id).await;
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        // Send the terminal response to Discord.
        let relay_decision =
            terminal_relay_decision(has_assistant_response, task_notification_kind);
        debug_assert!(
            !relay_decision.should_enqueue_notify_outbox,
            "monitor/task-notification watcher relays must not use notify-bot outbox"
        );
        let relay_ok = if relay_decision.should_direct_send {
            let formatted = super::formatting::format_for_discord_with_provider(
                current_response,
                &watcher_provider,
            );
            let relay_text = if relay_decision.should_tag_monitor_origin {
                super::prepend_monitor_auto_turn_origin(&formatted)
            } else {
                formatted
            };
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Relaying terminal response to Discord ({} chars, offset {}, task_notification_kind={})",
                relay_text.len(),
                data_start_offset,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none")
            );
            let mut relay_ok = true;
            let mut direct_send_delivered = false;
            match placeholder_msg_id {
                Some(msg_id) => {
                    if has_current_response {
                        match replace_long_message_raw(
                            &http,
                            channel_id,
                            msg_id,
                            &relay_text,
                            &shared,
                        )
                        .await
                        {
                            Ok(_) => {
                                direct_send_delivered = true;
                            }
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!("  [{ts}] 👁 Failed to relay: {e}");
                                relay_ok = false;
                            }
                        }
                    } else {
                        let _ = channel_id.delete_message(&http, msg_id).await;
                    }
                }
                None => {
                    if has_current_response {
                        match send_long_message_raw(&http, channel_id, &relay_text, &shared).await {
                            Ok(_) => {
                                direct_send_delivered = true;
                            }
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!("  [{ts}] 👁 Failed to relay: {e}");
                                relay_ok = false;
                            }
                        }
                    }
                }
            }
            if relay_ok {
                if direct_send_delivered || !has_current_response {
                    last_relayed_offset = Some(data_start_offset);
                    if let Some((pk, _)) =
                        parse_provider_and_channel_from_tmux_name(&tmux_session_name)
                    {
                        if let Some(mut inflight) =
                            super::inflight::load_inflight_state(&pk, channel_id.get())
                        {
                            inflight.last_watcher_relayed_offset = Some(data_start_offset);
                            let _ = super::inflight::save_inflight_state(&inflight);
                        }
                    }
                }
                clear_provider_overload_retry_state(channel_id);
            }
            relay_ok
        } else if relay_decision.suppressed {
            if matches!(
                task_notification_kind,
                Some(TaskNotificationKind::MonitorAutoTurn)
            ) {
                let _ = enqueue_monitor_auto_turn_suppressed_notification(
                    shared.pg_pool.as_ref(),
                    sqlite_runtime_db(shared.as_ref()),
                    channel_id,
                    &tmux_session_name,
                    data_start_offset,
                    tool_state.transcript_events.len(),
                );
            }
            let task_notification_detail = format!(
                "{} kind={} offset={}",
                tmux_session_name,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none"),
                data_start_offset,
            );
            let ctx = PlaceholderSuppressContext {
                origin: PlaceholderSuppressOrigin::TaskNotificationTerminal,
                placeholder_msg_id,
                response_sent_offset,
                last_edit_text: &last_edit_text,
                inflight_state: None,
                has_active_turn: false,
                tmux_session_name: &tmux_session_name,
                task_notification_kind,
                reattach_offset_match: false,
            };
            apply_placeholder_suppression(
                &http,
                channel_id,
                &shared,
                placeholder_msg_id,
                ctx.origin,
                decide_placeholder_suppression(&ctx),
                Some(&task_notification_detail),
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Suppressed task-notification relay for {} (kind={}, offset {})",
                tmux_session_name,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none"),
                data_start_offset
            );
            clear_provider_overload_retry_state(channel_id);
            false
        } else {
            if let Some(msg_id) = placeholder_msg_id {
                // No response text but placeholder exists — clean up
                let _ = channel_id.delete_message(&http, msg_id).await;
            }
            false
        };
        let relay_suppressed = relay_decision.suppressed;

        // Advance the shared confirmed-delivery watermark on any committed
        // direct emission or empty-turn cleanup. CAS loop ensures we only ever move the
        // watermark FORWARD, even if some other instance has raced ahead.
        if relay_ok || relay_suppressed {
            let mut cur = relay_coord
                .confirmed_end_offset
                .load(std::sync::atomic::Ordering::Acquire);
            while cur < current_offset {
                match relay_coord.confirmed_end_offset.compare_exchange(
                    cur,
                    current_offset,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(observed) => cur = observed,
                }
            }
            let confirmed_end = relay_coord
                .confirmed_end_offset
                .load(std::sync::atomic::Ordering::Acquire);
            let confirmed_reached_current = confirmed_end >= current_offset;
            record_watcher_invariant(
                confirmed_reached_current,
                Some(&watcher_provider),
                channel_id,
                "tmux_confirmed_end_monotonic",
                "src/services/discord/tmux.rs:tmux_output_watcher_confirmed_end",
                "watcher confirmed_end_offset must reach the committed tmux output end",
                serde_json::json!({
                    "current_offset": current_offset,
                    "confirmed_end": confirmed_end,
                    "tmux_session_name": tmux_session_name.as_str(),
                }),
            );
            debug_assert!(
                confirmed_reached_current,
                "watcher confirmed_end_offset must reach committed output end"
            );
        }
        // Release the emission slot regardless of success. If delivery failed
        // the local `last_relayed_offset` also stayed put, so the same watcher
        // (or its replacement) can retry on the next tick without fighting
        // the slot.
        relay_coord
            .relay_slot
            .store(0, std::sync::atomic::Ordering::Release);

        finish_monitor_auto_turn_if_claimed(
            &shared,
            &watcher_provider,
            channel_id,
            &mut monitor_auto_turn_claimed,
            &mut monitor_auto_turn_finished,
        )
        .await;

        let provider_kind = watcher_provider.clone();
        let inflight_state = super::inflight::load_inflight_state(&provider_kind, channel_id.get());
        let watcher_session_id = state.last_session_id.clone();
        let result_usage = stream_line_state_token_usage(&state);
        if inflight_state.is_none() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: inflight state missing for channel {} — using DB dispatch fallback",
                channel_id.get()
            );
        }

        // Mark user message as completed: ⏳ → ✅ when inflight metadata is
        // available. #897 round-3 Medium: skip the reaction + transcript +
        // analytics block entirely for `rebind_origin` inflights. Their
        // `user_msg_id=0` points at no real message, and persisting a
        // transcript with `turn_id=discord:<channel>:0` poisons
        // session_transcripts / turn_analytics. The notify-bot outbox
        // enqueue above already delivered the recovered response to the
        // user; nothing else on the success path is legitimate here.
        if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin) {
            let user_msg_id = serenity::MessageId::new(state.user_msg_id);
            super::formatting::remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
            super::formatting::add_reaction_raw(&http, channel_id, user_msg_id, '✅').await;

            if has_assistant_response && (shared.sqlite.is_some() || shared.pg_pool.is_some()) {
                let turn_id = format!("discord:{}:{}", channel_id.get(), state.user_msg_id);
                let channel_id_text = channel_id.get().to_string();
                let resolved_did = inflight_state
                    .as_ref()
                    .and_then(|s| s.dispatch_id.clone())
                    .or_else(|| super::adk_session::parse_dispatch_id(&state.user_text))
                    .or(super::adk_session::lookup_pending_dispatch_for_thread(
                        shared.api_port,
                        channel_id.get(),
                    )
                    .await)
                    .or_else(|| {
                        resolve_dispatched_thread_dispatch_from_db(
                            shared.sqlite.as_ref(),
                            shared.pg_pool.as_ref(),
                            channel_id.get(),
                        )
                    });
                if let Err(e) = crate::db::session_transcripts::persist_turn_db(
                    shared.sqlite.as_ref(),
                    shared.pg_pool.as_ref(),
                    crate::db::session_transcripts::PersistSessionTranscript {
                        turn_id: &turn_id,
                        session_key: state.session_key.as_deref(),
                        channel_id: Some(channel_id_text.as_str()),
                        agent_id: resolve_role_binding(channel_id, state.channel_name.as_deref())
                            .as_ref()
                            .map(|binding| binding.role_id.as_str()),
                        provider: Some(provider_kind.as_str()),
                        dispatch_id: resolved_did.as_deref().or(state.dispatch_id.as_deref()),
                        user_message: &state.user_text,
                        assistant_message: &full_response,
                        events: &tool_state.transcript_events,
                        duration_ms: inflight_duration_ms(Some(state.started_at.as_str())),
                    },
                )
                .await
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!("  [{ts}] ⚠ watcher: failed to persist session transcript: {e}");
                }

                super::turn_bridge::persist_turn_analytics_row_with_handles(
                    shared.sqlite.as_ref(),
                    shared.pg_pool.as_ref(),
                    &provider_kind,
                    channel_id,
                    user_msg_id,
                    resolve_role_binding(channel_id, state.channel_name.as_deref()).as_ref(),
                    resolved_did.as_deref().or(state.dispatch_id.as_deref()),
                    state.session_key.as_deref(),
                    watcher_session_id
                        .as_deref()
                        .or(state.session_id.as_deref()),
                    state,
                    result_usage.unwrap_or_default(),
                    inflight_duration_ms(Some(state.started_at.as_str())).unwrap_or(0),
                );
            }
        }

        let resolved_did = inflight_state
            .as_ref()
            .and_then(|state| state.dispatch_id.clone())
            .or_else(|| {
                inflight_state
                    .as_ref()
                    .and_then(|state| super::adk_session::parse_dispatch_id(&state.user_text))
            })
            .or(super::adk_session::lookup_pending_dispatch_for_thread(
                shared.api_port,
                channel_id.get(),
            )
            .await)
            .or_else(|| {
                resolve_dispatched_thread_dispatch_from_db(
                    shared.sqlite.as_ref(),
                    shared.pg_pool.as_ref(),
                    channel_id.get(),
                )
            });

        if resolved_did.is_none() && has_assistant_response {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: no dispatch id resolved for channel {} after terminal success",
                channel_id.get()
            );
        }
        let current_worktree_path = {
            let mut data = shared.core.lock().await;
            data.sessions
                .get_mut(&channel_id)
                .and_then(|session| session.validated_path(channel_id.get()))
        };

        let dispatch_ok = if let Some(did) = resolved_did.as_deref() {
            let dispatch_type = super::internal_api::lookup_dispatch_type(did)
                .await
                .ok()
                .flatten();

            match dispatch_type.as_deref() {
                Some("implementation") | Some("rework") => {
                    if !has_assistant_response {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ watcher: refusing to complete work dispatch {did} without assistant response"
                        );
                        false
                    } else if let (Some(db), Some(engine)) = (&shared.sqlite, &shared.engine) {
                        let mut work_completion_context =
                            super::turn_bridge::build_work_dispatch_completion_result(
                                shared.sqlite.as_ref(),
                                shared.pg_pool.as_ref(),
                                did,
                                "watcher_completed",
                                false,
                                current_worktree_path.as_deref(),
                                Some(&full_response),
                            );
                        if let Some(obj) = work_completion_context.as_object_mut() {
                            obj.insert(
                                "agent_response_present".to_string(),
                                serde_json::Value::Bool(true),
                            );
                        }
                        match crate::dispatch::finalize_dispatch(
                            db,
                            engine,
                            did,
                            "watcher_completed",
                            Some(&work_completion_context),
                        ) {
                            Ok(_) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] ✓ watcher: completed dispatch {did} via finalize_dispatch"
                                );
                                let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                    Some(db),
                                    shared.pg_pool.as_ref(),
                                    did,
                                    "watcher_completed",
                                )
                                .await;
                                true
                            }
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    "  [{ts}] ⚠ watcher: finalize_dispatch failed for {did}: {e}"
                                );
                                let mut fallback_result =
                                    super::turn_bridge::build_work_dispatch_completion_result(
                                        shared.sqlite.as_ref(),
                                        shared.pg_pool.as_ref(),
                                        did,
                                        "watcher_db_fallback",
                                        true,
                                        current_worktree_path.as_deref(),
                                        Some(&full_response),
                                    );
                                if let Some(obj) = fallback_result.as_object_mut() {
                                    obj.insert(
                                        "agent_response_present".to_string(),
                                        serde_json::Value::Bool(true),
                                    );
                                }
                                let completed =
                                    super::turn_bridge::runtime_db_fallback_complete_with_result(
                                        did,
                                        &fallback_result,
                                    );
                                if completed {
                                    let _ =
                                        super::turn_bridge::queue_dispatch_followup_with_handles(
                                            shared.sqlite.as_ref(),
                                            shared.pg_pool.as_ref(),
                                            did,
                                            "watcher_completed_fallback",
                                        )
                                        .await;
                                }
                                completed
                            }
                        }
                    } else {
                        let mut fallback_result =
                            super::turn_bridge::build_work_dispatch_completion_result(
                                shared.sqlite.as_ref(),
                                shared.pg_pool.as_ref(),
                                did,
                                "watcher_db_fallback",
                                true,
                                current_worktree_path.as_deref(),
                                Some(&full_response),
                            );
                        if let Some(obj) = fallback_result.as_object_mut() {
                            obj.insert(
                                "agent_response_present".to_string(),
                                serde_json::Value::Bool(true),
                            );
                        }
                        let completed =
                            super::turn_bridge::runtime_db_fallback_complete_with_result(
                                did,
                                &fallback_result,
                            );
                        if completed {
                            let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                shared.sqlite.as_ref(),
                                shared.pg_pool.as_ref(),
                                did,
                                "watcher_completed_runtime_fallback",
                            )
                            .await;
                        }
                        completed
                    }
                }
                Some(_) => {
                    // Non-work dispatches — leave for their own completion flow
                    true
                }
                None => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ watcher: cannot determine dispatch type for {did} — preserving state"
                    );
                    false
                }
            }
        } else {
            true
        };

        // #225 P1-2: Only mark relayed + clear inflight if Discord relay succeeded.
        // If relay failed, preserve retry/handoff path for next startup.
        if relay_ok {
            if has_assistant_response
                && let Some(state) = inflight_state.as_ref().filter(|state| !state.rebind_origin)
            {
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    if !session.cleared {
                        session.history.push(crate::ui::ai_screen::HistoryItem {
                            item_type: crate::ui::ai_screen::HistoryType::User,
                            content: state.user_text.clone(),
                        });
                        session.history.push(crate::ui::ai_screen::HistoryItem {
                            item_type: crate::ui::ai_screen::HistoryType::Assistant,
                            content: full_response.clone(),
                        });
                    }
                }
                drop(data);
            }
            turn_result_relayed = true;
            if dispatch_ok {
                super::inflight::clear_inflight_state(&provider_kind, channel_id.get());
                finish_restored_watcher_active_turn(
                    &shared,
                    &provider_kind,
                    channel_id,
                    finish_mailbox_on_completion,
                    "restored watcher completed with queued backlog",
                )
                .await;
            }
            let mailbox = shared.mailbox(channel_id);
            let has_active_turn = mailbox.has_active_turn().await;
            let should_kickoff_queue =
                if finish_mailbox_on_completion || monitor_auto_turn_finished || has_active_turn {
                    false
                } else {
                    mailbox
                        .has_pending_soft_queue(super::queue_persistence_context(
                            &shared,
                            &provider_kind,
                            channel_id,
                        ))
                        .await
                        .has_pending
                };
            if dispatch_ok && should_kickoff_queue {
                super::schedule_deferred_idle_queue_kickoff(
                    shared.clone(),
                    provider_kind.clone(),
                    channel_id,
                    "watcher completed with queued backlog",
                );
            }
        } else if !relay_suppressed {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ watcher: relay failed — preserving inflight for retry");
        }

        let missing_inflight_plan = missing_inflight_fallback_plan(
            inflight_state.is_none(),
            resolved_did.is_some(),
            relay_ok || relay_suppressed,
        );
        if missing_inflight_plan.trigger_reattach {
            if wait_for_reacquired_turn_bridge_inflight_state(
                &provider_kind,
                channel_id,
                &tmux_session_name,
                MISSING_INFLIGHT_REATTACH_GRACE_ATTEMPTS,
                MISSING_INFLIGHT_REATTACH_GRACE_DELAY,
            )
            .await
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ watcher: explicit reattach skipped for channel {} — turn bridge reacquired inflight state during grace window",
                    channel_id.get()
                );
            } else {
                trigger_missing_inflight_reattach(
                    &http,
                    &shared,
                    &provider_kind,
                    channel_id,
                    &tmux_session_name,
                );
            }
        }

        // Update session tokens from result event and auto-compact if threshold exceeded
        if let Some(tokens) = result_usage.map(|usage| usage.total_input_tokens()) {
            let provider = shared.settings.read().await.provider.clone();
            let session_key =
                super::adk_session::build_adk_session_key(&shared, channel_id, &provider).await;
            let channel_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let thread_channel_id = channel_name
                .as_deref()
                .and_then(super::adk_session::parse_thread_channel_id_from_name);
            let agent_id = resolve_role_binding(channel_id, channel_name.as_deref())
                .map(|binding| binding.role_id);
            super::adk_session::post_adk_session_status(
                session_key.as_deref(),
                channel_name.as_deref(),
                None,
                "idle",
                &provider,
                None,
                Some(tokens),
                None,
                None,
                thread_channel_id,
                agent_id.as_deref(),
                shared.api_port,
            )
            .await;

            let ctx_cfg = super::adk_session::fetch_context_thresholds(shared.api_port).await;
            let pct = (tokens * 100) / ctx_cfg.context_window.max(1);
            // #227: Re-enabled with 5-min cooldown (matches turn_bridge path).
            // Without cooldown, the compact turn's own result could re-trigger compact.
            let cooldown_key = format!("auto_compact_cooldown:{}", channel_id.get());
            let cooldown_value = match super::internal_api::get_kv_value(&cooldown_key) {
                Ok(value) => value,
                Err(_) => {
                    if let Some(pg_pool) = shared.pg_pool.as_ref() {
                        sqlx::query_scalar::<_, Option<String>>(
                            "SELECT value
                             FROM kv_meta
                             WHERE key = $1
                               AND (expires_at IS NULL OR expires_at > NOW())
                             LIMIT 1",
                        )
                        .bind(&cooldown_key)
                        .fetch_optional(pg_pool)
                        .await
                        .ok()
                        .flatten()
                        .flatten()
                    } else {
                        sqlite_runtime_db(shared.as_ref()).and_then(|db| {
                            db.lock().ok().and_then(|conn| {
                                conn.query_row(
                                    "SELECT value FROM kv_meta WHERE key = ?1",
                                    [&cooldown_key],
                                    |row| row.get(0),
                                )
                                .ok()
                            })
                        })
                    }
                }
            };
            let compact_cooldown_ok =
                cooldown_value
                    .and_then(|v| v.parse::<i64>().ok())
                    .map_or(true, |ts| {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        now - ts > 300 // 5 min cooldown
                    });
            // DISABLED — token counting still unreliable
            if false && pct >= ctx_cfg.compact_pct && !is_prompt_too_long && compact_cooldown_ok {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚡ [watcher] Auto-compact: {} at {pct}% ({tokens} tokens)",
                    tmux_session_name
                );
                let name = tmux_session_name.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    crate::services::platform::tmux::send_keys(&name, &["/compact", "Enter"])
                })
                .await;
                // Set cooldown timestamp
                let cooldown_key = format!("auto_compact_cooldown:{}", channel_id.get());
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let now_text = now.to_string();
                if super::internal_api::set_kv_value(&cooldown_key, &now_text).is_err() {
                    if let Some(pg_pool) = shared.pg_pool.as_ref() {
                        let _ = sqlx::query(
                            "INSERT INTO kv_meta (key, value, expires_at)
                             VALUES ($1, $2, NULL)
                             ON CONFLICT (key) DO UPDATE
                             SET value = EXCLUDED.value,
                                 expires_at = EXCLUDED.expires_at",
                        )
                        .bind(&cooldown_key)
                        .bind(&now_text)
                        .execute(pg_pool)
                        .await;
                    } else if let Some(db) = sqlite_runtime_db(shared.as_ref())
                        && let Ok(conn) = db.lock()
                    {
                        conn.execute(
                            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                            [cooldown_key.as_str(), now_text.as_str()],
                        )
                        .ok();
                    }
                }
                // Notify: auto-compact triggered
                let target = format!("channel:{}", channel_id.get());
                let content = format!("🗜️ 자동 컨텍스트 압축 (사용률: {pct}%)");
                let _ = enqueue_outbox_best_effort(
                    shared.pg_pool.as_ref(),
                    sqlite_runtime_db(shared.as_ref()),
                    OutboxMessage {
                        target: target.as_str(),
                        content: content.as_str(),
                        bot: "notify",
                        source: "system",
                        reason_code: None,
                        session_key: None,
                    },
                )
                .await;
            }
        }
    }

    // Cleanup: only remove from DashMap if we weren't cancelled/replaced.
    // #243: When a watcher is cancelled (replaced by a new watcher or shutdown),
    // the replacement already occupies the slot — removing would delete the new entry.
    if !cancel.load(Ordering::Relaxed) {
        shared.tmux_watchers.remove(&channel_id);
    }

    let api_port = shared.api_port;
    let provider = shared.settings.read().await.provider.clone();
    let session_key =
        super::adk_session::build_adk_session_key(&shared, channel_id, &provider).await;
    let channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.clone())
    };
    let dispatch_protection = super::tmux_lifecycle::resolve_dispatch_tmux_protection(
        shared.sqlite.as_ref(),
        shared.pg_pool.as_ref(),
        &shared.token_hash,
        &provider,
        &tmux_session_name,
        channel_name.as_deref(),
    );
    let cleanup_plan = dead_session_cleanup_plan(dispatch_protection.is_some());

    if let Some(protection) = dispatch_protection {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ tmux watcher: preserving dispatch session {} — {}",
            tmux_session_name,
            protection.log_reason()
        );
    }

    if !cleanup_plan.preserve_tmux_session {
        // Kill dead tmux session to prevent accumulation (especially for thread sessions
        // which are created per-dispatch and would otherwise linger for 24h).
        // #145: skip kill for unified-thread sessions with active auto-queue runs.
        {
            let sess = tmux_session_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if tmux_session_exists(&sess) && !tmux_session_has_live_pane(&sess) {
                    // Check if this is a unified-thread session before killing
                    if let Some((_, ch_name)) =
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(&sess)
                    {
                        if crate::dispatch::is_unified_thread_channel_name_active(&ch_name) {
                            return;
                        }
                    }
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "dead_after_turn",
                        Some("watcher cleanup: dead session after turn"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: dead session after turn");
                    crate::services::platform::tmux::kill_session_with_reason(
                        &sess,
                        "watcher cleanup: dead session after turn",
                    );
                }
            })
            .await;
        }
    }

    if cleanup_plan.report_idle_status {
        // Report idle status to DB so the dashboard doesn't show stale "working" state.
        // Always report idle when the watcher exits, even if dispatch protection
        // keeps the dead tmux session around for the active-dispatch safety path.
        let thread_channel_id = channel_name
            .as_deref()
            .and_then(super::adk_session::parse_thread_channel_id_from_name);
        let agent_id = resolve_role_binding(channel_id, channel_name.as_deref())
            .map(|binding| binding.role_id);
        super::adk_session::post_adk_session_status(
            session_key.as_deref(),
            channel_name.as_deref(),
            None, // model
            "idle",
            &provider,
            None, // session_info
            None, // tokens
            None, // cwd
            None, // dispatch_id
            thread_channel_id,
            agent_id.as_deref(),
            api_port,
        )
        .await;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] 👁 tmux watcher stopped for #{tmux_session_name}");
}

/// Tracks tool/thinking status during watcher output processing.
pub(super) struct WatcherToolState {
    /// Current tool status line (e.g. "⚙ Bash: `ls`")
    pub current_tool_line: Option<String>,
    /// Previous distinct tool/thinking status for 2-line trail rendering.
    pub prev_tool_status: Option<String>,
    /// Accumulated thinking text from streaming deltas
    pub thinking_buffer: String,
    /// Whether we are currently inside a thinking block
    pub in_thinking: bool,
    /// Whether any tool_use block has been seen in this turn
    pub any_tool_used: bool,
    /// Whether a text block was streamed after the last tool_use
    pub has_post_tool_text: bool,
    /// Structured transcript events collected during watcher replay
    pub transcript_events: Vec<SessionTranscriptEvent>,
}

impl WatcherToolState {
    pub fn new() -> Self {
        Self {
            current_tool_line: None,
            prev_tool_status: None,
            thinking_buffer: String::new(),
            in_thinking: false,
            any_tool_used: false,
            has_post_tool_text: false,
            transcript_events: Vec::new(),
        }
    }

    fn set_current_tool_line(&mut self, next_tool_line: Option<String>) {
        let current_tool_line = self.current_tool_line.clone();
        super::formatting::preserve_previous_tool_status(
            &mut self.prev_tool_status,
            current_tool_line.as_deref(),
            next_tool_line.as_deref(),
        );
        self.current_tool_line = next_tool_line;
    }

    fn clear_current_tool_line(&mut self) {
        let current_tool_line = self.current_tool_line.clone();
        super::formatting::preserve_previous_tool_status(
            &mut self.prev_tool_status,
            current_tool_line.as_deref(),
            None,
        );
        self.current_tool_line = None;
    }
}

/// Process buffered lines for the tmux watcher.
/// Extracts text content, tracks tool status, and detects result events.
/// Returns true if a "result" event was found.
pub(super) fn process_watcher_lines(
    buffer: &mut String,
    state: &mut StreamLineState,
    full_response: &mut String,
    tool_state: &mut WatcherToolState,
) -> WatcherLineOutcome {
    let mut outcome = WatcherLineOutcome::default();

    while let Some(pos) = buffer.find('\n') {
        let line: String = buffer.drain(..=pos).collect();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse the JSON line
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            observe_stream_context(&val, state);
            let event_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match event_type {
                "assistant" => {
                    if let Some(message) = val.get("message") {
                        if let Some(model) = message.get("model").and_then(|value| value.as_str()) {
                            state.last_model = Some(model.to_string());
                        }
                        if let Some(usage) = message.get("usage") {
                            state.accum_input_tokens = state.accum_input_tokens.saturating_add(
                                usage
                                    .get("input_tokens")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0),
                            );
                            state.accum_cache_read_tokens =
                                state.accum_cache_read_tokens.saturating_add(
                                    usage
                                        .get("cache_read_input_tokens")
                                        .and_then(|value| value.as_u64())
                                        .unwrap_or(0),
                                );
                            state.accum_cache_create_tokens =
                                state.accum_cache_create_tokens.saturating_add(
                                    usage
                                        .get("cache_creation_input_tokens")
                                        .and_then(|value| value.as_u64())
                                        .unwrap_or(0),
                                );
                            state.accum_output_tokens = state.accum_output_tokens.saturating_add(
                                usage
                                    .get("output_tokens")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0),
                            );
                        }
                        // Text content from assistant message
                        if let Some(content) = message.get("content") {
                            if let Some(arr) = content.as_array() {
                                for block in arr {
                                    let block_type = block.get("type").and_then(|t| t.as_str());
                                    if block_type == Some("text") {
                                        if let Some(text) =
                                            block.get("text").and_then(|t| t.as_str())
                                        {
                                            full_response.push_str(text);
                                            push_transcript_event(
                                                &mut tool_state.transcript_events,
                                                SessionTranscriptEvent {
                                                    kind: SessionTranscriptEventKind::Assistant,
                                                    tool_name: None,
                                                    summary: None,
                                                    content: text.to_string(),
                                                    status: Some("success".to_string()),
                                                    is_error: false,
                                                },
                                            );
                                            if tool_state.any_tool_used {
                                                tool_state.has_post_tool_text = true;
                                            }
                                            tool_state.clear_current_tool_line();
                                        }
                                    } else if block_type == Some("tool_use") {
                                        tool_state.any_tool_used = true;
                                        tool_state.has_post_tool_text = false;
                                        let name = block
                                            .get("name")
                                            .and_then(|n| n.as_str())
                                            .unwrap_or("Tool");
                                        let input_str = block
                                            .get("input")
                                            .map(|i| i.to_string())
                                            .unwrap_or_default();
                                        let summary = format_tool_input(name, &input_str);
                                        let display = if summary.is_empty() {
                                            format!("⚙ {}", name)
                                        } else {
                                            let truncated: String =
                                                summary.chars().take(500).collect();
                                            format!("⚙ {}: {}", name, truncated)
                                        };
                                        tool_state.set_current_tool_line(Some(display));
                                        push_transcript_event(
                                            &mut tool_state.transcript_events,
                                            SessionTranscriptEvent {
                                                kind: SessionTranscriptEventKind::ToolUse,
                                                tool_name: Some(name.to_string()),
                                                summary: (!summary.is_empty()).then_some(summary),
                                                content: input_str,
                                                status: Some("running".to_string()),
                                                is_error: false,
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                "content_block_start" => {
                    if let Some(cb) = val.get("content_block") {
                        let cb_type = cb.get("type").and_then(|t| t.as_str());
                        if cb_type == Some("thinking") {
                            tool_state.in_thinking = true;
                            tool_state.thinking_buffer.clear();
                            tool_state.set_current_tool_line(Some("💭 Thinking...".to_string()));
                        } else if cb_type == Some("tool_use") {
                            tool_state.any_tool_used = true;
                            tool_state.has_post_tool_text = false;
                            let name = cb.get("name").and_then(|n| n.as_str()).unwrap_or("Tool");
                            tool_state.set_current_tool_line(Some(format!("⚙ {}", name)));
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = val.get("delta") {
                        if let Some(thinking) = delta.get("thinking").and_then(|t| t.as_str()) {
                            // Accumulate thinking text and update display
                            tool_state.thinking_buffer.push_str(thinking);
                            let display = tool_state.thinking_buffer.trim().to_string();
                            if !display.is_empty() {
                                tool_state.set_current_tool_line(Some(format!("💭 {display}")));
                            }
                        } else if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            full_response.push_str(text);
                            if tool_state.any_tool_used {
                                tool_state.has_post_tool_text = true;
                            }
                            tool_state.clear_current_tool_line();
                        }
                    }
                }
                "content_block_stop" => {
                    if tool_state.in_thinking {
                        // Thinking block completed — show full text
                        tool_state.in_thinking = false;
                        let display = tool_state.thinking_buffer.trim().to_string();
                        if !display.is_empty() {
                            tool_state.set_current_tool_line(Some(format!("💭 {display}")));
                            push_transcript_event(
                                &mut tool_state.transcript_events,
                                SessionTranscriptEvent {
                                    kind: SessionTranscriptEventKind::Thinking,
                                    tool_name: None,
                                    summary: Some(truncate_str(&display, 120).to_string()),
                                    content: display,
                                    status: Some("info".to_string()),
                                    is_error: false,
                                },
                            );
                        }
                    } else if let Some(line) = tool_state.current_tool_line.clone() {
                        // Tool completed — mark with checkmark
                        if line.starts_with("⚙") {
                            tool_state.set_current_tool_line(Some(line.replacen("⚙", "✓", 1)));
                        }
                    }
                }
                "result" => {
                    outcome.stale_resume_detected = outcome.stale_resume_detected
                        || super::turn_bridge::result_event_has_stale_resume_error(&val);
                    if let Some(session_id) = val.get("session_id").and_then(|value| value.as_str())
                    {
                        state.last_session_id = Some(session_id.to_string());
                    }
                    let is_error = val
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let result_str = extract_result_error_text(&val);
                    push_transcript_event(
                        &mut tool_state.transcript_events,
                        SessionTranscriptEvent {
                            kind: if is_error {
                                SessionTranscriptEventKind::Error
                            } else {
                                SessionTranscriptEventKind::Result
                            },
                            tool_name: None,
                            summary: Some(if result_str.trim().is_empty() {
                                if is_error {
                                    "error".to_string()
                                } else {
                                    "completed".to_string()
                                }
                            } else {
                                truncate_str(&result_str, 120).to_string()
                            }),
                            content: result_str.clone(),
                            status: Some(if is_error { "error" } else { "success" }.to_string()),
                            is_error,
                        },
                    );

                    if is_error {
                        if is_prompt_too_long_message(&result_str) {
                            outcome.is_prompt_too_long = true;
                        }
                        if is_auth_error_message(&result_str) {
                            outcome.is_auth_error = true;
                            outcome.auth_error_message.get_or_insert(result_str.clone());
                        }
                        if let Some(message) = detect_provider_overload_message(&result_str) {
                            outcome.is_provider_overloaded = true;
                            outcome.provider_overload_message.get_or_insert(message);
                        }
                    }

                    // Use result text when streaming didn't capture the final response:
                    // 1. full_response is empty — no text was streamed at all
                    // 2. tools were used but no text was streamed after the last tool
                    //    (accumulated text is stale pre-tool narration)
                    if !outcome.is_prompt_too_long
                        && !outcome.is_auth_error
                        && !outcome.is_provider_overloaded
                        && !result_str.is_empty()
                    {
                        if full_response.is_empty()
                            || (tool_state.any_tool_used && !tool_state.has_post_tool_text)
                        {
                            full_response.clear();
                            full_response.push_str(&result_str);
                        }
                    }
                    if let Some(usage) = val.get("usage") {
                        state.accum_input_tokens = usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        state.accum_cache_read_tokens = usage
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        state.accum_cache_create_tokens = usage
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        state.accum_output_tokens = usage
                            .get("output_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }

                    state.final_result = Some(String::new());
                    outcome.found_result = true;
                }
                "system" => {
                    if val.get("subtype").and_then(|s| s.as_str()) == Some("init")
                        && let Some(session_id) =
                            val.get("session_id").and_then(|value| value.as_str())
                    {
                        state.last_session_id = Some(session_id.to_string());
                    }
                    // Detect auto-compaction events from Claude Code
                    if let Some(msg) = val.get("message").and_then(|m| m.as_str()) {
                        let lower = msg.to_ascii_lowercase();
                        if lower.contains("compacted")
                            || lower.contains("auto-compact")
                            || lower.contains("conversation has been compressed")
                        {
                            outcome.auto_compacted = true;
                        }
                    }
                    if let Some(subtype) = val.get("subtype").and_then(|s| s.as_str()) {
                        if subtype == "compact" || subtype == "auto_compact" {
                            outcome.auto_compacted = true;
                        }
                        // `task_notification` is the authoritative
                        // provider-normalized marker for a background-trigger
                        // turn (Claude emits it directly; Codex normalizes
                        // `background_event` into the same JSONL shape). It
                        // lets us distinguish a background-trigger turn from
                        // a normal foreground turn whose inflight file was
                        // merely cleared early by turn_bridge.
                        if subtype == "task_notification" {
                            outcome.task_notification_kind = merge_task_notification_kind(
                                outcome.task_notification_kind,
                                classify_task_notification_kind(&val, state),
                            );
                        }
                    }
                }
                _ => {}
            }
        } else if is_auth_error_message(trimmed) {
            outcome.found_result = true;
            outcome.is_auth_error = true;
            outcome
                .auth_error_message
                .get_or_insert(trimmed.to_string());
            push_transcript_event(
                &mut tool_state.transcript_events,
                SessionTranscriptEvent {
                    kind: SessionTranscriptEventKind::Error,
                    tool_name: None,
                    summary: Some("authentication error".to_string()),
                    content: trimmed.to_string(),
                    status: Some("error".to_string()),
                    is_error: true,
                },
            );
            state.final_result = Some(String::new());
        } else if let Some(message) = detect_provider_overload_message(trimmed) {
            outcome.found_result = true;
            outcome.is_provider_overloaded = true;
            outcome.provider_overload_message.get_or_insert(message);
            push_transcript_event(
                &mut tool_state.transcript_events,
                SessionTranscriptEvent {
                    kind: SessionTranscriptEventKind::Error,
                    tool_name: None,
                    summary: Some("provider overload".to_string()),
                    content: trimmed.to_string(),
                    status: Some("error".to_string()),
                    is_error: true,
                },
            );
            state.final_result = Some(String::new());
        }
    }

    outcome
}

/// On startup, scan for surviving tmux sessions (AgentDesk-*) and restore watchers.
/// This handles the case where AgentDesk was restarted but tmux sessions are still alive.
pub(super) async fn restore_tmux_watchers(http: &Arc<serenity::Http>, shared: &Arc<SharedData>) {
    let settings_snapshot = { shared.settings.read().await.clone() };
    let provider = settings_snapshot.provider.clone();

    // List tmux sessions matching our naming convention
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names,
        _ => return, // No tmux, timeout, or no sessions
    };

    let agent_sessions: Vec<&str> = output
        .iter()
        .map(|l| l.trim())
        .filter(|l| {
            parse_provider_and_channel_from_tmux_name(l)
                .map(|(session_provider, _)| session_provider == provider)
                .unwrap_or(false)
        })
        .collect();

    if agent_sessions.is_empty() {
        return;
    }

    // Build channel name → ChannelId map from Discord API (sessions map may be empty after restart)
    let mut name_to_channel: std::collections::HashMap<String, (ChannelId, String)> =
        std::collections::HashMap::new();

    // Try from in-memory sessions first
    {
        let data = shared.core.lock().await;
        for (&ch_id, session) in &data.sessions {
            if let Some(ref ch_name) = session.channel_name {
                let tmux_name = provider.build_tmux_session_name(ch_name);
                name_to_channel.insert(tmux_name, (ch_id, ch_name.clone()));
            }
        }
    }

    // If in-memory sessions don't cover all tmux sessions, fetch from Discord API
    let unresolved: Vec<&&str> = agent_sessions
        .iter()
        .filter(|s| !name_to_channel.contains_key(**s))
        .collect();

    if !unresolved.is_empty() {
        // Fetch guild channels via Discord API
        if let Ok(guilds) = http.get_guilds(None, None).await {
            for guild_info in &guilds {
                if let Ok(channels) = guild_info.id.channels(http).await {
                    for (ch_id, channel) in &channels {
                        let role_binding = resolve_role_binding(*ch_id, Some(&channel.name));
                        if !channel_supports_provider(
                            &provider,
                            Some(&channel.name),
                            false,
                            role_binding.as_ref(),
                        ) {
                            continue;
                        }
                        let tmux_name = provider.build_tmux_session_name(&channel.name);
                        name_to_channel
                            .entry(tmux_name)
                            .or_insert((*ch_id, channel.name.clone()));
                    }
                }
            }
        }

        // Fallback for thread sessions: guild.channels() doesn't return threads.
        // Extract thread_id from the channel name suffix (-t{id}) and use it
        // as the channel_id directly, since Discord thread IDs are channel IDs.
        let still_unresolved: Vec<&&str> = agent_sessions
            .iter()
            .filter(|s| !name_to_channel.contains_key(**s))
            .collect();
        for session_name in &still_unresolved {
            if let Some((_, ch_name)) = parse_provider_and_channel_from_tmux_name(session_name) {
                if let Some(pos) = ch_name.rfind("-t") {
                    let suffix = &ch_name[pos + 2..];
                    if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                        if let Ok(thread_id) = suffix.parse::<u64>() {
                            let channel_id = ChannelId::new(thread_id);
                            name_to_channel
                                .entry(session_name.to_string())
                                .or_insert((channel_id, ch_name.clone()));
                        }
                    }
                }
            }
        }
    }

    // Collect sessions to restore
    struct PendingWatcher {
        channel_id: ChannelId,
        output_path: String,
        session_name: String,
        initial_offset: u64,
        restored_turn: Option<RestoredWatcherTurn>,
    }

    // Dead sessions that need DB cleanup (idle status report + tmux kill)
    struct DeadSessionCleanup {
        channel_id: u64,
        channel_name: String,
        session_name: String,
    }

    let mut pending: Vec<PendingWatcher> = Vec::new();
    let mut dead_cleanups: Vec<DeadSessionCleanup> = Vec::new();
    let mut owned_sessions: std::collections::HashMap<ChannelId, String> =
        std::collections::HashMap::new();

    for session_name in &agent_sessions {
        let Some((channel_id, channel_name)) = name_to_channel.get(*session_name) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — channel mapping not found",
                session_name
            );
            continue;
        };

        // #148: Do NOT register in owned_sessions yet — QUARANTINE check below may
        // skip this session. Registering early blocks new session creation for the channel.
        let is_dm = matches!(
            channel_id.to_channel(http.as_ref()).await,
            Ok(serenity::model::channel::Channel::Private(_))
        );
        // Resolve thread parent so validation uses the same semantics
        // as normal message routing (router.rs).
        let (allowlist_channel_id, provider_channel_name) =
            if let Some((pid, pname)) = super::resolve_thread_parent(http, *channel_id).await {
                (pid, pname.unwrap_or_else(|| channel_name.clone()))
            } else {
                (*channel_id, channel_name.clone())
            };
        if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
            &settings_snapshot,
            &provider,
            allowlist_channel_id,
            Some(&channel_name),
            Some(&provider_channel_name),
            is_dm,
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — {reason} for channel {}",
                session_name,
                channel_id
            );
            continue;
        }

        if let Some(started) = super::mailbox_snapshot(&shared, *channel_id)
            .await
            .recovery_started_at
        {
            if started.elapsed() < std::time::Duration::from_secs(60) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏳ watcher skip for {} — recovery in progress ({:.0}s ago)",
                    session_name,
                    started.elapsed().as_secs_f64()
                );
                continue;
            }
            // Stale recovery — remove marker and proceed with watcher
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ clearing stale recovery marker for {} ({:.0}s elapsed)",
                session_name,
                started.elapsed().as_secs_f64()
            );
            super::mailbox_clear_recovery_marker(&shared, *channel_id).await;
        }

        if shared.tmux_watchers.contains_key(channel_id) {
            continue;
        }

        // Accept either the new persistent location or the legacy /tmp
        // location — older wrappers still write to /tmp, and a dcserver
        // restart that lost /tmp files should not falsely flag a live
        // session as "no output file". See issue #892.
        let Some(output_path) =
            crate::services::tmux_common::resolve_session_temp_path(session_name, "jsonl")
        else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — no output file",
                session_name
            );
            continue;
        };

        // Old-gen sessions: adopt instead of killing.
        // The tmux session and Claude CLI process are still alive from the
        // previous dcserver — just update the generation marker and re-attach
        // a watcher. Auto-retry handles stale Claude session IDs if needed.
        let gen_marker_path =
            crate::services::tmux_common::session_temp_path(session_name, "generation");
        let session_gen = std::fs::read_to_string(&gen_marker_path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let current_gen = super::runtime_store::load_generation();
        if session_gen < current_gen && current_gen > 0 {
            // Skip sessions belonging to other runtimes
            let current_owner_marker = current_tmux_owner_marker();
            if !session_belongs_to_current_runtime(session_name, &current_owner_marker) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — owned by other runtime",
                    session_name
                );
                continue;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Adopting old-gen session {} (gen {} → {})",
                session_name,
                session_gen,
                current_gen
            );
            // Update generation marker to current gen
            let _ = std::fs::write(&gen_marker_path, current_gen.to_string());
        }

        if !tmux_session_has_live_pane(session_name) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            if let Some(diag) = build_tmux_death_diagnostic(session_name, Some(&output_path)) {
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane dead ({diag})",
                    session_name
                );
            } else {
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane dead",
                    session_name
                );
            }
            // Schedule DB cleanup + tmux kill for this dead session
            dead_cleanups.push(DeadSessionCleanup {
                channel_id: channel_id.get(),
                channel_name: channel_name.clone(),
                session_name: session_name.to_string(),
            });
            continue;
        }

        // #148: Only register in owned_sessions after passing QUARANTINE + live-pane checks.
        // Earlier registration blocked new session creation for quarantined/dead channels.
        owned_sessions
            .entry(*channel_id)
            .or_insert_with(|| channel_name.clone());

        let mut restored_turn = None;
        let initial_offset = if let Some(state) =
            super::inflight::load_inflight_state(&provider, channel_id.get())
        {
            if let Some(restored_tmux) =
                restored_watcher_turn_from_inflight(&state, session_name, false)
            {
                let finish_mailbox_on_completion =
                    super::recovery::reregister_active_turn_from_inflight(&shared, &state).await;
                restored_turn = Some(RestoredWatcherTurn {
                    finish_mailbox_on_completion,
                    ..restored_tmux
                });
                let file_len = std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                if file_len >= state.last_offset {
                    state.last_offset
                } else {
                    0
                }
            } else {
                std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0)
            }
        } else {
            std::fs::metadata(&output_path)
                .map(|m| m.len())
                .unwrap_or(0)
        };

        pending.push(PendingWatcher {
            channel_id: *channel_id,
            output_path,
            session_name: session_name.to_string(),
            initial_offset,
            restored_turn,
        });
    }

    // Register sessions in CoreState so cleanup_orphan_tmux_sessions recognizes them
    // and message handlers find an active session with current_path
    if !owned_sessions.is_empty() {
        let mut data = shared.core.lock().await;
        let sqlite_settings_db = if shared.pg_pool.is_some() {
            None
        } else {
            shared.sqlite.as_ref()
        };
        for (channel_id, channel_name) in &owned_sessions {
            let persisted_path = load_last_session_path(
                sqlite_settings_db,
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                channel_id.get(),
            );
            let remote_profile = load_last_remote_profile(
                sqlite_settings_db,
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                channel_id.get(),
            );
            let persisted_session_id = load_restored_provider_session_id(
                sqlite_settings_db,
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                &provider,
                channel_name,
            );
            let configured_path =
                super::settings::resolve_workspace(*channel_id, Some(channel_name.as_str()));
            let tmux_name = provider.build_tmux_session_name(channel_name);
            let session_keys = super::adk_session::build_session_key_candidates(
                &shared.token_hash,
                &provider,
                &tmux_name,
            );
            let db_cwd = load_restored_session_cwd(
                shared.sqlite.as_ref(),
                shared.pg_pool.as_ref(),
                &session_keys,
            );

            let session =
                data.sessions
                    .entry(*channel_id)
                    .or_insert_with(|| super::DiscordSession {
                        session_id: persisted_session_id.clone(),
                        memento_context_loaded:
                            super::session_runtime::restored_memento_context_loaded(
                                false,
                                None,
                                persisted_session_id.as_deref(),
                            ),
                        memento_reflected: false,
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        channel_name: Some(channel_name.clone()),
                        category_name: None,
                        remote_profile_name: remote_profile.clone(),
                        channel_id: Some(channel_id.get()),

                        last_active: tokio::time::Instant::now(),
                        worktree: None,

                        born_generation: super::runtime_store::load_generation(),
                        assistant_turns: 0,
                    });

            if session.session_id.is_none() && persisted_session_id.is_some() {
                session.restore_provider_session(persisted_session_id.clone());
            }

            // Restore current_path: DB cwd (worktree-aware) > last_sessions (yaml, main workspace)
            if session.current_path.is_none() {
                if let (Some(configured), Some(restored)) =
                    (configured_path.as_ref(), db_cwd.as_ref())
                {
                    if configured != restored {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⚠ Ignoring restored DB cwd for channel {}: {} (configured workspace: {})",
                            channel_id,
                            restored,
                            configured
                        );
                    }
                }
                let effective_path = super::select_restored_session_path(
                    configured_path,
                    db_cwd,
                    persisted_path,
                    remote_profile.as_deref(),
                );
                if let Some(path) = effective_path {
                    session.current_path = Some(path);
                }
            }
        }
    }

    // Spawn watchers
    // #226: Use try_claim_watcher for atomic check-and-insert. The pending list
    // was built during the scan phase, which includes async Discord API calls.
    // A normal turn may have created a watcher in the meantime.
    for pw in pending {
        // #226: Skip channels that recovery already handled — their watchers may have
        // ended quickly (session died), removing themselves from the DashMap, but we
        // should not create a second watcher because recovery already processed the turn.
        let recovery_handled =
            recovery_handled_channel_exists(shared.as_ref(), pw.channel_id.get());
        if recovery_handled {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — recovery already handled this channel",
                pw.session_name
            );
            continue;
        }

        if pw.restored_turn.is_none() {
            reconcile_orphan_suppressed_placeholder_for_restored_watcher(
                http,
                shared,
                &provider,
                pw.channel_id,
                &pw.session_name,
            )
            .await;
        }

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
        let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let handle = TmuxWatcherHandle {
            paused: paused.clone(),
            resume_offset: resume_offset.clone(),
            cancel: cancel.clone(),
            pause_epoch: pause_epoch.clone(),
            turn_delivered: turn_delivered.clone(),
        };
        if !try_claim_watcher(&shared.tmux_watchers, pw.channel_id, handle) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — already watching (created during scan)",
                pw.session_name
            );
            continue;
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ Restoring tmux watcher for {} (offset {})",
            pw.session_name,
            pw.initial_offset
        );

        tokio::spawn(tmux_output_watcher_with_restore(
            pw.channel_id,
            http.clone(),
            shared.clone(),
            pw.output_path,
            pw.session_name,
            pw.initial_offset,
            cancel,
            paused,
            resume_offset,
            pause_epoch,
            turn_delivered,
            pw.restored_turn,
        ));
    }

    // Clean up dead sessions: report idle to DB and kill tmux sessions
    if !dead_cleanups.is_empty() {
        let api_port = shared.api_port;
        let provider = shared.settings.read().await.provider.clone();

        let mut cleaned_dead_sessions = 0usize;
        for dc in &dead_cleanups {
            let dispatch_protection = super::tmux_lifecycle::resolve_dispatch_tmux_protection(
                shared.sqlite.as_ref(),
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                &provider,
                &dc.session_name,
                Some(&dc.channel_name),
            );
            let cleanup_plan = dead_session_cleanup_plan(dispatch_protection.is_some());

            if let Some(protection) = dispatch_protection {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ♻ tmux startup: preserving dispatch session {} — {}",
                    dc.session_name,
                    protection.log_reason()
                );
            }

            let tmux_name = provider.build_tmux_session_name(&dc.channel_name);
            let thread_channel_id =
                super::adk_session::parse_thread_channel_id_from_name(&dc.channel_name);
            let session_key = super::adk_session::build_namespaced_session_key(
                &shared.token_hash,
                &provider,
                &tmux_name,
            );
            let agent_id =
                resolve_role_binding(ChannelId::new(dc.channel_id), Some(&dc.channel_name))
                    .map(|binding| binding.role_id);

            if cleanup_plan.report_idle_status {
                super::adk_session::post_adk_session_status(
                    Some(&session_key),
                    Some(&dc.channel_name),
                    None,
                    "idle",
                    &provider,
                    None,
                    None,
                    None,
                    None,
                    thread_channel_id,
                    agent_id.as_deref(),
                    api_port,
                )
                .await;
            }

            if cleanup_plan.preserve_tmux_session {
                continue;
            }

            // Kill the dead tmux session
            let sess = dc.session_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::services::termination_audit::record_termination_for_tmux(
                    &sess,
                    None,
                    "tmux_startup",
                    "startup_dead_session",
                    Some("startup cleanup: dead session"),
                    None,
                );
                record_tmux_exit_reason(&sess, "startup cleanup: dead session");
                crate::services::platform::tmux::kill_session_with_reason(
                    &sess,
                    "startup cleanup: dead session",
                );
            })
            .await;
            cleaned_dead_sessions += 1;
        }

        if cleaned_dead_sessions > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 Cleaned {} dead tmux session(s) on startup",
                cleaned_dead_sessions
            );
        }

        // Sweep orphan session temp files (no matching tmux session AND
        // owner marker older than the threshold). Conservative: skip the
        // legacy /tmp directory (those files may still be held open by
        // pre-migration wrappers) — we only clean the new persistent
        // directory. See issue #892.
        sweep_orphan_session_files().await;
    }
}

/// Remove jsonl/input/prompt/owner/etc files in the persistent sessions
/// directory that no longer belong to a running tmux session. Conservative:
/// require an owner marker (or the jsonl) to be older than
/// `ORPHAN_MIN_AGE_SECS` and require the session to be absent from tmux
/// before deleting. Legacy `/tmp/` files are *never* swept at startup —
/// pre-migration wrappers may still be writing into them.
async fn sweep_orphan_session_files() {
    const ORPHAN_MIN_AGE_SECS: u64 = 10 * 60; // 10 minutes

    let Some(dir) = crate::services::tmux_common::persistent_sessions_dir() else {
        return;
    };
    if !dir.exists() {
        return;
    }

    // List live tmux sessions.
    let live: std::collections::HashSet<String> = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names.into_iter().collect(),
        _ => return, // tmux unavailable — skip sweep rather than risk false positives
    };

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    // Group files under the sessions dir by the `agentdesk-<hash>-<host>-<session>`
    // prefix. Any prefix whose session name is not in `live` *and* whose
    // oldest file mtime is older than ORPHAN_MIN_AGE_SECS is swept.
    let mut groups: std::collections::HashMap<String, (String, std::time::SystemTime)> =
        std::collections::HashMap::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with("agentdesk-") {
            continue;
        }
        // Strip extension.
        let stem = match name.rsplit_once('.') {
            Some((s, _)) => s.to_string(),
            None => name.clone(),
        };
        // Session name is the last token after the fourth dash — but our
        // prefix format is `agentdesk-<12hex>-<host>-<session>` and host
        // may contain dashes. The simplest robust approach: split_once on
        // `agentdesk-<hash>-<host>-` is hard to reverse, so instead we use
        // the owner file's prefix as the grouping key directly — any file
        // whose stem matches some live session (ends with `-<live>`) is kept.
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| std::time::SystemTime::now());
        groups
            .entry(stem.clone())
            .and_modify(|slot| {
                if mtime < slot.1 {
                    *slot = (stem.clone(), mtime);
                }
            })
            .or_insert((stem, mtime));
    }

    let now = std::time::SystemTime::now();
    let mut swept = 0usize;
    for (stem, (_, oldest_mtime)) in groups {
        // Is this stem associated with any live tmux session? We check
        // whether ANY live session name appears as a suffix of the stem.
        // Since session names are distinctive (provider:channel shape), a
        // conservative suffix match keeps ambiguity low; we also require
        // that the match is preceded by a dash so we don't match e.g.
        // "claude:foo" against a stem ending with "-thisisnotclaude:foo".
        let is_live = live.iter().any(|live_name| {
            let needle = format!("-{}", live_name);
            stem.ends_with(&needle) || stem == *live_name
        });
        if is_live {
            continue;
        }
        // Conservative: require age threshold.
        let age = now
            .duration_since(oldest_mtime)
            .unwrap_or(std::time::Duration::ZERO);
        if age.as_secs() < ORPHAN_MIN_AGE_SECS {
            continue;
        }
        // Delete every file under this stem.
        let Ok(iter) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in iter.flatten() {
            if let Ok(fname) = entry.file_name().into_string() {
                if fname.starts_with(&format!("{}.", stem)) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        swept += 1;
    }
    if swept > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🧹 Swept {} orphan session file group(s) from {}",
            swept,
            dir.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DeadSessionCleanupPlan, MONITOR_AUTO_TURN_DEFERRED_REASON_CODE,
        MONITOR_AUTO_TURN_REASON_CODE, OffsetAdvanceDecision, PlaceholderSuppressContext,
        PlaceholderSuppressDecision, PlaceholderSuppressOrigin, READY_FOR_INPUT_STUCK_REASON,
        SUPPRESSED_INTERNAL_LABEL, SUPPRESSED_RESTART_LABEL, SuppressedPlaceholderAction,
        TmuxWatcherHandle, WatcherToolState, build_bg_trigger_session_key,
        claim_or_replace_watcher, clear_recent_watcher_reattach_offsets_for_tests,
        dead_session_cleanup_plan, decide_placeholder_suppression,
        enqueue_background_trigger_response_to_notify_outbox,
        enqueue_monitor_auto_turn_suppressed_notification, fail_dispatch_for_ready_for_input_stall,
        finish_monitor_auto_turn, lifecycle_reason_code_for_tmux_exit,
        load_restored_provider_session_id, matching_recent_watcher_reattach_offset,
        missing_inflight_fallback_plan, notify_path_offset_advance_decision,
        orphan_suppressed_placeholder_action, parse_bg_trigger_offset_from_session_key,
        process_watcher_lines, record_recent_watcher_reattach_offset,
        refresh_session_heartbeat_from_tmux_output, restored_watcher_turn_from_inflight,
        rollback_enqueued_offset_for_reconciled_failures, start_monitor_auto_turn_when_available,
        strip_inprogress_indicators, suppressed_placeholder_action, terminal_relay_decision,
        tmux_death_is_normal_completion, wait_for_reacquired_turn_bridge_inflight_state,
        watcher_ready_for_input_turn_completed, watcher_should_yield_to_inflight_state,
        watcher_stream_seed,
    };
    use crate::services::agent_protocol::TaskNotificationKind;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::discord::runtime_store::test_env_lock;
    use crate::services::provider::{CancelToken, ProviderKind, ReadyForInputIdleTracker};
    use crate::services::session_backend::StreamLineState;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;

    fn test_watcher_handle() -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            paused: Arc::new(AtomicBool::new(true)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn normal_completion_exit_reason_maps_to_dedicated_lifecycle_code() {
        assert_eq!(
            lifecycle_reason_code_for_tmux_exit("turn completed (code 0)"),
            "lifecycle.normal_completion"
        );
        assert_eq!(
            lifecycle_reason_code_for_tmux_exit("dispatch turn completed"),
            "lifecycle.normal_completion"
        );
        assert_eq!(
            lifecycle_reason_code_for_tmux_exit("unified-thread run completed"),
            "lifecycle.normal_completion"
        );
    }

    #[test]
    fn normal_completion_detection_requires_a_trusted_exit_reason() {
        assert!(tmux_death_is_normal_completion(
            Some("turn completed (code 0)"),
            Some("recent_output=completed_result_present")
        ));
        assert!(!tmux_death_is_normal_completion(
            None,
            Some("recent_output=completed_result_present")
        ));
    }

    #[test]
    fn restored_live_tmux_session_loads_namespaced_provider_session_id() {
        let db = crate::db::test_db();
        let provider = ProviderKind::Codex;
        let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
            "tokenxyz",
            &provider,
            &provider.build_tmux_session_name("adk-cdx"),
        );
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions (session_key, provider, claude_session_id) VALUES (?1, ?2, ?3)",
                [session_key.as_str(), provider.as_str(), "persisted-sid-1"],
            )
            .unwrap();

        assert_eq!(
            load_restored_provider_session_id(Some(&db), None, "tokenxyz", &provider, "adk-cdx",)
                .as_deref(),
            Some("persisted-sid-1")
        );
    }

    #[test]
    fn restored_live_tmux_session_falls_back_to_legacy_session_key() {
        let db = crate::db::test_db();
        let provider = ProviderKind::Codex;
        let session_key = crate::services::discord::adk_session::build_legacy_session_key(
            &provider.build_tmux_session_name("adk-cdx"),
        );
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions (session_key, provider, claude_session_id) VALUES (?1, ?2, ?3)",
                [session_key.as_str(), provider.as_str(), "legacy-sid-1"],
            )
            .unwrap();

        assert_eq!(
            load_restored_provider_session_id(Some(&db), None, "tokenxyz", &provider, "adk-cdx",)
                .as_deref(),
            Some("legacy-sid-1")
        );
    }

    #[test]
    fn watcher_output_activity_refreshes_namespaced_session_heartbeat() {
        let db = crate::db::test_db();
        let provider = ProviderKind::Codex;
        let channel_name = "adk-cdx-t1485506232256168011";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
            "tokenxyz", &provider, &tmux_name,
        );
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions
                 (session_key, provider, status, thread_channel_id, last_heartbeat, created_at)
                 VALUES (?1, ?2, 'idle', '1485506232256168011', '2026-04-09 01:02:03', '2026-04-09 01:02:03')",
                [session_key.as_str(), provider.as_str()],
            )
            .unwrap();

        assert!(refresh_session_heartbeat_from_tmux_output(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some(1485506232256168011),
        ));

        let last_heartbeat: String = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT last_heartbeat FROM sessions WHERE session_key = ?1",
                [session_key.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(last_heartbeat, "2026-04-09 01:02:03");
    }

    #[test]
    fn claim_or_replace_watcher_keeps_one_entry_per_channel() {
        let watchers = dashmap::DashMap::new();
        let channel_id = ChannelId::new(1485506232256168123);

        assert!(super::try_claim_watcher(
            &watchers,
            channel_id,
            test_watcher_handle()
        ));
        assert_eq!(watchers.len(), 1);

        assert!(!claim_or_replace_watcher(
            &watchers,
            channel_id,
            test_watcher_handle(),
            &ProviderKind::Codex,
            "unit-test"
        ));
        assert_eq!(watchers.len(), 1);

        let watcher = watchers.get(&channel_id).expect("watcher should exist");
        assert!(watcher.paused.load(Ordering::Relaxed));
    }

    #[test]
    fn missing_inflight_fallback_warns_and_triggers_reattach_on_db_miss() {
        let plan = missing_inflight_fallback_plan(true, false, true);
        assert!(plan.warn);
        assert!(plan.trigger_reattach);

        let resolved = missing_inflight_fallback_plan(true, true, true);
        assert!(resolved.warn);
        assert!(!resolved.trigger_reattach);

        let uncommitted = missing_inflight_fallback_plan(true, false, false);
        assert!(uncommitted.warn);
        assert!(!uncommitted.trigger_reattach);
    }

    #[tokio::test]
    async fn missing_inflight_reattach_grace_preserves_same_offset_bridge_placeholder()
    -> Result<(), Box<dyn std::error::Error>> {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir()?;
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(987_1044_001);
        let channel_name = "adk-cdx-issue-1044";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let turn_offset = 44_096_u64;

        let terminal_success_plan = missing_inflight_fallback_plan(true, false, true);
        assert!(terminal_success_plan.trigger_reattach);
        assert!(super::super::inflight::load_inflight_state(&provider, channel.get()).is_none());

        let writer_provider = provider.clone();
        let writer_tmux_name = tmux_name.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let mut state = InflightTurnState::new(
                writer_provider,
                channel.get(),
                Some(channel_name.to_string()),
                7,
                9,
                11,
                "next turn at same offset".to_string(),
                Some("session-1044".to_string()),
                Some(writer_tmux_name),
                Some("/tmp/issue-1044.jsonl".to_string()),
                Some("/tmp/issue-1044.fifo".to_string()),
                turn_offset,
            );
            state.turn_start_offset = Some(turn_offset);
            state.full_response = "already visible bridge placeholder body".to_string();
            state.response_sent_offset = state.full_response.len();
            let _ = super::super::inflight::save_inflight_state(&state);
        });

        let bridge_reacquired = wait_for_reacquired_turn_bridge_inflight_state(
            &provider,
            channel,
            &tmux_name,
            3,
            Duration::from_millis(50),
        )
        .await;
        let _ = writer.await;

        assert!(
            bridge_reacquired,
            "next turn should reacquire inflight during the missing-inflight reattach grace window"
        );
        let next_turn_state = super::super::inflight::load_inflight_state(&provider, channel.get())
            .ok_or_else(|| std::io::Error::other("expected next turn inflight state"))?;
        assert!(watcher_should_yield_to_inflight_state(
            Some(&next_turn_state),
            &tmux_name,
            turn_offset,
            turn_offset + 128,
        ));

        let placeholder_body = "already visible bridge placeholder body";
        let final_placeholder_body = if bridge_reacquired {
            placeholder_body.to_string()
        } else {
            match suppressed_placeholder_action(true, placeholder_body.len(), placeholder_body) {
                SuppressedPlaceholderAction::Edit(content) => content,
                _ => String::new(),
            }
        };

        assert_eq!(final_placeholder_body, placeholder_body);
        assert!(!final_placeholder_body.contains(SUPPRESSED_INTERNAL_LABEL));
        assert!(!final_placeholder_body.contains(SUPPRESSED_RESTART_LABEL));

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        Ok(())
    }

    #[tokio::test]
    async fn bridge_guard_preserves_placeholder_when_range_matches_recent_reattach()
    -> Result<(), Box<dyn std::error::Error>> {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_watcher_reattach_offsets_for_tests();
        let tmp = tempfile::tempdir()?;
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(987_1044_002);
        let channel_name = "adk-cdx-issue-1044b";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let reattach_offset = 7_628_900_u64;
        let suppressed_end_offset = 7_636_322_u64;
        let placeholder_body = "real response body already delivered by watcher reattach";

        let test_result = async {
            super::super::inflight::clear_inflight_state(&provider, channel.get());
            let terminal_success_plan = missing_inflight_fallback_plan(true, false, true);
            assert!(terminal_success_plan.trigger_reattach);
            assert!(
                super::super::inflight::load_inflight_state(&provider, channel.get()).is_none()
            );

            let bridge_reacquired = wait_for_reacquired_turn_bridge_inflight_state(
                &provider,
                channel,
                &tmux_name,
                1,
                Duration::from_millis(1),
            )
            .await;
            assert!(
                !bridge_reacquired,
                "grace window should still see no bridge-owned inflight state"
            );

            record_recent_watcher_reattach_offset(channel, &tmux_name, reattach_offset);

            let mut state = InflightTurnState::new(
                provider.clone(),
                channel.get(),
                Some(channel_name.to_string()),
                0,
                0,
                44,
                "watcher missing-inflight reattach".to_string(),
                None,
                Some(tmux_name.clone()),
                Some("/tmp/issue-1044b.jsonl".to_string()),
                Some("/tmp/issue-1044b.fifo".to_string()),
                reattach_offset,
            );
            state.rebind_origin = true;
            state.full_response = placeholder_body.to_string();
            state.response_sent_offset = placeholder_body.len();
            super::super::inflight::save_inflight_state_create_new(&state).map_err(|error| {
                std::io::Error::other(format!("failed to save reattach inflight state: {error}"))
            })?;

            assert!(watcher_should_yield_to_inflight_state(
                Some(&state),
                &tmux_name,
                reattach_offset,
                suppressed_end_offset,
            ));
            let matched_reattach =
                matching_recent_watcher_reattach_offset(channel, &tmux_name, reattach_offset);
            assert!(
                matched_reattach.is_some(),
                "suppressed range start should match the recent watcher reattach offset"
            );

            let final_placeholder_body = if matched_reattach.is_some() {
                placeholder_body.to_string()
            } else {
                match suppressed_placeholder_action(true, placeholder_body.len(), placeholder_body)
                {
                    SuppressedPlaceholderAction::Edit(content) => content,
                    SuppressedPlaceholderAction::Delete | SuppressedPlaceholderAction::None => {
                        String::new()
                    }
                }
            };

            assert_eq!(final_placeholder_body, placeholder_body);
            assert!(!final_placeholder_body.contains(SUPPRESSED_INTERNAL_LABEL));

            let non_reattach_suppress =
                suppressed_placeholder_action(true, placeholder_body.len(), placeholder_body);
            assert!(matches!(
                non_reattach_suppress,
                SuppressedPlaceholderAction::Edit(ref content)
                    if content.contains(SUPPRESSED_INTERNAL_LABEL)
            ));

            Ok::<(), Box<dyn std::error::Error>>(())
        }
        .await;

        super::super::inflight::clear_inflight_state(&provider, channel.get());
        clear_recent_watcher_reattach_offsets_for_tests();
        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        test_result
    }

    #[test]
    fn watcher_yields_to_active_bridge_turn_when_batch_overlaps_turn_start() {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("deadlock-manager".to_string()),
            7,
            9,
            11,
            "ping".to_string(),
            Some("session-1".to_string()),
            Some("#AgentDesk-codex-deadlock-manager".to_string()),
            Some("/tmp/output.jsonl".to_string()),
            Some("/tmp/input.fifo".to_string()),
            0,
        );
        state.turn_start_offset = Some(120);
        state.last_offset = 180;
        let should_yield = watcher_should_yield_to_inflight_state(
            Some(&state),
            "#AgentDesk-codex-deadlock-manager",
            100,
            180,
        );

        assert!(should_yield);
    }

    #[test]
    fn watcher_does_not_yield_for_non_overlapping_or_other_session_turns() {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("deadlock-manager".to_string()),
            7,
            9,
            11,
            "ping".to_string(),
            Some("session-1".to_string()),
            Some("#AgentDesk-codex-deadlock-manager".to_string()),
            Some("/tmp/output.jsonl".to_string()),
            Some("/tmp/input.fifo".to_string()),
            0,
        );
        state.turn_start_offset = Some(220);
        state.last_offset = 260;
        let different_range = watcher_should_yield_to_inflight_state(
            Some(&state),
            "#AgentDesk-codex-deadlock-manager",
            100,
            180,
        );
        let different_session = watcher_should_yield_to_inflight_state(
            Some(&state),
            "#AgentDesk-codex-somewhere-else",
            200,
            280,
        );

        assert!(!different_range);
        assert!(!different_session);
    }

    #[test]
    fn watcher_output_activity_refreshes_legacy_session_heartbeat() {
        let db = crate::db::test_db();
        let provider = ProviderKind::Codex;
        let channel_name = "adk-cdx-t1485506232256168011";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key =
            crate::services::discord::adk_session::build_legacy_session_key(&tmux_name);
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions
                 (session_key, provider, status, thread_channel_id, last_heartbeat, created_at)
                 VALUES (?1, ?2, 'idle', '1485506232256168011', '2026-04-09 01:02:03', '2026-04-09 01:02:03')",
                [session_key.as_str(), provider.as_str()],
            )
            .unwrap();

        assert!(refresh_session_heartbeat_from_tmux_output(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some(1485506232256168011),
        ));

        let last_heartbeat: String = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT last_heartbeat FROM sessions WHERE session_key = ?1",
                [session_key.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(last_heartbeat, "2026-04-09 01:02:03");
    }

    #[test]
    fn watcher_ignores_assistant_text_that_mentions_stale_resume_phrase() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"The log contained No conversation found while I was debugging.\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(!outcome.stale_resume_detected);
        assert_eq!(
            full_response,
            "The log contained No conversation found while I was debugging."
        );
    }

    #[test]
    fn watcher_detects_structured_stale_resume_error_result() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"partial\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"errors\":[\"No conversation found\"]}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.stale_resume_detected);
        assert_eq!(full_response, "partial");
    }

    // ── #378 E2E: normal success result is not flagged ──

    #[test]
    fn normal_success_result_has_no_error_flags() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Here is the answer.\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        ).to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(!outcome.is_prompt_too_long);
        assert!(!outcome.is_auth_error);
        assert!(!outcome.is_provider_overloaded);
        assert!(!outcome.stale_resume_detected);
        assert_eq!(full_response, "Here is the answer.");
    }

    #[test]
    fn watcher_tracks_previous_tool_status_for_two_line_trail() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Read\",\"input\":{\"file_path\":\"src/config.rs\"}}]}}\n",
            "{\"type\":\"content_block_stop\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{\"command\":\"cargo build\"}}]}}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(!outcome.found_result);
        assert_eq!(
            tool_state.prev_tool_status.as_deref(),
            Some("✓ Read: src/config.rs")
        );
        assert_eq!(
            tool_state.current_tool_line.as_deref(),
            Some("⚙ Bash: `cargo build`")
        );
    }

    #[test]
    fn dead_session_cleanup_plan_preserves_tmux_but_still_reports_idle() {
        let plan = dead_session_cleanup_plan(true);

        assert_eq!(
            plan,
            DeadSessionCleanupPlan {
                preserve_tmux_session: true,
                report_idle_status: true,
            }
        );
    }

    #[test]
    fn dead_session_cleanup_plan_kills_unprotected_sessions_and_reports_idle() {
        let plan = dead_session_cleanup_plan(false);

        assert_eq!(
            plan,
            DeadSessionCleanupPlan {
                preserve_tmux_session: false,
                report_idle_status: true,
            }
        );
    }

    #[test]
    fn watcher_ready_for_input_completion_requires_stable_idle_prompt_after_output() {
        let mut tracker = ReadyForInputIdleTracker::default();
        let start = std::time::Instant::now();

        assert_eq!(
            watcher_ready_for_input_turn_completed(&mut tracker, 100, 100, true, true, start),
            crate::services::provider::ReadyForInputIdleState::None
        );

        tracker.record_output();
        assert_eq!(
            watcher_ready_for_input_turn_completed(&mut tracker, 100, 120, true, true, start),
            crate::services::provider::ReadyForInputIdleState::None
        );
        assert_eq!(
            watcher_ready_for_input_turn_completed(
                &mut tracker,
                100,
                120,
                true,
                true,
                start + std::time::Duration::from_secs(10)
            ),
            crate::services::provider::ReadyForInputIdleState::None
        );
        assert_eq!(
            watcher_ready_for_input_turn_completed(
                &mut tracker,
                100,
                120,
                true,
                true,
                start + std::time::Duration::from_secs(16)
            ),
            crate::services::provider::ReadyForInputIdleState::PostWorkIdleTimeout
        );
    }

    #[test]
    fn watcher_ready_for_input_fresh_idle_does_not_trigger_failure_path() {
        let mut tracker = ReadyForInputIdleTracker::default();
        let start = std::time::Instant::now();

        tracker.record_output();
        assert_eq!(
            watcher_ready_for_input_turn_completed(&mut tracker, 100, 120, true, false, start),
            crate::services::provider::ReadyForInputIdleState::None
        );
        assert_eq!(
            watcher_ready_for_input_turn_completed(
                &mut tracker,
                100,
                120,
                true,
                false,
                start + std::time::Duration::from_secs(10)
            ),
            crate::services::provider::ReadyForInputIdleState::None
        );
        assert_eq!(
            watcher_ready_for_input_turn_completed(
                &mut tracker,
                100,
                120,
                true,
                false,
                start + std::time::Duration::from_secs(16)
            ),
            crate::services::provider::ReadyForInputIdleState::FreshIdle
        );
    }

    #[tokio::test]
    async fn ready_for_input_stall_marks_dispatch_failed_and_alerts_humans() {
        let db = crate::db::test_db();
        let shared = super::super::make_shared_data_for_tests_with_storage(Some(db.clone()), None);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id)
                 VALUES ('agent-1', 'Agent 1', '444111')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
                 VALUES ('run-ready-stall', 'test/repo', 'agent-1', 'running')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority)
                 VALUES ('card-ready-stall', 'Ready stall card', 'in_progress', 'medium')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at
                 ) VALUES (
                    'dispatch-ready-stall', 'card-ready-stall', 'agent-1', 'implementation', 'dispatched', 'Ready stall', '123456', datetime('now'), datetime('now')
                 )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (
                    id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at
                 ) VALUES (
                    'entry-ready-stall', 'run-ready-stall', 'card-ready-stall', 'agent-1', 'dispatched', 'dispatch-ready-stall', datetime('now')
                 )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kv_meta (key, value) VALUES ('kanban_human_alert_channel_id', '555123')",
                [],
            )
            .unwrap();
        }

        let result = fail_dispatch_for_ready_for_input_stall(
            &shared,
            "dispatch-ready-stall",
            "AgentDesk-ready-stall",
        )
        .await
        .expect("ready-for-input failure helper");

        assert!(result.dispatch_failed);
        assert_eq!(result.card_id.as_deref(), Some("card-ready-stall"));
        assert!(result.card_marked);
        assert!(result.human_alert_sent);

        let conn = db.lock().unwrap();
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-ready-stall'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_status, "failed");

        let entry_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-ready-stall'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(entry_status, "failed");

        let (blocked_reason, metadata_raw): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT blocked_reason, metadata FROM kanban_cards WHERE id = 'card-ready-stall'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            blocked_reason.as_deref(),
            Some(READY_FOR_INPUT_STUCK_REASON)
        );
        let metadata: serde_json::Value =
            serde_json::from_str(metadata_raw.as_deref().expect("metadata after ready stall"))
                .unwrap();
        assert_eq!(metadata["labels"], "stuck_at_ready");

        let (target, reason_code, content): (String, Option<String>, String) = conn
            .query_row(
                "SELECT target, reason_code, content
                 FROM message_outbox
                 ORDER BY id DESC
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(target, "channel:555123");
        assert_eq!(reason_code.as_deref(), Some("dispatch.stuck_at_ready"));
        assert!(content.contains("dispatch-ready-stall"));
        assert!(content.contains("card-ready-stall"));
    }

    // ── #826: background-task auto-trigger relay routes through notify outbox ──

    /// When a `Bash run_in_background` (or codex `--background`) task completes
    /// and Claude Code's `<task-notification>` mechanism fires the auto turn
    /// after the bridge has already cleaned up, the watcher must enqueue the
    /// terminal response on the notify-bot outbox so the user sees it. Going
    /// through the command bot would risk other agents in the channel treating
    /// the response as an actionable directive (infinite-loop hazard).
    #[tokio::test]
    async fn background_trigger_response_enqueues_notify_outbox_row() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(987_654_321);
        let content = "PR #825 리뷰 4건 fix 완료";

        let enqueued = enqueue_background_trigger_response_to_notify_outbox(
            /*pg_pool*/ None,
            Some(&db),
            channel,
            content,
            /*data_start_offset*/ 4096,
        )
        .await;
        assert!(
            enqueued,
            "background-trigger enqueue must succeed when db is present"
        );

        let conn = db.lock().unwrap();
        let (target, stored_content, bot, source, reason_code, session_key): (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT target, content, bot, source, reason_code, session_key
                 FROM message_outbox ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .expect("expected one outbox row");

        assert_eq!(target, format!("channel:{}", channel.get()));
        assert_eq!(stored_content, content);
        assert_eq!(bot, "notify", "must use notify bot to avoid loop hazard");
        assert_eq!(source, "system");
        // #897 counter-model review P1 #3: both reason_code and session_key
        // must be populated so the lifecycle dedupe in message_outbox can arm.
        assert_eq!(reason_code.as_deref(), Some("bg_trigger.auto_turn"));
        let session_key = session_key.expect("session_key must be populated for dedupe");
        assert!(
            session_key.starts_with(&format!("bg_trigger:ch:{}:off:4096:h:", channel.get())),
            "session_key must encode channel + offset + content hash; got {session_key}"
        );
    }

    /// #897 P1 #3: consecutive background-task completions in the same
    /// channel must each produce their own outbox row — each event is a
    /// distinct tmux range, so the `session_key` (which includes
    /// `data_start_offset` and a content hash) must differ between them and
    /// the dedupe must NOT collapse legitimately-separate events into one.
    #[tokio::test]
    async fn background_trigger_response_does_not_dedupe_distinct_events() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(555_111_222);
        assert!(
            enqueue_background_trigger_response_to_notify_outbox(
                None,
                Some(&db),
                channel,
                "first completion",
                /*data_start_offset*/ 1_000,
            )
            .await
        );
        assert!(
            enqueue_background_trigger_response_to_notify_outbox(
                None,
                Some(&db),
                channel,
                "second completion",
                /*data_start_offset*/ 2_000,
            )
            .await
        );

        let count: i64 = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM message_outbox WHERE target = ?1 AND bot = 'notify'",
                [format!("channel:{}", channel.get()).as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 2,
            "consecutive events with distinct offsets/content must land as separate rows"
        );
    }

    /// #897 P1 #3: a genuine retry of the SAME tmux range (same offset +
    /// identical content) within the dedupe TTL must collapse into a single
    /// outbox row, preventing the watcher from re-enqueuing while the outbox
    /// worker is still driving the same message to Discord.
    #[tokio::test]
    async fn background_trigger_response_dedupes_identical_retry() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(666_222_333);
        assert!(
            enqueue_background_trigger_response_to_notify_outbox(
                None,
                Some(&db),
                channel,
                "same content",
                /*data_start_offset*/ 8_192,
            )
            .await
        );
        assert!(
            enqueue_background_trigger_response_to_notify_outbox(
                None,
                Some(&db),
                channel,
                "same content",
                /*data_start_offset*/ 8_192,
            )
            .await
        );

        let count: i64 = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM message_outbox WHERE target = ?1 AND bot = 'notify'",
                [format!("channel:{}", channel.get()).as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "identical retry at the same offset must dedupe to a single row"
        );
    }

    /// Empty/whitespace responses must short-circuit without writing a row —
    /// otherwise the user sees a noise notification with no content.
    #[tokio::test]
    async fn background_trigger_response_skips_empty_payload() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(111_222_333);
        assert!(
            enqueue_background_trigger_response_to_notify_outbox(
                None,
                Some(&db),
                channel,
                "   \n",
                0,
            )
            .await
        );
        let count: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM message_outbox", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "empty content must not produce an outbox row");
    }

    /// When the database is unavailable, the helper reports failure so the
    /// caller can fall back to a direct Discord send rather than silently
    /// dropping the response (#826 root cause was a silent drop).
    #[tokio::test]
    async fn background_trigger_response_reports_failure_when_db_missing() {
        let channel = ChannelId::new(999_888_777);
        let ok = enqueue_background_trigger_response_to_notify_outbox(
            /*pg_pool*/ None,
            /*db*/ None,
            channel,
            "would-have-been-delivered",
            0,
        )
        .await;
        assert!(!ok, "missing db must surface as failure to enable fallback");
    }

    /// #897 P1 #2 guard: `parse_bg_trigger_offset_from_session_key` must
    /// round-trip the exact offset that `build_bg_trigger_session_key`
    /// embedded, across a spread of offsets. Without a stable inverse, the
    /// reconciliation poll cannot identify which tmux range to re-stage
    /// after an outbox failure.
    #[test]
    fn parse_bg_trigger_offset_roundtrips_build_key() {
        for offset in [0u64, 1, 4096, 1 << 32, 1 << 48, u64::MAX] {
            let key = build_bg_trigger_session_key(42, offset, "payload");
            let parsed = parse_bg_trigger_offset_from_session_key(&key);
            assert_eq!(
                parsed,
                Some(offset),
                "offset {} must round-trip through session_key",
                offset
            );
        }
    }

    /// #897 P1 #2: malformed / foreign session_keys must not panic or
    /// produce spurious offsets — the reconcile poll has to be robust to
    /// hand-written rows or schema drift.
    #[test]
    fn parse_bg_trigger_offset_returns_none_for_non_matching_keys() {
        assert_eq!(parse_bg_trigger_offset_from_session_key(""), None);
        assert_eq!(
            parse_bg_trigger_offset_from_session_key("random:session:key"),
            None
        );
        assert_eq!(
            parse_bg_trigger_offset_from_session_key("bg_trigger:ch:1:off:not-a-number:h:abcd"),
            None
        );
        assert_eq!(
            parse_bg_trigger_offset_from_session_key("bg_trigger:ch:1:off:"),
            None
        );
    }

    /// #897 P1 #2 policy guard: rollback must pull the watermark back
    /// below the failed offset when it has moved past, but must NOT
    /// accidentally advance the watermark when it is already behind the
    /// failure. And it must never panic on a failed offset of 0.
    #[test]
    fn rollback_enqueued_offset_pulls_back_only_when_ahead_of_failure() {
        // Nothing staged → nothing to roll back.
        assert_eq!(
            rollback_enqueued_offset_for_reconciled_failures(None, 12_000),
            None,
        );

        // Watermark already at or below the failed offset → unchanged.
        assert_eq!(
            rollback_enqueued_offset_for_reconciled_failures(Some(8_000), 12_000),
            Some(8_000),
        );
        assert_eq!(
            rollback_enqueued_offset_for_reconciled_failures(Some(12_000), 12_000),
            Some(12_000),
        );

        // Watermark ahead of the failure → pulled back to just before it.
        assert_eq!(
            rollback_enqueued_offset_for_reconciled_failures(Some(20_000), 12_000),
            Some(11_999),
        );

        // Reconciled offset 0 must saturate at 0, not wrap.
        assert_eq!(
            rollback_enqueued_offset_for_reconciled_failures(Some(5), 0),
            Some(0),
        );
    }

    /// #897 P1 #2 end-to-end: after a bg_trigger row transitions to
    /// `status='failed'`, `reconcile_failed_bg_trigger_enqueues_for_channel`
    /// must (a) report the minimum offset so the watcher can roll back and
    /// (b) delete the row so it doesn't accumulate. Combined with the
    /// dedupe lookup's `status != 'failed'` filter, this lets a subsequent
    /// enqueue at the same session_key land as a fresh row.
    #[tokio::test]
    async fn reconcile_failed_bg_trigger_rows_returns_min_offset_and_deletes_them() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(777_444_111);

        // Seed three bg_trigger rows at different offsets, mark two as
        // failed and leave one pending. Only the failed pair should be
        // reconciled; the pending row stays.
        for (offset, status) in [
            (1_000u64, "failed"),
            (5_000u64, "failed"),
            (9_000u64, "pending"),
        ] {
            let session_key = build_bg_trigger_session_key(channel.get(), offset, "c");
            let target = format!("channel:{}", channel.get());
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO message_outbox
                 (target, content, bot, source, reason_code, session_key, status)
                 VALUES (?1, 'c', 'notify', 'system', 'bg_trigger.auto_turn', ?2, ?3)",
                libsql_rusqlite::params![target.as_str(), session_key.as_str(), status],
            )
            .unwrap();
        }

        let min =
            super::reconcile_failed_bg_trigger_enqueues_for_channel(None, Some(&db), channel).await;
        assert_eq!(
            min,
            Some(1_000),
            "smallest failed offset must be returned so watermark rollback lands there"
        );

        // Failed rows deleted; pending row still present.
        let (failed_left, pending_left): (i64, i64) = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT
                    SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END)
                 FROM message_outbox WHERE target = ?1",
                [format!("channel:{}", channel.get()).as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(failed_left, 0, "reconciled rows must be deleted");
        assert_eq!(pending_left, 1, "pending rows must be preserved");
    }

    /// #897 P1 #2: when there are no failed rows the reconciler returns
    /// `None` (no rollback needed) without side effects.
    #[tokio::test]
    async fn reconcile_returns_none_when_no_failed_rows() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(888_555_222);
        let min =
            super::reconcile_failed_bg_trigger_enqueues_for_channel(None, Some(&db), channel).await;
        assert_eq!(min, None);
    }

    /// #897 P1 #3 guard: `build_bg_trigger_session_key` must produce the
    /// same key for identical inputs (so dedupe can arm) and differing keys
    /// when EITHER the offset OR the content changes.
    #[test]
    fn build_bg_trigger_session_key_is_stable_and_offset_sensitive() {
        let a = build_bg_trigger_session_key(100, 4096, "payload");
        let b = build_bg_trigger_session_key(100, 4096, "payload");
        assert_eq!(a, b, "identical inputs must yield identical keys");

        let different_offset = build_bg_trigger_session_key(100, 8192, "payload");
        assert_ne!(a, different_offset, "different offset must yield a new key");

        let different_content = build_bg_trigger_session_key(100, 4096, "payload2");
        assert_ne!(
            a, different_content,
            "different content must yield a new key"
        );

        let different_channel = build_bg_trigger_session_key(200, 4096, "payload");
        assert_ne!(
            a, different_channel,
            "different channel must yield a new key"
        );
    }

    #[test]
    fn terminal_relay_decision_suppresses_internal_task_notifications_without_notify_outbox() {
        assert_eq!(
            terminal_relay_decision(true, None),
            super::TerminalRelayDecision {
                should_direct_send: true,
                should_tag_monitor_origin: false,
                should_enqueue_notify_outbox: false,
                suppressed: false,
            }
        );
        assert_eq!(
            terminal_relay_decision(true, Some(TaskNotificationKind::MonitorAutoTurn)),
            super::TerminalRelayDecision {
                should_direct_send: true,
                should_tag_monitor_origin: true,
                should_enqueue_notify_outbox: false,
                suppressed: false,
            }
        );
        assert_eq!(
            terminal_relay_decision(false, Some(TaskNotificationKind::MonitorAutoTurn)),
            super::TerminalRelayDecision {
                should_direct_send: false,
                should_tag_monitor_origin: false,
                should_enqueue_notify_outbox: false,
                suppressed: true,
            }
        );
        assert_eq!(
            terminal_relay_decision(true, Some(TaskNotificationKind::Subagent)),
            super::TerminalRelayDecision {
                should_direct_send: false,
                should_tag_monitor_origin: false,
                should_enqueue_notify_outbox: false,
                suppressed: true,
            }
        );
        // Background kind with assistant response = user-facing content after a
        // mid-turn background event (e.g. Monitor completion). Must relay (#1058).
        assert_eq!(
            terminal_relay_decision(true, Some(TaskNotificationKind::Background)),
            super::TerminalRelayDecision {
                should_direct_send: true,
                should_tag_monitor_origin: false,
                should_enqueue_notify_outbox: false,
                suppressed: false,
            }
        );
        // Background kind without any assistant response = only the tag arrived,
        // nothing to show user. Suppress.
        assert_eq!(
            terminal_relay_decision(false, Some(TaskNotificationKind::Background)),
            super::TerminalRelayDecision {
                should_direct_send: false,
                should_tag_monitor_origin: false,
                should_enqueue_notify_outbox: false,
                suppressed: true,
            }
        );
    }

    #[test]
    fn strip_inprogress_indicators_removes_spinner_tool_preview_lines() {
        let input = concat!(
            "작업 요약\n",
            "  ⠼ ⚙ TodoWrite: Todo: 1 pending, 0 in progress, 5 completed\n",
            "중요한 결과\n",
            "⠋ ⚙ Bash: cargo check\n",
            "\n"
        );

        assert_eq!(strip_inprogress_indicators(input), "작업 요약\n중요한 결과");
    }

    #[test]
    fn strip_inprogress_indicators_leaves_plain_text_unchanged() {
        let input = "작업 요약\n⚙ spinner 없이 시작한 일반 텍스트\n중요한 결과";

        assert_eq!(strip_inprogress_indicators(input), input);
    }

    #[test]
    fn suppressed_placeholder_preserves_exposed_live_edit() {
        assert_eq!(
            suppressed_placeholder_action(
                true,
                32,
                "partial response\n\n⠼ ⚙ TodoWrite: Todo: 1 pending, 0 in progress, 5 completed",
            ),
            SuppressedPlaceholderAction::Edit(format!(
                "partial response\n\n{SUPPRESSED_INTERNAL_LABEL}"
            ))
        );
        assert_eq!(
            suppressed_placeholder_action(true, 0, "status only"),
            SuppressedPlaceholderAction::Edit(format!(
                "status only\n\n{SUPPRESSED_INTERNAL_LABEL}"
            ))
        );
    }

    #[test]
    fn suppressed_placeholder_deletes_only_clean_placeholder() {
        assert_eq!(
            suppressed_placeholder_action(true, 0, ""),
            SuppressedPlaceholderAction::Delete
        );
        assert_eq!(
            suppressed_placeholder_action(false, 99, "already visible"),
            SuppressedPlaceholderAction::None
        );
    }

    fn test_placeholder_suppress_context<'a>(
        origin: PlaceholderSuppressOrigin,
        placeholder_msg_id: Option<MessageId>,
        response_sent_offset: usize,
        last_edit_text: &'a str,
        tmux_session_name: &'a str,
        task_notification_kind: Option<TaskNotificationKind>,
        reattach_offset_match: bool,
    ) -> PlaceholderSuppressContext<'a> {
        PlaceholderSuppressContext {
            origin,
            placeholder_msg_id,
            response_sent_offset,
            last_edit_text,
            inflight_state: None,
            has_active_turn: false,
            tmux_session_name,
            task_notification_kind,
            reattach_offset_match,
        }
    }

    #[test]
    fn decide_placeholder_suppression_bridge_guard_preserves_on_reattach_match() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::ActiveBridgeTurnGuard,
            Some(MessageId::new(1)),
            42,
            "already delivered body",
            "AgentDesk-claude-adk-cc",
            None,
            true,
        );
        match decide_placeholder_suppression(&ctx) {
            PlaceholderSuppressDecision::Preserve {
                reason: "reattach-offset-match",
                cleaned_body,
            } => {
                assert_eq!(cleaned_body, "already delivered body");
            }
            other => panic!("expected Preserve reattach-offset-match, got {other:?}"),
        }
    }

    #[test]
    fn decide_placeholder_suppression_preserve_strips_inprogress_indicators() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::ActiveBridgeTurnGuard,
            Some(MessageId::new(1)),
            42,
            "real body\n\n⠼ ⚙ TodoWrite: Todo: 1 pending, 0 in progress, 5 completed\n",
            "AgentDesk-claude-adk-cc",
            None,
            true,
        );
        match decide_placeholder_suppression(&ctx) {
            PlaceholderSuppressDecision::Preserve { cleaned_body, .. } => {
                assert_eq!(cleaned_body, "real body");
                assert!(!cleaned_body.contains("⠼"));
                assert!(!cleaned_body.contains("⚙"));
            }
            other => panic!("expected Preserve with stripped body, got {other:?}"),
        }
    }

    #[test]
    fn decide_placeholder_suppression_bridge_guard_falls_through_to_edit_label_without_match() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::ActiveBridgeTurnGuard,
            Some(MessageId::new(1)),
            42,
            "visible body",
            "AgentDesk-claude-adk-cc",
            None,
            false,
        );
        match decide_placeholder_suppression(&ctx) {
            PlaceholderSuppressDecision::Edit(content) => {
                assert!(content.contains(SUPPRESSED_INTERNAL_LABEL))
            }
            other => panic!("expected Edit with label, got {other:?}"),
        }
    }

    #[test]
    fn decide_placeholder_suppression_task_notification_preserves_background_body() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::TaskNotificationTerminal,
            Some(MessageId::new(1)),
            42,
            "live user-facing content",
            "AgentDesk-claude-adk-cc",
            Some(TaskNotificationKind::Background),
            false,
        );
        match decide_placeholder_suppression(&ctx) {
            PlaceholderSuppressDecision::Preserve {
                reason: "background-or-subagent-kind",
                cleaned_body,
            } => {
                assert_eq!(cleaned_body, "live user-facing content");
            }
            other => panic!("expected Preserve background-or-subagent-kind, got {other:?}"),
        }
    }

    #[test]
    fn decide_placeholder_suppression_task_notification_preserves_subagent_body() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::TaskNotificationTerminal,
            Some(MessageId::new(1)),
            42,
            "subagent body",
            "AgentDesk-claude-adk-cc",
            Some(TaskNotificationKind::Subagent),
            false,
        );
        match decide_placeholder_suppression(&ctx) {
            PlaceholderSuppressDecision::Preserve {
                reason: "background-or-subagent-kind",
                cleaned_body,
            } => {
                assert_eq!(cleaned_body, "subagent body");
            }
            other => panic!("expected Preserve background-or-subagent-kind, got {other:?}"),
        }
    }

    #[test]
    fn decide_placeholder_suppression_task_notification_edits_for_monitor_auto_turn() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::TaskNotificationTerminal,
            Some(MessageId::new(1)),
            42,
            "monitor-auto body",
            "AgentDesk-claude-adk-cc",
            Some(TaskNotificationKind::MonitorAutoTurn),
            false,
        );
        match decide_placeholder_suppression(&ctx) {
            PlaceholderSuppressDecision::Edit(content) => {
                assert!(content.contains(SUPPRESSED_INTERNAL_LABEL))
            }
            other => panic!("expected Edit with label, got {other:?}"),
        }
    }

    #[test]
    fn decide_placeholder_suppression_task_notification_deletes_unexposed_placeholder() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::TaskNotificationTerminal,
            Some(MessageId::new(1)),
            0,
            "",
            "AgentDesk-claude-adk-cc",
            Some(TaskNotificationKind::MonitorAutoTurn),
            false,
        );
        assert_eq!(
            decide_placeholder_suppression(&ctx),
            PlaceholderSuppressDecision::Delete
        );
    }

    #[test]
    fn orphan_suppressed_placeholder_reconcile_rewrites_terminal_marker() {
        let tmux_name = ProviderKind::Codex.build_tmux_session_name("adk-cdx-t42");
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            7,
            9,
            11,
            "background task".to_string(),
            Some("session-1".to_string()),
            Some(tmux_name.clone()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            128,
        );
        state.full_response = "already delivered\npending tail".to_string();
        state.response_sent_offset = "already delivered\n".len();
        state.current_tool_line =
            Some("⚙ TodoWrite: Todo: 1 pending, 0 in progress, 5 completed".to_string());

        let action = orphan_suppressed_placeholder_action(&state, false, &tmux_name);

        assert_eq!(
            action,
            SuppressedPlaceholderAction::Edit(format!(
                "pending tail\n\n{SUPPRESSED_RESTART_LABEL}"
            ))
        );
    }

    #[test]
    fn internal_suppress_and_orphan_reconcile_use_distinct_labels() {
        let tmux_name = ProviderKind::Codex.build_tmux_session_name("adk-cdx-t42");
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            7,
            9,
            11,
            "background task".to_string(),
            Some("session-1".to_string()),
            Some(tmux_name.clone()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            128,
        );
        state.full_response = "already delivered\nshared tail".to_string();
        state.response_sent_offset = "already delivered\n".len();

        assert_eq!(
            suppressed_placeholder_action(true, state.response_sent_offset, "shared tail"),
            SuppressedPlaceholderAction::Edit(format!(
                "shared tail\n\n{SUPPRESSED_INTERNAL_LABEL}"
            ))
        );
        assert_eq!(
            orphan_suppressed_placeholder_action(&state, false, &tmux_name),
            SuppressedPlaceholderAction::Edit(format!("shared tail\n\n{SUPPRESSED_RESTART_LABEL}"))
        );
    }

    #[test]
    fn orphan_suppressed_placeholder_reconcile_skips_active_turns() {
        let tmux_name = ProviderKind::Codex.build_tmux_session_name("adk-cdx-t42");
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            7,
            9,
            11,
            "background task".to_string(),
            Some("session-1".to_string()),
            Some(tmux_name.clone()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            128,
        );
        state.full_response = "already delivered\npending tail".to_string();
        state.response_sent_offset = "already delivered\n".len();

        assert_eq!(
            orphan_suppressed_placeholder_action(&state, true, &tmux_name),
            SuppressedPlaceholderAction::None
        );
    }

    #[test]
    fn restored_watcher_seed_uses_existing_placeholder_and_offset() {
        let tmux_name = ProviderKind::Codex.build_tmux_session_name("adk-cdx-t42");
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            7,
            9,
            11,
            "continue".to_string(),
            Some("session-1".to_string()),
            Some(tmux_name.clone()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            128,
        );
        state.full_response = "already delivered\npending".to_string();
        state.response_sent_offset = "already delivered\n".len();
        state.task_notification_kind = Some(TaskNotificationKind::Background);

        let restored = restored_watcher_turn_from_inflight(&state, &tmux_name, true)
            .expect("valid inflight should seed watcher resume");
        let seed = watcher_stream_seed(Some(restored));

        assert_eq!(seed.placeholder_msg_id, Some(MessageId::new(11)));
        assert_eq!(seed.response_sent_offset, "already delivered\n".len());
        assert_eq!(seed.full_response, "already delivered\npending");
        assert_eq!(
            seed.task_notification_kind,
            Some(TaskNotificationKind::Background)
        );
        assert!(seed.finish_mailbox_on_completion);
    }

    #[test]
    fn restored_watcher_seed_rejects_mismatched_tmux_session() {
        let tmux_name = ProviderKind::Codex.build_tmux_session_name("adk-cdx-t42");
        let state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            7,
            9,
            11,
            "continue".to_string(),
            Some("session-1".to_string()),
            Some(tmux_name),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            128,
        );

        assert!(
            restored_watcher_turn_from_inflight(&state, "AgentDesk-codex-other-channel", true)
                .is_none()
        );
    }

    #[tokio::test]
    async fn monitor_auto_turn_suppress_enqueues_notify_outbox_row() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(987_000_111);

        let enqueued = enqueue_monitor_auto_turn_suppressed_notification(
            None,
            Some(&db),
            channel,
            "monitor-session",
            14_900,
            7,
        );
        assert!(enqueued);

        let row = {
            if let Ok(conn) = db.lock() {
                conn.query_row(
                    "SELECT target, content, bot, source, reason_code, session_key
                     FROM message_outbox ORDER BY id DESC LIMIT 1",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                        ))
                    },
                )
                .ok()
            } else {
                None
            }
        };

        assert_eq!(
            row,
            Some((
                format!("channel:{}", channel.get()),
                "🔔 Monitor 완료: monitor-session (events=7)".to_string(),
                "notify".to_string(),
                "system".to_string(),
                Some(MONITOR_AUTO_TURN_REASON_CODE.to_string()),
                Some(format!("monitor_auto_turn:ch:{}:off:14900", channel.get())),
            ))
        );
    }

    #[tokio::test]
    async fn monitor_auto_turn_normal_relay_does_not_request_notify_outbox() {
        let db = crate::db::test_db();
        let decision = terminal_relay_decision(true, Some(TaskNotificationKind::MonitorAutoTurn));

        assert!(decision.should_direct_send);
        assert!(!decision.suppressed);

        let count = {
            if let Ok(conn) = db.lock() {
                conn.query_row("SELECT COUNT(*) FROM message_outbox", [], |row| {
                    row.get::<_, i64>(0)
                })
                .ok()
            } else {
                None
            }
        };
        assert_eq!(count, Some(0));
    }

    #[tokio::test]
    async fn monitor_auto_turn_defers_until_user_turn_finishes_and_notifies()
    -> Result<(), Box<dyn std::error::Error>> {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir()?;
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let db = crate::db::test_db();
        let shared = super::super::make_shared_data_for_tests_with_storage(Some(db.clone()), None);
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(987_000_222);
        let user_started = super::super::mailbox_try_start_turn(
            &shared,
            channel,
            Arc::new(CancelToken::new()),
            UserId::new(42),
            MessageId::new(100),
        )
        .await;
        assert!(user_started);

        let cancel = Arc::new(AtomicBool::new(false));
        let shared_for_task = shared.clone();
        let cancel_for_task = cancel.clone();
        let provider_for_task = provider.clone();
        let handle = tokio::spawn(async move {
            start_monitor_auto_turn_when_available(
                &shared_for_task,
                &provider_for_task,
                channel,
                24_000,
                cancel_for_task.as_ref(),
            )
            .await
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;
        let deferred_count = {
            if let Ok(conn) = db.lock() {
                conn.query_row(
                    "SELECT COUNT(*) FROM message_outbox WHERE reason_code = ?1",
                    [MONITOR_AUTO_TURN_DEFERRED_REASON_CODE],
                    |row| row.get::<_, i64>(0),
                )
                .ok()
            } else {
                None
            }
        };
        assert_eq!(deferred_count, Some(1));

        let finish = super::super::mailbox_finish_turn(&shared, &provider, channel).await;
        assert!(finish.removed_token.is_some());

        let start = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await??;
        assert_eq!(
            start,
            super::MonitorAutoTurnStart {
                acquired: true,
                deferred: true,
            }
        );

        let snapshot = super::super::mailbox_snapshot(&shared, channel).await;
        assert!(snapshot.cancel_token.is_some());
        assert_eq!(snapshot.active_request_owner, Some(UserId::new(1)));
        assert_eq!(
            snapshot.active_user_message_id,
            Some(MessageId::new(24_000))
        );

        finish_monitor_auto_turn(&shared, &provider, channel).await;
        assert!(
            !super::super::mailbox_has_active_turn(&shared, channel).await,
            "monitor mailbox claim must clear after the monitor turn finishes"
        );

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        Ok(())
    }

    #[tokio::test]
    async fn user_message_queues_while_monitor_auto_turn_is_active()
    -> Result<(), Box<dyn std::error::Error>> {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir()?;
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(987_000_333);
        let cancel = AtomicBool::new(false);
        let start =
            start_monitor_auto_turn_when_available(&shared, &provider, channel, 31_000, &cancel)
                .await;
        assert_eq!(
            start,
            super::MonitorAutoTurnStart {
                acquired: true,
                deferred: false,
            }
        );
        assert!(super::super::mailbox_has_active_turn(&shared, channel).await);

        let queued = super::super::mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel,
            super::super::Intervention {
                author_id: UserId::new(99),
                message_id: MessageId::new(200),
                source_message_ids: vec![MessageId::new(200)],
                text: "queued behind monitor".to_string(),
                mode: super::super::InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            },
        )
        .await;
        assert!(queued);

        let snapshot = super::super::mailbox_snapshot(&shared, channel).await;
        assert!(snapshot.cancel_token.is_some());
        assert_eq!(snapshot.intervention_queue.len(), 1);

        finish_monitor_auto_turn(&shared, &provider, channel).await;

        let snapshot = super::super::mailbox_snapshot(&shared, channel).await;
        assert!(snapshot.cancel_token.is_none());
        assert_eq!(snapshot.intervention_queue.len(), 1);
        let next = super::super::mailbox_take_next_soft_intervention(&shared, &provider, channel)
            .await
            .map(|(intervention, _)| intervention.text);
        assert_eq!(next.as_deref(), Some("queued behind monitor"));

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        Ok(())
    }

    #[test]
    fn process_watcher_lines_classifies_background_task_notification() {
        let mut buffer = concat!(
            "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"bg-42\",\"status\":\"completed\",\"summary\":\"CI green\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"PR #825 리뷰 반영 완료\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert_eq!(
            outcome.task_notification_kind,
            Some(TaskNotificationKind::Background)
        );
        assert_eq!(full_response, "PR #825 리뷰 반영 완료");
    }

    #[test]
    fn process_watcher_lines_classifies_subagent_task_notification() {
        let mut buffer = concat!(
            "{\"type\":\"system\",\"subtype\":\"task_started\",\"task_id\":\"sub-1\",\"task_type\":\"local_agent\"}\n",
            "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"sub-1\",\"status\":\"completed\",\"summary\":\"Subagent finished\"}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert_eq!(
            outcome.task_notification_kind,
            Some(TaskNotificationKind::Subagent)
        );
        assert_eq!(full_response, "done");
    }

    #[test]
    fn process_watcher_lines_classifies_monitor_auto_turn_task_notification() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_mon_1\",\"name\":\"Monitor\",\"input\":{\"command\":\"gh pr view\"}}]}}\n",
            "{\"type\":\"system\",\"subtype\":\"task_started\",\"task_id\":\"mon-1\",\"tool_use_id\":\"toolu_mon_1\",\"task_type\":\"tool\"}\n",
            "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"mon-1\",\"status\":\"completed\",\"summary\":\"Monitor event: PR updated\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"PR #938 상태 갱신 완료\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert_eq!(
            outcome.task_notification_kind,
            Some(TaskNotificationKind::MonitorAutoTurn)
        );
        assert_eq!(full_response, "PR #938 상태 갱신 완료");
    }

    #[test]
    fn process_watcher_lines_leaves_task_notification_kind_empty_for_foreground_turn() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert_eq!(outcome.task_notification_kind, None);
        assert_eq!(full_response, "hello");
    }

    /// #826 P1 #2 regression guard: when the notify-bot outbox enqueue fails
    /// AND no direct-send fallback reaches Discord, the watcher MUST leave
    /// BOTH offset watermarks untouched so the same tmux range can be
    /// re-relayed on the next scan. Advancing the canonical relayed offset
    /// here is the bug that permanently loses a completion notification when
    /// notify-bot is unavailable.
    #[test]
    fn notify_path_does_not_advance_offset_on_enqueue_failure_without_fallback() {
        // Enqueue failed AND direct-send fallback also failed → leave both
        // watermarks alone (the content is still in flight from the watcher's
        // point of view; next tick must retry).
        assert_eq!(
            notify_path_offset_advance_decision(
                /*has_current_response*/ true, /*enqueue_succeeded*/ false,
                /*direct_send_delivered*/ false,
            ),
            OffsetAdvanceDecision {
                advance_relayed: false,
                advance_enqueued: false
            },
            "enqueue-fail + fallback-fail with content must leave both watermarks untouched"
        );

        // Enqueue SUCCEEDED but no foreground delivery confirmation yet —
        // advance ONLY the enqueue watermark so the outbox row is deduped on
        // the next tick, while the canonical relayed watermark waits for
        // actual Discord delivery. THIS is the P1 #2 fix: the original code
        // treated enqueue success as a delivery-equivalent and advanced the
        // relayed offset.
        assert_eq!(
            notify_path_offset_advance_decision(
                /*has_current_response*/ true, /*enqueue_succeeded*/ true,
                /*direct_send_delivered*/ false,
            ),
            OffsetAdvanceDecision {
                advance_relayed: false,
                advance_enqueued: true
            },
            "enqueue success without delivery confirmation must NOT advance last_relayed_offset"
        );

        // Enqueue failed but fallback direct-send reached Discord → both
        // watermarks lift together.
        assert_eq!(
            notify_path_offset_advance_decision(true, false, true),
            OffsetAdvanceDecision {
                advance_relayed: true,
                advance_enqueued: true
            }
        );

        // Both succeeded (uncommon but possible) → lock-step advance.
        assert_eq!(
            notify_path_offset_advance_decision(true, true, true),
            OffsetAdvanceDecision {
                advance_relayed: true,
                advance_enqueued: true
            }
        );

        // No content to deliver → trivially safe to advance past the empty
        // range (preserves the original single-offset behaviour so the
        // watcher doesn't spin on an empty turn).
        assert_eq!(
            notify_path_offset_advance_decision(false, false, false),
            OffsetAdvanceDecision {
                advance_relayed: true,
                advance_enqueued: true
            }
        );
    }

    /// #826 P1 #2 regression guard: the dedupe-floor in the watcher's
    /// duplicate-relay guard must be `max(last_relayed_offset,
    /// last_enqueued_offset)`. After a notify-path enqueue advances ONLY the
    /// enqueue watermark, a later tick that re-reads the same tmux range
    /// must still be suppressed — otherwise we'd double-enqueue the same
    /// response while the outbox worker was still delivering the first copy.
    #[test]
    fn enqueued_offset_gates_dedupe_even_without_relayed_advance() {
        // Mirror the max()-dedupe logic from the watcher loop (kept inline
        // there for hot-path performance — this test pins the invariant).
        fn dedupe_floor(relayed: Option<u64>, enqueued: Option<u64>) -> Option<u64> {
            match (relayed, enqueued) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            }
        }

        // Enqueue advanced but relayed did not — dedupe still protects
        // against re-emit of the same start offset.
        assert_eq!(
            dedupe_floor(/*relayed*/ None, /*enqueued*/ Some(4096)),
            Some(4096),
            "enqueue-only advance must still guard the dedupe floor"
        );

        // Relayed leapfrogs a stale enqueue marker (e.g. a genuine
        // foreground delivery arrived later) — floor follows the higher
        // watermark.
        assert_eq!(dedupe_floor(Some(8192), Some(4096)), Some(8192));

        // Both absent — no floor, watcher may relay freely.
        assert_eq!(dedupe_floor(None, None), None);
    }
}
