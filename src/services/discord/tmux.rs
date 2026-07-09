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
use super::placeholder_cleanup::{PlaceholderCleanupOperation, PlaceholderCleanupOutcome};
use super::placeholder_live_events::{
    RecentPlaceholderEvent, events_from_json, status_events_from_json,
};
use super::settings::{
    channel_supports_provider, load_last_session_path, resolve_role_binding,
    validate_bot_channel_routing_with_provider_channel,
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
// Extracted lifecycle code stays a tmux child module until its callers split out.
#[path = "tmux_placeholder_suppression/mod.rs"]
mod placeholder_suppression;
#[path = "tmux_reattach_offsets.rs"]
mod tmux_reattach_offsets;
#[path = "tmux_session_files.rs"]
mod tmux_session_files;
#[path = "watchers/lifecycle.rs"]
mod watcher_lifecycle;

use self::placeholder_suppression::*;
use self::tmux_reattach_offsets::matching_recent_watcher_reattach_offset;
pub(in crate::services::discord) use self::tmux_session_files::committed_frontier_for_current_generation;
pub(super) use self::tmux_session_files::read_generation_file_mtime_ns;
pub(in crate::services::discord) use self::tmux_session_files::reset_relay_watermark_on_generation_change;
pub(in crate::services::discord) use self::tmux_session_files::reset_stale_relay_watermark_if_output_regressed;
pub(super) use self::tmux_session_files::session_panel_instance_key;
pub(crate) use self::tmux_session_files::write_spawn_nonce;
use self::tmux_session_files::{
    preserve_mtime_after_write, reset_stale_local_relay_offset_if_output_regressed,
    sweep_orphan_session_files,
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
const BACKGROUND_AGENT_PENDING_SNIFF_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(2);
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

pub(in crate::services::discord) async fn sniff_background_agent_pending_for_completion(
    tmux_session_name: Option<&str>,
) -> bool {
    let Some(tmux_session_name) = tmux_session_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        // No live tmux-session context exists for this completion producer.
        return false;
    };
    let tmux_session_name = tmux_session_name.to_string();
    tokio::time::timeout(
        BACKGROUND_AGENT_PENDING_SNIFF_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            crate::services::tmux_common::sniff_background_agent_pending(&tmux_session_name)
        }),
    )
    .await
    .unwrap_or(Ok(false))
    .unwrap_or(false)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct WatcherLineOutcome {
    pub found_result: bool,
    pub terminal_kind: Option<WatcherTerminalKind>,
    pub terminal_evidence_offset: Option<u64>,
    pub pre_turn_bytes_skipped: usize,
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
    /// Identity of the inflight row that seeded this restore. A long-lived watcher
    /// may consume the seed only after a later direct-input turn starts; compare
    /// this to the current row before carrying response text forward.
    turn_identity: Option<super::inflight::InflightTurnIdentity>,
    /// #3871: frozen streamed rollover-prefix message ids restored from the
    /// persisted row so a terminal full-body fallback in a later iteration / after
    /// a restart still deletes every accumulated prefix (no residual duplicate).
    streaming_rollover_frozen_msg_ids: Vec<MessageId>,
    /// Same-process retry seed produced by watcher send-failure rewind. Unlike a
    /// restart-restored seed, this is the current turn and must not be discarded by
    /// idle direct-prompt stale-seed cleanup.
    same_turn_rewind: bool,
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
    /// #3871: see [`RestoredWatcherTurn::streaming_rollover_frozen_msg_ids`].
    streaming_rollover_frozen_msg_ids: Vec<MessageId>,
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

    let provider = state.provider_kind()?;
    let response_sent_offset =
        normalize_response_sent_offset(&state.full_response, state.response_sent_offset);
    Some(RestoredWatcherTurn {
        current_msg_id: MessageId::new(state.current_msg_id),
        status_message_id: state.status_message_id.map(MessageId::new),
        response_sent_offset,
        full_response: state.full_response.clone(),
        last_edit_text: reconstructed_inflight_placeholder_body(state, &provider),
        task_notification_kind: state.task_notification_kind,
        finish_mailbox_on_completion,
        injected_prompt_message_id: state.injected_prompt_message_id,
        turn_identity: Some(super::inflight::InflightTurnIdentity::from_state(state)),
        streaming_rollover_frozen_msg_ids: state
            .streaming_rollover_frozen_msg_ids
            .iter()
            .copied()
            .map(MessageId::new)
            .collect(),
        same_turn_rewind: false,
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
            streaming_rollover_frozen_msg_ids: restored.streaming_rollover_frozen_msg_ids,
        },
        None => WatcherStreamSeed {
            placeholder_msg_id: None,
            status_panel_msg_id: None,
            response_sent_offset: 0,
            full_response: String::new(),
            last_edit_text: String::new(),
            task_notification_kind: None,
            finish_mailbox_on_completion: false,
            streaming_rollover_frozen_msg_ids: Vec::new(),
        },
    }
}

fn should_discard_restored_seed_for_idle_direct_prompt(
    restored_turn_present: bool,
    prompt_anchor_present: bool,
    seed_has_undelivered_body: bool,
    same_turn_rewind_seed: bool,
    seed_reassigned_to_different_turn: bool,
) -> bool {
    restored_turn_present
        && !same_turn_rewind_seed
        && ((prompt_anchor_present && !seed_has_undelivered_body)
            || seed_reassigned_to_different_turn)
}

