use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::{StatusEvent, TaskNotificationKind};
use crate::services::message_outbox::{
    LIFECYCLE_NOTIFIER_SOURCE, OutboxMessage, enqueue_lifecycle_notification_best_effort,
    enqueue_outbox_best_effort,
};
use crate::services::observability::turn_lifecycle::TurnEvent;
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};
use crate::services::session_backend::{
    StreamLineState, classify_task_notification_kind, observe_stream_context,
};
use crate::services::tmux_diagnostics::{
    build_tmux_death_diagnostic, read_tmux_exit_reason, record_tmux_exit_reason,
    tmux_exit_reason_is_normal_completion, tmux_session_exists, tmux_session_has_live_pane,
};

use super::formatting::{
    ReplaceLongMessageOutcome, build_streaming_placeholder_text, format_tool_input,
    plan_streaming_rollover, replace_long_message_raw_with_outcome, truncate_str,
};
use super::placeholder_cleanup::{
    PlaceholderCleanupOperation, PlaceholderCleanupOutcome, PlaceholderCleanupRecord,
    classify_delete_error,
};
use super::placeholder_live_events::{
    RecentPlaceholderEvent, events_from_json, status_events_from_json,
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
use super::{
    SharedData, TmuxWatcherHandle, TmuxWatcherRegistry, lock_tmux_watcher_registry, rate_limit_wait,
};
// Keep the extracted lifecycle code as a tmux child module until the remaining
// watcher helpers it calls are split out of this file.
#[path = "tmux_reattach_offsets.rs"]
mod tmux_reattach_offsets;
#[path = "tmux_session_files.rs"]
mod tmux_session_files;
#[path = "watchers/lifecycle.rs"]
mod watcher_lifecycle;

use self::tmux_reattach_offsets::matching_recent_watcher_reattach_offset;
pub(super) use self::tmux_session_files::read_generation_file_mtime_ns;
pub(super) use self::tmux_session_files::session_panel_instance_key;
pub(crate) use self::tmux_session_files::write_spawn_nonce;
use self::tmux_session_files::{
    preserve_mtime_after_write, reset_stale_local_relay_offset_if_output_regressed,
    reset_stale_relay_watermark_if_output_regressed, sweep_orphan_session_files,
};
use self::watcher_lifecycle::*;
pub(in crate::services::discord) use self::watcher_lifecycle::{
    claim_or_reuse_watcher, clear_recovery_handled_channels,
    fail_dispatch_for_ready_for_input_stall, refresh_session_heartbeat_from_tmux_output,
    restore_tmux_watchers, session_belongs_to_current_runtime, store_recovery_handled_channels,
};
use super::watcher_lifecycle_decision::*;
const READY_FOR_INPUT_IDLE_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const SOFT_TERMINAL_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(1500);
pub(super) const WATCHER_ACTIVITY_HEARTBEAT_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);
const READY_FOR_INPUT_STUCK_LABEL: &str = "stuck_at_ready";
const READY_FOR_INPUT_STUCK_REASON: &str = "agent ended at Ready for input without commit/push";
const SUPPRESSED_INTERNAL_LABEL: &str = "(자동으로 처리된 내부 작업이라 여기서 멈췄어요)";
const SUPPRESSED_RESTART_LABEL: &str =
    "(서버가 재시작되면서 답변이 중간에 멈췄어요 — 필요하시면 다시 질문해 주세요)";
#[path = "tmux_kill_policy.rs"]
mod tmux_kill_policy;
#[allow(unused_imports)]
pub(super) use self::tmux_kill_policy::{
    CANCEL_TEARDOWN_GRACE_BYTES, MONITOR_AUTO_TURN_DEFERRED_REASON_CODE,
    MONITOR_AUTO_TURN_REASON_CODE, RECENT_TURN_STOP_METADATA_FALLBACK_TTL,
    TMUX_LIVENESS_PROBE_INTERVAL, cancel_induced_watcher_death, cancel_induced_watcher_death_async,
    recent_turn_stop_for_channel, recent_turn_stop_for_watcher_range, record_recent_turn_stop,
    tmux_output_offset,
};
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct WatcherLineOutcome {
    pub found_result: bool,
    pub terminal_kind: Option<WatcherTerminalKind>,
    pub soft_terminal_candidate: bool,
    pub is_prompt_too_long: bool,
    pub is_auth_error: bool,
    pub auth_error_message: Option<String>,
    pub is_provider_overloaded: bool,
    pub provider_overload_message: Option<String>,
    pub stale_resume_detected: bool,
    pub auto_compacted: bool,
    pub task_notification_kind: Option<TaskNotificationKind>,
    pub assistant_text_seen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WatcherTerminalKind {
    HardResult,
    SoftStopHookSummary,
    SoftUserBoundary,
    AuthError,
    ProviderOverload,
}

impl WatcherTerminalKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::HardResult => "hard_result",
            Self::SoftStopHookSummary => "soft_stop_hook_summary",
            Self::SoftUserBoundary => "soft_user_boundary",
            Self::AuthError => "auth_error",
            Self::ProviderOverload => "provider_overload",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct RestoredWatcherTurn {
    current_msg_id: MessageId,
    status_message_id: Option<MessageId>,
    response_sent_offset: usize,
    full_response: String,
    last_edit_text: String,
    task_notification_kind: Option<TaskNotificationKind>,
    finish_mailbox_on_completion: bool,
    /// #3107 codex re-review (P2#3): the #3099 hourglass anchor from the
    /// restored inflight, carried so a watcher-owned re-acquire (after the row
    /// is cleared mid-turn) can re-pin it instead of orphaning the `⏳`.
    pub(super) injected_prompt_message_id: Option<u64>,
}

#[derive(Debug)]
struct WatcherStreamSeed {
    placeholder_msg_id: Option<MessageId>,
    status_panel_msg_id: Option<MessageId>,
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
        status_message_id: state.status_message_id.map(MessageId::new),
        response_sent_offset,
        full_response: state.full_response.clone(),
        last_edit_text: reconstructed_inflight_placeholder_body(state),
        task_notification_kind: state.task_notification_kind,
        finish_mailbox_on_completion,
        injected_prompt_message_id: state.injected_prompt_message_id,
    })
}

fn watcher_stream_seed(restored_turn: Option<RestoredWatcherTurn>) -> WatcherStreamSeed {
    match restored_turn {
        Some(restored) => WatcherStreamSeed {
            placeholder_msg_id: Some(restored.current_msg_id),
            status_panel_msg_id: restored.status_message_id,
            response_sent_offset: restored.response_sent_offset,
            full_response: restored.full_response,
            last_edit_text: restored.last_edit_text,
            task_notification_kind: restored.task_notification_kind,
            finish_mailbox_on_completion: restored.finish_mailbox_on_completion,
        },
        None => WatcherStreamSeed {
            placeholder_msg_id: None,
            status_panel_msg_id: None,
            response_sent_offset: 0,
            full_response: String::new(),
            last_edit_text: String::new(),
            task_notification_kind: None,
            finish_mailbox_on_completion: false,
        },
    }
}

