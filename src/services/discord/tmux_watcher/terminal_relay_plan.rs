use super::*;
use std::sync::Arc;

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::cluster::stream_relay::RelayProducer;
use crate::services::discord::TmuxRelayCoord;

pub(super) struct TerminalRelayPlanContext<'a> {
    pub(super) http: &'a Arc<serenity::Http>,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) channel_id: serenity::ChannelId,
    pub(super) watcher_provider: &'a ProviderKind,
    pub(super) tmux_session_name: &'a String,
    pub(super) output_path: &'a String,
    pub(super) inflight_before_relay: &'a Option<InflightTurnState>,
    pub(super) cached_relay_producer: &'a Option<RelayProducer>,
    pub(super) prompt_anchor_present_before_relay: bool,
    pub(super) external_input_lease_before_relay: bool,
    pub(super) session_bound_relay_turn_fully_mirrored: bool,
    pub(super) session_bound_relay_turn_first_forwarded_sequence: Option<u64>,
    pub(super) split_trailing_turn_follows: bool,
    /// #5175: the watcher's turn-identity binding captured at turn-stream exit.
    /// Soft-terminal authority is re-decided HERE against `inflight_before_relay`
    /// instead of the pre-turn snapshot verdict this field used to carry.
    pub(super) startup_soft_terminal_authority: WatcherSoftTerminalAuthority,
}

pub(super) struct TerminalRelayPlanLocals<'a> {
    pub(super) current_offset: u64,
    pub(super) data_start_offset: u64,
    pub(super) all_data: &'a String,
    pub(super) full_response: &'a String,
    pub(super) current_response: &'a str,
    pub(super) response_sent_offset: usize,
    pub(super) has_assistant_response: bool,
    pub(super) terminal_kind: Option<WatcherTerminalKind>,
    pub(super) task_notification_kind: Option<TaskNotificationKind>,
    pub(super) assistant_text_seen: bool,
    pub(super) fresh_assistant_text_seen: bool,
    pub(super) tool_state: &'a WatcherToolState,
    pub(super) placeholder_msg_id: Option<serenity::MessageId>,
    pub(super) status_panel_msg_id: Option<serenity::MessageId>,
}

pub(super) struct TerminalRelayPlanState<'a> {
    pub(super) all_data_session_bound_relay_ack: &'a mut Option<SessionBoundRelayAckTarget>,
    pub(super) monitor_auto_turn_claimed: &'a mut bool,
    pub(super) monitor_auto_turn_finished: &'a mut bool,
    pub(super) monitor_auto_turn_synthetic_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) monitor_auto_turn_ledger_generation: &'a mut Option<u64>,
}

pub(super) enum TerminalRelayPlanOutcome<'a> {
    ContinueWatcherLoop,
    Proceed(TerminalRelayPlan<'a>),
}

pub(super) struct TerminalRelayPlan<'a> {
    pub(super) relay_coord: Arc<TmuxRelayCoord>,
    pub(super) slot_guard: RelaySlotGuard,
    pub(super) relay_decision: TerminalRelayDecision,
    pub(super) session_bound_relay_owns_terminal_delivery: bool,
    pub(super) ssh_direct_pending: bool,
    pub(super) watcher_direct_fallback_requested: bool,
    pub(super) watcher_direct_fallback_authorized: bool,
    pub(super) watcher_direct_fallback_after_session_bound_ack: bool,
    pub(super) session_bound_fallback_uses_full_body: bool,
    pub(super) direct_terminal_response: &'a str,
    pub(super) has_direct_terminal_response: bool,
    pub(super) direct_terminal_response_refused_duplicate: bool,
    pub(super) watcher_resend_action: Option<WatcherTerminalResendAction>,
    pub(super) watcher_direct_terminal_idle_committed: bool,
    pub(super) tui_direct_anchor_terminal_body_visible: bool,
    pub(super) tui_direct_anchor_or_lease_present_for_lifecycle: bool,
}