fn restored_seed_reassigned_to_different_turn(
    restored_turn: Option<&RestoredWatcherTurn>,
    current_turn_identity: Option<&super::inflight::InflightTurnIdentity>,
    prompt_anchor_message_id: Option<u64>,
) -> bool {
    let Some(restored) = restored_turn else {
        return false;
    };
    if restored.same_turn_rewind {
        return false;
    }
    if let (Some(seed_anchor), Some(current_anchor)) = (
        restored.injected_prompt_message_id,
        prompt_anchor_message_id,
    ) && seed_anchor != current_anchor
    {
        return true;
    }
    if let (Some(seed_identity), Some(current_identity)) =
        (restored.turn_identity.as_ref(), current_turn_identity)
    {
        return seed_identity != current_identity;
    }
    false
}
#[cfg(test)]
mod restored_seed_discard_tests {
    use super::{
        RestoredWatcherTurn, restored_seed_reassigned_to_different_turn,
        should_discard_restored_seed_for_idle_direct_prompt, watcher_stream_seed,
    };
    use crate::services::discord::inflight::InflightTurnIdentity;
    use poise::serenity_prelude::MessageId;

    #[test]
    fn idle_direct_prompt_preserves_restored_seed_with_undelivered_body() {
        assert!(!should_discard_restored_seed_for_idle_direct_prompt(
            true, true, true, false, false,
        ));
    }

    #[test]
    fn idle_direct_prompt_still_discards_empty_restored_seed_with_anchor() {
        assert!(should_discard_restored_seed_for_idle_direct_prompt(
            true, true, false, false, false,
        ));
    }

    #[test]
    fn idle_direct_prompt_preserves_same_turn_rewind_seed_with_anchor() {
        assert!(!should_discard_restored_seed_for_idle_direct_prompt(
            true, true, false, true, true,
        ));
    }

    #[test]
    fn idle_direct_prompt_discard_still_requires_restored_turn_and_anchor() {
        assert!(!should_discard_restored_seed_for_idle_direct_prompt(
            true, false, false, false, false,
        ));
        assert!(!should_discard_restored_seed_for_idle_direct_prompt(
            true, false, true, false, false,
        ));
        assert!(!should_discard_restored_seed_for_idle_direct_prompt(
            false, true, false, false, false,
        ));
        assert!(!should_discard_restored_seed_for_idle_direct_prompt(
            false, true, true, false, false,
        ));
    }

    #[test]
    fn cross_turn_watcher_reuse_discards_restored_seed_before_terminal_commit() {
        let seed_identity = InflightTurnIdentity {
            user_msg_id: 0,
            started_at: "2026-07-07T01:00:00Z".to_string(),
            tmux_session_name: Some("AgentDesk-claude-adk".to_string()),
            turn_start_offset: Some(100),
        };
        let current_identity = InflightTurnIdentity {
            started_at: "2026-07-07T01:00:10Z".to_string(),
            turn_start_offset: Some(240),
            ..seed_identity.clone()
        };
        let restored = RestoredWatcherTurn {
            current_msg_id: MessageId::new(4105),
            status_message_id: None,
            response_sent_offset: 0,
            full_response: "WARMUP".to_string(),
            last_edit_text: String::new(),
            task_notification_kind: None,
            finish_mailbox_on_completion: false,
            injected_prompt_message_id: Some(9001),
            turn_identity: Some(seed_identity),
            streaming_rollover_frozen_msg_ids: Vec::new(),
            same_turn_rewind: false,
        };

        let seed_reassigned_to_different_turn = restored_seed_reassigned_to_different_turn(
            Some(&restored),
            Some(&current_identity),
            Some(9002),
        );
        assert!(seed_reassigned_to_different_turn);
        assert!(should_discard_restored_seed_for_idle_direct_prompt(
            true,
            true,
            true,
            restored.same_turn_rewind,
            seed_reassigned_to_different_turn,
        ));

        let stream_seed = watcher_stream_seed(None);
        assert!(stream_seed.full_response.is_empty());
        assert_eq!(stream_seed.response_sent_offset, 0);
    }
}

#[allow(dead_code)] // #3034: #826/#897/#898 bg-trigger notify-outbox subsystem (unwired).
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

#[allow(dead_code)] // #3034: #826/#897 lifecycle-notify subsystem, see note above.
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

/// #1009: Shared formatter for the monitor auto-turn suppressed-notification
/// summary line. Produces:
///   - `🔔 Monitor n회 처리 · 다음 모니터: {key1, key2, ...}` when entries > 0
///   - `🔔 Monitor n회 처리 · (등록된 모니터 없음)` when entries == 0
/// Entry keys are emitted in the order the store returns them.
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
        pg_pool,
        target.as_str(),
        Some(session_key.as_str()),
        MONITOR_AUTO_TURN_REASON_CODE,
        content.as_str(),
    )
}

fn enqueue_monitor_auto_turn_deferred_notification(
    pg_pool: Option<&sqlx::PgPool>,
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
    /// The synthetic mailbox message id this monitor turn started under
    /// (`data_start_offset.max(1)`), `Some` only when `acquired`. #3016 P1:
    /// the finalizer keys its ledger on this so SEQUENTIAL monitor turns in the
    /// same channel within `FINALIZED_TTL` are DISTINCT entries — a finalized
    /// monitor turn must not make the next one resolve to `AlreadyFinalized`
    /// (which would strand its mailbox token + counter).
    synthetic_message_id: Option<MessageId>,
    /// #3016 P1 (codex r3): a PROCESS-MONOTONIC ledger generation for this
    /// monitor turn, `Some` only when `acquired`. The synthetic mailbox message
    /// id is derived from the JSONL byte offset, which REPEATS after a wrapper
    /// respawn / JSONL truncation while `current_generation` stays the same — so
    /// two monitor turns at the same offset within `FINALIZED_TTL` would share a
    /// finalizer ledger key and the second would resolve to `AlreadyFinalized`,
    /// stranding its mailbox token + counter. Keying the ledger generation on
    /// this never-repeating counter makes every monitor turn a DISTINCT entry.
    /// (It is used ONLY for ledger keying; `do_finalize`'s identity-guarded
    /// mailbox finish matches on `channel_id` + `synthetic_message_id`, not the
    /// generation, so this does not affect which mailbox token is released.)
    ledger_generation: Option<u64>,
}

