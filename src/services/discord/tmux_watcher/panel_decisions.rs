//! #3038 S1 tmux watcher status-panel and finalize decisions.

use super::*;

/// #3016 S3 fresh-idle finalize decision, extracted for unit-testing. `Done`
/// finalizes even empty; `PausedLive` defers; non-JSONL `Unknown` uses the
/// pane-idle fallback (empty waits on the 1800s backstop). The A2 race guards
/// preserve a follow-up that took the session during cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FreshIdleFinalizeDecision {
    /// No terminator — defer, preserve inflight, keep waiting.
    DeferPausedLive,
    /// Empty non-JSONL `Unknown` (#3016 5b1): could be awaiting an interactive
    /// prompt with no `PausedLive` signal — defer; the 1800s far-backstop
    /// finalizes it.
    DeferEmptyUnknown,
    /// A follow-up turn paused the watcher / bumped the epoch during cleanup —
    /// abort before the destructive clear; preserve inflight.
    AbortFollowupTookOver,
    /// The pinned pre-cleanup snapshot is a NEWER turn — skip the finalize so
    /// the follow-up is not released.
    SkipStale { pinned_user_msg_id: u64 },
    /// `Done` (even empty) or non-empty `Unknown`, and no follow-up took over —
    /// finalize with the pinned current-turn id.
    Finalize { user_msg_id: u64 },
}

#[allow(clippy::too_many_arguments)]
pub(super) fn watcher_fresh_idle_finalize_decision(
    completion_signal: crate::services::discord::turn_finalizer::CompletionSignal,
    full_response_is_empty: bool,
    paused_now: bool,
    epoch_changed: bool,
    pinned_pre_cleanup_inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
    current_offset: u64,
) -> FreshIdleFinalizeDecision {
    use crate::services::discord::turn_finalizer::CompletionSignal;
    // Reaching here already proves pane-idle (the fresh-idle gate fires only
    // after `watcher_session_ready_for_input` held past the idle timeout).
    match completion_signal {
        CompletionSignal::PausedLive => return FreshIdleFinalizeDecision::DeferPausedLive,
        CompletionSignal::Unknown if full_response_is_empty => {
            return FreshIdleFinalizeDecision::DeferEmptyUnknown;
        }
        CompletionSignal::Done | CompletionSignal::Unknown => {}
    }
    // A2 wrong-turn-race guards, applied before releasing the turn.
    if paused_now || epoch_changed {
        return FreshIdleFinalizeDecision::AbortFollowupTookOver;
    }
    let stale = committed_completion_is_stale_for_newer_turn(
        pinned_pre_cleanup_inflight,
        None,
        tmux_session_name,
        current_offset,
    );
    let pinned = pinned_finalize_user_msg_id(
        pinned_pre_cleanup_inflight,
        tmux_session_name,
        current_offset,
    );
    if stale || pinned == 0 {
        return FreshIdleFinalizeDecision::SkipStale {
            pinned_user_msg_id: pinned,
        };
    }
    FreshIdleFinalizeDecision::Finalize {
        user_msg_id: pinned,
    }
}

pub(super) fn watcher_should_clear_stale_terminal_message_ids(
    inflight_present: bool,
    has_assistant_response: bool,
    placeholder_msg_id: Option<serenity::MessageId>,
) -> bool {
    has_assistant_response && !inflight_present && placeholder_msg_id.is_some()
}

/// #3003: does the watcher need to proactively create a status-panel-v2
/// message for the live turn? A pure TUI-direct turn has no preceding
/// Discord-origin message to re-designate as a panel, so the watcher creates
/// one itself once v2 is enabled and none exists yet.
pub(super) fn watcher_should_create_external_input_status_panel(
    status_panel_v2_enabled: bool,
    status_panel_present: bool,
    inflight_represents_external_input: bool,
) -> bool {
    status_panel_v2_enabled && !status_panel_present && inflight_represents_external_input
}

pub(super) fn enqueue_watcher_status_panel_orphan(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    panel_msg_id: serenity::MessageId,
) {
    crate::services::discord::status_panel_orphan_store::enqueue_separate_status_panel_orphan(
        shared.ui.status_panel_v2_enabled,
        provider,
        &shared.token_hash,
        channel_id.get(),
        panel_msg_id.get(),
    );
}

/// #3077 (codex P1): decision for the TUI-direct status-panel publish site
/// once the atomic [`bind_status_panel`] has returned — the bind, not the
/// pre-send snapshot, is authoritative for whether the just-sent panel landed
/// on the inflight row. `Bound`/`AlreadyBound` → adopt it, don't delete.
/// `SkippedPanelAlreadySet(owned)` → the row owns a different panel: delete
/// the duplicate and adopt the row's current `owned` id (not the pre-bind
/// snapshot, which can be stale under a concurrent writer). `GuardMismatch` /
/// `Missing` / `IoError` → the bind never happened: delete the duplicate and
/// adopt nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TuiStatusPanelBindDecision {
    /// Delete (or enqueue-delete) the just-sent panel message.
    pub(super) delete_sent_panel: bool,
    /// Adopt the just-sent panel id; else adopt `owned_panel_id` (same turn only).
    pub(super) adopt_sent_panel: bool,
    /// Row's current owned panel id under `SkippedPanelAlreadySet`; `None` otherwise.
    pub(super) owned_panel_id: Option<u64>,
}