fn should_discard_restored_seed_for_idle_direct_prompt(
    restored_turn_present: bool,
    prompt_anchor_present: bool,
) -> bool {
    restored_turn_present && prompt_anchor_present
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

fn tmux_death_lifecycle_notification_reason(reason: Option<&str>) -> Option<&str> {
    let reason = reason?.trim();
    if reason.is_empty() {
        return None;
    }

    let reason = reason
        .strip_prefix('[')
        .and_then(|s| s.find("] ").map(|i| &s[i + 2..]))
        .unwrap_or(reason)
        .trim();
    if reason.is_empty() || reason.eq_ignore_ascii_case("unknown") {
        return None;
    }

    let lower = reason.to_ascii_lowercase();
    if tmux_exit_reason_is_normal_completion(reason) || lower.contains("force-kill") {
        return None;
    }

    Some(reason)
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

async fn emit_context_compacted_lifecycle_from_watcher(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    model: Option<&str>,
    usage: Option<TurnTokenUsage>,
) -> bool {
    let ctx_cfg = super::adk_session::fetch_context_thresholds(shared.api_port).await;
    // Provider auto-compaction output does not expose the exact pre-compaction
    // token total, so record the configured trigger threshold as the lower-bound
    // before percentage.
    let before_pct = ctx_cfg.compact_pct_for(provider);
    let context_window = provider.resolve_context_window(model);
    let after_pct = usage.and_then(|usage| {
        let pct = super::adk_session::context_usage_percent(
            usage.context_occupancy_input_tokens(),
            context_window,
        )
        .min(before_pct);
        (pct > 0).then_some(pct)
    });
    let emitted = super::adk_session::emit_context_compacted_lifecycle_for_inflight(
        shared, channel_id, provider, before_pct, after_pct,
    )
    .await;

    if !emitted {
        let target = format!("channel:{}", channel_id.get());
        let details = super::adk_session::context_compaction_details(before_pct, after_pct);
        let content = TurnEvent::ContextCompacted(details)
            .notification_content()
            .unwrap_or_else(|| "📦 컨텍스트 자동 압축".to_string());
        enqueue_lifecycle_notification_best_effort(
            sqlite_runtime_db(shared.as_ref()),
            shared.pg_pool.as_ref(),
            target.as_str(),
            None,
            "lifecycle.context_compacted",
            content.as_str(),
        );
        tracing::warn!(
            channel_id = channel_id.get(),
            source = LIFECYCLE_NOTIFIER_SOURCE,
            "context compacted lifecycle emit skipped; enqueued fallback notification"
        );
    }

    true
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
    assistant_text_seen: bool,
) -> TerminalRelayDecision {
    let is_task_notification = task_notification_kind.is_some();
    let has_user_visible_assistant_text = if is_task_notification {
        // Task-notification routing is source-neutral: monitor/background/
        // subagent events all relay only when the session buffer/inflight
        // state has user-visible assistant text. The kind remains available
        // for audit/format decoration, not for deciding relay lifetime.
        has_assistant_response && assistant_text_seen
    } else {
        has_assistant_response
    };

    TerminalRelayDecision {
        should_direct_send: has_user_visible_assistant_text,
        should_tag_monitor_origin: matches!(
            task_notification_kind,
            Some(TaskNotificationKind::MonitorAutoTurn)
        ) && has_user_visible_assistant_text,
        should_enqueue_notify_outbox: false,
        suppressed: is_task_notification && !has_user_visible_assistant_text,
    }
}

fn watcher_should_render_status_only_placeholder(
    placeholder_exists: bool,
    current_tool_line: Option<&str>,
    task_notification_kind: Option<TaskNotificationKind>,
) -> bool {
    placeholder_exists
        || current_tool_line.is_some_and(|line| !line.trim().is_empty())
        || task_notification_kind.is_some()
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

fn rewrite_placeholder_as_terminal_suppressed(text: &str, label: &str) -> String {
    let cleaned = strip_inprogress_indicators(text);
    let trimmed = cleaned.trim_end();
    if trimmed.ends_with(label) {
        return trimmed.to_string();
    }
    if trimmed.is_empty() {
        // #1009: label itself may exceed DISCORD_MSG_LIMIT when monitor entries
        // balloon — guard here too (the with-body branch below already guards).
        let limit = super::DISCORD_MSG_LIMIT;
        if label.len() > limit {
            return truncate_str(label, limit);
        }
        return label.to_string();
    }

    let suffix = format!("\n\n{label}");
    let max_base_len = super::DISCORD_MSG_LIMIT.saturating_sub(suffix.len());
    let base = if trimmed.len() > max_base_len {
        truncate_str(trimmed, max_base_len)
    } else {
        trimmed.to_string()
    };
    let composed = format!("{base}{suffix}");
    // Final belt-and-suspenders guard (rare: suffix.len() ≥ DISCORD_MSG_LIMIT).
    if composed.len() > super::DISCORD_MSG_LIMIT {
        truncate_str(&composed, super::DISCORD_MSG_LIMIT)
    } else {
        composed
    }
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
            // #1708: Background, Subagent, and MonitorAutoTurn task notifications
            // are all auto-fired by tooling, not by the user. If the main turn is
            // streaming user-facing body when one of them terminates, we must
            // preserve that body — otherwise the SUPPRESSED_INTERNAL_LABEL edit
            // overwrites the actual response (Monitor stream-end was the trigger
            // observed in the 2026-05-04 adk-cc incident).
            let preserves_body = matches!(
                ctx.task_notification_kind,
                Some(
                    TaskNotificationKind::Background
                        | TaskNotificationKind::Subagent
                        | TaskNotificationKind::MonitorAutoTurn
                )
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
                        reason: "auto-task-notification-kind",
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
    provider: &ProviderKind,
    tmux_session_name: &str,
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
                    delete_nonterminal_placeholder(
                        http,
                        channel_id,
                        shared,
                        provider,
                        tmux_session_name,
                        msg_id,
                        origin.log_scope(),
                    )
                    .await;
                } else {
                    edit_preserve_placeholder(
                        http,
                        channel_id,
                        shared,
                        provider,
                        tmux_session_name,
                        msg_id,
                        &cleaned_body,
                        origin.log_scope(),
                    )
                    .await;
                }
            }
        }
        PlaceholderSuppressDecision::Delete => {
            if let Some(msg_id) = placeholder_msg_id {
                delete_terminal_placeholder(
                    http,
                    channel_id,
                    shared,
                    provider,
                    tmux_session_name,
                    msg_id,
                    origin.log_scope(),
                )
                .await;
            }
        }
        PlaceholderSuppressDecision::Edit(content) => {
            if let Some(msg_id) = placeholder_msg_id {
                edit_terminal_placeholder(
                    http,
                    channel_id,
                    shared,
                    provider,
                    tmux_session_name,
                    msg_id,
                    &content,
                    origin.log_scope(),
                )
                .await;
            }
        }
    }
}

