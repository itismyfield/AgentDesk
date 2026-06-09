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
use super::super::turn_finalizer::TurnKey;
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
    tmux_last_offset: Option<u64>,
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
                shared_owned.current_generation,
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