pub(super) fn resolve_tui_status_panel_bind_decision(
    outcome: crate::services::discord::inflight::StatusPanelBindOutcome,
) -> TuiStatusPanelBindDecision {
    use crate::services::discord::inflight::StatusPanelBindOutcome as Outcome;
    match outcome {
        Outcome::Bound { .. } | Outcome::AlreadyBound => TuiStatusPanelBindDecision {
            delete_sent_panel: false,
            adopt_sent_panel: true,
            owned_panel_id: None,
        },
        Outcome::SkippedPanelAlreadySet(owned) => TuiStatusPanelBindDecision {
            delete_sent_panel: true,
            adopt_sent_panel: false,
            owned_panel_id: Some(owned),
        },
        Outcome::GuardMismatch | Outcome::Missing | Outcome::IoError => {
            TuiStatusPanelBindDecision {
                delete_sent_panel: true,
                adopt_sent_panel: false,
                owned_panel_id: None,
            }
        }
    }
}

pub(super) fn watcher_persisted_status_panel_msg_id(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> Option<serenity::MessageId> {
    inflight.and_then(|state| {
        if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
            return None;
        }
        crate::services::discord::turn_bridge::normalize_status_panel_message_id(
            state.status_message_id.map(serenity::MessageId::new),
        )
    })
}

/// #3003 (codex P2 r2/r25): is the loaded inflight a TUI-direct/external-input
/// turn for *this* `tmux_session_name`, owned by the watcher relay (not
/// `turn_bridge` / the session-bound relay)? Both guards prevent the watcher
/// from publishing an orphan/duplicate status panel for the wrong turn or
/// wrong owner.
#[allow(dead_code)] // #3034: pinned by the watcher unit tests.
pub(super) fn watcher_inflight_is_external_input_for_session(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    inflight
        .filter(|state| state.tmux_session_name.as_deref() == Some(tmux_session_name))
        .is_some_and(|state| {
            watcher_inflight_represents_external_input(Some(state))
                && matches!(
                    state.effective_relay_owner_kind(),
                    crate::services::discord::inflight::RelayOwnerKind::Watcher
                )
        })
}

/// status-panel-v2 variant of `watcher_inflight_is_external_input_for_session`:
/// same session + watcher-relay-owner guards, but gated on the broader
/// `watcher_inflight_is_panel_eligible` so synthetic monitor/self-paced turns
/// also get a panel. Used only at panel-lifecycle sites.
pub(super) fn watcher_inflight_is_panel_eligible_for_session(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    inflight
        .filter(|state| state.tmux_session_name.as_deref() == Some(tmux_session_name))
        .is_some_and(|state| {
            watcher_inflight_is_panel_eligible(Some(state))
                && matches!(
                    state.effective_relay_owner_kind(),
                    crate::services::discord::inflight::RelayOwnerKind::Watcher
                )
        })
}

/// #3969: true when the inflight (for this `tmux_session_name`) is anything
/// other than a genuine Discord-user turn (`TurnSource::Managed`) — i.e. a TUI
/// mirror (`ExternalInput`/`ExternalAdopted`/`MonitorTriggered`) whose #3089
/// footer duplicates what the user already sees live in the terminal and must
/// be suppressed. Reads the chokepoint-fresh `inflight_before_relay` rather
/// than the cached top-of-iteration flag, so a turn whose row is created later
/// in the same iteration (e.g. `/loop` self-paced) is still caught.
pub(super) fn watcher_inflight_is_non_managed_tui_mirror_for_session(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    inflight
        .filter(|state| state.tmux_session_name.as_deref() == Some(tmux_session_name))
        .is_some_and(|state| {
            state.turn_source != crate::services::discord::inflight::TurnSource::Managed
        })
}

/// #3003: has the in-flight TUI-direct turn been abandoned, so a
/// watcher-created v2 panel can never reach terminal completion? True when the
/// inflight row is gone, replaced by a different turn on the same channel, or
/// a recent turn-stop tombstone covers this turn's byte range. Checked before
/// every early-return guard so none can bypass the reclaim.
pub(super) fn watcher_external_input_turn_abandoned(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    data_start_offset: u64,
    expected_identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) -> bool {
    match crate::services::discord::inflight::load_inflight_state(provider, channel_id.get()) {
        // #3107: absence alone isn't abandonment — a live turn can momentarily
        // lose its inflight row while the pane keeps producing. Probe the pane
        // lazily (only here) to tell a live turn from a real orphan.
        None => watcher_inflight_absence_is_abandonment(watcher_pane_live_turn_in_progress(
            tmux_session_name,
            output_path,
        )),
        Some(state) => {
            let replaced = expected_identity.is_some_and(|expected| {
                *expected
                    != crate::services::discord::inflight::InflightTurnIdentity::from_state(&state)
            });
            replaced
                || recent_turn_stop_for_watcher_range(
                    channel_id,
                    tmux_session_name,
                    data_start_offset,
                )
                .is_some()
        }
    }
}