fn record_placeholder_cleanup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: &str,
    operation: PlaceholderCleanupOperation,
    outcome: PlaceholderCleanupOutcome,
    source: &'static str,
) {
    if let PlaceholderCleanupOutcome::Failed { class, detail } = &outcome {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ placeholder cleanup {} failed ({}) for channel {} msg {}: {}",
            operation.as_str(),
            class.as_str(),
            channel_id.get(),
            message_id.get(),
            detail
        );
    }
    shared.placeholder_cleanup.record(PlaceholderCleanupRecord {
        provider: provider.clone(),
        channel_id,
        message_id,
        tmux_session_name: Some(tmux_session_name.to_string()),
        operation,
        outcome,
        source,
    });
}

async fn delete_terminal_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    delete_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        PlaceholderCleanupOperation::DeleteTerminal,
        source,
    )
    .await
}

async fn delete_nonterminal_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    delete_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        PlaceholderCleanupOperation::DeleteNonterminal,
        source,
    )
    .await
}

async fn delete_placeholder_with_operation(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    operation: PlaceholderCleanupOperation,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    let outcome = match channel_id.delete_message(http, message_id).await {
        Ok(_) => PlaceholderCleanupOutcome::Succeeded,
        Err(error) => classify_delete_error(&error.to_string()),
    };
    record_placeholder_cleanup(
        shared,
        provider,
        channel_id,
        message_id,
        tmux_session_name,
        operation,
        outcome.clone(),
        source,
    );
    outcome
}

async fn edit_terminal_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    content: &str,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    edit_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        content,
        PlaceholderCleanupOperation::EditTerminal,
        source,
    )
    .await
}

async fn edit_preserve_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    content: &str,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    edit_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        content,
        PlaceholderCleanupOperation::EditPreserve,
        source,
    )
    .await
}

