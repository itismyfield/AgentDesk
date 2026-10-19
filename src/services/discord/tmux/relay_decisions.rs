//! Watcher relay ownership, terminal, and background-completion decisions.

use std::time::Duration;

use poise::serenity_prelude::ChannelId;

use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::provider::ProviderKind;
use crate::services::session_backend::StreamLineState;
use crate::services::tmux_diagnostics::tmux_exit_reason_is_normal_completion;

use super::{
    TerminalRelayDecision, WATCHER_POST_TERMINAL_IDLE_WINDOW, WatcherStopDecision,
    WatcherStopInput, watcher_stop_decision_after_terminal_success,
};

const BACKGROUND_AGENT_PENDING_SNIFF_TIMEOUT: Duration = Duration::from_secs(2);

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
pub(in crate::services::discord) struct WatcherLineOutcome {
    pub found_result: bool,
    pub terminal_kind: Option<WatcherTerminalKind>,
    pub terminal_evidence_offset: Option<u64>,
    pub pre_turn_bytes_skipped: usize,
    pub soft_terminal_candidate: bool,
    pub is_prompt_too_long: bool,
    pub stale_resume_detected: bool,
    pub auto_compacted: bool,
    pub task_notification_kind: Option<TaskNotificationKind>,
    pub task_notification_context:
        Option<super::super::task_notification_delivery::TaskNotificationContext>,
    pub assistant_text_seen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum WatcherTerminalKind {
    HardResult,
    SoftStopHookSummary,
    SoftUserBoundary,
}

impl WatcherTerminalKind {
    pub(in crate::services::discord) fn as_str(self) -> &'static str {
        match self {
            Self::HardResult => "hard_result",
            Self::SoftStopHookSummary => "soft_stop_hook_summary",
            Self::SoftUserBoundary => "soft_user_boundary",
        }
    }
}

pub(super) fn record_watcher_invariant(
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

pub(super) fn tmux_death_is_normal_completion(
    reason: Option<&str>,
    _diagnostic: Option<&str>,
) -> bool {
    reason.is_some_and(tmux_exit_reason_is_normal_completion)
}

pub(super) fn stream_line_state_token_usage(state: &StreamLineState) -> Option<TurnTokenUsage> {
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

pub(super) fn watcher_ready_for_input_turn_completed(
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

pub(super) fn merge_task_notification_kind(
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

pub(super) fn terminal_relay_decision(
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

pub(super) fn advance_buffer_start_offset(
    start_offset: u64,
    before_len: usize,
    after_len: usize,
) -> u64 {
    start_offset.saturating_add(before_len.saturating_sub(after_len) as u64)
}

#[inline]
pub(super) fn should_suppress_relay_before_emit(
    paused: bool,
    epoch_changed: bool,
    turn_delivered: bool,
    deferred_monitor_ready: bool,
) -> bool {
    (paused || epoch_changed || turn_delivered) && !deferred_monitor_ready
}

#[inline]
pub(super) fn is_terminal_finalize_stop_candidate(
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
pub(in crate::services::discord) fn should_gate_completion_for_tui_quiescence(
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
pub(super) fn watcher_stop_decision_after_terminal_finalize(
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

pub(super) fn watcher_should_yield_to_active_bridge_turn(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
    current_offset: u64,
) -> bool {
    let state = super::super::inflight::load_inflight_state(provider, channel_id.get());
    watcher_should_yield_to_inflight_state(
        state.as_ref(),
        tmux_session_name,
        data_start_offset,
        current_offset,
    )
}

pub(super) fn watcher_should_yield_to_inflight_state(
    state: Option<&super::super::inflight::InflightTurnState>,
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
        super::super::inflight::RelayOwnerKind::Watcher => return false,
        super::super::inflight::RelayOwnerKind::StandbyRelay => {
            return current_offset > data_start_offset;
        }
        super::super::inflight::RelayOwnerKind::SessionBoundRelay => {
            return current_offset > data_start_offset;
        }
        super::super::inflight::RelayOwnerKind::Unknown => {
            return current_offset > data_start_offset;
        }
        super::super::inflight::RelayOwnerKind::None => {}
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
    if super::super::recovery_engine::crash_readopt_live_relay_resume_required(state) {
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
