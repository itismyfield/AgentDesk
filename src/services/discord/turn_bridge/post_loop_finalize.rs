//! #4230 S4 post-loop owner classification + finalizer for `turn_bridge::spawn_turn_bridge`.
//!
//! Moved from the post-loop tail of `spawn_turn_bridge`: stream-exit
//! placeholder cleanup, orphaned tool finalization, API friction extraction,
//! follow-up requeue candidate, review dispatch guard, bridge output owner
//! classification, `TURN_ACTIVE` publish, finalizing counters, early TUI gate,
//! busy-watcher handoff, and single-authority finalizer submission.

use std::sync::Arc;

use super::output_lifecycle::{BridgeOutputOwner, classify_bridge_output_owner};
use super::stream_tick::{
    LongRunningPlaceholderActive, PendingLongRunningOpenAfterStateSave,
    PendingLongRunningRetargetAfterStateSave,
};
use super::streaming_edit_text::TuiErrorClassification;
use super::*;

pub(super) struct PostLoopFinalizeContext {
    pub(super) shared_owned: Arc<SharedData>,
    pub(super) gateway: Arc<dyn TurnGateway>,
    pub(super) channel_id: ChannelId,
    pub(super) provider: ProviderKind,
    pub(super) adk_session_key: Option<String>,
    pub(super) adk_session_name: Option<String>,
    pub(super) adk_session_info: Option<String>,
    pub(super) adk_cwd: Option<String>,
    pub(super) dispatch_id: Option<String>,
    pub(super) role_binding: Option<RoleBinding>,
    pub(super) turn_id: String,
    pub(super) current_msg_id: MessageId,
    pub(super) cancelled: bool,
    pub(super) transport_error: bool,
    pub(super) tui_error_classification: TuiErrorClassification,
    pub(super) recovery_retry: bool,
    pub(super) rx_disconnected: bool,
    pub(super) tmux_handed_off: bool,
    pub(super) standby_relay_owns_output: bool,
    pub(super) watcher_owns_assistant_relay: bool,
    pub(super) watcher_relay_available_for_turn: bool,
    pub(super) initial_relay_owner_kind: super::super::inflight::RelayOwnerKind,
    pub(super) response_sent_offset: usize,
    pub(super) tmux_last_offset: Option<u64>,
    pub(super) watcher_owner_channel_id: ChannelId,
    pub(super) accumulated_input_tokens: u64,
    pub(super) accumulated_cache_create_tokens: u64,
    pub(super) accumulated_cache_read_tokens: u64,
    pub(super) accumulated_output_tokens: u64,
}

pub(super) struct PostLoopFinalizeState {
    pub(super) full_response: String,
    pub(super) active_background_child_session_ids: Vec<i64>,
    pub(super) pending_long_running_open_after_state_save: PendingLongRunningOpenAfterStateSave,
    pub(super) pending_long_running_retarget_after_state_save:
        PendingLongRunningRetargetAfterStateSave,
    pub(super) long_running_placeholder_active: LongRunningPlaceholderActive,
    pub(super) current_tool_line: Option<String>,
    pub(super) prev_tool_status: Option<String>,
    pub(super) inflight_state: InflightTurnState,
    pub(super) api_friction_reports: Vec<crate::services::api_friction::ApiFrictionReport>,
}