async fn edit_placeholder_with_operation(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    content: &str,
    operation: PlaceholderCleanupOperation,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    rate_limit_wait(shared, channel_id).await;
    let outcome =
        match super::http::edit_channel_message(http, channel_id, message_id, content).await {
            Ok(_) => PlaceholderCleanupOutcome::Succeeded,
            Err(error) => PlaceholderCleanupOutcome::failed(error.to_string()),
        };
    record_placeholder_cleanup(
        shared,
        provider,
        channel_id,
        message_id,
        tmux_session_name,
        operation,
        outcome.clone(),
        source,
    );
    outcome
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FallbackPlaceholderCleanupDecision {
    RelayCommitted,
    PreserveInflightForCleanupRetry,
}

fn fallback_placeholder_cleanup_decision(
    cleanup: &PlaceholderCleanupOutcome,
) -> FallbackPlaceholderCleanupDecision {
    if cleanup.is_committed() {
        FallbackPlaceholderCleanupDecision::RelayCommitted
    } else {
        FallbackPlaceholderCleanupDecision::PreserveInflightForCleanupRetry
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

/// #1009: Lifecycle-notice variant of the shared monitor summary line. Calls
/// `format_monitor_suppressed_label` for the trailing summary so the
/// suppressed-placeholder edit body and the lifecycle notify-outbox row use
/// identical copy (DRY enforcement). The `label` (channel/tmux session name)
/// stays as the human-readable scope prefix; `entry_keys` come from the
/// channel's `MonitoringStore` snapshot.
fn monitor_auto_turn_completion_notice(
    label: &str,
    event_count: usize,
    entry_keys: &[String],
) -> String {
    let summary = format_monitor_suppressed_label(event_count, entry_keys);
    format!("{summary} · 대상: {label}")
}

/// #1009: Shared formatter for the monitor auto-turn suppressed-placeholder
/// summary line. Produces:
///   - `🔔 Monitor n회 처리 · 다음 모니터: {key1, key2, ...}` when entries > 0
///   - `🔔 Monitor n회 처리 · (등록된 모니터 없음)` when entries == 0
/// Entry keys are emitted in the order the store returns them. Called from
/// both `suppressed_placeholder_action` (via the monitor-aware wrapper) and
/// the lifecycle-notice path so both channels use identical copy.
pub(super) fn format_monitor_suppressed_label(event_count: usize, entry_keys: &[String]) -> String {
    if entry_keys.is_empty() {
        format!("🔔 Monitor {}회 처리 · (등록된 모니터 없음)", event_count)
    } else {
        format!(
            "🔔 Monitor {}회 처리 · 다음 모니터: {{{}}}",
            event_count,
            entry_keys.join(", ")
        )
    }
}

/// #1009: Compose the full suppressed-placeholder body for monitor auto-turn.
/// Rebuilds the existing `last_edit_text`-preserve + label-append behaviour
/// from `rewrite_placeholder_as_terminal_suppressed` but with the dynamic
/// `format_monitor_suppressed_label` text. Length-guarded against
/// `DISCORD_MSG_LIMIT` by the underlying rewrite helper.
pub(super) fn format_monitor_suppressed_body(
    last_edit_text: &str,
    event_count: usize,
    entry_keys: &[String],
) -> String {
    let label = format_monitor_suppressed_label(event_count, entry_keys);
    rewrite_placeholder_as_terminal_suppressed(last_edit_text, &label)
}

/// #1009: System-level hint injected once per monitor auto-turn entry so the
/// agent produces a 1-line summary + next action before terminating. Returned
/// verbatim at the claim sites; callers log it and record the one-shot flag.
pub(super) const MONITOR_AUTO_TURN_PREAMBLE_HINT: &str = "[system] 모니터 자동 턴 종료 전, 이번 턴에서 확인한 상태 1줄 요약과 다음 액션을 반드시 남겨주세요.";

/// #1009: One-shot guard for `MONITOR_AUTO_TURN_PREAMBLE_HINT` injection.
/// Returns `Some(hint)` on first call per turn and flips `injected` to true;
/// subsequent calls return `None` so the hint is never repeated within a
/// single monitor auto-turn frame.
pub(super) fn consume_monitor_auto_turn_preamble_once(injected: &mut bool) -> Option<&'static str> {
    if *injected {
        None
    } else {
        *injected = true;
        Some(MONITOR_AUTO_TURN_PREAMBLE_HINT)
    }
}

fn enqueue_monitor_auto_turn_suppressed_notification(
    pg_pool: Option<&sqlx::PgPool>,
    db: Option<&crate::db::Db>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
    event_count: usize,
    entry_keys: &[String],
) -> bool {
    let target = format!("channel:{}", channel_id.get());
    let session_key = monitor_auto_turn_session_key(channel_id, data_start_offset);
    let label = monitor_auto_turn_label(tmux_session_name);
    let content = monitor_auto_turn_completion_notice(&label, event_count, entry_keys);
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

const MONITOR_AUTO_TURN_MISSED_SIGNAL_FALLBACK: tokio::time::Duration =
    tokio::time::Duration::from_secs(30);

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
            super::increment_global_active(shared, "tmux_monitor_auto_turn");
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

        // #2424 — wait on the mailbox actor's latched turn-finished signal
        // instead of probing mailbox state every 200ms. The long timeout is
        // only a missed-signal fallback for notify/backend bugs; the next
        // loop iteration re-checks cancellation and the mailbox slot.
        let signal = shared.mailboxes.turn_finished(channel_id);
        let _ = tokio::time::timeout(MONITOR_AUTO_TURN_MISSED_SIGNAL_FALLBACK, signal.wait()).await;
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
        super::saturating_decrement_global_active(shared);
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

fn advance_buffer_start_offset(start_offset: u64, before_len: usize, after_len: usize) -> u64 {
    start_offset.saturating_add(before_len.saturating_sub(after_len) as u64)
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
    // #2285 audit trail: monitor pattern fired this turn without an
    // originating Discord message. The session-bound relay does NOT branch
    // on this — recorded for diagnostics only.
    synthetic.turn_source = super::inflight::TurnSource::MonitorTriggered;

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

fn advance_watcher_confirmed_end(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    committed_end_offset: u64,
    context: &'static str,
) {
    let relay_coord = shared.tmux_relay_coord(channel_id);
    let mut cur = relay_coord
        .confirmed_end_offset
        .load(std::sync::atomic::Ordering::Acquire);
    // #1270 codex P2 (round 4): capture the `.generation` mtime BEFORE
    // the CAS so the stored mtime reflects what was on disk when we
    // decided to label `committed_end_offset` as delivered. Reading after
    // the CAS opens a TOCTOU window where a fresh respawn writes a new
    // `.generation` between our advance and our marker store, then the
    // new mtime ends up paired with the OLD offset and the next
    // regression check mis-classifies the next fresh respawn as
    // same-wrapper rotation.
    let mtime_at_attempt = {
        let m = read_generation_file_mtime_ns(tmux_session_name);
        if m == 0 { None } else { Some(m) }
    };
    let mut won_advance = false;
    while cur < committed_end_offset {
        match relay_coord.confirmed_end_offset.compare_exchange(
            cur,
            committed_end_offset,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => {
                won_advance = true;
                break;
            }
            Err(observed) => cur = observed,
        }
    }
    // Pair the pre-CAS mtime with the offset only on a real advance. If
    // the loop exits because the stored watermark is already at/past
    // `committed_end_offset` (the stale-high watermark from an older
    // session — exactly the regression case this PR is trying to
    // recover from), refreshing the mtime would associate the OLD offset
    // with the NEW wrapper and break the next regression check
    // (PR #1271 round 3).
    if won_advance && let Some(mtime) = mtime_at_attempt {
        relay_coord
            .confirmed_end_generation_mtime_ns
            .store(mtime, std::sync::atomic::Ordering::Release);
    }
    let confirmed_end = relay_coord
        .confirmed_end_offset
        .load(std::sync::atomic::Ordering::Acquire);
    let confirmed_reached_current = confirmed_end >= committed_end_offset;
    record_watcher_invariant(
        confirmed_reached_current,
        Some(provider),
        channel_id,
        "tmux_confirmed_end_monotonic",
        context,
        "watcher confirmed_end_offset must reach the committed tmux output end",
        serde_json::json!({
            "current_offset": committed_end_offset,
            "confirmed_end": confirmed_end,
            "tmux_session_name": tmux_session_name,
        }),
    );
    debug_assert!(
        confirmed_reached_current,
        "watcher confirmed_end_offset must reach committed output end"
    );
}

async fn drain_watcher_output_tail_to_eof(
    output_path: &str,
    mut current_offset: u64,
) -> Result<u64, String> {
    loop {
        let read_more = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking({
                let path = output_path.to_string();
                let offset = current_offset;
                move || -> Result<(Vec<u8>, u64), String> {
                    use std::io::{Read as _, Seek as _};

                    let mut file =
                        std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                    file.seek(std::io::SeekFrom::Start(offset))
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
            }
            Ok(Ok(Ok((_, off)))) => return Ok(off),
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(error)) => return Err(format!("join error: {error}")),
            Err(_) => return Err("timeout reading tmux output tail".to_string()),
        }
    }
}

async fn drain_missing_inflight_dead_tmux_tail_to_eof(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    current_offset: u64,
) -> u64 {
    match drain_watcher_output_tail_to_eof(output_path, current_offset).await {
        Ok(drained_offset) => {
            if drained_offset > current_offset {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 missing-inflight dead-tmux drain advanced {} from offset {} to EOF {} before watcher shutdown",
                    tmux_session_name,
                    current_offset,
                    drained_offset
                );
            }
            advance_watcher_confirmed_end(
                shared,
                provider,
                channel_id,
                tmux_session_name,
                drained_offset,
                "src/services/discord/tmux.rs:missing_inflight_dead_tmux_tail_drain",
            );
            drained_offset
        }
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ missing-inflight dead-tmux drain failed for {} at offset {}: {}",
                tmux_session_name,
                current_offset,
                error
            );
            current_offset
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

#[inline]
fn should_suppress_relay_before_emit(
    paused: bool,
    epoch_changed: bool,
    turn_delivered: bool,
    deferred_monitor_ready: bool,
) -> bool {
    (paused || epoch_changed || turn_delivered) && !deferred_monitor_ready
}

#[inline]
fn is_terminal_finalize_stop_candidate(
    terminal_output_committed: bool,
    dispatch_ok: bool,
    watcher_handled_mailbox_finish: bool,
) -> bool {
    terminal_output_committed && dispatch_ok && watcher_handled_mailbox_finish
}

/// #2161 TUI completion gate — decide whether the user-visible
/// `✅ 응답 완료` / `✅ 백그라운드 완료` status event must wait for the
/// underlying TUI pane to reach quiescence before being emitted.
///
/// The CLI provider session can write a terminal `result` JSONL event before
/// the interactive TUI has finished rendering tool output, plan presentations,
/// or trailing assistant text into its tmux pane. Without this gate the
/// caller relays the response text, immediately marks the turn as
/// `TurnCompleted`, and the user sees `응답 완료` on Discord while their
/// right-side tmux pane is still actively scrolling / showing
/// `almost done thinking`. Subsequent relay messages can then continue past
/// the completion marker — a lifecycle bug.
///
/// We currently only gate `RuntimeHandoffKind::ClaudeTui`. `LegacyTmuxWrapper`
/// drives a non-interactive wrapper script whose `result` event coincides
/// with the script exiting, so no extra quiescence step is needed.
/// The gate is based on structured provider JSONL state, not visible TUI
/// composer chrome.
///
/// `task_notification_kind` is accepted but intentionally does NOT skip the
/// gate — a Background or MonitorAutoTurn that runs inside ClaudeTui still
/// emits a visible status transition (`✅ 백그라운드 완료`) and the user
/// observes the same premature-completion bug on the same pane. Codex review
/// on #2161 flagged the original task-notification skip as the H3 finding.
#[inline]
pub(super) fn should_gate_completion_for_tui_quiescence(
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    rebind_origin: bool,
    _task_notification_kind: Option<crate::services::agent_protocol::TaskNotificationKind>,
) -> bool {
    // rebind_origin inflights describe an externally-launched tmux session
    // that AgentDesk did not start — the operator (not the Discord turn)
    // owns input cadence and the "응답 완료" marker is suppressed by other
    // rebind-origin guards anyway. Don't add an additional wait.
    if rebind_origin {
        return false;
    }
    matches!(
        runtime_kind,
        Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui)
    )
}

/// Upper bound for `should_gate_completion_for_tui_quiescence` polling.
/// Kept short enough that a hung pane cannot stall the user-visible
/// `응답 완료` marker for more than a few seconds — the gate is best-effort
/// observability, not a correctness primitive.
pub(super) const TUI_COMPLETION_QUIESCENCE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(3);

/// Poll interval inside the quiescence wait. Matches the existing
/// `READY_FOR_INPUT_IDLE_PROBE_INTERVAL` cadence used elsewhere in the
/// watcher so concurrent ticks don't fight each other on the same tmux
/// session.
pub(super) const TUI_COMPLETION_QUIESCENCE_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(250);

#[inline]
fn watcher_stop_decision_after_terminal_finalize(
    terminal_output_committed: bool,
    dispatch_ok: bool,
    watcher_handled_mailbox_finish: bool,
    tmux_alive: bool,
    confirmed_end: u64,
    tmux_tail_offset: u64,
    idle_duration: Option<std::time::Duration>,
) -> WatcherStopDecision {
    if !is_terminal_finalize_stop_candidate(
        terminal_output_committed,
        dispatch_ok,
        watcher_handled_mailbox_finish,
    ) {
        return WatcherStopDecision::Continue;
    }

    watcher_stop_decision_after_terminal_success(WatcherStopInput {
        terminal_success_seen: terminal_output_committed,
        tmux_alive,
        confirmed_end,
        tmux_tail_offset,
        idle_duration,
        idle_threshold: WATCHER_POST_TERMINAL_IDLE_WINDOW,
    })
}

#[cfg(test)]
mod terminal_finalize_liveness_tests {
    use super::{
        WATCHER_POST_TERMINAL_IDLE_WINDOW, WatcherStopDecision,
        is_terminal_finalize_stop_candidate, watcher_stop_decision_after_terminal_finalize,
    };

    #[test]
    fn terminal_finalize_stop_candidate_requires_all_terminal_flags() {
        assert!(is_terminal_finalize_stop_candidate(true, true, true));

        assert!(!is_terminal_finalize_stop_candidate(false, true, true));
        assert!(!is_terminal_finalize_stop_candidate(true, false, true));
        assert!(!is_terminal_finalize_stop_candidate(true, true, false));
    }

    #[test]
    fn terminal_finalize_stop_decision_requires_dead_tmux() {
        assert_eq!(
            watcher_stop_decision_after_terminal_finalize(
                true,
                true,
                true,
                true,
                4096,
                4096,
                Some(WATCHER_POST_TERMINAL_IDLE_WINDOW),
            ),
            WatcherStopDecision::Continue,
            "terminal finalization with a live idle tmux pane must keep the watcher attached"
        );

        assert_eq!(
            watcher_stop_decision_after_terminal_finalize(true, true, true, true, 2048, 4096, None,),
            WatcherStopDecision::PostTerminalSuccessContinuation,
            "terminal finalization with live tmux and unread tail bytes must stay attached"
        );

        assert_eq!(
            watcher_stop_decision_after_terminal_finalize(
                true, true, true, false, 4096, 4096, None,
            ),
            WatcherStopDecision::Stop,
            "terminal finalization may stop only after tmux liveness is gone"
        );

        assert_eq!(
            watcher_stop_decision_after_terminal_finalize(
                false, true, true, false, 4096, 4096, None,
            ),
            WatcherStopDecision::Continue,
            "non-committed terminal output is not a stop candidate even if liveness is gone"
        );
    }
}

#[cfg(test)]
mod monitor_auto_turn_signal_tests {
    use super::*;

    #[tokio::test]
    async fn monitor_auto_turn_wait_wakes_on_turn_finished_signal() {
        let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
        let channel = ChannelId::new(2_424_000);
        let signal = registry.turn_finished(channel);
        let waiter = tokio::spawn({
            let signal = signal.clone();
            async move {
                tokio::time::timeout(std::time::Duration::from_secs(1), signal.wait())
                    .await
                    .expect("turn-finished signal should wake monitor auto-turn wait");
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        signal.mark_done();
        waiter.await.expect("waiter task should not panic");
    }
}

#[cfg(test)]
mod tui_completion_gate_tests {
    use super::should_gate_completion_for_tui_quiescence;
    use crate::services::agent_protocol::{RuntimeHandoffKind, TaskNotificationKind};

    #[test]
    fn claude_tui_managed_turn_is_gated() {
        assert!(should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::ClaudeTui),
            false,
            None,
        ));
    }

    #[test]
    fn legacy_tmux_wrapper_skips_the_gate() {
        // Legacy wrapper's `result` event coincides with the script exiting,
        // so the existing path already aligns with TUI quiescence.
        assert!(!should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::LegacyTmuxWrapper),
            false,
            None,
        ));
    }

    #[test]
    fn process_backend_skips_the_gate() {
        assert!(!should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::ProcessBackend),
            false,
            None,
        ));
    }

    #[test]
    fn codex_tui_is_excluded_for_minimal_scope() {
        // Codex TUI has its own completion contract (#2189) and is
        // intentionally outside the scope of this #2161 fix to keep the
        // change focused and reviewable.
        assert!(!should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::CodexTui),
            false,
            None,
        ));
    }

    #[test]
    fn missing_runtime_kind_skips_the_gate() {
        assert!(!should_gate_completion_for_tui_quiescence(
            None, false, None,
        ));
    }

    #[test]
    fn rebind_origin_inflight_skips_the_gate() {
        // Externally-adopted tmux sessions don't drive a Discord-origin turn;
        // the rebind path already suppresses user-visible completion markers.
        assert!(!should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::ClaudeTui),
            true,
            None,
        ));
    }

    #[test]
    fn background_and_monitor_task_notifications_still_gated_for_tui() {
        // Codex review on #2161 H3: Background and MonitorAutoTurn emit a
        // visible `백그라운드 완료` marker that the user sees on the same
        // pane, so the gate applies to them too when running on ClaudeTui.
        // task_notification_kind is intentionally not a skip condition.
        assert!(should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::ClaudeTui),
            false,
            Some(TaskNotificationKind::Background),
        ));
        assert!(should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::ClaudeTui),
            false,
            Some(TaskNotificationKind::MonitorAutoTurn),
        ));
        assert!(should_gate_completion_for_tui_quiescence(
            Some(RuntimeHandoffKind::ClaudeTui),
            false,
            Some(TaskNotificationKind::Subagent),
        ));
    }
}