/// #5175: decide whether this watcher may direct-send the terminal body.
///
/// Split out of `run_terminal_relay_plan` so the decision has a seam that can
/// be pinned by tests: the whole defect was that the inputs were wrong, not
/// that the predicate was. `binding` is the watcher's turn-identity binding
/// captured at turn-stream exit; `inflight_before_relay` is the row loaded a
/// few lines before the relay — the row that actually exists when the turn
/// ENDS. Authority must be read from the latter. `binding`'s own pre-turn
/// snapshot verdict is deliberately not consulted here.
fn watcher_soft_terminal_direct_send_authority(
    binding: &WatcherSoftTerminalAuthority,
    inflight_before_relay: Option<&InflightTurnState>,
    current_offset: u64,
    terminal_kind: Option<WatcherTerminalKind>,
) -> (bool, Option<SoftTerminalAuthorityDenial>) {
    let denial = binding
        .authorize_pre_relay_inflight(inflight_before_relay, current_offset)
        .err();
    let authorized = watcher_direct_fallback_has_turn_authority(terminal_kind, denial.is_none());
    // A hard provider result keeps its recovery fallback regardless of the soft
    // contract, so a denial recorded there is not the reason anybody skipped
    // delivery — report it only when it actually gated the send.
    (authorized, denial.filter(|_| !authorized))
}

fn write_terminal_relay_plan_state(
    state: &mut TerminalRelayPlanState<'_>,
    all_data_session_bound_relay_ack: Option<SessionBoundRelayAckTarget>,
    monitor_auto_turn_claimed: bool,
    monitor_auto_turn_finished: bool,
    monitor_auto_turn_synthetic_msg_id: Option<serenity::MessageId>,
    monitor_auto_turn_ledger_generation: Option<u64>,
) {
    *state.all_data_session_bound_relay_ack = all_data_session_bound_relay_ack;
    *state.monitor_auto_turn_claimed = monitor_auto_turn_claimed;
    *state.monitor_auto_turn_finished = monitor_auto_turn_finished;
    *state.monitor_auto_turn_synthetic_msg_id = monitor_auto_turn_synthetic_msg_id;
    *state.monitor_auto_turn_ledger_generation = monitor_auto_turn_ledger_generation;
}

