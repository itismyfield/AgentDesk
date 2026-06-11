//! #3268 (Defect B): self-healing watcher handoff on a TUI quiescence timeout.
//!
//! Extracted from `turn_bridge::spawn_turn_bridge` so the hot `turn_bridge/mod.rs`
//! stays under its frozen production-LoC giant-file baseline (#3028 ratchet); the
//! behavior is byte-for-byte identical to the inline block it replaced.
//!
//! When the bridge would otherwise take its bridge-owned finalize path for a
//! still-busy long-lived turn (the early TUI quiescence gate TIMED OUT because
//! the pane is genuinely still producing output), `submit_terminal(Complete)`
//! would strand the turn: the watcher then sees it as bridge-delivered and
//! suppresses all subsequent output (relay permanently stops). Instead this
//! hands the turn back to a GENUINELY-LIVE watcher, reusing the EXISTING
//! watcher-delegation machinery.

use super::super::inflight::RelayOwnerKind;
use super::super::turn_finalizer::{CompletionSignal, TurnKey, completion_signal_from_transcript};
use super::super::*;
use super::output_lifecycle::BridgeOutputOwner;
use crate::services::provider::ProviderKind;

/// Should the bridge delegate this turn's assistant relay to the watcher (the
/// watcher already owns the relay, it is available for the turn, the bridge has
/// no pending response of its own, and this is not a terminal-error path)?
pub(super) fn should_delegate_bridge_relay_to_watcher(
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

/// A watcher handle is registered for `owner_channel_id` and is not cancelled.
/// (Looser than [`genuinely_live_watcher_for_relay`], which additionally
/// rejects a heartbeat-stale handle; used by the non-handoff observability /
/// availability sites that only need handle-presence.)
pub(super) fn live_watcher_registered_for_relay(
    shared: &SharedData,
    owner_channel_id: ChannelId,
) -> bool {
    shared
        .tmux_watchers
        .get(&owner_channel_id)
        .is_some_and(|watcher| !watcher.cancel.load(std::sync::atomic::Ordering::Relaxed))
}

/// #3268 (Defect B): should the bridge hand a still-busy long-lived turn back to
/// the live watcher instead of finalizing it on the bridge side?
///
/// True ONLY when ALL hold: the TUI quiescence gate TIMED OUT (the pane is
/// genuinely still producing output), the turn was NOT already being delegated
/// to the watcher, this is NOT a terminal-error path (terminal errors still
/// finalize on the bridge), and a LIVE watcher is registered for this turn's
/// relay (so there is an authority to keep relaying + finalize on real idle).
///
/// Pure so the self-healing handoff CONDITION is unit-testable without driving
/// the whole turn loop, mirroring `should_delegate_bridge_relay_to_watcher`.
pub(super) fn bridge_should_hand_off_busy_turn_to_watcher(
    bridge_early_gate_timed_out: bool,
    terminal_error_path: bool,
    bridge_relay_delegated_to_watcher: bool,
    live_watcher_registered: bool,
) -> bool {
    bridge_early_gate_timed_out
        && !terminal_error_path
        && !bridge_relay_delegated_to_watcher
        && live_watcher_registered
}

/// #3268 FIX 1 (codex blocker): true ONLY for a GENUINELY-LIVE watcher = a
/// handle is present AND it is neither cancelled NOR heartbeat-stale.
///
/// The handoff MUST gate on real liveness (`tmux_session_is_stale(name) ==
/// Some(false)`), NOT on handle-presence + `!cancel` alone. A STALE handle
/// (heartbeat dead but not yet cancelled by the sweeper, and deliberately kept
/// by watcher cleanup) has no real authority to finalize: handing off to it
/// re-strands the turn (the bridge suppresses its own finalize while the
/// far-backstop also treats the lingering paused handle as live). When the
/// watcher is stale / cancelled / absent this returns `false` so the bridge
/// finalizes exactly as before — never a handoff to a dead watcher.
pub(super) fn genuinely_live_watcher_for_relay(
    tmux_watchers: &TmuxWatcherRegistry,
    owner_channel_id: ChannelId,
) -> bool {
    tmux_watchers
        .channel_binding(&owner_channel_id)
        .map(|binding| {
            tmux_watchers.tmux_session_is_stale(&binding.tmux_session_name) == Some(false)
        })
        .unwrap_or(false)
}

/// #3277 (Defect A): is this "still-busy" turn actually ALREADY PROVEN
/// delivered? The #3268 handoff gates on watcher LIVENESS, but in #3277 the
/// genuinely-live watcher was parked at transcript EOF over a turn whose JSONL
/// terminator was already on disk — it could never relay another byte nor
/// finalize the turn, so handing off stranded the channel for the full 1800s
/// far-backstop horizon. A handoff target must still be ABLE to finish the
/// turn, not merely be alive.
///
/// True ONLY when the transcript completion signal is `Done` (terminator
/// PROVEN on disk) AND the bridge's confirmed relay offset advanced past the
/// turn's start offset (so the terminator belongs to THIS turn, not a stale
/// one from before the turn's user line was appended). Everything else —
/// `PausedLive`, `Unknown` (non-JSONL runtimes), or missing/unadvanced
/// offsets — returns `false` and keeps the #3268 handoff byte-for-byte
/// (fail-open). In the proven case the bridge finalizing is not premature:
/// it is the CORRECT completion handling.
pub(super) fn busy_turn_already_proven_delivered(
    signal: CompletionSignal,
    tmux_last_offset: Option<u64>,
    turn_start_offset: u64,
) -> bool {
    signal == CompletionSignal::Done
        && tmux_last_offset.is_some_and(|last| last > turn_start_offset)
}

/// #3281: which empty-terminal-response visibility event (if any) applies to
/// this finalize. Pure so the gating is unit-testable. `None` owner keeps the
/// pre-#3281 `bridge_output_owner_none_empty_response` semantics verbatim;
/// `Some(WatcherRelay)` adds the delegated quadrant ("the watcher must carry
/// the whole body from its resume offset") so a watcher parked past the
/// response bytes (#3277-shape loss) is measurable. Terminal-error paths, a
/// missing placeholder message, and a non-empty unsent response never emit.
pub(super) fn empty_terminal_response_visibility_kind(
    bridge_output_owner: Option<BridgeOutputOwner>,
    terminal_error_path: bool,
    current_msg_id: u64,
    response_unsent_empty: bool,
) -> Option<&'static str> {
    if terminal_error_path || current_msg_id == 0 || !response_unsent_empty {
        return None;
    }
    match bridge_output_owner {
        None => Some("bridge_output_owner_none_empty_response"),
        Some(BridgeOutputOwner::WatcherRelay) => Some("bridge_delegated_watcher_empty_response"),
        Some(BridgeOutputOwner::StandbyRelay) => None,
    }
}