#[cfg(test)]
mod tui_completion_gate_outcome_tests {
    use super::TuiCompletionGateOutcome;

    #[test]
    fn not_gated_emits_immediately() {
        assert!(TuiCompletionGateOutcome::NotGated.should_emit_completion());
    }

    #[test]
    fn confirmed_idle_emits() {
        assert!(TuiCompletionGateOutcome::ConfirmedIdle.should_emit_completion());
    }

    #[test]
    fn skipped_dead_emits() {
        assert!(TuiCompletionGateOutcome::SkippedDead.should_emit_completion());
    }

    #[test]
    fn timed_out_suppresses_emit() {
        // Codex H2: timeout MUST suppress the TurnCompleted emit;
        // placeholder sweeper / next-turn intake reconciles later.
        assert!(!TuiCompletionGateOutcome::TimedOut.should_emit_completion());
    }
}

/// #2293 regression coverage — the `lifecycle_stage_paused` boolean derived
/// from `TuiCompletionGateOutcome` is what gates every TimedOut side-effect
/// in `tmux_watcher.rs` (✅ reaction, transcript persist, history append,
/// confirmed-end watermark, clear_inflight, finish_restored_watcher_active_turn,
/// queue kickoff, terminal-finalize stop). Same pattern in `turn_bridge` and
/// `recovery_engine`. The pure derivation lives in this module's
/// `should_emit_completion` contract, but the consumers compute their flag
/// inline via `matches!(outcome, TuiCompletionGateOutcome::TimedOut)` — these
/// tests pin the matrix so a future refactor that adds another variant
/// cannot silently widen the "proceed" set without also updating the side-
/// effect gates.
#[cfg(test)]
mod lifecycle_stage_pause_matrix_tests {
    use super::TuiCompletionGateOutcome;

