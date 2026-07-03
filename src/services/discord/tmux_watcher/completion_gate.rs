//! #3038 S1 tmux watcher TUI completion gate decisions.

use super::*;
use crate::services::discord::turn_finalizer::{
    CompletionSignal, completion_signal_from_transcript,
};

/// #2161/#4047 — TUI completion observation. Callers ask
/// `run_tui_completion_gate` to record pane liveness and the strict provider
/// terminator signal before pushing `StatusEvent::TurnCompleted` to the live
/// status panel.
///
/// Only `RuntimeHandoffKind::ClaudeTui` turns are gated; other runtime kinds
/// return `NotGated` (= emit immediately) so existing completion contracts
/// stay unchanged (see `should_gate_completion_for_tui_quiescence` in
/// `tmux.rs` for the full matrix).
///
/// S2-b makes the provider JSONL terminator (`CompletionSignal::Done`) the sole
/// finalize truth source. Pane scraping no longer delays or suppresses
/// completion for structured JSONL sessions; it is retained only for process
/// liveness, a JSONL-less fallback readiness observation, and the
/// background-agent payload bit consumed by the footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TuiCompletionGateOutcome {
    NotGated,
    ConfirmedIdle,
    SkippedDead,
    BusyObserved,
}

impl TuiCompletionGateOutcome {
    /// S2-b: completion emission is unconditional once the finalizer authority
    /// has proven `Done`; this helper remains for call-site readability.
    pub(in crate::services::discord) fn should_emit_completion(self) -> bool {
        let _ = self;
        true
    }
}

pub(super) fn inflight_skips_tui_completion_observation(
    inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
) -> bool {
    inflight.is_some_and(|state| state.relay_ownership_only)
}

/// Matched-session terminal probe using the same strict provider terminator
/// authority as the finalizer. `InflightTurnState::turn_source` is audit
/// metadata only (#2346/#2285).
pub(super) fn matched_session_completion_signal(
    provider: &ProviderKind,
    inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
) -> Option<CompletionSignal> {
    let state = inflight?;
    if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return None;
    }
    let output_path = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    Some(completion_signal_from_transcript(
        provider,
        state.runtime_kind,
        std::path::Path::new(output_path),
    ))
}

pub(super) fn jsonl_terminal_can_confirm_completion(
    inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
) -> bool {
    inflight.is_some_and(|state| {
        if state.relay_ownership_only {
            return false;
        }
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
    if inflight_skips_tui_completion_observation(inflight.as_ref()) {
        tracing::info!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            tmux_session = %tmux_session_name,
            inflight_user_msg_id = inflight.as_ref().map(|state| state.user_msg_id).unwrap_or(0),
            inflight_current_msg_id = inflight.as_ref().map(|state| state.current_msg_id).unwrap_or(0),
            "skipped TUI completion observation for relay-only synthetic turn; relay remains owned by the bridge/sink path"
        );
        return TuiCompletionGateOutcome::NotGated;
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
    let pane_liveness = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            crate::services::tmux_diagnostics::tmux_session_pane_liveness(
                &tmux_session_for_liveness,
            )
        }),
    )
    .await
    .unwrap_or(Ok(
        crate::services::platform::tmux::PaneLiveness::DeadOrAbsent,
    ))
    .unwrap_or(crate::services::platform::tmux::PaneLiveness::DeadOrAbsent);
    if !matches!(
        pane_liveness,
        crate::services::platform::tmux::PaneLiveness::Live
    ) {
        tracing::info!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            tmux_session = %tmux_session_name,
            "TUI completion gate skipped because tmux pane is no longer live"
        );
        return TuiCompletionGateOutcome::SkippedDead;
    }

    let completion_signal = if jsonl_terminal_can_confirm_completion(inflight.as_ref()) {
        matched_session_completion_signal(provider, inflight.as_ref(), tmux_session_name)
            .unwrap_or(CompletionSignal::Unknown)
    } else {
        CompletionSignal::Unknown
    };
    let fallback_ready = if completion_signal == CompletionSignal::Unknown {
        crate::services::provider::tmux_session_fallback_ready_for_input(
            tmux_session_name,
            provider,
            runtime_kind,
        )
        .is_some_and(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
    } else {
        false
    };
    let legacy_observer_outcome = if completion_signal == CompletionSignal::Done || fallback_ready {
        TuiCompletionGateOutcome::ConfirmedIdle
    } else {
        TuiCompletionGateOutcome::BusyObserved
    };
    tracing::info!(
        provider = %provider.as_str(),
        channel = channel_id.get(),
        tmux_session = %tmux_session_name,
        completion_signal = ?completion_signal,
        fallback_ready,
        legacy_gate_outcome = ?legacy_observer_outcome,
        "TUI completion quiescence observation recorded; finalize verdict remains provider terminator authority"
    );
    legacy_observer_outcome
}
