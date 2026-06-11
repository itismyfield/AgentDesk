//! #3038 S1 tmux watcher TUI completion gate decisions.

use super::*;

/// #2161 — TUI completion gate. Callers ask `run_tui_completion_gate` to
/// confirm the underlying tmux pane has reached a `Ready for input`
/// quiescent state before pushing `StatusEvent::TurnCompleted` to the live
/// status panel.
///
/// Only `RuntimeHandoffKind::ClaudeTui` turns are gated; other runtime kinds
/// return `NotGated` (= emit immediately) so existing completion contracts
/// stay unchanged (see `should_gate_completion_for_tui_quiescence` in
/// `tmux.rs` for the full matrix).
///
/// The wait is bounded by `TUI_COMPLETION_QUIESCENCE_TIMEOUT`. On `TimedOut`
/// the caller MUST suppress the `TurnCompleted` emit — promoting the panel
/// to `✅ 응답 완료` on a still-busy pane reproduces the bug this gate
/// exists to prevent (Codex review #2161 H2). If terminal delivery is not
/// yet durably mirrored, the placeholder sweeper and next-turn intake
/// reconcile the lingering Active panel; already-committed delivery may still
/// proceed with non-visual lifecycle cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TuiCompletionGateOutcome {
    NotGated,
    ConfirmedIdle,
    SkippedDead,
    TimedOut,
}

impl TuiCompletionGateOutcome {
    /// `true` when callers should proceed with emitting the user-visible
    /// `TurnCompleted` status event. `false` only on `TimedOut`, where
    /// the pane is still busy past the bounded wait and emitting would
    /// reproduce the #2161 premature-completion bug. The placeholder
    /// sweeper / next-turn intake reconciles the still-Active panel later.
    pub(in crate::services::discord) fn should_emit_completion(self) -> bool {
        match self {
            Self::NotGated | Self::ConfirmedIdle | Self::SkippedDead => true,
            Self::TimedOut => false,
        }
    }
}

/// Source-agnostic terminal probe for a matched session's provider JSONL.
/// `InflightTurnState::turn_source` is audit metadata only (#2346/#2285).
pub(super) fn matched_session_jsonl_turn_state(
    provider: &ProviderKind,
    inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
) -> Option<crate::services::tui_turn_state::TuiTurnState> {
    let state = inflight?;
    if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return None;
    }
    let output_path = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    let path = std::path::Path::new(output_path);
    let Ok(metadata) = std::fs::metadata(path) else {
        return Some(crate::services::tui_turn_state::TuiTurnState::Unknown);
    };
    if !metadata.is_file() || metadata.len() == 0 {
        return Some(crate::services::tui_turn_state::TuiTurnState::Unknown);
    }
    Some(crate::services::tui_turn_state::observe_provider_jsonl_turn_state(provider, path))
}

pub(super) fn matched_session_structured_ready_for_input(
    provider: &ProviderKind,
    inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
) -> Option<crate::services::tui_turn_state::TuiReadyState> {
    let state = inflight?;
    if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return None;
    }
    let output_path = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    crate::services::tui_turn_state::jsonl_ready_for_input(
        provider,
        state.runtime_kind,
        std::path::Path::new(output_path),
        None,
    )
}

pub(super) fn jsonl_terminal_can_confirm_completion(
    inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
) -> bool {
    inflight.is_some_and(|state| {
        let has_session_binding = state
            .tmux_session_name
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            && state
                .output_path
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty());
        let placeholderless_discord_turn =
            state.user_msg_id != 0 && state.current_msg_id == state.user_msg_id;
        let adopted_session_turn =
            state.rebind_origin && state.user_msg_id == 0 && state.current_msg_id == 0;
        let watcher_owned_session_bound_turn = matches!(
            state.effective_relay_owner_kind(),
            crate::services::discord::inflight::RelayOwnerKind::Watcher
        ) && !state.rebind_origin;
        let managed_terminal_runtime_turn = matches!(
            state.runtime_kind,
            Some(
                crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
                    | crate::services::agent_protocol::RuntimeHandoffKind::ProcessBackend,
            )
        ) && !state.rebind_origin
            && state.user_msg_id != 0
            && state.current_msg_id != 0
            && state
                .turn_start_offset
                .map(|start| state.last_offset > start)
                .unwrap_or(false);
        let legacy_terminal_shortcut = if state.rebind_origin {
            adopted_session_turn
        } else {
            placeholderless_discord_turn
        };

        has_session_binding
            && ((state.status_message_id.is_none() && legacy_terminal_shortcut)
                || watcher_owned_session_bound_turn
                || managed_terminal_runtime_turn)
    })
}