    /// Mirrors the `matches!` predicate inlined at the watcher / bridge /
    /// recovery side-effect gates. Keeping it as a tiny helper here makes
    /// the gate matrix testable without re-implementing the consumers.
    fn lifecycle_stage_paused(outcome: TuiCompletionGateOutcome) -> bool {
        matches!(outcome, TuiCompletionGateOutcome::TimedOut)
    }

    #[test]
    fn paused_only_on_timed_out() {
        assert!(!lifecycle_stage_paused(TuiCompletionGateOutcome::NotGated));
        assert!(!lifecycle_stage_paused(
            TuiCompletionGateOutcome::ConfirmedIdle
        ));
        assert!(!lifecycle_stage_paused(
            TuiCompletionGateOutcome::SkippedDead
        ));
        assert!(lifecycle_stage_paused(TuiCompletionGateOutcome::TimedOut));
    }

    #[test]
    fn pause_matrix_is_complement_of_emit_matrix() {
        // Every outcome where the gate DOES emit completion is also a
        // outcome where lifecycle MUST proceed — and vice versa. If these
        // two ever drift apart we'd either suppress the user-visible
        // `응답 완료` while still releasing the mailbox (the original
        // #2293 cascade) or emit completion while pausing the cleanup.
        for outcome in [
            TuiCompletionGateOutcome::NotGated,
            TuiCompletionGateOutcome::ConfirmedIdle,
            TuiCompletionGateOutcome::SkippedDead,
            TuiCompletionGateOutcome::TimedOut,
        ] {
            assert_eq!(
                outcome.should_emit_completion(),
                !lifecycle_stage_paused(outcome),
                "emit/pause complementarity violated for {outcome:?}",
            );
        }
    }
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
/// **Storage backend**: uses the Postgres message outbox when available.
/// Without a PG pool the caller should direct-send, because no production
/// worker drains a local legacy outbox for this path.
///
/// **Session identity** (#897 round 1 P1 #3): both `reason_code` and
/// `session_key` are set so reconciliation and any future dedupe policy can
/// identify the watcher range. `session_key` encodes
/// `channel_id + data_start_offset + content hash`, so:
///   * Distinct background completions in the same channel produce distinct
///     session_keys (different offsets or different content) → each lands
///     as its own outbox row.
///   * Reconcile can parse failed rows back to the minimum tmux offset that
///     needs re-staging.
///
/// The PG path currently does INSERT without a per-tick dedupe query (the
/// SQLite-only `enqueue` helper lives in `message_outbox.rs`; porting it to a
/// shared sqlx/rusqlite interface is tracked separately). Same-row dedupe on
/// the PG side is still achievable via a `UNIQUE(reason_code, session_key,
/// status) WHERE status != 'failed'` partial index, but that's a schema
/// change outside this PR's scope. Follow-up tracked in #898-family.
///
/// Returns `false` when Postgres is unavailable or the insert fails — the
/// caller falls back to a direct command-bot send in that case so the message
/// is never silently lost.
pub(super) async fn enqueue_background_trigger_response_to_notify_outbox(
    pg_pool: Option<&sqlx::PgPool>,
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
    // drains PG exclusively. On PG insert failure we return `false` so the
    // caller falls back to a direct Discord send rather than papering over
    // the failure with an undeliverable local row.
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

    let _ = session_key;
    false
}

/// #897 counter-model review P1 #2: Find permanently-failed notify-bot
/// outbox rows that originated from this watcher's background-trigger
/// enqueues, extract the tmux offsets that caused them, and delete the
/// rows so they don't accumulate. Returns the MINIMUM observed
/// `data_start_offset` encoded in `session_key`, which the caller uses to
/// roll `last_enqueued_offset` back and re-stage the same tmux range on
/// the next watcher tick.
///
/// **Storage backend**: reconciles Postgres only. Without a PG pool there is
/// no authoritative outbox store for failed background-trigger rows.
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
    channel_id: ChannelId,
) -> Option<u64> {
    let target = format!("channel:{}", channel_id.get());

    // #897 round-3 High: when `pg_pool` is configured it is the only
    // authoritative store. Consulting a local legacy store on PG failure or
    // on an empty PG result would surface rows that the outbox worker never
    // produced, and worse could delete rows written by a prior run. On PG
    // error we surface `None` so the next poll retries; there is no data-safe
    // fallback.
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

    None
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
    match state.effective_relay_owner_kind() {
        super::inflight::RelayOwnerKind::Watcher => return false,
        super::inflight::RelayOwnerKind::StandbyRelay => {
            return current_offset > data_start_offset;
        }
        super::inflight::RelayOwnerKind::SessionBoundRelay => {
            return current_offset > data_start_offset;
        }
        super::inflight::RelayOwnerKind::Unknown => {
            return current_offset > data_start_offset;
        }
        super::inflight::RelayOwnerKind::None => {}
    }

    let turn_start_offset = state.turn_start_offset.unwrap_or(state.last_offset);
    data_start_offset <= turn_start_offset && turn_start_offset < current_offset
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
    // A restored inflight with no current message id (current_msg_id == 0 — e.g. a
    // TUI-direct/recovery turn that never anchored a Discord placeholder) has no
    // placeholder message to reconcile, and `MessageId::new(0)` panics. Skip it so
    // a single such orphan cannot abort startup watcher reconciliation (which would
    // leave `reconcile_done` stuck false and the provider permanently degraded).
    if state.current_msg_id == 0 {
        return;
    }
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
        provider,
        state.tmux_session_name.as_deref().unwrap_or("unknown"),
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
    any_tool_used: bool,
    has_post_tool_text: bool,
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
    inflight.any_tool_used = any_tool_used;
    inflight.has_post_tool_text = has_post_tool_text;
    if task_notification_kind.is_some() {
        inflight.task_notification_kind = task_notification_kind;
    }
    let _ = super::inflight::save_inflight_state(&inflight);
}