const MONITOR_AUTO_TURN_MISSED_SIGNAL_FALLBACK: tokio::time::Duration =
    tokio::time::Duration::from_secs(30);

/// Process-monotonic counter for monitor-auto-turn finalizer ledger
/// generations (#3016 P1). Starts high to avoid colliding with the
/// `current_generation` (dcserver restart) namespace used by ordinary turns.
static MONITOR_AUTO_TURN_LEDGER_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1 << 48);

fn next_monitor_auto_turn_ledger_generation() -> u64 {
    MONITOR_AUTO_TURN_LEDGER_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

async fn start_monitor_auto_turn_when_available(
    // #3016 phase-5a: `&Arc<SharedData>` (not `&SharedData`) so `register_start`
    // can downgrade a `Weak` into the `Start` message and prime the finalizer's
    // reconcile cache at register time — the watcher callers already pass
    // `&shared` (an `Arc`), so the `&SharedData` deref-coercion is simply
    // dropped here; no caller change is needed.
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    data_start_offset: u64,
    cancel: &std::sync::atomic::AtomicBool,
) -> MonitorAutoTurnStart {
    let mut deferred = false;
    let synthetic_message_id = MessageId::new(data_start_offset.max(1));

    loop {
        if cancel.load(Ordering::Relaxed) || shared.restart.shutting_down.load(Ordering::Relaxed) {
            return MonitorAutoTurnStart {
                acquired: false,
                deferred,
                synthetic_message_id: None,
                ledger_generation: None,
            };
        }

        let token = Arc::new(crate::services::provider::CancelToken::new());
        // #3167 — the monitor auto-turn is a low-priority background relay; mark
        // it with the distinct monitor kind so queued external USER intervention
        // is not starved and synthetic stale-reclaim never preempts it.
        let started = super::mailbox_try_start_turn_kinded(
            shared,
            channel_id,
            token,
            UserId::new(1),
            synthetic_message_id,
            crate::services::turn_orchestrator::ActiveTurnKind::MonitorAutoTurn,
        )
        .await;
        if started {
            super::increment_global_active(shared, "tmux_monitor_auto_turn");
            shared
                .turn_start_times
                .insert(channel_id, std::time::Instant::now());
            // #3016 P1: register the monitor turn in the finalizer ledger under
            // its synthetic message id keyed by a PROCESS-MONOTONIC ledger
            // generation (the byte-offset-derived synthetic id repeats after a
            // wrapper respawn, so the generation is what guarantees distinct
            // entries for sequential monitor turns — see `MonitorAutoTurnStart`).
            let ledger_generation = next_monitor_auto_turn_ledger_generation();
            shared.turn_finalizer.register_start(
                super::turn_finalizer::TurnKey::new(
                    channel_id,
                    synthetic_message_id.get(),
                    ledger_generation,
                ),
                provider.clone(),
                crate::services::discord::inflight::RelayOwnerKind::Watcher,
                // #3016 phase-5a: prime the reconcile cache at register time so
                // the watcher far-backstop fires even for a fresh actor whose
                // first watcher turn never submits its own terminal.
                shared,
            );
            return MonitorAutoTurnStart {
                acquired: true,
                deferred,
                synthetic_message_id: Some(synthetic_message_id),
                ledger_generation: Some(ledger_generation),
            };
        }

        if !deferred {
            deferred = true;
            let _ = enqueue_monitor_auto_turn_deferred_notification(
                shared.pg_pool.as_ref(),
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
    synthetic_message_id: Option<MessageId>,
    ledger_generation: Option<u64>,
) {
    // #3016 phase 4: route the monitor-auto-turn terminal through the
    // single-authority finalizer instead of calling mailbox_finish_turn +
    // counter + kickoff inline. The monitor turn was `register_start`'d under
    // its synthetic message id keyed by a PROCESS-MONOTONIC ledger generation,
    // so we finalize under THAT exact key — keeping sequential monitor turns in
    // the same channel within `FINALIZED_TTL` as DISTINCT ledger entries even
    // when the byte-offset-derived synthetic id repeats after a wrapper respawn
    // (codex P1: a colliding key would make the second monitor turn resolve to
    // `AlreadyFinalized` and strand its mailbox token + counter). `do_finalize`
    // takes the identity-guarded `mailbox_finish_turn_if_matches` path for the
    // real id (generation is ledger-keying only). The ledger phase gate makes a
    // racing watcher/bridge terminal exactly-once safe. `FinalizeContext::monitor`
    // reproduces the inline side-effect set: no inflight clear, no
    // completion-cleanup, no voice drain, but kick off any queued backlog.
    let user_msg_id = synthetic_message_id.map(|id| id.get()).unwrap_or(0);
    let generation = ledger_generation.unwrap_or(shared.restart.current_generation);
    let _ = shared
        .turn_finalizer
        .submit_terminal(
            super::turn_finalizer::TurnKey::new(channel_id, user_msg_id, generation),
            provider.clone(),
            super::turn_finalizer::TerminalEvent::Complete,
            super::turn_finalizer::FinalizeContext::monitor(),
            shared.clone(),
        )
        .await;
    shared.turn_start_times.remove(&channel_id);
    if let Ok(mut last) = shared.last_turn_at.lock() {
        *last = Some(chrono::Local::now().to_rfc3339());
    }
}

async fn finish_monitor_auto_turn_if_claimed(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    claimed: &mut bool,
    finished: &mut bool,
    synthetic_message_id: &mut Option<MessageId>,
    ledger_generation: &mut Option<u64>,
) {
    if *claimed {
        finish_monitor_auto_turn(
            shared,
            provider,
            channel_id,
            *synthetic_message_id,
            *ledger_generation,
        )
        .await;
        *claimed = false;
        *finished = true;
        *synthetic_message_id = None;
        *ledger_generation = None;
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
    // status-panel-v2: make this watcher-owned so the panel-eligibility
    // predicate (watcher_inflight_is_panel_eligible_for_session) recognises the
    // synthetic monitor/self-paced-loop turn and the watcher can create/update/
    // clean up a live status panel for it. The shared external-input predicate
    // (lease + ⏳ anchor lifecycle, #3164/#3174) stays untouched.
    synthetic.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);

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

/// Monotonic-CAS advance of the channel's `confirmed_end_offset` watermark to
/// `committed_end_offset`, pairing the pre-CAS `.generation` mtime on a real
/// advance and recording the `tmux_confirmed_end_monotonic` invariant. #3041
/// P1-1 makes this `pub(in crate::services::discord)`. The watcher commits its
/// delivery lease and then calls this to advance the watermark INLINE
/// (synchronously) on a `Delivered` outcome — the lease commit + this inline
/// advance are the single thing that advances the watermark for the watcher
/// terminal path (§5.2). (The `TurnFinalizer` actor's `CommitDelivery`/
/// `ReleaseDelivery` handlers are DORMANT — retained for a later phase, NOT the
/// live watcher path after the R2 revert.) The monotonic CAS keeps the advance
/// idempotent: a second `Delivered` commit of an already-confirmed range
/// observes `cur >= committed_end_offset` and does not move (and does not
/// refresh the mtime), so no double-advance.
pub(in crate::services::discord) fn advance_watcher_confirmed_end(
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
#[allow(dead_code)] // #3034: notify-path bg-trigger offset gate (unwired; #826/#897/#898).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct OffsetAdvanceDecision {
    pub advance_relayed: bool,
    pub advance_enqueued: bool,
}

#[allow(dead_code)] // #3034: notify-path offset gate, see note above.
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

/// #2161/#4047 TUI completion observation — decide whether the terminal path
/// should record pane liveness and strict completion-signal telemetry before
/// emitting `✅ 응답 완료` / `✅ 백그라운드 완료`.
///
/// The CLI provider session can write a terminal `result` JSONL event before
/// the interactive TUI has finished rendering tool output, plan presentations,
/// or trailing assistant text into its tmux pane. S2-b makes the JSONL
/// terminator the sole finalize truth source, so this path no longer waits for
/// or suppresses completion based on quiescence.
///
/// We currently only gate `RuntimeHandoffKind::ClaudeTui`. `LegacyTmuxWrapper`
/// drives a non-interactive wrapper script whose `result` event coincides with
/// the script exiting, so no extra observation is needed.
///
/// `task_notification_kind` is accepted but intentionally does NOT skip the
/// observation — a Background or MonitorAutoTurn that runs inside ClaudeTui
/// still emits a visible status transition (`✅ 백그라운드 완료`).
#[inline]
pub(super) fn should_gate_completion_for_tui_quiescence(
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    rebind_origin: bool,
    _task_notification_kind: Option<crate::services::agent_protocol::TaskNotificationKind>,
) -> bool {
    // rebind_origin inflights describe an externally-launched tmux session
    // that AgentDesk did not start — the operator (not the Discord turn)
    // owns input cadence. Don't attach AgentDesk's pane observation to it.
    if rebind_origin {
        return false;
    }
    matches!(
        runtime_kind,
        Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui)
    )
}

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

    struct EnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_root(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self { previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.as_ref() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

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

    #[tokio::test]
    async fn finish_monitor_auto_turn_if_claimed_releases_mailbox_token() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("runtime root");
        let _env = EnvGuard::set_root(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_019_240);
        let synthetic_message_id = MessageId::new(4_019_340);
        let ledger_generation = next_monitor_auto_turn_ledger_generation();
        let token = Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn_kinded(
                &shared,
                channel_id,
                token.clone(),
                UserId::new(1),
                synthetic_message_id,
                crate::services::turn_orchestrator::ActiveTurnKind::MonitorAutoTurn,
            )
            .await
        );
        crate::services::discord::increment_global_active(&shared, "test_monitor_auto_turn");
        shared.turn_finalizer.register_start(
            crate::services::discord::turn_finalizer::TurnKey::new(
                channel_id,
                synthetic_message_id.get(),
                ledger_generation,
            ),
            provider.clone(),
            crate::services::discord::inflight::RelayOwnerKind::Watcher,
            &shared,
        );

        let mut claimed = true;
        let mut finished = false;
        let mut claimed_message_id = Some(synthetic_message_id);
        let mut claimed_generation = Some(ledger_generation);
        finish_monitor_auto_turn_if_claimed(
            &shared,
            &provider,
            channel_id,
            &mut claimed,
            &mut finished,
            &mut claimed_message_id,
            &mut claimed_generation,
        )
        .await;

        assert!(!claimed);
        assert!(finished);
        assert_eq!(claimed_message_id, None);
        assert_eq!(claimed_generation, None);
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        let snapshot = shared.mailbox(channel_id).snapshot().await;
        assert_eq!(snapshot.active_user_message_id, None);
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
    fn busy_observed_still_emits() {
        assert!(TuiCompletionGateOutcome::BusyObserved.should_emit_completion());
    }
}

/// #4047 regression coverage — the old lifecycle pause derived from
/// `TuiCompletionGateOutcome` is gone. A busy pane observation is telemetry only
/// and must not suppress completion or cleanup after the finalizer authority
/// proves `Done`.
#[cfg(test)]
mod lifecycle_stage_pause_matrix_tests {
    use super::TuiCompletionGateOutcome;

    fn lifecycle_stage_paused(outcome: TuiCompletionGateOutcome) -> bool {
        let _ = outcome;
        false
    }

    #[test]
    fn gate_outcomes_never_pause_lifecycle() {
        assert!(!lifecycle_stage_paused(TuiCompletionGateOutcome::NotGated));
        assert!(!lifecycle_stage_paused(
            TuiCompletionGateOutcome::ConfirmedIdle
        ));
        assert!(!lifecycle_stage_paused(
            TuiCompletionGateOutcome::SkippedDead
        ));
        assert!(!lifecycle_stage_paused(
            TuiCompletionGateOutcome::BusyObserved
        ));
    }

    #[test]
    fn every_outcome_emits_completion() {
        for outcome in [
            TuiCompletionGateOutcome::NotGated,
            TuiCompletionGateOutcome::ConfirmedIdle,
            TuiCompletionGateOutcome::SkippedDead,
            TuiCompletionGateOutcome::BusyObserved,
        ] {
            assert!(outcome.should_emit_completion(), "{outcome:?}");
            assert!(!lifecycle_stage_paused(outcome), "{outcome:?}");
        }
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
    let range_intersects_turn =
        data_start_offset <= turn_start_offset && turn_start_offset < current_offset;
    if !range_intersects_turn {
        return false;
    }

    // #4380: a CRASH restart (no graceful drain → no `restart_report`, so
    // `restart_mode == None`) re-adopts a still-live bridge turn whose bridge died
    // mid-stream WITHOUT a Watcher handoff, so the row keeps `relay_owner_kind ==
    // None`. The recovery path stamps `readopted_from_inflight` before spawning the
    // recovery watcher — honour it exactly like the planned-restart hatch below,
    // otherwise the recovered watcher yields to the DEAD bridge and black-holes
    // 100% of the turn's remaining output (the recurring `.stuck-manual-*` wedge).
    if super::recovery_engine::crash_readopt_live_relay_resume_required(state) {
        return false;
    }

    // After a planned dcserver restart the old bridge owner is gone. If the
    // terminal body has not been durably committed, yielding here black-holes the
    // recovered watcher output instead of preventing a duplicate.
    if state.restart_mode.is_some() && !state.terminal_delivery_committed {
        return false;
    }

    true
}

#[cfg(test)]
mod active_bridge_turn_guard_tests {
    use super::watcher_should_yield_to_inflight_state;
    use crate::services::discord::InflightRestartMode;
    use crate::services::discord::inflight::{InflightTurnState, RelayOwnerKind};
    use crate::services::provider::ProviderKind;
    use std::ffi::OsString;
    use std::path::Path;

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set_path(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn with_ownerless_codex_tui_state(test: impl FnOnce(InflightTurnState)) {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());

        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            1_479_671_301_387_059_200,
            Some("adk-cdx".to_string()),
            343_742_347_365_974_026,
            1_520_972_895_491_325_952,
            1_520_975_526_431_424_663,
            "deploy release".to_string(),
            Some("019f10e3-3dad-73c2-9d8c-e6188e4ccc7c".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/codex-rollout.jsonl".to_string()),
            None,
            0,
        );
        state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
        state.turn_start_offset = Some(0);
        state.set_relay_owner_kind(RelayOwnerKind::None);

        test(state);
    }

    #[test]
    fn planned_restart_ownerless_uncommitted_turn_does_not_yield_to_dead_bridge() {
        with_ownerless_codex_tui_state(|mut state| {
            state.set_restart_mode(InflightRestartMode::DrainRestart);

            assert!(
                !watcher_should_yield_to_inflight_state(
                    Some(&state),
                    "AgentDesk-codex-adk-cdx",
                    0,
                    2_019_364,
                ),
                "restore_inflight watcher must deliver when no terminal delivery was committed"
            );
        });
    }

    #[test]
    fn ownerless_ordinary_turn_still_yields_to_active_bridge() {
        with_ownerless_codex_tui_state(|state| {
            assert!(watcher_should_yield_to_inflight_state(
                Some(&state),
                "AgentDesk-codex-adk-cdx",
                0,
                2_019_364,
            ));
        });
    }

    #[test]
    fn planned_restart_after_terminal_commit_may_still_yield_as_duplicate() {
        with_ownerless_codex_tui_state(|mut state| {
            state.set_restart_mode(InflightRestartMode::DrainRestart);
            state.terminal_delivery_committed = true;

            assert!(watcher_should_yield_to_inflight_state(
                Some(&state),
                "AgentDesk-codex-adk-cdx",
                0,
                2_019_364,
            ));
        });
    }

    #[test]
    fn session_bound_relay_owner_still_suppresses_watcher_duplicate() {
        with_ownerless_codex_tui_state(|mut state| {
            state.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);

            assert!(watcher_should_yield_to_inflight_state(
                Some(&state),
                "AgentDesk-codex-adk-cdx",
                0,
                2_019_364,
            ));
        });
    }

    // #4380 reproduction: a CRASH restart (no `restart_report` → `restart_mode ==
    // None`) re-adopts a still-live real-user bridge turn (`relay_owner_kind ==
    // None`) and stamps `readopted_from_inflight`. The recovered watcher MUST resume
    // relay, not yield to the dead bridge. Before the fix this yielded → 100% silent
    // loss. Removing the `crash_readopt_live_relay_resume_required` escape branch in
    // `watcher_should_yield_to_inflight_state` makes this assert FAIL (the fn returns
    // `true` again), which is the mutation proof.
    #[test]
    fn crash_readopt_ownerless_uncommitted_turn_does_not_yield_to_dead_bridge() {
        with_ownerless_codex_tui_state(|mut state| {
            state.readopted_from_inflight = true; // restart_mode stays None (crash)

            assert!(
                !watcher_should_yield_to_inflight_state(
                    Some(&state),
                    "AgentDesk-codex-adk-cdx",
                    0,
                    2_019_364,
                ),
                "a crash-re-adopted uncommitted live turn must resume relay, not yield to the dead bridge (#4380)"
            );
        });
    }

    // The crash escape hatch is scoped to LIVE turns: once terminal delivery is
    // committed a watcher relay would be a duplicate, so a committed re-adopt still
    // yields. This guards the `!terminal_delivery_committed` clause of the predicate.
    #[test]
    fn crash_readopt_after_terminal_commit_may_still_yield_as_duplicate() {
        with_ownerless_codex_tui_state(|mut state| {
            state.readopted_from_inflight = true;
            state.terminal_delivery_committed = true;

            assert!(watcher_should_yield_to_inflight_state(
                Some(&state),
                "AgentDesk-codex-adk-cdx",
                0,
                2_019_364,
            ));
        });
    }
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
        provider,
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

#[allow(clippy::too_many_arguments)]
fn persist_watcher_stream_progress(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    // #3558: the per-turn identity the watcher captured for this panel. `Some`
    // rejects a write onto a fresh row B (different turn started after this
    // frame); `None` (early frames before identity capture) falls back to the
    // historical tmux-session-only guard inside the helper.
    require_identity: Option<&super::inflight::InflightTurnIdentity>,
    current_msg_id: Option<MessageId>,
    full_response: &str,
    response_sent_offset: usize,
    current_tool_line: Option<&str>,
    prev_tool_status: Option<&str>,
    task_notification_kind: Option<TaskNotificationKind>,
    any_tool_used: bool,
    has_post_tool_text: bool,
    // #3871: the frozen streamed rollover-prefix ids accumulated this invocation,
    // persisted so a later-iteration / post-restart terminal fallback can delete them.
    streaming_rollover_frozen_msg_ids: &[MessageId],
) {
    if full_response.len() < response_sent_offset {
        tracing::debug!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session = %tmux_session_name,
            response_sent_offset,
            full_response_len = full_response.len(),
            "watcher: skipping stream-progress persistence until parsed body catches up"
        );
        return;
    }

    // #3558: pre-emit the in-bounds telemetry against the caller's snapshot for
    // continuity; the helper re-clamps `response_sent_offset` against the
    // freshly reloaded `full_response` under the lock, so the persisted value is
    // always in-bounds regardless of this advisory check.
    let normalized_response_sent_offset =
        normalize_response_sent_offset(full_response, response_sent_offset);
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

    // #3558: single-flock read-modify-write. `last_offset` is NOT in the patch
    // — the helper preserves whatever the in-lock disk reload carries, so a
    // concurrent owner-gated `refresh_inflight_last_offset_*` advance can no
    // longer be clobbered backward by this previously-unlocked load→save TOCTOU.
    let _ = super::inflight::persist_watcher_stream_progress_locked(
        provider,
        channel_id.get(),
        require_identity,
        tmux_session_name,
        super::inflight::WatcherStreamProgressPatch {
            current_msg_id: current_msg_id.map(MessageId::get),
            full_response: full_response.to_string(),
            response_sent_offset,
            current_tool_line: current_tool_line.map(str::to_string),
            prev_tool_status: prev_tool_status.map(str::to_string),
            task_notification_kind,
            any_tool_used,
            has_post_tool_text,
            streaming_rollover_frozen_msg_ids: streaming_rollover_frozen_msg_ids
                .iter()
                .map(|id| id.get())
                .collect(),
        },
    );
}

async fn finish_restored_watcher_active_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    // #3016/#3645: finalizer ledger identity captured by the caller BEFORE it
    // cleared inflight (a reload here returns `None`). Real Discord turns use
    // `user_msg_id`; id-0 synthetic turns use their persisted finalizer_turn_id.
    finalizer_turn_id: u64,
    finish_mailbox_on_completion: bool,
    // #3016 option A: the caller observed a *normal completion* (terminal output
    // committed / pane confirmed idle past the relay) and wants the single-
    // authority finalizer driven regardless of the restore flag. The finalizer
    // is idempotent — a turn already finalized by the bridge resolves to
    // `AlreadyFinalized` — so an unconditional submit at the confirmed-completion
    // point cannot over-finalize. This decoupled the watcher's normal-completion
    // finalize from `mailbox_finalize_owed` (removed in #3016 phase-5b2) /
    // `finish_mailbox_on_completion`. Restore/recovery callers that have NOT
    // confirmed a normal completion pass `false` and keep the restore-gated path.
    normal_completion: bool,
    _kickoff_queue: bool,
    // #3350 codex r1-1: the caller's PRE-CLEAR row snapshot for the #3303
    // DeferredClaim marker ensure — inflight is cleared before this helper
    // (see `finalizer_turn_id`), so a row re-load cannot authenticate the
    // watcher's synthetic turns. `None` keeps the row-reload fallback.
    claim_snapshot: Option<super::turn_finalizer::SyntheticClaimSnapshot>,
    stop_source: &'static str,
) -> bool {
    // Default context for every caller EXCEPT the #4106 post-early-release site,
    // which routes through `finish_restored_watcher_active_turn_with_ctx` directly
    // to downgrade its EXPECTED identity-guard miss from WARN to debug.
    finish_restored_watcher_active_turn_with_ctx(
        shared,
        provider,
        channel_id,
        finalizer_turn_id,
        finish_mailbox_on_completion,
        normal_completion,
        _kickoff_queue,
        claim_snapshot,
        super::turn_finalizer::FinalizeContext::watcher(),
        stop_source,
    )
    .await
}

/// #4106: inner finalize that accepts an explicit `FinalizeContext`. The thin
/// wrapper above pins `FinalizeContext::watcher()` for all legacy callers; the
/// post-early-release watcher site passes `watcher_after_pre_panel_release()` so
/// its deterministic (already-released) identity-guard miss logs at debug.
#[allow(clippy::too_many_arguments)]
async fn finish_restored_watcher_active_turn_with_ctx(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    finalizer_turn_id: u64,
    finish_mailbox_on_completion: bool,
    normal_completion: bool,
    _kickoff_queue: bool,
    claim_snapshot: Option<super::turn_finalizer::SyntheticClaimSnapshot>,
    finalize_ctx: super::turn_finalizer::FinalizeContext,
    stop_source: &'static str,
) -> bool {
    // The mailbox is cleared now when EITHER gate holds:
    //   * `normal_completion` → confirmed terminal output committed / pane idle
    //     (#3016 option A). The canonical post-phase-5b1 finalize trigger.
    //   * `finish_mailbox_on_completion` → inflight-restore semantics (a
    //     restored/recovered watcher inherits its turn from the bridge, so
    //     it owns the cancel_token when the turn ends). Pre-existing.
    //
    // #3016 phase-5b2: the third gate (`delegated_finalize_owed`, the #1452
    // `mailbox_finalize_owed` flag) is removed — both production sites pass
    // `normal_completion = true`, and the ledger's exactly-once phase gate
    // makes a double `mailbox_finish_turn` idempotent.
    //
    // `kickoff_queue` is the dispatch-lifecycle gate (codex #1670 P2):
    // cleanup must run even on a failed dispatch (orphan prevention), but
    // auto-dispatching the next queued turn must not — callers pass
    // `dispatch_ok` here.
    //
    // #3016 (codex R1): returns whether this helper actually DROVE the
    // finalize (did not early-return); the caller folds it into
    // `watcher_handled_mailbox_finish` so the post-finalize lifecycle (queue
    // kickoff suppression + terminal-stop) reflects the real finalize.
    if !normal_completion && !finish_mailbox_on_completion {
        return false;
    }

    // #3016 phase 3: route the watcher terminal through the single-authority finalizer
    // instead of calling mailbox_finish_turn + counter + side-effects inline. The ledger
    // phase gate makes this exactly-once across bridge and watcher: whichever submits
    // first finalizes; the loser gets `AlreadyFinalized` and the watcher simply skips
    // the queue kickoff (the winner already owns it). Use the finalizer id the
    // caller captured BEFORE clearing inflight (reloading inflight here returns `None`
    // because the watcher already wiped it). The exact id makes the ledger match
    // precise: a stale terminal from an already-finalized turn resolves to that turn's
    // `Finalized` entry (→ AlreadyFinalized) instead of accidentally finalizing a
    // queued follow-up. The watcher cleared inflight inline before calling this helper,
    // and never marked completion-cleanup or drained voice, so `FinalizeContext::watcher`
    // reproduces exactly that side-effect set; the kickoff stays gated on the caller's
    // `kickoff_queue`.
    let outcome = shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            super::turn_finalizer::TurnKey::new(
                channel_id,
                finalizer_turn_id,
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::turn_finalizer::TerminalEvent::Complete,
            finalize_ctx,
            claim_snapshot,
            shared.clone(),
        )
        .await;
    let (_mailbox_online, _has_pending) = match outcome {
        super::turn_finalizer::FinalizeOutcome::Finalized {
            has_pending,
            mailbox_online,
            ..
        } => {
            // #3016 phase-5b2: the legacy `mailbox_finalize_owed` flag that was
            // revoked here (so a later watcher swap could not run stale cleanup
            // against the NEXT active turn) has been removed entirely. The
            // ledger's exactly-once phase gate is now the sole arbiter, so there
            // is no stale flag a surviving watcher could swap.
            (mailbox_online, has_pending)
        }
        super::turn_finalizer::FinalizeOutcome::AlreadyFinalized
        | super::turn_finalizer::FinalizeOutcome::Deferred => (false, false),
    };
    let _ = stop_source;
    // Drove the finalize (reached here past the early-return gate).
    true
}

async fn release_restored_watcher_active_turn_before_panel_edit(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    finalizer_turn_id: u64,
) -> bool {
    if finalizer_turn_id == 0 {
        return false;
    }

    // #4106 review-fix (codex): snapshot the channel role override THIS turn owns
    // BEFORE any await. The removal below runs after awaits (mailbox finish +
    // clear_watchdog_deadline_override), during which a fresh same-channel
    // counter-model follow-up can insert its OWN override (intake_turn.rs) even
    // before it claims the slot. A bare channel-keyed remove would clobber that;
    // remove_owned_role_override only drops the value we still own.
    let pre_release_role_override =
        super::turn_finalizer::cleanup::snapshot_role_override(shared, channel_id);

    let finish = super::mailbox_finish_turn_if_matches(
        shared,
        provider,
        channel_id,
        MessageId::new(finalizer_turn_id),
    )
    .await;
    let Some(token) = finish.removed_token.as_ref() else {
        return false;
    };

    // #4106 review-fix: cancel the removed token, decrement the counter, AND run
    // the finalizer's D-side channel cleanup here. Hoisting the release ahead of
    // the awaited panel edit makes the LATE do_finalize see removed_token=None
    // and take the guarded-miss SKIP branch (finalize.rs), so without this the
    // cleanup would be dropped on every normal completion. Running it here is
    // safe: we release turn A into a still-idle channel (no newer turn has
    // claimed yet, since a follow-up needs cancel_token.is_none() which this
    // release just produced), so it cannot clobber a follow-up's channel state.
    // Mirrors the finalizer non-miss branch (finalize.rs D-section) and the
    // recovery release bundle (health/recovery.rs); voice drain is omitted
    // because the watcher finalize path sets drain_voice=false.
    token.cancelled.store(true, Ordering::Relaxed);
    super::saturating_decrement_global_active(shared);

    super::turn_finalizer::cleanup::clear_watchdog_and_kick_thread_parents_after_turn_release(
        shared, provider, channel_id,
    )
    .await;
    if !finish.has_pending {
        super::turn_finalizer::cleanup::remove_owned_role_override(
            shared,
            channel_id,
            pre_release_role_override,
        );
    }
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
    force_next_watcher_status_update, process_watcher_lines, process_watcher_lines_for_turn,
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
        let mut state = InflightTurnState::new(
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
        // #3558: seed a non-zero relay watermark the streaming caller must NOT
        // own — the locked RMW must preserve it verbatim.
        state.last_offset = 777;
        super::super::inflight::save_inflight_state(&state).expect("save inflight");

        persist_watcher_stream_progress(
            &provider,
            channel_id,
            tmux_session_name,
            None,
            Some(MessageId::new(9100000000000000125)),
            "[E2E:E18:OK]\n\n",
            0,
            Some("Bash: sleep 60"),
            Some("Bash"),
            None,
            true,
            false,
            &[],
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
        // #3558: the streaming progress write does NOT own `last_offset` — it
        // must survive untouched (the core TOCTOU fix).
        assert_eq!(
            persisted.last_offset, 777,
            "streaming progress must preserve the non-owned last_offset watermark"
        );

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
    }

    #[test]
    fn persist_watcher_stream_progress_skips_rewind_seed_before_body_catches_up_4115() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1509350490461180415);
        let tmux_session_name = "AgentDesk-claude-adk-issue-4115-rewind-progress";
        let mut state = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some("claude-pipe".to_string()),
            4154,
            9_415,
            9_416,
            "prompt".to_string(),
            Some("session-4115".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/agentdesk-4115.jsonl".to_string()),
            None,
            128,
        );
        state.full_response = "already persisted prefix".to_string();
        state.response_sent_offset = state.full_response.len();
        super::super::inflight::save_inflight_state(&state).expect("save inflight");

        persist_watcher_stream_progress(
            &provider,
            channel_id,
            tmux_session_name,
            None,
            Some(MessageId::new(9_416)),
            "",
            128,
            None,
            None,
            None,
            false,
            false,
            &[],
        );

        let reloaded = super::super::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("reload row");
        assert_eq!(reloaded.full_response, "already persisted prefix");
        assert_eq!(reloaded.response_sent_offset, state.response_sent_offset);

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

#[cfg(test)]
mod streaming_rollover_frozen_prefix_persistence_tests {
    use super::{
        persist_watcher_stream_progress, restored_watcher_turn_from_inflight, watcher_stream_seed,
    };
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};

    // #3871: the frozen rollover-prefix ids must survive ACROSS `'watcher_loop`
    // iterations / a watcher restart. The local accumulator is `Vec::new()`'d each
    // iteration, so a terminal full-body fallback that runs in a LATER iteration
    // than the rollover-freeze (idle-split where the result JSONL lags, or a
    // watcher restart mid-turn) would — without persistence — start with an empty
    // set and leave the earlier frozen prefix UNDELETED (residual duplicate). This
    // pins the persist→restore round-trip + the monotonic union-merge so iteration
    // B still deletes the prefix iteration A froze.
    #[test]
    fn frozen_prefix_persists_across_iteration_and_restore_for_terminal_delete() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1_521_269_012_347_097_158);
        let tmux_session_name = "AgentDesk-claude-adk-3871-persist";
        let state = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some("claude-pipe".to_string()),
            42,
            7_001,
            7_002, // current_msg_id non-zero => restorable
            "long answer".to_string(),
            Some("session-3871".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/agentdesk-3871.jsonl".to_string()),
            None,
            0,
        );
        super::super::inflight::save_inflight_state(&state).expect("save inflight");

        // Iteration A froze prefix F1 mid-stream and persisted progress.
        let f1 = MessageId::new(9_001);
        persist_watcher_stream_progress(
            &provider,
            channel_id,
            tmux_session_name,
            None,
            Some(MessageId::new(9_100)),
            "first chunk frozen…",
            0,
            None,
            None,
            None,
            false,
            false,
            &[f1],
        );

        // Iteration B / a watcher restart re-enters with an EMPTY local vec and
        // SEEDS from the persisted row: the frozen prefix must come back so the
        // terminal full-body fallback can still delete it.
        let reloaded = super::super::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("reload row A");
        let restored = restored_watcher_turn_from_inflight(&reloaded, tmux_session_name, false)
            .expect("a non-rebind inflight with a real current_msg_id restores");
        let seed = watcher_stream_seed(Some(restored));
        assert_eq!(
            seed.streaming_rollover_frozen_msg_ids,
            vec![f1],
            "iteration B / restart must restore the prefix frozen in iteration A"
        );
        assert_eq!(
            super::placeholder_suppression::watcher_rollover_prefixes_to_delete_on_terminal(
                true,
                &seed.streaming_rollover_frozen_msg_ids,
            ),
            vec![f1],
            "the restored frozen prefix is deleted on iteration B's full-body fallback (no residual dup)"
        );

        // A SECOND freeze (F2) in iteration B union-merges monotonically — F1 is
        // never dropped and no id is duplicated.
        let f2 = MessageId::new(9_002);
        persist_watcher_stream_progress(
            &provider,
            channel_id,
            tmux_session_name,
            None,
            Some(MessageId::new(9_101)),
            "first chunk frozen…second chunk frozen…",
            0,
            None,
            None,
            None,
            false,
            false,
            &[f1, f2],
        );
        let reloaded2 = super::super::inflight::load_inflight_state(&provider, channel_id.get())
            .expect("reload row B");
        assert_eq!(
            reloaded2.streaming_rollover_frozen_msg_ids,
            vec![f1.get(), f2.get()],
            "the persisted frozen-prefix set is monotonic (union, no dup)"
        );

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
    }
}