pub(super) struct PostLoopFinalizeOutput {
    pub(super) full_response: String,
    pub(super) active_background_child_session_ids: Vec<i64>,
    pub(super) pending_long_running_open_after_state_save: PendingLongRunningOpenAfterStateSave,
    pub(super) pending_long_running_retarget_after_state_save:
        PendingLongRunningRetargetAfterStateSave,
    pub(super) long_running_placeholder_active: LongRunningPlaceholderActive,
    pub(super) inflight_state: InflightTurnState,
    pub(super) api_friction_reports: Vec<crate::services::api_friction::ApiFrictionReport>,
    pub(super) claude_tui_followup_pre_submit_requeue_candidate: bool,
    pub(super) tui_error_classification: TuiErrorClassification,
    pub(super) review_dispatch_warning: Option<String>,
    pub(super) is_prompt_too_long: bool,
    pub(super) bridge_relay_delegated_to_watcher: bool,
    pub(super) bridge_output_owner: Option<BridgeOutputOwner>,
    pub(super) should_complete_work_dispatch_after_delivery: bool,
    pub(super) should_fail_dispatch_after_delivery: bool,
    pub(super) final_session_status: &'static str,
    pub(super) can_chain_locally: bool,
    pub(super) has_queued_turns: bool,
    #[cfg(unix)]
    pub(super) bridge_tui_gate_outcome_early: Option<super::super::tmux::TuiCompletionGateOutcome>,
}