async fn finish_restored_watcher_active_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    // #3016: the turn's real `user_msg_id`, captured by the caller BEFORE it
    // cleared inflight (a reload here returns `None`). 0 means "unknown" (true
    // orphan); a real id makes the finalizer ledger match exact so a stale
    // channel-only terminal cannot finalize a queued follow-up turn.
    user_msg_id: u64,
    finish_mailbox_on_completion: bool,
    delegated_finalize_owed: bool,
    // #3016 option A: the caller observed a *normal completion* (terminal output
    // committed / pane confirmed idle past the relay) and wants the single-
    // authority finalizer driven regardless of the legacy flags. The finalizer
    // is idempotent — a turn already finalized by the bridge resolves to
    // `AlreadyFinalized` — so an unconditional submit at the confirmed-completion
    // point cannot over-finalize. This decouples the watcher's normal-completion
    // finalize from `mailbox_finalize_owed` / `finish_mailbox_on_completion` so
    // phase 5 can delete the flag. Restore/recovery callers that have NOT
    // confirmed a normal completion pass `false` and keep the flag-gated path.
    normal_completion: bool,
    kickoff_queue: bool,
    stop_source: &'static str,
) -> bool {
    // Either flag implies the watcher must clear the channel mailbox now:
    //   * `finish_mailbox_on_completion` → inflight-restore semantics (a
    //     restored/recovered watcher inherits its turn from the bridge, so
    //     it owns the cancel_token when the turn ends). Pre-existing.
    //   * `delegated_finalize_owed` → bridge handed finalization debt to the
    //     watcher because it skipped `mailbox_finish_turn` to avoid racing
    //     the in-flight watcher relay. New for #1452.
    //
    // The two flags can coincide (e.g., a recovered watcher whose first
    // post-recovery turn also went through stream-lost handoff). Calling
    // `mailbox_finish_turn` once per turn is idempotent for our purposes —
    // the second call would just observe an empty active slot — but we
    // gate on the OR to keep the call site to a single place.
    //
    // `kickoff_queue` is the dispatch-lifecycle gate (codex #1670 P2): the
    // mailbox/inflight cleanup above is required for orphan prevention even
    // when the dispatch finalization failed, but auto-dispatching the next
    // queued turn must NOT happen on a failed dispatch — that decision is
    // left to the operator/user. Callers pass `dispatch_ok` (or an
    // equivalent gate) here.
    // #3016 option A: a confirmed normal completion always drives the finalizer.
    // The two legacy flags remain as *additional* triggers for the restore /
    // delegated-debt paths, but they are no longer the only way the watcher's
    // normal-completion finalize fires — that's the decoupling. (The finalizer's
    // idempotence guarantees no double finalize when the bridge already won.)
    //
    // #3016 (codex R1): the return value reports whether this helper actually
    // DROVE the finalize (i.e. did NOT early-return). The caller folds it into
    // `watcher_handled_mailbox_finish` so the post-finalize lifecycle (queue
    // kickoff suppression + terminal-stop-candidate path) reflects the real
    // finalize, not just the legacy flag intent — otherwise the newly decoupled
    // `normal_completion`-only finalize would leave that signal false and
    // double-schedule kickoff / skip terminal-stop handling.
    if !normal_completion && !finish_mailbox_on_completion && !delegated_finalize_owed {
        return false;
    }

    if delegated_finalize_owed {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] watcher_finalized_delegated_turn: clearing channel {} mailbox after bridge→watcher handoff (#1452)",
            channel_id.get()
        );
    }

    // #3016 phase 3: route the watcher terminal through the single-authority
    // finalizer instead of calling mailbox_finish_turn + counter + side-effects
    // inline. The ledger phase gate makes this exactly-once across bridge and
    // watcher: whichever submits first finalizes; the loser gets
    // `AlreadyFinalized` and the watcher simply skips the queue kickoff (the
    // winner already owns it).
    //
    // Use the REAL `user_msg_id` the caller captured BEFORE clearing inflight
    // (reloading inflight here returns `None` because the watcher already wiped
    // it). The exact id makes the ledger match precise: a stale terminal from an
    // already-finalized turn resolves to that turn's `Finalized` entry
    // (→ AlreadyFinalized) instead of accidentally finalizing a queued follow-up.
    //
    // The watcher cleared inflight inline before calling this helper, and never
    // marked completion-cleanup or drained voice, so `FinalizeContext::watcher`
    // reproduces exactly that side-effect set; the kickoff stays gated on the
    // caller's `kickoff_queue`.
    let outcome = shared
        .turn_finalizer
        .submit_terminal(
            super::turn_finalizer::TurnKey::new(channel_id, user_msg_id, shared.current_generation),
            provider.clone(),
            super::turn_finalizer::TerminalEvent::Complete,
            super::turn_finalizer::FinalizeContext::watcher(),
            shared.clone(),
        )
        .await;
    let (mailbox_online, has_pending) = match outcome {
        super::turn_finalizer::FinalizeOutcome::Finalized {
            has_pending,
            mailbox_online,
            ..
        } => {
            // #3016 (codex P1): this watcher finalize consumed the delegated
            // debt. Revoke the legacy `mailbox_finalize_owed` flag so a later
            // watcher swap can't run stale cleanup against the NEXT active turn.
            // (Removed wholesale in phase 5; revoked here in the meantime.)
            if let Some(watcher) = shared.tmux_watchers.get(&channel_id) {
                watcher
                    .mailbox_finalize_owed
                    .store(false, std::sync::atomic::Ordering::Release);
            }
            (mailbox_online, has_pending)
        }
        super::turn_finalizer::FinalizeOutcome::AlreadyFinalized
        | super::turn_finalizer::FinalizeOutcome::Deferred => (false, false),
    };
    if kickoff_queue && mailbox_online && has_pending {
        super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            stop_source,
        );
    }
    // Drove the finalize (reached here past the early-return gate).
    true
}

