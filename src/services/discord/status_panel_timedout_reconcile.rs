//! #3886 — TimedOut-completion-gate status-panel reconcile.
//!
//! A warm hosted TUI turn (`AgentDesk-claude-dm-*`, `AgentDesk-claude-adk-cc`,
//! `AgentDesk-codex-adk-cdx`, …) can leave its Discord live status panel stuck
//! at `진행 중 / Active` forever: the `tui_completion_quiescence` gate returns
//! `TimedOut` (a skill turn keeps running past the 3s window for memento writes
//! etc.), so per #2161 the caller SUPPRESSES `StatusEvent::TurnCompleted`. The
//! gate doc names the placeholder sweeper / next-turn intake as the reconcile
//! that closes the lingering Active panel — but neither emitted a panel-finalize
//! event, and on the relay-dead frontier-0 path the watcher GateTimeout
//! finalizer (gated on `terminal_output_committed`) never fires either. The
//! live-events `DerivedStatus::Running` therefore never transitions to
//! `Completed`.
//!
//! This module adds the missing reconcile, driven from the placeholder sweeper:
//! for a still-tracked warm-TUI inflight whose live panel is still unfinished,
//! finalize the panel to `✅ 응답 완료` IFF the matched session's provider JSONL
//! DETERMINISTICALLY confirms the turn is terminal. The decision is on the turn
//! status (the same `result`-envelope + ready-for-input signal the gate uses to
//! short-circuit to `ConfirmedIdle`), NEVER on timestamp age — so the panel
//! resolves as soon as the turn is actually done, not on a 600s backstop.
//!
//! Lives OUTSIDE the #3016 hot files (declared from the non-hot `tmux.rs`); it
//! never re-runs the gate and never touches relay/cleanup bookkeeping.

use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::services::discord::SharedData;
use crate::services::discord::inflight::{InflightTurnState, parse_started_at_unix};
use crate::services::discord::turn_bridge::{
    complete_status_panel_v2_with_http, normalize_status_panel_message_id,
};
use crate::services::provider::ProviderKind;
use crate::services::tui_turn_state::{TuiTurnState, observe_provider_jsonl_turn_state};

/// Deterministic "this turn is terminal" probe used by the reconcile.
///
/// Mirrors the gate's `ConfirmedIdle` signal with PUBLIC inputs only (provider
/// JSONL turn-state + inflight offset fields), so it carries the SAME honesty
/// guarantees without reaching into the hot `tmux_watcher` module:
///   * `!rebind_origin` — operator-launched panes are never AgentDesk-gated.
///   * session-bound (a tmux session + a non-empty output JSONL path).
///   * the CURRENT turn advanced its own output past `turn_start_offset` — so a
///     stale PRIOR `result` envelope still in the shared session JSONL cannot
///     masquerade as this turn's completion.
///   * the provider JSONL structured turn state is `Idle` (a terminal `result`
///     envelope landed and the runtime is back at ready-for-input).
///
/// Any other shape returns `false` (the row is left for the existing age-based
/// safety nets), so this never finalizes a pane that is still producing output —
/// the #2161 premature-completion guard is preserved.
fn turn_jsonl_deterministically_terminal(
    provider: &ProviderKind,
    state: &InflightTurnState,
) -> bool {
    if state.rebind_origin {
        return false;
    }
    let Some(output_path) = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return false;
    };
    if state
        .tmux_session_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .is_none()
    {
        return false;
    }
    let advanced = state
        .turn_start_offset
        .map(|start| state.last_offset > start)
        .unwrap_or(false);
    if !advanced {
        return false;
    }
    // Mirror the gate's `matched_session_jsonl_turn_state` file guard: a missing
    // or empty JSONL is `Unknown`, NOT terminal — the bare `observe_*` probe maps
    // an absent file to `Idle`, which would falsely finalize a turn whose output
    // file is gone. Only a real, non-empty JSONL can confirm completion.
    let path = std::path::Path::new(output_path);
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() > 0 => {}
        _ => return false,
    }
    observe_provider_jsonl_turn_state(provider, path) == TuiTurnState::Idle
}

/// Pure reconcile decision: a status panel left unfinished by a suppressed
/// completion (`TuiCompletionGateOutcome::TimedOut.should_emit_completion() ==
/// false`) is finalized to `✅ 응답 완료` IFF — AND ONLY IFF — the turn is now
/// `deterministic_terminal`. `panel_unfinished` keeps it idempotent (an already-
/// `Completed` panel is never re-finalized → no needless re-edit → heartbeat
/// byte-stability preserved); `deterministic_terminal` keeps it honest (a pane
/// still streaming is never marked done).
fn timed_out_panel_should_reconcile_to_done(
    panel_unfinished: bool,
    deterministic_terminal: bool,
) -> bool {
    panel_unfinished && deterministic_terminal
}

/// Reconcile a status panel stuck at `진행 중` after a `TimedOut` completion
/// gate. Returns `true` only when a panel-finalize edit/send actually committed.
///
/// Gating on `status_panel_is_unfinished` means this fires AT MOST ONCE per turn
/// (the push of `StatusEvent::TurnCompleted` flips the live state to `Completed`,
/// so the next sweep skips it) — preserving the heartbeat byte-stability
/// invariant. It only ever ADDS the terminal event the suppressed gate withheld;
/// it does not touch the #3812 confidence line or #3920 subagent surfacing.
pub(in crate::services::discord) async fn reconcile_timed_out_tui_status_panel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
) -> bool {
    if !shared.ui.status_panel_v2_enabled || state.channel_id == 0 {
        return false;
    }
    let channel_id = serenity::ChannelId::new(state.channel_id);
    let panel_unfinished = shared
        .ui
        .placeholder_live_events
        .status_panel_is_unfinished(channel_id);
    let deterministic_terminal = turn_jsonl_deterministically_terminal(provider, state);
    if !timed_out_panel_should_reconcile_to_done(panel_unfinished, deterministic_terminal) {
        return false;
    }
    let started_at_unix =
        parse_started_at_unix(&state.started_at).unwrap_or_else(|| chrono::Utc::now().timestamp());
    let status_panel_msg_id =
        normalize_status_panel_message_id(state.status_message_id.map(serenity::MessageId::new));
    // The reconcile owns no prior render text; `complete_status_panel_v2_with_http`
    // edits the persisted panel id (or sends a fallback when none) to the freshly
    // rendered `응답 완료` text and pushes the withheld `TurnCompleted` event.
    let mut last_status_panel_text = String::new();
    let committed = complete_status_panel_v2_with_http(
        shared,
        http,
        channel_id,
        status_panel_msg_id,
        provider,
        started_at_unix,
        &mut last_status_panel_text,
        false,
        "placeholder_sweeper_timedout_reconcile",
        (Some(state.user_msg_id), Some(state)),
    )
    .await;
    if committed {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            tmux_session = state.tmux_session_name.as_deref().unwrap_or(""),
            "[{ts}] \u{2705} #3886 reconciled status panel stuck at '진행 중' after TUI completion-gate TimedOut — provider JSONL deterministically confirms the turn is terminal; finalized panel to 응답 완료"
        );
    }
    committed
}

#[cfg(test)]
#[path = "status_panel_timedout_reconcile_tests.rs"]
mod status_panel_timedout_reconcile_tests;
