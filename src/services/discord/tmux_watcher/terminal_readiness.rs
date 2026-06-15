//! #3479 Phase-1 rank-2 extraction: the tmux watcher's terminal-readiness +
//! inflight-classification PREDICATES and the pure buffer/message-id reconcilers
//! — the panel/lease eligibility checks, the JSONL `ready_for_input` sentinel
//! probes, the direct-terminal idle-commit predicate, and the suppressed-turn
//! buffer discard. PURE MOVE from `tmux_watcher.rs` (zero logic change) to shrink
//! the frozen root file below its maintainability baseline.
//!
//! These are all synchronous, side-effect-free (or local-`std::fs`-only) helpers
//! with ZERO coupling to `shared`/`http`. The async, `shared`-touching
//! `commit_watcher_direct_terminal_session_idle` sibling that sits BETWEEN these
//! two clusters in the root deliberately STAYS in `tmux_watcher.rs`. Items are
//! `pub(super)` so the parent watcher loop (and the sibling `panel_decisions`
//! module, which calls `watcher_inflight_is_panel_eligible` via the parent's
//! re-export glob) keep calling them by their original names. `InflightTurnState`
//! and the rank-1 `SessionBoundRelayAckTarget` resolve through `use super::*`.

use super::*;

pub(super) fn adopt_watcher_terminal_message_ids_from_inflight(
    placeholder_msg_id: &mut Option<serenity::MessageId>,
    placeholder_from_restored_inflight: &mut bool,
    status_panel_msg_id: &mut Option<serenity::MessageId>,
    inflight: &InflightTurnState,
    tmux_session_name: &str,
) {
    if inflight.rebind_origin {
        return;
    }
    let matches_current_watcher_session = inflight
        .tmux_session_name
        .as_deref()
        .map(str::trim)
        .is_some_and(|name| !name.is_empty() && name == tmux_session_name);
    if !matches_current_watcher_session {
        return;
    }
    let placeholderless_discord_turn = inflight.user_msg_id != 0
        && inflight.current_msg_id != 0
        && inflight.current_msg_id == inflight.user_msg_id;
    if placeholderless_discord_turn {
        return;
    }
    if placeholder_msg_id.is_none() && inflight.current_msg_id != 0 {
        *placeholder_msg_id = Some(serenity::MessageId::new(inflight.current_msg_id));
        *placeholder_from_restored_inflight = true;
    }
    if status_panel_msg_id.is_none() {
        *status_panel_msg_id =
            crate::services::discord::turn_bridge::normalize_status_panel_message_id(
                inflight.status_message_id.map(serenity::MessageId::new),
            );
    }
}

pub(super) fn watcher_inflight_represents_external_input(
    inflight: Option<&InflightTurnState>,
) -> bool {
    inflight.is_some_and(|inflight| {
        matches!(
            inflight.turn_source,
            crate::services::discord::inflight::TurnSource::ExternalInput
                | crate::services::discord::inflight::TurnSource::ExternalAdopted
        )
    })
}

/// status-panel-v2 eligibility for a watcher-driven inflight turn.
///
/// SEPARATE from `watcher_inflight_represents_external_input` on purpose: that
/// shared predicate backs the external-input delivery LEASE and the `⏳` anchor
/// lifecycle (#3164/#3174), and broadening it there would regress both. The
/// panel only needs to know whether the watcher should create/update/clean up a
/// live status panel for this turn, so it ALSO covers the synthetic
/// monitor/self-paced-loop turns (`TurnSource::MonitorTriggered`, created by
/// `ensure_monitor_auto_turn_inflight`) — which the lease/anchor sites must
/// keep ignoring.
pub(super) fn watcher_inflight_is_panel_eligible(inflight: Option<&InflightTurnState>) -> bool {
    inflight.is_some_and(|state| {
        watcher_inflight_represents_external_input(Some(state))
            || matches!(
                state.turn_source,
                crate::services::discord::inflight::TurnSource::MonitorTriggered
            )
    })
}