pub(super) async fn run_post_loop_finalize(
    ctx: PostLoopFinalizeContext,
    state: PostLoopFinalizeState,
) -> PostLoopFinalizeOutput {
    let shared_owned = ctx.shared_owned;
    let gateway = ctx.gateway;
    let channel_id = ctx.channel_id;
    let provider = ctx.provider;
    let adk_session_key = ctx.adk_session_key;
    let adk_session_name = ctx.adk_session_name;
    let adk_session_info = ctx.adk_session_info;
    let adk_cwd = ctx.adk_cwd;
    let dispatch_id = ctx.dispatch_id;
    let role_binding = ctx.role_binding;
    let turn_id = ctx.turn_id;
    let current_msg_id = ctx.current_msg_id;
    let cancelled = ctx.cancelled;
    let transport_error = ctx.transport_error;
    let tui_error_classification = ctx.tui_error_classification;
    let recovery_retry = ctx.recovery_retry;
    let rx_disconnected = ctx.rx_disconnected;
    let tmux_handed_off = ctx.tmux_handed_off;
    let standby_relay_owns_output = ctx.standby_relay_owns_output;
    let watcher_owns_assistant_relay = ctx.watcher_owns_assistant_relay;
    let watcher_relay_available_for_turn = ctx.watcher_relay_available_for_turn;
    let initial_relay_owner_kind = ctx.initial_relay_owner_kind;
    let response_sent_offset = ctx.response_sent_offset;
    let tmux_last_offset = ctx.tmux_last_offset;
    let watcher_owner_channel_id = ctx.watcher_owner_channel_id;
    let accumulated_input_tokens = ctx.accumulated_input_tokens;
    let accumulated_cache_create_tokens = ctx.accumulated_cache_create_tokens;
    let accumulated_cache_read_tokens = ctx.accumulated_cache_read_tokens;
    let accumulated_output_tokens = ctx.accumulated_output_tokens;

    let mut full_response = state.full_response;
    let mut active_background_child_session_ids = state.active_background_child_session_ids;
    let mut pending_long_running_open_after_state_save =
        state.pending_long_running_open_after_state_save;
    let mut pending_long_running_retarget_after_state_save =
        state.pending_long_running_retarget_after_state_save;
    let mut long_running_placeholder_active = state.long_running_placeholder_active;
    let mut current_tool_line = state.current_tool_line;
    let mut prev_tool_status = state.prev_tool_status;
    let mut inflight_state = state.inflight_state;
    let mut api_friction_reports = state.api_friction_reports;

    // codex round-9 P3 on PR #1308: drain any active long-running
    // placeholder on stream-error / receive-disconnect exits too. The
    // cancel branch already drives the controller to `Aborted`; here we
    // also need to handle `StreamMessage::Error` and `rx_disconnected`
    // exits so the controller does not leak an `Active` row and the
    // persisted `long_running_placeholder_active` flag does not survive
    // for the sweeper to abandon the card. Skip when `cancelled` (the
    // dedicated cancel block below handles it) or when a relay owner will
    // continue the visible output lifecycle.
    let relay_owns_output_at_stream_end = standby_relay_owns_output
        || (rx_disconnected && tmux_handed_off && full_response.is_empty());
    if pending_long_running_open_after_state_save.take().is_some() {
        inflight_state.long_running_placeholder_active = false;
        let _ = save_inflight_state(&inflight_state);
    }
    if !cancelled && relay_owns_output_at_stream_end {
        let relay_owned_pending_retarget_matches_active =
            if let Some((active_key, _, _, _)) = long_running_placeholder_active.as_ref() {
                pending_long_running_retarget_after_state_save
                    .as_ref()
                    .is_some_and(|(pending_key, _, _, _, _)| pending_key == active_key)
            } else {
                false
            };
        if relay_owned_pending_retarget_matches_active
            && let Some((key, _, _, _)) = long_running_placeholder_active.take()
        {
            let _ = pending_long_running_retarget_after_state_save.take();
            shared_owned.ui.placeholder_controller.detach(&key);
            inflight_state.long_running_placeholder_active = false;
            let _ = save_inflight_state(&inflight_state);
        }
    }
    if !cancelled && !relay_owns_output_at_stream_end {
        if let Some((key, _, _, _)) = long_running_placeholder_active.take() {
            let target = if transport_error || rx_disconnected {
                super::super::placeholder_controller::PlaceholderLifecycle::Aborted
            } else {
                super::super::placeholder_controller::PlaceholderLifecycle::Completed
            };
            let pending_retarget_matches_key = pending_long_running_retarget_after_state_save
                .as_ref()
                .is_some_and(|(pending_key, _, _, _, _)| *pending_key == key);
            if pending_retarget_matches_key {
                let _ = pending_long_running_retarget_after_state_save.take();
                shared_owned.ui.placeholder_controller.detach(&key);
                inflight_state.long_running_placeholder_active = false;
            } else {
                let outcome = shared_owned
                    .ui
                    .placeholder_controller
                    .transition(gateway.as_ref(), key, target)
                    .await;
                // codex round-10 P2: keep the persisted flag on EditFailed so
                // the sweeper can finalize the still-visible 🔄 card later.
                use super::super::placeholder_controller::PlaceholderControllerOutcome::*;
                if matches!(outcome, Edited | Coalesced | AlreadyTerminal) {
                    inflight_state.long_running_placeholder_active = false;
                }
            }
            let _ = save_inflight_state(&inflight_state);
        }
        if transport_error || rx_disconnected {
            close_all_tracked_background_children(
                shared_owned.pg_pool.as_ref(),
                &mut active_background_child_session_ids,
                "aborted",
                "turn loop exit",
            )
            .await;
        }
    }

    // #1113 stream-end finalization: the main turn loop has exited, which
    // means we won't receive any more StreamMessage events for this turn.
    // If `current_tool_line` still carries the running ⚙ marker, the
    // ToolResult never landed (process exit, parser error, transport
    // disconnect, or relay ownership transfer for the rest of the
    // delivery). Promote it to its terminal ⚠ form and stash in
    // prev_tool_status so any follow-on placeholder edit (recovery notice,
    // recovery notice, terminal response) reflects the orphaned state.
    if let Some(running) = current_tool_line.as_deref() {
        if running.starts_with("⚙") {
            let finalized = super::super::formatting::finalize_in_progress_tool_status(running);
            super::super::formatting::preserve_previous_tool_status(
                &mut prev_tool_status,
                Some(finalized.as_str()),
                None,
            );
            current_tool_line = Some(finalized);
            inflight_state.current_tool_line = current_tool_line.clone();
            inflight_state.prev_tool_status = prev_tool_status.clone();
            let _ = save_inflight_state(&inflight_state);
        }
    }

    let extracted_api_friction =
        crate::services::api_friction::extract_api_friction_reports(&full_response);
    if !extracted_api_friction.reports.is_empty() {
        api_friction_reports.extend(extracted_api_friction.reports);
        full_response = extracted_api_friction.cleaned_response;
        inflight_state.full_response = full_response.clone();
    }
    for error in extracted_api_friction.parse_errors {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ invalid API_FRICTION marker: {error}");
    }

    let claude_tui_followup_pre_submit_requeue_candidate = {
        let base = crate::services::claude::claude_tui_followup_requeue_enabled()
            && bridge_claude_tui_followup_requeue_prompt_error(
                &provider,
                inflight_state.runtime_kind,
                &full_response,
                tui_error_classification,
            );
        // #3885 (reworked): a follow-up pre-submit readiness timeout normally
        // requeues the inflight ("prompt never reached the pane → safe to
        // retry"). The dup risk is re-injecting an input that ALREADY landed:
        // when the SAME input is the turn the pane is streaming (or just
        // completed), that turn already delivers the response, so a requeue
        // produces duplicate prose. The first cut gated on a channel-scoped
        // busy probe, which (a) DROPPED a genuinely-unsubmitted follow-up that
        // happened to sit behind a DIFFERENT streaming turn, and (b) missed
        // the already-completed same-input case (idle pane). Gate instead on
        // INPUT CORRELATION: suppress ONLY when the recorded prompt anchor for
        // this pane resolves to THIS inflight's user_msg_id (the relay records
        // the submitted prompt's anchor as the synthetic inflight's
        // user_msg_id; a non-consuming peek leaves it for the watcher). A
        // different / absent anchor means the follow-up is genuinely
        // unsubmitted, so it STILL requeues and remains preserved in the
        // mailbox behind the active turn until the completion event kicks the
        // drain.
        let same_input_occupies_pane = base
            && claude_tui_followup_same_input_occupies_pane(
                inflight_state
                    .tmux_session_name
                    .as_deref()
                    .and_then(|tmux_session_name| {
                        crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                            provider.as_str(),
                            tmux_session_name,
                            channel_id.get(),
                        )
                    })
                    .map(|anchor| anchor.message_id),
                inflight_state.user_msg_id,
            );
        claude_tui_followup_requeue_streaming_aware(base, same_input_occupies_pane)
    };
    if claude_tui_followup_pre_submit_requeue_candidate {
        full_response = CLAUDE_TUI_FOLLOWUP_REQUEUE_DELIVERY_NOTICE.to_string();
        inflight_state.full_response = full_response.clone();
    }

    let is_prompt_too_long = full_response.contains("__prompt too long__");
    let review_dispatch_warning = if !cancelled && !is_prompt_too_long {
        guard_review_dispatch_completion(
            shared_owned.api_port,
            dispatch_id.as_deref(),
            &full_response,
            provider.as_str(),
        )
        .await
    } else {
        None
    };
    if review_dispatch_warning.is_some() {
        crate::services::observability::emit_guard_fired(
            provider.as_str(),
            channel_id.get(),
            dispatch_id.as_deref(),
            adk_session_key.as_deref(),
            Some(turn_id.as_str()),
            "review_dispatch_pending",
        );
    }
    let terminal_error_path = cancelled || is_prompt_too_long || transport_error || recovery_retry;
    // A bridge rebuilt from durable state must honor the row's existing
    // relay owner. The pending-response guard below only applies to
    // in-process handoffs where the bridge may already own unsent bytes.
    let recovered_watcher_owns_output = matches!(
        initial_relay_owner_kind,
        super::super::inflight::RelayOwnerKind::Watcher
    ) && watcher_owns_assistant_relay
        && watcher_relay_available_for_turn
        && !terminal_error_path;
    let response_unsent = response_portion_after_offset(&full_response, response_sent_offset);
    let response_pending_trimmed_empty = response_unsent.trim().is_empty();
    // #3268: `mut` so the post-gate self-healing handoff (below) can promote it.
    let mut bridge_relay_delegated_to_watcher = recovered_watcher_owns_output
        || should_delegate_bridge_relay_to_watcher(
            watcher_owns_assistant_relay,
            watcher_relay_available_for_turn,
            !response_pending_trimmed_empty,
            cancelled,
            is_prompt_too_long,
            transport_error,
            recovery_retry,
        );
    // #3268: `mut` so the post-gate self-healing handoff can promote it to
    // `WatcherRelay` once the gate confirms the pane is still busy.
    let mut bridge_output_owner = classify_bridge_output_owner(
        standby_relay_owns_output
            && !cancelled
            && !is_prompt_too_long
            && !transport_error
            && !recovery_retry,
        bridge_relay_delegated_to_watcher,
    );
    // #3281: empty-terminal-response visibility (owner-`None` kind/payload
    // preserved verbatim; adds the delegated-watcher quadrant) — see
    // `watcher_handoff::emit_bridge_empty_terminal_response_visibility`.
    watcher_handoff::emit_bridge_empty_terminal_response_visibility(
        shared_owned.as_ref(),
        watcher_owner_channel_id,
        bridge_output_owner,
        terminal_error_path,
        &provider,
        channel_id,
        dispatch_id.as_deref(),
        adk_session_key.as_deref(),
        turn_id.as_str(),
        current_msg_id.get(),
        response_pending_trimmed_empty,
        watcher_owns_assistant_relay,
        watcher_relay_available_for_turn,
        standby_relay_owns_output,
        rx_disconnected,
        tmux_handed_off,
        response_sent_offset,
        full_response.len(),
        tmux_last_offset,
        inflight_state.turn_start_offset,
    );

    // Explicitly complete implementation/rework dispatches only after the
    // terminal Discord delivery commits. Completing here used to let
    // dispatch followups / auto-queue slot release race ahead of the final
    // message edit, so an archived/deleted thread could strand the turn
    // while the queue already advanced.
    // #3268: `mut` so the post-gate self-healing handoff can clear it — a
    // handed-off turn is NOT done on the bridge side.
    let mut should_complete_work_dispatch_after_delivery =
        !cancelled && !is_prompt_too_long && !transport_error && bridge_output_owner.is_none();
    let should_fail_dispatch_after_delivery = transport_error && !cancelled;

    let final_session_status = if active_background_child_session_ids.is_empty() {
        IDLE
    } else {
        AWAITING_BG
    };

    // Keep the session visibly active while Discord terminal delivery and
    // status-panel finalization are still pending. Publishing idle here lets
    // observers race ahead of the final response/status edit.
    post_adk_session_status(
        adk_session_key.as_deref(),
        adk_session_name.as_deref(),
        Some(provider.as_str()),
        TURN_ACTIVE,
        &provider,
        adk_session_info.as_deref(),
        persisted_context_tokens(
            accumulated_input_tokens,
            accumulated_cache_create_tokens,
            accumulated_cache_read_tokens,
            accumulated_output_tokens,
        ),
        adk_cwd.as_deref(),
        dispatch_id.as_deref(),
        adk_session_name
            .as_deref()
            .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name),
        Some(channel_id),
        role_binding
            .as_ref()
            .map(|binding| binding.role_id.as_str()),
        shared_owned.api_port,
    )
    .await;

    let can_chain_locally = gateway.can_chain_locally();
    // Mark this turn as finalizing — deferred restart must wait until we finish
    // sending the Discord response and cleaning up state.
    shared_owned
        .restart
        .finalizing_turns
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    shared_owned
        .restart
        .global_finalizing
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // #2293/#2780 — hoist the TUI completion gate BEFORE the visible
    // completion/status cleanup so we can still suppress `응답 완료` when
    // the pane has not quiesced. Do NOT use this gate as a mailbox
    // correctness primitive: the bounded 3s probe is best-effort
    // observability, and blocking `mailbox_finish_turn` here strands
    // later user messages behind a stale active turn. The hosted-TUI
    // pre-submit guard below is the correctness barrier that prevents
    // follow-up input from being injected into a still-busy pane.
    // #3038: the early TUI completion gate (eligibility filter + bounded
    // quiescence probe + timed-out warning) is extracted verbatim to
    // `early_tui_completion.rs`. The two outputs are consumed later, so the
    // `#[cfg]` `let` declarations stay here (preserving the exact unix /
    // non-unix split) and the helper returns the computed values; behavior
    // is byte-identical (see the module doc for the seam-fix note).
    #[cfg(unix)]
    let bridge_early_gate_timed_out;
    #[cfg(not(unix))]
    let bridge_early_gate_timed_out = false;
    #[cfg(unix)]
    let bridge_tui_gate_outcome_early: Option<super::super::tmux::TuiCompletionGateOutcome>;
    #[cfg(unix)]
    {
        let (outcome_early, gate_timed_out) = early_tui_completion::run_early_tui_completion_gate(
            cancelled,
            is_prompt_too_long,
            transport_error,
            recovery_retry,
            &inflight_state,
            &provider,
            channel_id,
        )
        .await;
        bridge_tui_gate_outcome_early = outcome_early;
        bridge_early_gate_timed_out = gate_timed_out;
    }
    // #3268 (Defect B): on (gate timeout + non-terminal + genuinely-live
    // watcher) hand the busy turn back to the watcher — see `watcher_handoff`.
    #[cfg(unix)]
    watcher_handoff::maybe_hand_off_busy_turn_to_watcher(
        &shared_owned,
        bridge_early_gate_timed_out,
        terminal_error_path,
        watcher_owner_channel_id,
        channel_id,
        &provider,
        dispatch_id.as_deref(),
        adk_session_key.as_deref(),
        turn_id.as_str(),
        current_msg_id.get(),
        can_chain_locally,
        tmux_last_offset,
        response_unsent,
        &mut inflight_state,
        &mut bridge_relay_delegated_to_watcher,
        &mut bridge_output_owner,
        &mut should_complete_work_dispatch_after_delivery,
    );
    let has_queued_turns = if bridge_relay_delegated_to_watcher {
        // #1452 (Codex P1): the actual `mailbox_finalize_owed.store(true,
        // Release)` happens EARLIER, at the watcher-unpause site in the
        // `TmuxReady` branch (~line 1980). Doing it there guarantees we
        // win any race with a fast watcher whose terminal `swap(false,
        // AcqRel)` could otherwise execute before this late delegation
        // decision and leave stale debt that would clear the next turn's
        // cancel_token.
        //
        // Here we only verify the invariant: the single-authority finalizer
        // ledger still holds a live (non-`Finalized`) watcher-owned Pending
        // entry for this turn's channel/generation. If it does not, the
        // watcher will not finalize and the channel mailbox would leak its
        // cancel_token; we surface the violation via
        // `record_turn_bridge_invariant`.
        //
        // #3016 phase-5b1: this replaces the legacy
        // `mailbox_finalize_owed.load()` consumer with the ledger query. The
        // two are equivalent because `owed = true` is set atomically with the
        // `register_start(.., RelayOwnerKind::Watcher)` at the watcher-unpause
        // sites (~4196, ~6129) keyed by `TurnKey::new(channel_id, _,
        // current_generation)` — i.e. `owed ⟺ a live watcher Pending entry`.
        // The query keys on the SAME `channel_id` + `current_generation`
        // `register_start` used (the flag field/producers are removed in
        // phase-5b2).
        let handoff_recorded = shared_owned
            .turn_finalizer
            .has_live_watcher_pending(channel_id, shared_owned.restart.current_generation)
            .await;
        record_turn_bridge_invariant(
            handoff_recorded,
            &provider,
            channel_id,
            dispatch_id.as_deref(),
            adk_session_key.as_deref(),
            Some(turn_id.as_str()),
            "bridge_handoff_finds_watcher_handle",
            "src/services/discord/turn_bridge/mod.rs:bridge_relay_delegated_to_watcher",
            "bridge delegation expected to find a live watcher handle holding finalization debt",
            serde_json::json!({
                "watcher_owner_channel_id": watcher_owner_channel_id.get(),
            }),
        );
        if handoff_recorded {
            false
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                watcher_owner_channel = watcher_owner_channel_id.get(),
                tui_gate_timed_out = bridge_early_gate_timed_out,
                "  [{ts}] ⚠ bridge watcher handoff missing finalizer; bridge is releasing mailbox to avoid stranded queued turns"
            );
            // #3016 phase 2: route through the single-authority finalizer.
            // It owns the exact sequence this branch ran inline (mailbox
            // cancel_token release + `mark_completion_cleanup`/`cancelled`
            // + counter decrement gated on `removed_token.is_some()` +
            // watchdog override clear + dispatch_thread_parents retain +
            // voice drain + dispatch_role_overrides cleanup). The ledger
            // gate makes a transitional double-finalize with a still-direct
            // watcher (phase 3 routes it) a harmless idempotent no-op.
            let outcome = shared_owned
                .turn_finalizer
                .submit_terminal(
                    super::super::turn_finalizer::TurnKey::new(
                        channel_id,
                        inflight_state.effective_finalizer_turn_id(),
                        shared_owned.restart.current_generation,
                    ),
                    provider.clone(),
                    if cancelled {
                        super::super::turn_finalizer::TerminalEvent::Cancel
                    } else {
                        super::super::turn_finalizer::TerminalEvent::Complete
                    },
                    super::super::turn_finalizer::FinalizeContext::bridge(),
                    shared_owned.clone(),
                )
                .await;
            // `finalize_owned` is the real invariant: SOME authority (this
            // bridge submission OR whoever already finalized — the ledger
            // guarantees exactly-once) owns the finalize for this turn. All
            // three outcomes satisfy it, so none is a violation.
            // `this_finalized` records only whether THIS submission was the
            // one finalizer; an `AlreadyFinalized` (someone else won, e.g. a
            // sweeper/watcher race) is the normal/info path and must NOT
            // emit a false `[invariant]` ERROR.
            let (this_finalized, finalize_owned, has_pending_after_voice) = match outcome {
                super::super::turn_finalizer::FinalizeOutcome::Finalized {
                    removed_token,
                    has_pending,
                    ..
                } => (removed_token.is_some(), true, has_pending),
                super::super::turn_finalizer::FinalizeOutcome::AlreadyFinalized
                | super::super::turn_finalizer::FinalizeOutcome::Deferred => {
                    // Something else finalized first (rare sweeper race —
                    // the watcher handle is gone in this branch). This
                    // branch always drained deferred voice work before
                    // #3016, so drain it here too rather than leaking
                    // deferred voice prompts.
                    let voice_deferred_enqueued = shared_owned
                        .voice_barge_in
                        .drain_deferred_after_turn(&shared_owned, &provider, channel_id)
                        .await;
                    (false, true, voice_deferred_enqueued)
                }
            };
            let _ = this_finalized;
            record_turn_bridge_invariant(
                finalize_owned,
                &provider,
                channel_id,
                dispatch_id.as_deref(),
                adk_session_key.as_deref(),
                Some(turn_id.as_str()),
                "mailbox_active_turn_recovered_after_missing_watcher_handoff",
                "src/services/discord/turn_bridge/mod.rs:bridge_relay_delegated_to_watcher",
                "missing watcher handoff finalizer must leave the turn finalized by some authority",
                serde_json::json!({
                    "this_finalized": this_finalized,
                    "has_pending": has_pending_after_voice,
                    "watcher_owner_channel_id": watcher_owner_channel_id.get(),
                }),
            );
            has_pending_after_voice
        }
    } else {
        // #1452 non-delegation path: we are finalizing on the bridge side
        // (cancelled / prompt_too_long / transport_error / recovery_retry,
        // or the watcher never ended up owning relay).
        //
        // #3016 phase 2/3: the single-authority finalizer's per-turn
        // ledger phase gate ARBITRATES exactly-once — whoever submits
        // the terminal first finalizes; the loser receives
        // `AlreadyFinalized` and does nothing.
        //
        // #3016 phase-5b2: the legacy `mailbox_finalize_owed` flag has been
        // removed entirely, so the CAS true→false revoke that used to run
        // here (gated on `bridge_published_finalize_owed_for_this_turn`) is
        // gone. The ledger's exactly-once gate already guards the
        // cross-turn hazard the CAS protected against — a watcher surviving
        // into the NEXT turn no longer has a flag to swap.
        if bridge_early_gate_timed_out {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                "  [{ts}] ⚠ #2293/#2780: bridge releasing mailbox despite TUI quiescence timeout; follow-up pre-submit gate will requeue if pane is still busy"
            );
        }
        let outcome = shared_owned
            .turn_finalizer
            .submit_terminal(
                super::super::turn_finalizer::TurnKey::new(
                    channel_id,
                    inflight_state.effective_finalizer_turn_id(),
                    shared_owned.restart.current_generation,
                ),
                provider.clone(),
                if cancelled {
                    super::super::turn_finalizer::TerminalEvent::Cancel
                } else {
                    super::super::turn_finalizer::TerminalEvent::Complete
                },
                super::super::turn_finalizer::FinalizeContext::bridge(),
                shared_owned.clone(),
            )
            .await;
        // `finalize_owned` is the actual invariant: SOMEONE (this bridge
        // submission OR the watcher that won the race OR the deadline-armed
        // backstop for a deferred gate-timeout) owns the one finalize for
        // this turn. The single-authority ledger guarantees that, so all
        // three outcomes are the normal/info path. `this_finalized` only
        // records whether THIS submission was the one that finalized — it
        // must NOT be conflated with an invariant violation, otherwise a
        // watcher-won `AlreadyFinalized` (e.g. the watcher consumed
        // `mailbox_finalize_owed` before the bridge revoked it) emits a
        // false `[invariant]` ERROR for completely normal exactly-once
        // behaviour — the pre-#3016 code treated that as an info path.
        let (this_finalized, finalize_owned, has_pending_after_voice) = match outcome {
            super::super::turn_finalizer::FinalizeOutcome::Finalized {
                removed_token,
                has_pending,
                ..
            } => (removed_token.is_some(), true, has_pending),
            super::super::turn_finalizer::FinalizeOutcome::AlreadyFinalized
            | super::super::turn_finalizer::FinalizeOutcome::Deferred => {
                // The watcher won the finalize race (or the gate-timeout
                // deferred), so the bridge's `do_finalize` did NOT run and
                // the watcher's finalize context does not drain voice. The
                // pre-#3016 `watcher_already_finalized` branch still drained
                // deferred voice barge-in work here, so we must too —
                // otherwise prompts deferred during the turn stay stuck.
                let voice_deferred_enqueued = shared_owned
                    .voice_barge_in
                    .drain_deferred_after_turn(&shared_owned, &provider, channel_id)
                    .await;
                (false, true, voice_deferred_enqueued)
            }
        };
        let _ = this_finalized;
        record_turn_bridge_invariant(
            finalize_owned,
            &provider,
            channel_id,
            dispatch_id.as_deref(),
            adk_session_key.as_deref(),
            Some(turn_id.as_str()),
            "mailbox_active_turn_matches_dispatch",
            "src/services/discord/turn_bridge/mod.rs:mailbox_finish_turn",
            "turn_bridge finalization expected the turn to be finalized by some authority",
            serde_json::json!({
                "this_finalized": this_finalized,
                "has_pending": has_pending_after_voice,
            }),
        );
        has_pending_after_voice
    };

    PostLoopFinalizeOutput {
        full_response,
        active_background_child_session_ids,
        pending_long_running_open_after_state_save,
        pending_long_running_retarget_after_state_save,
        long_running_placeholder_active,
        inflight_state,
        api_friction_reports,
        claude_tui_followup_pre_submit_requeue_candidate,
        tui_error_classification,
        review_dispatch_warning,
        is_prompt_too_long,
        bridge_relay_delegated_to_watcher,
        bridge_output_owner,
        should_complete_work_dispatch_after_delivery,
        should_fail_dispatch_after_delivery,
        final_session_status,
        can_chain_locally,
        has_queued_turns,
        #[cfg(unix)]
        bridge_tui_gate_outcome_early,
    }
}