pub(super) fn session_bound_relay_should_own_terminal_delivery(
    should_direct_send: bool,
    session_bound_discord_delivery_enabled: bool,
    session_bound_relay_turn_fully_mirrored: bool,
    relay_producer_session_name: Option<&str>,
    inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    should_direct_send
        && session_bound_discord_delivery_enabled
        && session_bound_relay_turn_fully_mirrored
        && relay_producer_session_name == Some(tmux_session_name)
        && crate::services::discord::session_relay_sink::session_bound_discord_relay_can_own_terminal_delivery(
            inflight,
            tmux_session_name,
        )
}

pub(super) fn post_terminal_jsonl_payload_contains_init_without_user_event(payload: &[u8]) -> bool {
    let mut contains_init = false;
    for line in String::from_utf8_lossy(payload).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("user") => return false,
            Some("system")
                if value.get("subtype").and_then(serde_json::Value::as_str) == Some("init") =>
            {
                contains_init = true;
            }
            _ => {}
        }
    }
    contains_init
}

#[cfg(test)]
#[path = "completion_gate_tests.rs"]
mod matched_session_jsonl_gate_tests;

pub(in crate::services::discord) async fn run_tui_completion_gate(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    task_notification_kind: Option<crate::services::agent_protocol::TaskNotificationKind>,
) -> TuiCompletionGateOutcome {
    let inflight =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get());
    if jsonl_terminal_can_confirm_completion(inflight.as_ref())
        && matched_session_jsonl_turn_state(provider, inflight.as_ref(), tmux_session_name)
            == Some(crate::services::tui_turn_state::TuiTurnState::Idle)
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            tmux_session = %tmux_session_name,
            "confirmed matched session completion from provider JSONL terminal envelope"
        );
        return TuiCompletionGateOutcome::ConfirmedIdle;
    }
    let runtime_kind = inflight.as_ref().and_then(|state| state.runtime_kind);
    let rebind_origin = inflight
        .as_ref()
        .map(|state| state.rebind_origin)
        .unwrap_or(false);

    if !crate::services::discord::tmux::should_gate_completion_for_tui_quiescence(
        runtime_kind,
        rebind_origin,
        task_notification_kind,
    ) {
        return TuiCompletionGateOutcome::NotGated;
    }
    let tmux_session_for_liveness = tmux_session_name.to_string();
    let pane_alive = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            crate::services::tmux_diagnostics::tmux_session_has_live_pane(
                &tmux_session_for_liveness,
            )
        }),
    )
    .await
    .unwrap_or(Ok(false))
    .unwrap_or(false);
    if !pane_alive {
        tracing::info!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            tmux_session = %tmux_session_name,
            "TUI completion gate skipped because tmux pane is no longer live"
        );
        return TuiCompletionGateOutcome::SkippedDead;
    }

    let started_at = tokio::time::Instant::now();
    loop {
        let ready = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::task::spawn_blocking({
                let provider = provider.clone();
                let tmux_session_name = tmux_session_name.to_string();
                let inflight = inflight.clone();
                move || {
                    matched_session_structured_ready_for_input(
                        &provider,
                        inflight.as_ref(),
                        &tmux_session_name,
                    )
                    .is_some_and(crate::services::tui_turn_state::TuiReadyState::is_ready)
                }
            }),
        )
        .await
        .unwrap_or(Ok(false))
        .unwrap_or(false);

        if ready {
            return TuiCompletionGateOutcome::ConfirmedIdle;
        }
        if started_at.elapsed() >= crate::services::discord::tmux::TUI_COMPLETION_QUIESCENCE_TIMEOUT
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                gate = "tui_completion_quiescence",
                "[{ts}] \u{26a0} TUI structured turn state was not idle after {:?} — suppressing turn-complete status to avoid premature completion (#2161); placeholder sweeper / next-turn intake will reconcile",
                crate::services::discord::tmux::TUI_COMPLETION_QUIESCENCE_TIMEOUT,
            );
            return TuiCompletionGateOutcome::TimedOut;
        }
        tokio::time::sleep(crate::services::discord::tmux::TUI_COMPLETION_QUIESCENCE_POLL_INTERVAL)
            .await;
    }
}