/// #3099: an external-input (TUI-direct / task-notification) inflight whose
/// `user_msg_id == 0` (or a `rebind_origin` synthetic) will be SKIPPED by the
/// `⏳ → ✅` reaction block (it targets `state.user_msg_id`, and `0` is no real
/// message). When such a turn completes, the `⏳` was added to a real notify-bot
/// message tracked by the prompt anchor, so the anchor-lifecycle cleanup must
/// run instead — otherwise the hourglass goes stale next to a `✅`.
pub(super) fn watcher_inflight_needs_anchor_lifecycle_cleanup(
    inflight: &InflightTurnState,
) -> bool {
    watcher_inflight_represents_external_input(Some(inflight))
        && (inflight.user_msg_id == 0 || inflight.rebind_origin)
}

pub(super) fn watcher_direct_terminal_should_commit_session_idle(
    direct_send_delivered: bool,
    inflight_present: bool,
    _external_input_lease_consumed_by_relay: bool,
    _prompt_anchor_present_before_relay: bool,
    _external_input_lease_before_relay: bool,
    _ssh_direct_pending: bool,
) -> bool {
    direct_send_delivered && !inflight_present
}

pub(super) fn watcher_terminal_token_update_status(
    watcher_direct_terminal_idle_committed: bool,
) -> &'static str {
    if watcher_direct_terminal_idle_committed {
        crate::db::session_status::IDLE
    } else {
        crate::db::session_status::TURN_ACTIVE
    }
}

/// #2442 (H3) — fast-path check for the wrapper's `ready_for_input` JSONL
/// sentinel in the tail of the session jsonl. Reads only the last ~4 KiB
/// so it stays O(1) regardless of jsonl size. False negatives just fall
/// back to the existing 2s `READY_FOR_INPUT_IDLE_PROBE_INTERVAL` cadence,
/// so partial-line / rotation edge cases are harmless.
pub(super) fn jsonl_tail_contains_ready_for_input_sentinel(output_path: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    const TAIL_WINDOW_BYTES: u64 = 4 * 1024;

    let Ok(mut file) = std::fs::File::open(output_path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    let len = meta.len();
    if len == 0 {
        return false;
    }
    let start = len.saturating_sub(TAIL_WINDOW_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut buf = Vec::with_capacity(TAIL_WINDOW_BYTES as usize);
    if file.read_to_end(&mut buf).is_err() {
        return false;
    }
    let needle = format!(
        "\"type\":\"{}\"",
        crate::services::tmux_common::WRAPPER_READY_FOR_INPUT_EVENT
    );
    String::from_utf8_lossy(&buf).contains(&needle)
}

pub(super) fn watcher_jsonl_turn_state_ready_for_input(
    provider: &crate::services::provider::ProviderKind,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    output_path: &str,
    current_offset: u64,
) -> Option<bool> {
    let path = std::path::Path::new(output_path);
    crate::services::tui_turn_state::jsonl_ready_for_input(
        provider,
        runtime_kind,
        path,
        Some(current_offset),
    )
    .map(crate::services::tui_turn_state::TuiReadyState::is_ready)
}

pub(super) fn watcher_session_ready_for_input(
    tmux_session_name: &str,
    provider: &crate::services::provider::ProviderKind,
    output_path: &str,
    current_offset: u64,
) -> bool {
    let runtime_kind =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
            .map(|binding| binding.runtime_kind)
            .or_else(|| {
                crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux_session_name)
            });
    if let Some(ready) = watcher_jsonl_turn_state_ready_for_input(
        provider,
        runtime_kind,
        output_path,
        current_offset,
    ) {
        return ready;
    }
    if crate::services::tui_turn_state::pane_ready_fallback_allowed(provider, runtime_kind) {
        crate::services::provider::tmux_session_ready_for_input(tmux_session_name, provider)
    } else {
        false
    }
}

pub(super) fn discard_watcher_pending_buffer_after_suppressed_turn(
    all_data: &mut String,
    all_data_start_offset: &mut u64,
    all_data_fully_mirrored_to_session_relay: &mut bool,
    all_data_session_bound_relay_ack: &mut Option<SessionBoundRelayAckTarget>,
    current_offset: u64,
) {
    all_data.clear();
    *all_data_start_offset = current_offset;
    *all_data_fully_mirrored_to_session_relay = true;
    *all_data_session_bound_relay_ack = None;
}

#[cfg(test)]
#[path = "terminal_readiness_tests.rs"]
mod tests;