/// Background watcher that continuously tails a tmux output file.
/// When Claude produces output from terminal input (not Discord), relay it to Discord.
#[path = "tmux_watcher.rs"]
mod tmux_watcher;
pub(super) use self::tmux_watcher::{
    TuiCompletionGateOutcome, emit_explicit_inflight_cleanup_signal, run_tui_completion_gate,
    tmux_output_watcher, tmux_output_watcher_with_restore,
};
#[path = "tmux_output_stream.rs"]
mod tmux_output_stream;
pub(in crate::services::discord) use self::tmux_output_stream::{
    WatcherToolState, build_watcher_placeholder_status_block, flush_placeholder_live_events,
    force_next_watcher_status_update, process_watcher_lines,
};

#[cfg(test)]
mod buffer_offset_tests {
    use super::advance_buffer_start_offset;

    #[test]
    fn advance_buffer_start_offset_tracks_drained_leftover_tail() {
        assert_eq!(advance_buffer_start_offset(100, 80, 30), 150);
        assert_eq!(advance_buffer_start_offset(100, 30, 80), 100);
    }
}

#[cfg(test)]
mod watcher_placeholder_status_tests {
    use super::watcher_should_render_status_only_placeholder;
    use crate::services::agent_protocol::TaskNotificationKind;

    #[test]
    fn status_only_placeholder_requires_existing_card_or_activity() {
        assert!(!watcher_should_render_status_only_placeholder(
            false, None, None
        ));
        assert!(!watcher_should_render_status_only_placeholder(
            false,
            Some("   "),
            None
        ));
        assert!(watcher_should_render_status_only_placeholder(
            true, None, None
        ));
        assert!(watcher_should_render_status_only_placeholder(
            false,
            Some("⚙ Bash: cargo check"),
            None
        ));
        assert!(watcher_should_render_status_only_placeholder(
            false,
            None,
            Some(TaskNotificationKind::Background)
        ));
    }
}

#[cfg(test)]
mod watcher_stream_progress_tests {
    use super::persist_watcher_stream_progress;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};

    #[test]
    fn persist_watcher_stream_progress_persists_tool_hold_witness() {
        // Serialize on the PROCESS-WIDE `AGENTDESK_ROOT_DIR` lock so this test
        // is mutually exclusive with every other test that mutates the runtime
        // root (standby_relay, turn_finalizer, gateway, config, …). A module-
        // local mutex would only serialize within this module, letting a
        // concurrent root-mutating test stomp our tempdir env mid-flight.
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1509350490461180105);
        let tmux_session_name = "AgentDesk-claude-adk-issue-2985-hold-witness";
        let state = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some("claude-pipe".to_string()),
            42,
            9100000000000000123,
            9100000000000000124,
            "emit OK, call a tool, then wait".to_string(),
            Some("session-2985".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/issue-2985-output.jsonl".to_string()),
            Some("/tmp/issue-2985-input.fifo".to_string()),
            0,
        );
        super::super::inflight::save_inflight_state(&state).expect("save inflight");

        persist_watcher_stream_progress(
            &provider,
            channel_id,
            tmux_session_name,
            Some(MessageId::new(9100000000000000125)),
            "[E2E:E18:OK]\n\n",
            0,
            Some("Bash: sleep 60"),
            Some("Bash"),
            None,
            true,
            false,
        );

        let persisted = super::super::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("load inflight");
        assert_eq!(persisted.full_response, "[E2E:E18:OK]\n\n");
        assert!(persisted.any_tool_used);
        assert!(
            !persisted.has_post_tool_text,
            "pre-cancel hold witness must stay durable before post-tool text"
        );
        assert_eq!(
            persisted.current_tool_line.as_deref(),
            Some("Bash: sleep 60")
        );
        assert_eq!(persisted.current_msg_id, 9100000000000000125);

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
    }
}

#[cfg(test)]
mod restored_turn_injected_anchor_tests {
    use super::restored_watcher_turn_from_inflight;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;

    // #3107 codex re-review (P2, F3): the streaming-interval re-acquire site reads
    // the #3099 hourglass anchor (`injected_prompt_message_id`) from the restored
    // turn captured up front (before `restored_turn.take()` consumes it). This test
    // pins the source of that capture: a hourglass-anchored inflight that the
    // watcher restores must carry the anchor onto the `RestoredWatcherTurn`, so the
    // mid-stream re-acquire can re-pin it instead of orphaning the `⏳`.
    #[test]
    fn restored_turn_carries_injected_prompt_message_id_for_streaming_reacquire() {
        let provider = ProviderKind::Claude;
        let tmux_session_name = "AgentDesk-claude-adk-cc-3107-f3";
        let mut state = InflightTurnState::new(
            provider,
            123_456,
            Some("adk-cc".to_string()),
            0, // headless / synthetic user turn (task-notification auto-turn)
            0,
            55_555, // current_msg_id must be non-zero for a restorable turn
            "anchored auto-turn".to_string(),
            None,
            Some(tmux_session_name.to_string()),
            Some("/tmp/agentdesk-3107-f3.jsonl".to_string()),
            None,
            0,
        );
        state.injected_prompt_message_id = Some(424_242);

        let restored = restored_watcher_turn_from_inflight(&state, tmux_session_name, false)
            .expect("a non-rebind inflight with a real current_msg_id restores");
        assert_eq!(
            restored.injected_prompt_message_id,
            Some(424_242),
            "the restored turn must carry the #3099 hourglass anchor so the \
             streaming-interval re-acquire can re-pin it (F3 regression)"
        );
    }
}