// "Handoff occurred" is positional, not a parameter: the only emit site runs
// inside `maybe_hand_off_busy_turn_to_watcher` after the
// `bridge_should_hand_off_busy_turn_to_watcher` gate and past the
// proven-delivered early-return, so a pre-gate delegated turn can never reach
// this kind (no per-turn noise).
pub(super) fn post_gate_handoff_pending_response_visibility_kind(
    response_pending_bytes: usize,
    response_pending_trimmed_empty: bool,
) -> Option<&'static str> {
    (response_pending_bytes > 0 && !response_pending_trimmed_empty)
        .then_some("bridge_post_gate_handoff_pending_response")
}

/// #3281: emit the empty-terminal-response visibility event chosen by
/// [`empty_terminal_response_visibility_kind`]. Moved out of
/// `turn_bridge/mod.rs` (frozen giant baseline); the owner-`None` kind and
/// payload are byte-identical to the inline block this replaced, and the
/// delegated-watcher kind additionally carries `tmux_last_offset` /
/// `turn_start_offset` for offset forensics. Observability only — never
/// posts to Discord or alters relay ownership.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_bridge_empty_terminal_response_visibility(
    shared: &SharedData,
    watcher_owner_channel_id: ChannelId,
    bridge_output_owner: Option<BridgeOutputOwner>,
    terminal_error_path: bool,
    provider: &ProviderKind,
    channel_id: ChannelId,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: &str,
    current_msg_id: u64,
    response_unsent_empty: bool,
    watcher_owns_assistant_relay: bool,
    watcher_relay_available_for_turn: bool,
    standby_relay_owns_output: bool,
    rx_disconnected: bool,
    tmux_handed_off: bool,
    response_sent_offset: usize,
    full_response_len: usize,
    tmux_last_offset: Option<u64>,
    turn_start_offset: Option<u64>,
) {
    let Some(kind) = empty_terminal_response_visibility_kind(
        bridge_output_owner,
        terminal_error_path,
        current_msg_id,
        response_unsent_empty,
    ) else {
        return;
    };
    let mut extra = serde_json::json!({
        "current_msg_id": current_msg_id,
        "watcher_owns_assistant_relay": watcher_owns_assistant_relay,
        "watcher_relay_available_for_turn": watcher_relay_available_for_turn,
        "live_watcher_registered": live_watcher_registered_for_relay(
            shared,
            watcher_owner_channel_id,
        ),
        "standby_relay_owns_output": standby_relay_owns_output,
        "rx_disconnected": rx_disconnected,
        "tmux_handed_off": tmux_handed_off,
        "response_sent_offset": response_sent_offset,
        "full_response_len": full_response_len,
    });
    if kind == "bridge_delegated_watcher_empty_response"
        && let Some(map) = extra.as_object_mut()
    {
        map.insert(
            "tmux_last_offset".to_string(),
            serde_json::json!(tmux_last_offset),
        );
        map.insert(
            "turn_start_offset".to_string(),
            serde_json::json!(turn_start_offset),
        );
    }
    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        channel_id.get(),
        dispatch_id,
        session_key,
        Some(turn_id),
        kind,
        extra,
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_post_gate_handoff_pending_response_visibility(
    provider: &ProviderKind,
    channel_id: ChannelId,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: &str,
    current_msg_id: u64,
    response_unsent: &str,
    tmux_last_offset: Option<u64>,
    turn_start_offset: Option<u64>,
) {
    let response_pending_bytes = response_unsent.len();
    let Some(kind) = post_gate_handoff_pending_response_visibility_kind(
        response_pending_bytes,
        response_unsent.trim().is_empty(),
    ) else {
        return;
    };
    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        channel_id.get(),
        dispatch_id,
        session_key,
        Some(turn_id),
        kind,
        serde_json::json!({
            "current_msg_id": current_msg_id,
            "response_pending_bytes": response_pending_bytes,
            "tmux_last_offset": tmux_last_offset,
            "turn_start_offset": turn_start_offset,
        }),
    );
}