/// #3351: at the orphan-panel reclaim sites, should the turn's relay
/// placeholder be reclaimed too? Only for external-input turns whose
/// placeholder still reads as a placeholder (never edited into a real
/// response body) and produced no assistant text.
pub(super) fn watcher_should_reclaim_orphan_turn_placeholder(
    turn_is_external_input: bool,
    placeholder_msg_id: Option<serenity::MessageId>,
    has_assistant_response: bool,
    last_edit_text: &str,
) -> bool {
    turn_is_external_input
        && placeholder_msg_id.is_some()
        && !has_assistant_response
        && crate::services::discord::placeholder_sweeper::is_message_still_placeholder(
            last_edit_text,
        )
}

const REDRIVE_PLACEHOLDER_SHIELD_MILLIS: i64 = 900_000;
const REDRIVE_PLACEHOLDER_CLOCK_SKEW_MILLIS: i64 = 5_000;

pub(super) fn redrive_shielded_placeholder(
    nudged_at_millis: i64,
    message_created_at_millis: i64,
    frontier_not_advanced: bool,
    now_millis: i64,
) -> bool {
    message_created_at_millis.saturating_add(REDRIVE_PLACEHOLDER_CLOCK_SKEW_MILLIS)
        >= nudged_at_millis
        && frontier_not_advanced
        && now_millis.saturating_sub(nudged_at_millis).max(0) < REDRIVE_PLACEHOLDER_SHIELD_MILLIS
}

/// #3107: a missing inflight is abandonment only when the pane isn't actively
/// streaming — an actively-streaming pane is a live turn that merely lost its
/// inflight row, so its status panel must be preserved.
pub(super) fn watcher_inflight_absence_is_abandonment(pane_actively_streaming: bool) -> bool {
    !pane_actively_streaming
}

/// Pure decision for the watcher completion-footer idle refresh: tick only when a
/// footer target is registered AND the refresh interval has elapsed.
pub(super) fn watcher_completion_footer_should_tick(
    has_registered_target: bool,
    elapsed: std::time::Duration,
    interval: std::time::Duration,
) -> bool {
    has_registered_target && elapsed >= interval
}

/// #3964: should the #3089 completion footer be suppressed for this
/// WATCHER-relayed terminal mirror (duplicate chrome for what the user
/// already sees live in the terminal)? True when panel-footer mode is on AND
/// any of: (1) `turn_is_external_input_for_session` — the cached
/// panel-eligibility flag, reliable only when the row already existed at loop
/// top; (2) `completion_background` — `<task-notification>` background/
/// MonitorAutoTurn turns, correct regardless of row timing; (3)
/// `turn_is_non_managed_tui_mirror` (#3969) — the chokepoint-fresh
/// non-`Managed`-origin check for turns whose row is created later (e.g.
/// `/loop` self-paced). All three are `false` for a genuine Discord-origin
/// (`Managed`) turn, so its footer is never stripped.
pub(super) fn watcher_external_input_completion_footer_suppressed(
    single_message_panel_footer_mode: bool,
    turn_is_external_input_for_session: bool,
    completion_background: bool,
    turn_is_non_managed_tui_mirror: bool,
) -> bool {
    single_message_panel_footer_mode
        && (turn_is_external_input_for_session
            || completion_background
            || turn_is_non_managed_tui_mirror)
}

#[cfg(test)]
#[path = "panel_decisions_tests.rs"]
mod completion_footer_suppression_tests;

#[cfg(test)]
mod redrive_placeholder_shield_tests {
    use super::redrive_shielded_placeholder;

    #[test]
    fn redrive_placeholder_shield_truth_table_4299() {
        let nudged_at = 1_800_000_000_000;
        assert!(
            redrive_shielded_placeholder(nudged_at, nudged_at + 1, true, nudged_at + 899_999),
            "post-nudge placeholder at a frozen frontier must be preserved inside 900s"
        );
        assert!(
            !redrive_shielded_placeholder(nudged_at, nudged_at - 5_001, true, nudged_at + 1),
            "a pre-nudge orphan must retain the existing reclaim semantics"
        );
        assert!(
            !redrive_shielded_placeholder(nudged_at, nudged_at + 1, true, nudged_at + 900_000),
            "the shield must expire at its hard 900s bound"
        );
        assert!(
            !redrive_shielded_placeholder(nudged_at, nudged_at + 1, false, nudged_at + 1),
            "frontier progress must restore the existing reclaim semantics"
        );
    }
}
