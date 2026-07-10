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

/// A synthetic-headless bridge turn has no real Discord placeholder for the
/// watcher to edit. If the bridge already holds terminal text, keep ownership on
/// the bridge so it can run the headless outbox delivery path instead of
/// promoting the turn to watcher-only ownership with zero relay progress.
pub(super) fn headless_terminal_delivery_should_stay_on_bridge(
    can_chain_locally: bool,
    current_msg_id: u64,
    response_pending_bytes: usize,
    response_pending_trimmed_empty: bool,
) -> bool {
    !can_chain_locally
        && super::super::is_synthetic_headless_message_id_raw(current_msg_id)
        && response_pending_bytes > 0
        && !response_pending_trimmed_empty
}

/// #3268 FIX 1 (codex blocker): true ONLY for a GENUINELY-LIVE watcher = a
/// handle is present AND it is neither cancelled NOR heartbeat-stale.
///
/// The handoff MUST gate on relay liveness (`tmux_session_live_for_relay(name)
/// == Some(true)`), NOT on handle-presence alone. A STALE handle (heartbeat dead
/// but not yet cancelled by the sweeper, and deliberately kept by watcher
/// cleanup) or a cancelled handle has no real authority to finalize: handing off
/// to it re-strands the turn (the bridge suppresses its own finalize while the
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
            tmux_watchers.tmux_session_live_for_relay(&binding.tmux_session_name) == Some(true)
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
/// PROVEN on disk) AND the transcript itself GREW past the turn's start offset
/// (so the terminator + the response body belong to THIS turn, not a stale one
/// from before the turn's user line was appended). Everything else —
/// `PausedLive`, `Unknown` (non-JSONL runtimes), or a transcript that did not
/// grow past the start offset — returns `false` and keeps the #3268 handoff
/// byte-for-byte (fail-open). In the proven case the bridge finalizing is not
/// premature: it is the CORRECT completion handling.
///
/// #3540 (warm-followup strand): the #3277 gate originally compared the racy
/// `tmux_last_offset` against `turn_start_offset`. Both are seeded to the SAME
/// `inflight_offset` at turn start (`InflightTurnState::new` ←
/// `intake_turn.rs`'s `Some(inflight_offset)`), and a warm-followup turn only
/// advances `tmux_last_offset` via the trailing `RuntimeReady{last_offset}`.
/// When that ready signal misses the 250 ms drain window the offset stays at
/// `inflight_offset`, so `last > start` is `false` even though the producer
/// already wrote the WHOLE response (terminator + body) to the transcript →
/// the gate fails open → the #3268 handoff strands the finished turn behind a
/// "in progress" placeholder. The non-racy fact is on disk: the transcript
/// EOF. `inflight_offset` IS that transcript file's byte length stat-ed at
/// turn start (`std::fs::metadata(output_path).len()`), and `transcript_eof`
/// here re-stats the SAME `output_path`, so both live in one byte-space.
/// Comparing `transcript_eof > turn_start_offset` therefore asks the exact,
/// non-racy question "did the producer append (and terminate) this turn's
/// response past where the turn began?". A rotated/truncated transcript shrinks
/// its EOF below `turn_start_offset`, so the comparison stays `false` and the
/// handoff fails open (conservative: only an EOF that DEFINITELY grew proves
/// delivery).
pub(super) fn busy_turn_already_proven_delivered(
    signal: CompletionSignal,
    transcript_eof: Option<u64>,
    turn_start_offset: u64,
) -> bool {
    signal == CompletionSignal::Done && transcript_eof.is_some_and(|eof| eof > turn_start_offset)
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
    can_chain_locally: bool,
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
        if headless_terminal_delivery_should_stay_on_bridge(
            can_chain_locally,
            current_msg_id,
            response_unsent.len(),
            response_unsent.trim().is_empty(),
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                watcher_owner_channel = watcher_owner_channel_id.get(),
                current_msg_id,
                response_pending_bytes = response_unsent.len(),
                "  [{ts}] 👁 #3704: quiescence timeout with headless terminal text pending — bridge keeps delivery ownership instead of handing off to watcher"
            );
            return;
        }
        // #3277 (Defect A): a turn whose JSONL terminator is ALREADY on disk
        // (and whose transcript grew past the turn start) has nothing left for
        // the watcher to relay — handing it off parks the watcher at EOF and
        // strands the channel until the far-backstop. Keep the bridge finalize
        // instead (the pre-#3268 path; correct, not premature, for a
        // proven-complete turn). Read failures / Unknown / PausedLive /
        // missing offsets all fall through to the #3268 handoff (fail-open);
        // this gate-timeout path already spent 3s, so one transcript tail
        // read is negligible.
        //
        // #3540 (warm-followup strand): the growth check now uses the
        // transcript's on-disk EOF, NOT the racy `tmux_last_offset`. A
        // warm-followup turn whose trailing `RuntimeReady{last_offset}` missed
        // the 250 ms drain window leaves `tmux_last_offset == turn_start_offset`
        // even though the producer already wrote the whole response — the old
        // `tmux_last_offset > start` comparison was `false` there and fell open
        // to the handoff, stranding the finished turn. `turn_start_offset` is
        // `output_path`'s byte length stat-ed at turn start, so re-stat the SAME
        // `output_path` for a byte-space-matched EOF. A read failure → `None` →
        // fall through to the #3268 handoff (fail-open, unchanged). A
        // rotated/truncated transcript shrinks the EOF below the start offset,
        // so `eof > start` stays `false` and the handoff still fails open.
        let transcript_eof = inflight_state
            .output_path
            .as_deref()
            .and_then(|path| std::fs::metadata(path).map(|m| m.len()).ok());
        if let (Some(output_path), Some(turn_start_offset)) = (
            inflight_state.output_path.as_deref(),
            inflight_state.turn_start_offset,
        ) && busy_turn_already_proven_delivered(
            completion_signal_from_transcript(
                provider,
                inflight_state.runtime_kind,
                std::path::Path::new(output_path),
            ),
            transcript_eof,
            turn_start_offset,
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                watcher_owner_channel = watcher_owner_channel_id.get(),
                turn_start_offset,
                tmux_last_offset = tmux_last_offset.unwrap_or(0),
                transcript_eof = transcript_eof.unwrap_or(0),
                "  [{ts}] 👁 #3277/#3540: quiescence timeout but the turn is PROVEN delivered \
                 (JSONL terminator on disk + transcript grew past turn start) — bridge \
                 finalizes instead of handing a finished turn to the watcher (warm-followup: \
                 disk EOF, not racy tmux_last_offset)"
            );
            // #3501: the body is PROVEN delivered (JSONL terminator on disk past
            // turn start, already relayed), so STAMP the inflight delivered before
            // returning. Otherwise it persists with response_sent_offset=0 +
            // terminal_delivery_committed=false, and a later dcserver restart
            // restores it as "undelivered" → the watcher RE-RELAYS the finished
            // body at the next soft boundary (the #3501 re-relay). Mirrors the
            // committed-delivery reconciliation in turn_bridge/mod.rs. Only this
            // proven-COMPLETE branch is stamped; the still-streaming #3268 handoff
            // below leaves the offset un-advanced, so a delegated turn's tail is
            // never suppressed.
            let delivered_len = inflight_state.full_response.len();
            inflight_state.response_sent_offset = delivered_len;
            inflight_state.terminal_delivery_committed = true;
            let _ = crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
                inflight_state,
                "turn_bridge::watcher_handoff::proven_delivered",
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
            channel_id = channel_id.get(),
            watcher_owner_channel = watcher_owner_channel_id.get(),
            "  [{ts}] 👁 #3268: TUI still busy on quiescence timeout — bridge handing long-lived turn back to live watcher instead of finalizing (relay continues)"
        );
        // Persist watcher ownership so a later recovery rehydrates with the
        // correct relay owner (mirrors the watcher-unpause sites).
        inflight_state.set_relay_owner_kind(RelayOwnerKind::Watcher);
        let _ = crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
            inflight_state,
            "turn_bridge::watcher_handoff::watcher_ownership",
        );
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
                inflight_state.effective_finalizer_turn_id(),
                shared_owned.restart.current_generation,
            ),
            provider.clone(),
            RelayOwnerKind::Watcher,
            shared_owned,
        );
        // Resume the watcher from the bridge's CONFIRMED relay frontier (not the
        // produced offset) and clear the delivered flag so it relays the still-
        // unrelayed tail `[confirmed_end, last_offset]` (NOT marked delivered → no
        // suppression). #3459: seeding the produced `tmux_last_offset` here made the
        // watcher seek PAST that tail, so it was never relayed and was permanently
        // lost when the next turn superseded the inflight. Gate on the CURRENT
        // wrapper generation (#3358 TOCTOU-safe) so a post-restart/rotated frontier
        // never re-seeds the watcher into a different wrapper's content; fall back
        // to the produced offset when there is no current-generation frontier or no
        // unrelayed tail (byte-identical to the pre-#3459 seed in those cases). The
        // bridge owned relay for this turn, so the watcher is paused; unpause now.
        if let Some(produced) = tmux_last_offset
            && let Ok(mut guard) = watcher.resume_offset.lock()
        {
            let confirmed_frontier = inflight_state
                .tmux_session_name
                .as_deref()
                .and_then(|name| {
                    crate::services::discord::tmux::committed_frontier_for_current_generation(
                        shared_owned,
                        channel_id,
                        name,
                    )
                });
            *guard = Some(watcher_resume_seed_offset(produced, confirmed_frontier));
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

/// #3459 (pure, testable): the watcher's resume-offset seed at a quiescence-gate
/// handoff. Resume from the CONFIRMED relay frontier when it lags the PRODUCED
/// offset (an unrelayed tail `[confirmed, produced]` exists → the watcher reads
/// it forward and relays it once). Fall back to `produced` when there is no
/// current-generation frontier (`None`, #3358 gate failed → content-skip-safe) or
/// when the frontier is not behind (`confirmed >= produced`, no tail) — both
/// byte-identical to the pre-#3459 seed. Never returns a value above `produced`,
/// so the watcher can never seek past produced EOF. `#[cfg(unix)]`-gated to match
/// its sole caller `maybe_hand_off_busy_turn_to_watcher` (no dead code off-unix).
#[cfg(unix)]
fn watcher_resume_seed_offset(produced: u64, confirmed_frontier: Option<u64>) -> u64 {
    confirmed_frontier
        .filter(|&confirmed| confirmed < produced)
        .unwrap_or(produced)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3459 truth table: the handoff resume seed flushes the unrelayed tail by
    /// resuming from the confirmed frontier ONLY when it lags the produced offset,
    /// and never seeks past produced EOF. `#[cfg(unix)]` to match the gated helper.
    #[cfg(unix)]
    #[test]
    fn watcher_resume_seed_offset_flushes_tail_else_falls_back() {
        // Unrelayed tail exists (confirmed < produced) → resume from confirmed so
        // the watcher reads [confirmed, produced] forward and relays it (#3459 fix).
        assert_eq!(watcher_resume_seed_offset(376_405, Some(331_575)), 331_575);
        // No current-generation frontier (#3358 gate failed) → fall back to produced
        // (content-skip-safe; byte-identical to pre-#3459).
        assert_eq!(watcher_resume_seed_offset(376_405, None), 376_405);
        // Frontier not behind (no tail) → fall back to produced (no re-relay).
        assert_eq!(watcher_resume_seed_offset(376_405, Some(376_405)), 376_405);
        // Stale-high frontier can never seek the watcher PAST produced EOF.
        assert_eq!(watcher_resume_seed_offset(376_405, Some(999_999)), 376_405);
    }

    /// #3277 (Defect A) + #3540 (warm-followup strand) truth table: ONLY a
    /// `Done` signal whose TRANSCRIPT EOF grew PAST the turn start proves the
    /// turn delivered (and suppresses the #3268 handoff). Every other
    /// combination fails open — the handoff to a genuinely-live watcher proceeds
    /// exactly as before #3277. #3540: the growth fact is read from the
    /// transcript's on-disk EOF (`std::fs::metadata(output_path).len()`), NOT
    /// the racy `tmux_last_offset`, so a warm-followup whose trailing
    /// `RuntimeReady{last_offset}` was late still finalizes when the producer
    /// already wrote the response.
    #[test]
    fn busy_turn_proven_delivered_truth_table() {
        // Done + transcript EOF grew past turn start → PROVEN: no handoff.
        assert!(busy_turn_already_proven_delivered(
            CompletionSignal::Done,
            Some(37_154),
            17_737,
        ));
        // #3540 warm-followup: the producer wrote the WHOLE response — the
        // transcript EOF (929_166) grew well past `turn_start_offset` (886_134,
        // the `inflight_offset` both `turn_start_offset` AND the racy
        // `tmux_last_offset` were seeded to). Under the pre-#3540 gate the racy
        // `tmux_last_offset` was still parked at 886_134 (its trailing
        // `RuntimeReady` missed the 250 ms drain window) → `886_134 > 886_134`
        // was `false` → fail-open → the #3268 handoff stranded the finished turn
        // behind an "in progress" placeholder. Reading the non-racy disk EOF
        // proves delivery → finalize, no handoff, no strand.
        assert!(busy_turn_already_proven_delivered(
            CompletionSignal::Done,
            Some(929_166),
            886_134,
        ));
        // Done but the transcript did NOT grow (EOF == start): the producer
        // appended nothing past the turn start, so the terminator on disk is the
        // PRIOR turn's — a genuinely unfinished turn. Not proven, hand off
        // (fail-open) so the live watcher carries the still-streaming body.
        assert!(!busy_turn_already_proven_delivered(
            CompletionSignal::Done,
            Some(886_134),
            886_134,
        ));
        // Done but the transcript could not be stat-ed (read failure → None) →
        // not proven, fall through to the #3268 handoff (fail-open, unchanged).
        assert!(!busy_turn_already_proven_delivered(
            CompletionSignal::Done,
            None,
            17_737,
        ));
        // Still live (no terminator) → never proven, regardless of the EOF.
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
        // Done but transcript EOF BEHIND turn start (rotated/truncated transcript
        // shrank it below where the turn began) → not proven (conservative
        // fail-open: only a transcript that DEFINITELY grew proves delivery, so a
        // rotation that mixed byte-spaces can never spuriously finalize).
        assert!(!busy_turn_already_proven_delivered(
            CompletionSignal::Done,
            Some(100),
            17_737,
        ));
        // #3281 cold-start variant: /clear cold-start intake records
        // turn_start_offset = 0 (the transcript did not exist yet), so a Done
        // terminator with ANY non-zero transcript EOF is proven — the #3268
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

    #[test]
    fn headless_terminal_delivery_stays_on_bridge_truth_table() {
        let synthetic_id = super::super::SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR + 42;
        assert!(headless_terminal_delivery_should_stay_on_bridge(
            false,
            synthetic_id,
            128,
            false,
        ));
        assert!(
            !headless_terminal_delivery_should_stay_on_bridge(false, synthetic_id, 0, false),
            "no pending response means no bridge-owned terminal delivery to protect"
        );
        assert!(
            !headless_terminal_delivery_should_stay_on_bridge(false, synthetic_id, 128, true),
            "whitespace-only pending response should not block watcher handoff"
        );
        assert!(
            !headless_terminal_delivery_should_stay_on_bridge(true, synthetic_id, 128, false),
            "live Discord turns can still use the existing watcher handoff path"
        );
        assert!(
            !headless_terminal_delivery_should_stay_on_bridge(false, 42, 128, false),
            "real Discord placeholders are not the headless outbox path"
        );
    }
}
#[cfg(test)]
mod bridge_busy_turn_handoff_tests {
    use super::super::output_lifecycle::{BridgeOutputOwner, classify_bridge_output_owner};
    use super::super::terminal_delivery::bridge_epilogue_marks_watcher_delivered;
    use super::*;

    // Build a watcher handle with controllable liveness for the #3268 FIX 1
    // gate tests: `cancel` and the heartbeat age determine staleness.
    fn watcher_handle_with_liveness(
        tmux_session_name: &str,
        cancel: bool,
        heartbeat_ts_ms: i64,
    ) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: format!("/tmp/{tmux_session_name}.jsonl"),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(cancel)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(heartbeat_ts_ms)),
        }
    }

    // #3268 (Defect B) — the core regression. A NON-terminal turn whose early
    // TUI quiescence gate TIMED OUT (the pane is genuinely still busy on a
    // long-lived session) and that was NOT already delegated to the watcher,
    // with a LIVE watcher registered, MUST hand off to the watcher instead of
    // finalizing on the bridge. This is the exact condition that, when false in
    // production, let the bridge `submit_terminal(Complete)` strand the turn and
    // permanently stop relay.
    #[test]
    fn busy_timeout_with_live_watcher_hands_off() {
        assert!(
            bridge_should_hand_off_busy_turn_to_watcher(
                /* bridge_early_gate_timed_out */ true, /* terminal_error_path */ false,
                /* bridge_relay_delegated_to_watcher */ false,
                /* live_watcher_registered */ true,
            ),
            "gate timeout + non-terminal + not-yet-delegated + live watcher must hand off"
        );
    }

    // The handoff's relay-ownership promotion must route the rest of the turn
    // through the WatcherRelay branches — the bridge skips its own delivery and
    // (via `bridge_epilogue_marks_watcher_delivered`) does NOT mark the watcher
    // delivered, so the still-streaming output is NOT suppressed.
    #[test]
    fn handoff_promotes_ownership_to_watcher_relay_without_marking_delivered() {
        let handoff = bridge_should_hand_off_busy_turn_to_watcher(true, false, false, true);
        assert!(handoff);
        // After the promotion `bridge_relay_delegated_to_watcher == true`.
        let promoted_delegated = handoff;
        assert_eq!(
            classify_bridge_output_owner(/* standby */ false, promoted_delegated),
            Some(BridgeOutputOwner::WatcherRelay),
            "promoted ownership must classify as WatcherRelay so the bridge skips delivery"
        );
        assert!(
            !bridge_epilogue_marks_watcher_delivered(
                /* preserve_inflight_for_cleanup_retry */ false,
                promoted_delegated,
            ),
            "a handed-off (delegated) turn must NOT mark the watcher delivered — \
             marking it delivered is exactly what suppresses the still-streaming output"
        );
    }

    // A turn already delegated to the watcher needs no handoff (it is already
    // watcher-owned) — avoid a redundant second register/unpause.
    #[test]
    fn already_delegated_turn_does_not_re_hand_off() {
        assert!(
            !bridge_should_hand_off_busy_turn_to_watcher(true, false, true, true),
            "already-delegated turns are watcher-owned; no second handoff"
        );
    }

    // Terminal-error paths (cancelled / prompt_too_long / transport_error /
    // recovery_retry collapse into `terminal_error_path`) MUST still finalize on
    // the bridge as before — never hand off.
    #[test]
    fn terminal_error_paths_still_finalize_on_bridge() {
        assert!(
            !bridge_should_hand_off_busy_turn_to_watcher(true, true, false, true),
            "terminal-error turns finalize on the bridge, never hand off"
        );
    }

    // No gate timeout → the pane reported idle (or the gate did not apply); the
    // normal finalize path stands and there is nothing to hand off.
    #[test]
    fn quiesced_turn_does_not_hand_off() {
        assert!(
            !bridge_should_hand_off_busy_turn_to_watcher(false, false, false, true),
            "a quiesced (non-timed-out) turn finalizes normally"
        );
    }

    // No live watcher → there would be no authority to keep relaying or to
    // finalize on idle, so the bridge must NOT hand off (it owns the finalize).
    #[test]
    fn missing_live_watcher_does_not_hand_off() {
        assert!(
            !bridge_should_hand_off_busy_turn_to_watcher(true, false, false, false),
            "without a live watcher the bridge keeps finalize ownership"
        );
    }

    // #3268 FIX 1 (codex blocker): the handoff liveness gate must reject a STALE
    // watcher (heartbeat dead, not yet cancelled). Handing off to a stale handle
    // re-strands the turn — the bridge suppresses its own finalize while the
    // lingering handle has no real authority to finalize. A genuinely-live
    // watcher (recent heartbeat, not cancelled) is the ONLY one that may pass.
    #[test]
    fn handoff_liveness_gate_rejects_stale_watcher() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-claude-adk-cc-t1500000000000003268";
        let channel = ChannelId::new(1_500_000_000_000_003_268);
        // heartbeat_ts_ms = 1 → ancient → heartbeat_stale() == true, cancel=false.
        registry.insert(channel, watcher_handle_with_liveness(tmux, false, 1));
        assert!(
            !genuinely_live_watcher_for_relay(&registry, channel),
            "a heartbeat-stale watcher must NOT count as live for the handoff gate"
        );
        // The bridge therefore keeps finalize ownership (no handoff / no strand).
        assert!(
            !bridge_should_hand_off_busy_turn_to_watcher(
                true,
                false,
                false,
                genuinely_live_watcher_for_relay(&registry, channel),
            ),
            "a stale watcher on the timeout path must finalize on the bridge, not hand off"
        );
    }

    // A cancelled handle (sweeper set cancel=true, cleanup deliberately keeps the
    // handle) must also be rejected by the liveness gate.
    #[test]
    fn handoff_liveness_gate_rejects_cancelled_watcher() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-claude-adk-cc-t1500000000000003269";
        let channel = ChannelId::new(1_500_000_000_000_003_269);
        registry.insert(
            channel,
            watcher_handle_with_liveness(
                tmux,
                true,
                crate::services::discord::tmux_watcher_now_ms(),
            ),
        );
        assert!(
            !genuinely_live_watcher_for_relay(&registry, channel),
            "a cancelled watcher must NOT count as live for the handoff gate"
        );
    }

    // An absent handle (no live watcher at all) is rejected.
    #[test]
    fn handoff_liveness_gate_rejects_absent_watcher() {
        let registry = TmuxWatcherRegistry::new();
        let channel = ChannelId::new(1_500_000_000_000_003_270);
        assert!(
            !genuinely_live_watcher_for_relay(&registry, channel),
            "an absent watcher must NOT count as live for the handoff gate"
        );
    }

    // The positive case: a genuinely-live watcher (recent heartbeat, not
    // cancelled) DOES pass the liveness gate, so the timeout path hands off.
    #[test]
    fn handoff_liveness_gate_accepts_genuinely_live_watcher() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-claude-adk-cc-t1500000000000003271";
        let channel = ChannelId::new(1_500_000_000_000_003_271);
        registry.insert(
            channel,
            watcher_handle_with_liveness(
                tmux,
                false,
                crate::services::discord::tmux_watcher_now_ms(),
            ),
        );
        assert!(
            genuinely_live_watcher_for_relay(&registry, channel),
            "a present, non-cancelled, fresh-heartbeat watcher is genuinely live"
        );
        assert!(
            bridge_should_hand_off_busy_turn_to_watcher(
                true,
                false,
                false,
                genuinely_live_watcher_for_relay(&registry, channel),
            ),
            "a genuinely-live watcher on the timeout path must hand off as before"
        );
    }
}