/// #3268 (Defect B): the self-healing watcher handoff itself. When the gate
/// fires, registers the watcher in the single-authority finalizer ledger,
/// unpauses it at the bridge's confirmed offset with `turn_delivered = false`,
/// and PROMOTES the relay-ownership decisions (returned via the `&mut` outputs)
/// so every downstream branch behaves identically to a turn the watcher owned
/// from the start. No-op (leaves the outputs untouched) when the gate does not
/// fire. Terminal-error paths are excluded by `terminal_error_path` and still
/// finalize on the bridge as before.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(super) fn maybe_hand_off_busy_turn_to_watcher(
    shared_owned: &std::sync::Arc<SharedData>,
    bridge_early_gate_timed_out: bool,
    terminal_error_path: bool,
    watcher_owner_channel_id: ChannelId,
    channel_id: ChannelId,
    provider: &ProviderKind,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: &str,
    current_msg_id: u64,
    tmux_last_offset: Option<u64>,
    response_unsent: &str,
    inflight_state: &mut InflightTurnState,
    bridge_relay_delegated_to_watcher: &mut bool,
    bridge_output_owner: &mut Option<BridgeOutputOwner>,
    should_complete_work_dispatch_after_delivery: &mut bool,
) {
    if bridge_should_hand_off_busy_turn_to_watcher(
        bridge_early_gate_timed_out,
        terminal_error_path,
        *bridge_relay_delegated_to_watcher,
        // #3268 FIX 1: gate on GENUINE liveness (not handle-presence + !cancel),
        // so a heartbeat-stale handle can NEVER pass and re-strand the turn.
        genuinely_live_watcher_for_relay(&shared_owned.tmux_watchers, watcher_owner_channel_id),
    ) && let Some(watcher) = shared_owned.tmux_watchers.get(&watcher_owner_channel_id)
    {
        // #3277 (Defect A): a turn whose JSONL terminator is ALREADY on disk
        // (and whose relay offset advanced past the turn start) has nothing
        // left for the watcher to relay — handing it off parks the watcher at
        // EOF and strands the channel until the far-backstop. Keep the bridge
        // finalize instead (the pre-#3268 path; correct, not premature, for a
        // proven-complete turn). Read failures / Unknown / PausedLive /
        // missing offsets all fall through to the #3268 handoff (fail-open);
        // this gate-timeout path already spent 3s, so one transcript tail
        // read is negligible.
        if let (Some(output_path), Some(turn_start_offset)) = (
            inflight_state.output_path.as_deref(),
            inflight_state.turn_start_offset,
        ) && busy_turn_already_proven_delivered(
            completion_signal_from_transcript(
                provider,
                inflight_state.runtime_kind,
                std::path::Path::new(output_path),
            ),
            tmux_last_offset,
            turn_start_offset,
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                watcher_owner_channel = watcher_owner_channel_id.get(),
                turn_start_offset,
                tmux_last_offset = tmux_last_offset.unwrap_or(0),
                "  [{ts}] 👁 #3277: quiescence timeout but the turn is PROVEN delivered \
                 (JSONL terminator on disk past turn start) — bridge finalizes instead of \
                 handing a finished turn to the watcher"
            );
            return;
        }
        emit_post_gate_handoff_pending_response_visibility(
            provider,
            channel_id,
            dispatch_id,
            session_key,
            turn_id,
            current_msg_id,
            response_unsent,
            tmux_last_offset,
            inflight_state.turn_start_offset,
        );
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            watcher_owner_channel = watcher_owner_channel_id.get(),
            "  [{ts}] 👁 #3268: TUI still busy on quiescence timeout — bridge handing long-lived turn back to live watcher instead of finalizing (relay continues)"
        );
        // Persist watcher ownership so a later recovery rehydrates with the
        // correct relay owner (mirrors the watcher-unpause sites).
        inflight_state.set_relay_owner_kind(RelayOwnerKind::Watcher);
        let _ = save_inflight_state(inflight_state);
        // Register the turn as watcher-owned in the single-authority ledger
        // BEFORE unpausing (the modern replacement for the legacy
        // `mailbox_finalize_owed` publish, phase-5b2): (a) makes the delegated
        // finalize branch's `has_live_watcher_pending` TRUE so the bridge does
        // NOT submit a terminal, and (b) arms the watcher far-backstop so a turn
        // whose watcher never submits its own terminal is still GUARANTEED to
        // finalize (after a liveness re-check that never over-finalizes a busy
        // turn). Keyed on the SAME channel_id + current_generation the
        // finalize-branch query uses.
        shared_owned.turn_finalizer.register_start(
            TurnKey::new(
                channel_id,
                inflight_state.user_msg_id,
                shared_owned.restart.current_generation,
            ),
            provider.clone(),
            RelayOwnerKind::Watcher,
            shared_owned,
        );
        // Resume the watcher from the bridge's confirmed offset and clear the
        // delivered flag so it relays the still-producing output (NOT marked
        // delivered → no suppression). The bridge owned relay for this turn, so
        // the watcher is paused; unpause it now.
        if let Some(offset) = tmux_last_offset
            && let Ok(mut guard) = watcher.resume_offset.lock()
        {
            *guard = Some(offset);
        }
        watcher
            .turn_delivered
            .store(false, std::sync::atomic::Ordering::Relaxed);
        watcher
            .paused
            .store(false, std::sync::atomic::Ordering::Release);
        // Promote the relay-ownership decisions so the rest of the turn flows
        // through the existing watcher-delegation branches.
        *bridge_relay_delegated_to_watcher = true;
        *bridge_output_owner = Some(BridgeOutputOwner::WatcherRelay);
        *should_complete_work_dispatch_after_delivery = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3277 (Defect A) truth table: ONLY a `Done` signal with the relay offset
    /// advanced PAST the turn start proves the turn delivered (and suppresses
    /// the #3268 handoff). Every other combination fails open — the handoff to
    /// a genuinely-live watcher proceeds exactly as before #3277.
    #[test]
    fn busy_turn_proven_delivered_truth_table() {
        // Done + offset advanced past turn start → PROVEN: no handoff.
        assert!(busy_turn_already_proven_delivered(
            CompletionSignal::Done,
            Some(37_154),
            17_737,
        ));
        // Done but offset did NOT advance (== start): the terminator could be
        // the PRIOR turn's — not proven, hand off (fail-open).
        assert!(!busy_turn_already_proven_delivered(
            CompletionSignal::Done,
            Some(17_737),
            17_737,
        ));
        // Done but no confirmed offset at all → not proven.
        assert!(!busy_turn_already_proven_delivered(
            CompletionSignal::Done,
            None,
            17_737,
        ));
        // Still live (no terminator) → never proven, regardless of offsets.
        assert!(!busy_turn_already_proven_delivered(
            CompletionSignal::PausedLive,
            Some(37_154),
            17_737,
        ));
        // Non-JSONL runtime (no structured signal) → never proven.
        assert!(!busy_turn_already_proven_delivered(
            CompletionSignal::Unknown,
            Some(37_154),
            17_737,
        ));
        // Done but offset BEHIND turn start (stale/rotated transcript) → not proven.
        assert!(!busy_turn_already_proven_delivered(
            CompletionSignal::Done,
            Some(100),
            17_737,
        ));
        // #3281 cold-start variant: /clear cold-start intake records
        // turn_start_offset = 0 (the transcript did not exist yet), so a Done
        // terminator with ANY advanced relay offset is proven — the #3268
        // handoff stays suppressed, `bridge_output_owner` stays `None`, and
        // the bridge delivers `full_response` directly.
        assert!(busy_turn_already_proven_delivered(
            CompletionSignal::Done,
            Some(37_154),
            0,
        ));
    }

    /// #3281 truth table for the empty-terminal-response visibility gate:
    /// owner `None` keeps the pre-#3281 kind verbatim, delegated-watcher gets
    /// its own kind, and terminal errors / missing placeholder / non-empty
    /// unsent response / standby owner never emit.
    #[test]
    fn empty_terminal_response_visibility_kind_truth_table() {
        // Owner None + empty unsent response → original kind (verbatim).
        assert_eq!(
            empty_terminal_response_visibility_kind(None, false, 42, true),
            Some("bridge_output_owner_none_empty_response"),
        );
        // Delegated to the watcher + empty unsent response → new quadrant kind.
        assert_eq!(
            empty_terminal_response_visibility_kind(
                Some(BridgeOutputOwner::WatcherRelay),
                false,
                42,
                true,
            ),
            Some("bridge_delegated_watcher_empty_response"),
        );
        // Non-empty unsent response → the bridge still owns deliverable bytes.
        assert_eq!(
            empty_terminal_response_visibility_kind(None, false, 42, false),
            None,
        );
        // Delegated watcher + non-empty unsent response is not an empty-response signal.
        assert_eq!(
            empty_terminal_response_visibility_kind(
                Some(BridgeOutputOwner::WatcherRelay),
                false,
                42,
                false,
            ),
            None,
        );
        // Terminal-error path → excluded (matches the pre-#3281 gate).
        assert_eq!(
            empty_terminal_response_visibility_kind(None, true, 42, true),
            None,
        );
        // No placeholder message id → excluded (matches the pre-#3281 gate).
        assert_eq!(
            empty_terminal_response_visibility_kind(None, false, 0, true),
            None,
        );
        // Standby relay owns output → not part of this visibility surface.
        assert_eq!(
            empty_terminal_response_visibility_kind(
                Some(BridgeOutputOwner::StandbyRelay),
                false,
                42,
                true,
            ),
            None,
        );
    }

    #[test]
    fn post_gate_handoff_pending_response_visibility_kind_truth_table() {
        assert_eq!(
            post_gate_handoff_pending_response_visibility_kind(12, false),
            Some("bridge_post_gate_handoff_pending_response"),
        );
        assert_eq!(
            post_gate_handoff_pending_response_visibility_kind(0, false),
            None
        );
        assert_eq!(
            post_gate_handoff_pending_response_visibility_kind(12, true),
            None
        );
    }
}