pub(super) async fn run_terminal_relay_plan<'a>(
    context: &TerminalRelayPlanContext<'a>,
    locals: TerminalRelayPlanLocals<'a>,
    state: &mut TerminalRelayPlanState<'a>,
) -> TerminalRelayPlanOutcome<'a> {
    let http = context.http;
    let shared = context.shared;
    let channel_id = context.channel_id;
    let watcher_provider = context.watcher_provider;
    let tmux_session_name = context.tmux_session_name;
    let output_path = context.output_path;
    let inflight_before_relay = context.inflight_before_relay;
    let cached_relay_producer = context.cached_relay_producer;
    let prompt_anchor_present_before_relay = context.prompt_anchor_present_before_relay;
    let external_input_lease_before_relay = context.external_input_lease_before_relay;
    let mut session_bound_relay_turn_fully_mirrored =
        context.session_bound_relay_turn_fully_mirrored;
    let session_bound_relay_turn_first_forwarded_sequence =
        context.session_bound_relay_turn_first_forwarded_sequence;
    let split_trailing_turn_follows = context.split_trailing_turn_follows;
    let startup_soft_terminal_authority = &context.startup_soft_terminal_authority;
    let TerminalRelayPlanLocals {
        current_offset,
        data_start_offset,
        all_data,
        full_response,
        current_response,
        response_sent_offset,
        has_assistant_response,
        terminal_kind,
        task_notification_kind,
        assistant_text_seen,
        fresh_assistant_text_seen,
        tool_state,
        placeholder_msg_id,
        status_panel_msg_id,
    } = locals;
    let mut all_data_session_bound_relay_ack =
        std::mem::take(state.all_data_session_bound_relay_ack);
    let mut monitor_auto_turn_claimed = *state.monitor_auto_turn_claimed;
    let mut monitor_auto_turn_finished = *state.monitor_auto_turn_finished;
    let mut monitor_auto_turn_synthetic_msg_id = *state.monitor_auto_turn_synthetic_msg_id;
    let mut monitor_auto_turn_ledger_generation = *state.monitor_auto_turn_ledger_generation;

    // Relay coordination is limited to serialization plus telemetry. The
    // local `last_relayed_offset` guard handles self-duplicate relays, and
    // watcher registration enforces one live owner per tmux session. Do
    // not suppress a valid owner solely because another watcher advanced
    // the shared confirmed_end watermark.
    let relay_coord = shared.tmux_relay_coord(channel_id);
    if let Ok(meta) = std::fs::metadata(&output_path) {
        reset_stale_relay_watermark_if_output_regressed(
            &shared,
            channel_id,
            &tmux_session_name,
            meta.len(),
            "pre_relay",
        );
    }
    // CAS the emission slot. `0` = free; any non-zero value = a watcher
    // is mid-emission with that start offset. `.max(1)` guarantees the
    // stored value is non-zero even when `data_start_offset == 0`.
    let slot_claim_token = data_start_offset.max(1);
    let slot_busy = relay_coord
        .relay_slot
        .compare_exchange(
            0,
            slot_claim_token,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err();
    if slot_busy {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] 👁 Cross-watcher serialization: slot busy, skipped relay for {} (data_start={})",
            tmux_session_name,
            data_start_offset
        );
        if let Some(msg_id) = placeholder_msg_id {
            let _ = delete_nonterminal_placeholder(
                &http,
                channel_id,
                &shared,
                &watcher_provider,
                &tmux_session_name,
                msg_id,
                "watcher_cross_watcher_slot_busy_cleanup",
            )
            .await;
        }
        finish_monitor_auto_turn_if_claimed(
            &shared,
            &watcher_provider,
            channel_id,
            &mut monitor_auto_turn_claimed,
            &mut monitor_auto_turn_finished,
            &mut monitor_auto_turn_synthetic_msg_id,
            &mut monitor_auto_turn_ledger_generation,
        )
        .await;
        write_terminal_relay_plan_state(
            state,
            all_data_session_bound_relay_ack,
            monitor_auto_turn_claimed,
            monitor_auto_turn_finished,
            monitor_auto_turn_synthetic_msg_id,
            monitor_auto_turn_ledger_generation,
        );
        TerminalRelayPlanOutcome::ContinueWatcherLoop
    } else {
        // #2840: the CAS above acquired the emission slot. Hold it via an RAII
        // guard so ANY exit from here on (early `continue`, `?`, panic, task
        // abort) frees the slot on Drop instead of wedging the channel for
        // every replacement watcher. The two intended release points below call
        // `slot_guard.release()` explicitly to preserve their timing.
        let slot_guard = RelaySlotGuard::new(relay_coord.relay_slot.clone());

        // Send the terminal response to Discord, or delegate it when matched
        // session-bound inflight metadata assigns delivery to the StreamRelay sink.
        let relay_decision = terminal_relay_decision(
            has_assistant_response,
            task_notification_kind,
            assistant_text_seen,
        );
        debug_assert!(
            !relay_decision.should_enqueue_notify_outbox,
            "monitor/task-notification watcher relays must not use notify-bot outbox"
        );
        let session_bound_discord_delivery_enabled =
            crate::services::discord::session_relay_sink::session_bound_discord_delivery_enabled();
        let relay_producer_session_name = cached_relay_producer
            .as_ref()
            .map(|producer| producer.session_name());
        // #3579: INIT the ack outcome to the watcher-owned NON-attempt sentinel.
        // When `session_bound_relay_should_own_terminal_delivery` returns false
        // (e.g. relay_owner=Watcher) the ack-wait block below is SKIPPED and this
        // init value is what the flight recorder logs as `frame_ack_outcome`. It
        // is BENIGN (the watcher owns terminal delivery; the sink-delegated ack
        // path is intentionally not taken) — distinct from `MissingTarget`, which
        // `wait_for_session_bound_relay_delivery_ack` returns only when the block
        // ACTUALLY RAN but had no target (a real unconfirmed). Before #3579 this
        // init was `MissingTarget`, conflating the two and inflating relay-loss
        // tallies. Behavior is unchanged: `NotAttempted` folds to the same
        // `DeliveryOutcome::Unknown` as `MissingTarget` for the resend decision.
        let mut session_bound_ack_outcome = SessionBoundRelayAckOutcome::NotAttempted;
        let session_bound_terminal_delivery_attempted =
            session_bound_relay_should_own_terminal_delivery(
                relay_decision.should_direct_send,
                session_bound_discord_delivery_enabled,
                session_bound_relay_turn_fully_mirrored,
                relay_producer_session_name,
                inflight_before_relay.as_ref(),
                &tmux_session_name,
            );
        let session_bound_relay_owns_terminal_delivery =
            if session_bound_terminal_delivery_attempted {
                let ack_outcome = wait_for_session_bound_relay_delivery_ack(
                    all_data_session_bound_relay_ack.as_ref(),
                    std::time::Duration::from_secs(10),
                )
                .await;
                let ack_outcome = session_bound_ack_outcome_after_resolve_time_mirror_check(
                    ack_outcome,
                    &mut session_bound_relay_turn_fully_mirrored,
                    all_data_session_bound_relay_ack.as_ref(),
                    session_bound_relay_turn_first_forwarded_sequence,
                );
                session_bound_ack_outcome = ack_outcome;
                let delivered = session_bound_relay_turn_fully_mirrored
                    && session_bound_ack_confirms_transport(ack_outcome);
                if !delivered {
                    tracing::warn!(
                        provider = watcher_provider.as_str(),
                        channel_id = channel_id.get(),
                        tmux_session = %tmux_session_name,
                        ?ack_outcome,
                        "session-bound StreamRelay terminal delivery was not acknowledged"
                    );
                }
                delivered
            } else {
                false
            };
        let prompt_anchor_present = prompt_anchor_present_before_relay;
        let ssh_direct_pending = prompt_anchor_present
            || crate::services::tui_prompt_dedupe::is_ssh_direct_observation_pending(
                watcher_provider.as_str(),
                &tmux_session_name,
            );
        let external_input_lease_present = external_input_lease_before_relay;
        let recent_stop_reason =
            recent_turn_stop_for_watcher_range(channel_id, &tmux_session_name, data_start_offset)
                .map(|stop| stop.reason);
        let relay_owner_present = inflight_before_relay.as_ref().is_some_and(|state| {
            !matches!(
                state.effective_relay_owner_kind(),
                crate::services::discord::inflight::RelayOwnerKind::None
            )
        });
        let watcher_direct_fallback_requested = watcher_should_direct_send_after_session_bound_ack(
            relay_decision.should_direct_send,
            session_bound_ack_outcome,
            relay_owner_present,
        );
        // #5175: authenticate the soft terminal against the inflight row that
        // exists AT TURN END — the very row loaded a few lines above and logged
        // below as `inflight_present` / `inflight_relay_owner` — instead of the
        // `startup_inflight_snapshot` taken BEFORE the turn produced a byte. A
        // TUI-direct turn's row does not exist yet at snapshot time, so the old
        // wiring denied it authority permanently while the session-bound sink
        // skipped delivery believing the watcher owned it (nobody sent the body).
        let (watcher_direct_fallback_authorized, soft_terminal_authority_denial) =
            watcher_soft_terminal_direct_send_authority(
                startup_soft_terminal_authority,
                inflight_before_relay.as_ref(),
                current_offset,
                terminal_kind,
            );
        let watcher_direct_fallback_intended =
            watcher_direct_fallback_requested && watcher_direct_fallback_authorized;
        // #3041 P1-3 (Part b, §3.2): reconcile a non-`Delivered` ACK before re-send against the offset
        // authority FIRST, over the SAME consumed range `[data_start_offset, terminal_event_consumed_offset(current_offset, all_data))`.
        // Part (a) advances `committed_relay_offset` to the watcher's own `end` on a
        // confirmed sink delivery, so the consult is exact: committed >= end → SKIP (sink
        // delivered; ACK lagged → no duplicate, failure-mode-①); committed < end → re-send
        // the FULL response (no black-hole). codex BLOCKER 2: NO partial-suffix send (render
        // coordinate not derivable from the JSONL byte offset), delegation all-or-nothing so
        // `committed` is never strictly between start/end. Reconcile ONLY on the session-bound re-send path; plain watcher-direct unchanged.
        let watcher_resend_range_start = data_start_offset;
        let watcher_resend_range_end = terminal_event_consumed_offset(current_offset, &all_data);
        // #3593: self-heal a stale-high watermark BEFORE the resend-dedup `committed` read (no-inflight-gate parity; generation change → committed 0 → no false skip).
        reset_relay_watermark_on_generation_change(
            &shared,
            channel_id,
            &tmux_session_name,
            "watcher_terminal_resend_dedup",
        );
        let output_eof_for_resend_dedup =
            std::fs::metadata(&output_path).ok().map(|meta| meta.len());
        let watcher_resend_committed = dr::committed_floor_for_resend_dedup(
            &shared,
            &watcher_provider,
            channel_id,
            &tmux_session_name,
            output_eof_for_resend_dedup,
        ); // #3089 B2b + #3593 (codex HIGH): in-memory committed ∪ flag-independent durable frontier
        let watcher_resend_reconciled = session_bound_terminal_delivery_attempted
            && watcher_direct_fallback_intended
            && !matches!(
                session_bound_ack_outcome,
                SessionBoundRelayAckOutcome::Delivered
            );
        let watcher_resend_action = if watcher_resend_reconciled {
            // #3593: the stale-high self-heal ran unconditionally above (codex P2).
            // #3151: gate the re-send on the in-flight sink-delivery marker BEFORE
            // the committed-offset reconciliation. The marker is a `Leased{Sink}`
            // state on the SAME per-channel `DeliveryLeaseCell` the watcher's own
            // direct-send path acquires (B2). Read a coherent snapshot, then:
            //   * Leased{Sink, fresh}  → WaitInFlight: a sink POST is in flight; do
            //     NOT re-send this pass (the slow-sink-in-flight duplicate #3151).
            //   * Leased{Sink, expired} → reclaim the dead sink's marker, then
            //     SendFull (committed<end) — the no-black-hole arm.
            //   * Committed{Sink} → reconcile vs committed offset: committed>=end → Skip
            //     (delivered), committed<end → SendFull (#3159: refused/NotDelivered re-sends).
            //   * Unleased / non-Sink holder → unchanged (defer to the existing
            //     committed-offset reconciliation).
            let gate_cell = shared.delivery_lease(channel_id);
            let snapshot = gate_cell.read();
            // #3159 BUG 1 (codex race-1): read `committed` AFTER the lease snapshot. The sink's
            // CLEAR protocol advances `committed` FIRST, THEN commits the marker (`Committed{Sink}`),
            // so observing `Committed{Sink}` happens-after the committed write → reading `committed`
            // next sees the advanced value (committed>=end for a real Delivered → Skip). Reading it
            // BEFORE the snapshot could pair a pre-advance `committed < end` with a now-Committed
            // marker → a spurious SendFull duplicate.
            let committed = dr::effective_committed_offset(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                output_eof_for_resend_dedup,
            );
            let now_ms = crate::services::discord::lease_now_ms();
            let (action, reclaim_expired_sink) = watcher_terminal_resend_action_gated(
                &snapshot,
                committed,
                watcher_resend_range_start,
                watcher_resend_range_end,
                now_ms,
            );
            if reclaim_expired_sink {
                // Force the dead sink's marker Unleased so the watcher-direct path
                // below can re-acquire and SendFull (no black-hole). Deadline-only /
                // identity-agnostic — a LIVE sink (fresh deadline) is never reached.
                gate_cell.reclaim_if_expired(now_ms);
            }
            Some(action)
        } else if watcher_direct_fallback_intended
            && dr::range_already_committed(watcher_resend_range_end, watcher_resend_committed)
        {
            // #3593: already-delivered range (`committed >= end`) on the non-reconciled
            // synthetic-resume path (the placeholder path the #3520 new-message-only floor
            // missed) → EXISTING non-destructive `SkipAlreadyCommitted` arm, which PRESERVES
            // the restored placeholder (flipping `has_direct_terminal_response`/the fallback
            // flag would delete the already-delivered body — #3520 codex BLOCKER).
            Some(WatcherTerminalResendAction::SkipAlreadyCommitted)
        } else {
            None
        };
        // #3151: WaitInFlight suppresses BOTH the re-send and the skip-log this
        // pass — the watcher's NEXT terminal pass re-evaluates (bounded by the
        // sink's lease deadline). It must NOT be treated as "send" by the fallback.
        let watcher_resend_wait_in_flight = matches!(
            watcher_resend_action,
            Some(WatcherTerminalResendAction::WaitInFlight)
        );
        if watcher_resend_wait_in_flight {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                provider = watcher_provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                start = watcher_resend_range_start,
                end = watcher_resend_range_end,
                committed = watcher_resend_committed,
                ?session_bound_ack_outcome,
                "  [{ts}] 👁 #3151: deferred watcher terminal re-send — sink POST in flight (Leased{{Sink}}, fresh); will re-evaluate next pass (no duplicate)"
            );
        }
        if matches!(
            watcher_resend_action,
            Some(WatcherTerminalResendAction::SkipAlreadyCommitted)
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                provider = watcher_provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                start = watcher_resend_range_start,
                end = watcher_resend_range_end,
                committed = watcher_resend_committed,
                ?session_bound_ack_outcome,
                "  [{ts}] 👁 #3041 P1-3 §3.2: skipped watcher terminal re-send — range already committed by the sink (offset authority); no duplicate"
            );
        }
        // The watcher actually direct-sends only when the reconciliation did NOT
        // skip the range AND is not WAITING on an in-flight sink POST.
        // `SkipAlreadyCommitted` suppresses the re-send (no dup); `WaitInFlight`
        // (#3151) suppresses it this pass (re-evaluated next pass); `SendFull`/the
        // non-reconciled path proceed to send.
        let watcher_direct_fallback_after_session_bound_ack = watcher_direct_fallback_intended
            && !matches!(
                watcher_resend_action,
                Some(
                    WatcherTerminalResendAction::SkipAlreadyCommitted
                        | WatcherTerminalResendAction::WaitInFlight
                )
            );
        let session_bound_fallback_uses_full_body = session_bound_terminal_delivery_attempted
            && watcher_direct_fallback_after_session_bound_ack;
        let direct_terminal_response = watcher_terminal_response_for_direct_send(
            &full_response,
            response_sent_offset,
            session_bound_fallback_uses_full_body,
        );
        let direct_terminal_response_decision = watcher_direct_terminal_response_decision(
            &watcher_provider,
            channel_id,
            shared.restart.current_generation,
            &tmux_session_name,
            inflight_before_relay.as_ref(),
            current_offset,
            fresh_assistant_text_seen,
            direct_terminal_response,
        );
        let has_direct_terminal_response = direct_terminal_response_decision.has_sendable_body();
        let direct_terminal_response_refused_duplicate =
            watcher_direct_fallback_after_session_bound_ack
                && direct_terminal_response_decision.refused_duplicate();
        // #2838/#3042 (relay-stability P0-1): count the primary duplicate-emit vector — a
        // session-bound terminal ACK that timed out while the watcher direct-sends (sink may
        // have already posted; rising counts ⇒ P1 dual-authority lease overdue). Gate on the
        // raw `TimedOut` + original `should_direct_send` intent (records even when the ownerless-timeout suppression turned the fallback off).
        if relay_decision.should_direct_send
            && matches!(
                session_bound_ack_outcome,
                SessionBoundRelayAckOutcome::TimedOut
            )
        {
            crate::services::observability::metrics::record_relay_terminal_ack_timeout(
                channel_id.get(),
                watcher_provider.as_str(),
            );
        }
        // #3646 OBSERVATION-ONLY owner split: this is the INFLIGHT-snapshot owner
        // ONLY. The collapsed `="none"` could mean either a real None-ledger turn
        // OR "bridge cleared inflight but the ledger is still Watcher/finalized" —
        // the #3607 ambiguity. The finalizer-side `finalizer_ledger_owner` event
        // (ledger entry's relay_owner, same turn_id) supplies the second signal and
        // the two JOIN in PG. Computed once so we can emit it under BOTH the new
        // `inflight_relay_owner` name AND the legacy `relay_owner_kind` alias
        // (codex review #3678: keep the old field so existing dashboards/alerts/
        // runbooks that grep `relay_owner_kind=` don't break).
        let inflight_relay_owner_kind = inflight_before_relay
            .as_ref()
            .map(|state| state.effective_relay_owner_kind().as_str())
            .unwrap_or("none");
        tracing::info!(
            target: "agentdesk::relay_flight_recorder",
            provider = watcher_provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session = %tmux_session_name,
            data_start_offset,
            current_offset,
            terminal_kind = terminal_kind.map(WatcherTerminalKind::as_str).unwrap_or("unknown"),
            full_response_len = current_response.len(),
            assistant_text_seen,
            any_tool_used = tool_state.any_tool_used,
            has_post_tool_text = tool_state.has_post_tool_text,
            inflight_present = inflight_before_relay.is_some(),
            // #3646: new disambiguated name. Field rename/add only — control flow
            // unchanged (these are tracing fields, not branches).
            inflight_relay_owner = inflight_relay_owner_kind,
            // #3646: legacy alias preserved for backward-compatible log greps.
            relay_owner_kind = inflight_relay_owner_kind,
            session_bound_enabled = session_bound_discord_delivery_enabled,
            fully_mirrored = session_bound_relay_turn_fully_mirrored,
            frame_ack = session_bound_relay_frame_ack_reached(all_data_session_bound_relay_ack.as_ref()),
            terminal_commit_ack = session_bound_relay_owns_terminal_delivery,
            route = if session_bound_relay_owns_terminal_delivery {
                "session_bound"
            } else if direct_terminal_response_refused_duplicate {
                "duplicate_guard_refused"
            } else if watcher_direct_fallback_requested && !watcher_direct_fallback_authorized {
                // #5175: name the conjunct that denied authority. The historical
                // `soft_terminal_no_authority` prefix is preserved so existing
                // prefix greps keep matching; `soft_terminal_denial` below is the
                // stable exact-match field.
                soft_terminal_authority_denial
                    .map(SoftTerminalAuthorityDenial::route_label)
                    .unwrap_or("soft_terminal_no_authority")
            } else if watcher_direct_fallback_after_session_bound_ack {
                "watcher_direct"
            } else if relay_decision.suppressed {
                "suppressed"
            } else {
                "none"
            },
            // #5175: stable exact-match denial field + the pre-#5175 snapshot
            // verdict, so a lane that only the new rule authorizes is visible.
            soft_terminal_denial = soft_terminal_authority_denial
                .map(SoftTerminalAuthorityDenial::as_str)
                .unwrap_or("none"),
            startup_snapshot_authority = startup_soft_terminal_authority.startup_snapshot_authorized(),
            prompt_anchor_present,
            ssh_direct_pending,
            external_input_lease_present,
            recent_stop_reason = recent_stop_reason.as_deref().unwrap_or("none"),
            placeholder_msg_id = placeholder_msg_id.map(|id| id.get()).unwrap_or(0),
            status_panel_msg_id = status_panel_msg_id.map(|id| id.get()).unwrap_or(0),
            frame_ack_outcome = ?session_bound_ack_outcome,
            "relay flight recorder"
        );
        // #5175: a terminal frame that the sink did not deliver AND the watcher
        // is not authorized to deliver has NO owner — the body is silently lost
        // and the delivery frontier never advances, so redrive re-publishes the
        // previous answer forever. This used to leave only an INFO-level route
        // string, which is why the watchdog scored the wedged channel `gap 0 /
        // wedge 0` for a week. Promote it to WARN + a per-conjunct counter.
        if let Some(denial) = soft_terminal_authority_denial
            .filter(|_| watcher_direct_fallback_requested && !watcher_direct_fallback_authorized)
        {
            crate::services::observability::metrics::record_relay_terminal_authority_denied(
                channel_id.get(),
                watcher_provider.as_str(),
                denial.metric_name(),
            );
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = watcher_provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                data_start_offset,
                current_offset,
                terminal_kind = terminal_kind.map(WatcherTerminalKind::as_str).unwrap_or("unknown"),
                soft_terminal_denial = denial.as_str(),
                inflight_present = inflight_before_relay.is_some(),
                inflight_relay_owner = inflight_relay_owner_kind,
                startup_snapshot_authority = startup_soft_terminal_authority.startup_snapshot_authorized(),
                full_response_len = current_response.len(),
                ?session_bound_ack_outcome,
                "  [{ts}] ⚠ #5175: terminal frame has NO delivery owner — sink did not deliver and the soft terminal is unauthorized; body dropped and the delivery frontier will not advance"
            );
        }
        // #3041 P1-3 (codex P1-3 R7): turn-boundary ACK reset. THIS turn's terminal
        // ACK has now been waited on (`session_bound_ack_outcome` is captured) and
        // logged. If a forward on this pass SPLIT a result-bearing chunk with a
        // trailing tail, a LATER turn (B) follows in the leftover buffer. B is
        // processed on a SUBSEQUENT pass — possibly while `turn_identity_for_panel`
        // is STILL pinned to THIS turn's offset (B's inflight not yet established),
        // which would make `carry_session_bound_ack_for_turn` KEEP this turn's stale
        // ack and let this turn's `Delivered` falsely satisfy B's ACK → B
        // black-holed. RESET the stored ack to `None` HERE, AFTER this turn consumed
        // it, so B starts with NO inherited ack → MissingTarget → §3.2 reconcile
        // (committed-offset SendFull-or-Skip) → B is never black-holed (worst case a
        // duplicate, the #3151-deferred edge). This is the primary R7 guarantee and
        // is independent of whether the pinned identity refreshes.
        if split_trailing_turn_follows {
            all_data_session_bound_relay_ack = None;
        }
        let watcher_direct_terminal_idle_committed = false;
        let tui_direct_anchor_terminal_body_visible = false;
        let tui_direct_anchor_or_lease_present_for_lifecycle =
            prompt_anchor_present_before_relay || external_input_lease_before_relay;
        write_terminal_relay_plan_state(
            state,
            all_data_session_bound_relay_ack,
            monitor_auto_turn_claimed,
            monitor_auto_turn_finished,
            monitor_auto_turn_synthetic_msg_id,
            monitor_auto_turn_ledger_generation,
        );
        TerminalRelayPlanOutcome::Proceed(TerminalRelayPlan {
            relay_coord,
            slot_guard,
            relay_decision,
            session_bound_relay_owns_terminal_delivery,
            ssh_direct_pending,
            watcher_direct_fallback_requested,
            watcher_direct_fallback_authorized,
            watcher_direct_fallback_after_session_bound_ack,
            session_bound_fallback_uses_full_body,
            direct_terminal_response,
            has_direct_terminal_response,
            direct_terminal_response_refused_duplicate,
            watcher_resend_action,
            watcher_direct_terminal_idle_committed,
            tui_direct_anchor_terminal_body_visible,
            tui_direct_anchor_or_lease_present_for_lifecycle,
        })
    }
}

#[cfg(test)]
#[path = "terminal_relay_plan_tests.rs"]
mod soft_terminal_direct_send_authority_tests;
