//! #4230 S5 terminal outcome delivery for `turn_bridge::spawn_turn_bridge`.
//!
//! Moved from the post-finalizer terminal delivery block of `spawn_turn_bridge`:
//! cancel/prompt-too-long/recovery/empty-response delivery, bridge terminal
//! delivery lease commits, terminal-controller cutover, voice completion,
//! TUI completion gate, dispatch completion/fail, watcher-delivered mark,
//! `tv_done`, and terminal status readiness.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::output_lifecycle::BridgeOutputOwner;
use super::stream_tick::{
    LongRunningPlaceholderActive, PendingLongRunningOpenAfterStateSave,
    PendingLongRunningRetargetAfterStateSave,
};
use super::*;

pub(super) struct TerminalOutcomeDeliveryContext {
    pub(super) channel_id: ChannelId,
    pub(super) user_msg_id: Option<MessageId>,
    pub(super) current_msg_id: MessageId,
    pub(super) status_panel_msg_id: Option<MessageId>,
    pub(super) cancelled: bool,
    pub(super) transport_error: bool,
    pub(super) recovery_retry: bool,
    pub(super) rx_disconnected: bool,
    pub(super) tmux_last_offset: Option<u64>,
    pub(super) watcher_owner_channel_id: ChannelId,
    pub(super) watcher_handoff_claim_outcome: WatcherHandoffClaimOutcome,
    pub(super) bridge_created_response_placeholder_msg_id: Option<MessageId>,
    pub(super) bridge_relay_delegated_to_watcher: bool,
    pub(super) bridge_output_owner: Option<BridgeOutputOwner>,
    pub(super) should_complete_work_dispatch_after_delivery: bool,
    pub(super) should_fail_dispatch_after_delivery: bool,
    pub(super) can_chain_locally: bool,
    pub(super) single_message_panel_footer_mode: bool,
    pub(super) is_prompt_too_long: bool,
    pub(super) claude_tui_followup_pre_submit_requeue_candidate: bool,
    pub(super) had_prior_session_id_at_turn_start: bool,
    pub(super) session_handshake_seen: bool,
    pub(super) turn_start: std::time::Instant,
    #[cfg(unix)]
    pub(super) bridge_tui_gate_outcome_early: Option<super::super::tmux::TuiCompletionGateOutcome>,
}

pub(super) struct TerminalOutcomeDeliveryState {
    pub(super) shared_owned: Arc<SharedData>,
    pub(super) gateway: Arc<dyn TurnGateway>,
    pub(super) provider: ProviderKind,
    pub(super) cancel_token: Arc<crate::services::provider::CancelToken>,
    pub(super) turn_id: String,
    pub(super) user_text_owned: String,
    pub(super) adk_session_key: Option<String>,
    pub(super) adk_cwd: Option<String>,
    pub(super) dispatch_id: Option<String>,
    pub(super) new_session_id: Option<String>,
    pub(super) new_raw_provider_session_id: Option<String>,
    pub(super) full_response: String,
    pub(super) active_background_child_session_ids: Vec<i64>,
    pub(super) pending_long_running_open_after_state_save: PendingLongRunningOpenAfterStateSave,
    pub(super) pending_long_running_retarget_after_state_save:
        PendingLongRunningRetargetAfterStateSave,
    pub(super) long_running_placeholder_active: LongRunningPlaceholderActive,
    pub(super) inflight_state: InflightTurnState,
    pub(super) api_friction_reports: Vec<crate::services::api_friction::ApiFrictionReport>,
    pub(super) review_dispatch_warning: Option<String>,
    pub(super) last_edit_text: String,
    pub(super) terminal_empty_response_notice: Option<String>,
    pub(super) terminal_full_replay_cleanup_msg_ids: Vec<MessageId>,
    pub(super) resume_failure_detected: bool,
    pub(super) response_sent_offset: usize,
}

pub(super) enum TerminalOutcomeDeliveryOutcome {
    Completed,
}

pub(super) struct TerminalOutcomeDeliveryOutput {
    pub(super) outcome: TerminalOutcomeDeliveryOutcome,
    pub(super) shared_owned: Arc<SharedData>,
    pub(super) gateway: Arc<dyn TurnGateway>,
    pub(super) provider: ProviderKind,
    pub(super) cancel_token: Arc<crate::services::provider::CancelToken>,
    pub(super) turn_id: String,
    pub(super) user_text_owned: String,
    pub(super) adk_session_key: Option<String>,
    pub(super) adk_cwd: Option<String>,
    pub(super) dispatch_id: Option<String>,
    pub(super) new_session_id: Option<String>,
    pub(super) new_raw_provider_session_id: Option<String>,
    pub(super) full_response: String,
    pub(super) active_background_child_session_ids: Vec<i64>,
    pub(super) pending_long_running_open_after_state_save: PendingLongRunningOpenAfterStateSave,
    pub(super) pending_long_running_retarget_after_state_save:
        PendingLongRunningRetargetAfterStateSave,
    pub(super) long_running_placeholder_active: LongRunningPlaceholderActive,
    pub(super) inflight_state: InflightTurnState,
    pub(super) api_friction_reports: Vec<crate::services::api_friction::ApiFrictionReport>,
    pub(super) status_panel_terminal_committed: bool,
    pub(super) bridge_should_emit_completion: bool,
    pub(super) completion_footer_terminal_text: Option<String>,
    pub(super) preserve_inflight_for_cleanup_retry: bool,
    pub(super) bridge_skip_holder_owns_inflight: bool,
    pub(super) terminal_delivery_committed: bool,
    pub(super) resume_failure_detected: bool,
    pub(super) terminal_empty_response_notice: Option<String>,
    pub(super) terminal_full_replay_cleanup_msg_ids: Vec<MessageId>,
    pub(super) response_sent_offset: usize,
    pub(super) turn_start: std::time::Instant,
}

pub(super) async fn run_terminal_outcome_delivery(
    ctx: TerminalOutcomeDeliveryContext,
    state: TerminalOutcomeDeliveryState,
) -> TerminalOutcomeDeliveryOutput {
    let channel_id = ctx.channel_id;
    let user_msg_id = ctx.user_msg_id;
    let current_msg_id = ctx.current_msg_id;
    let status_panel_msg_id = ctx.status_panel_msg_id;
    let cancelled = ctx.cancelled;
    let transport_error = ctx.transport_error;
    let recovery_retry = ctx.recovery_retry;
    let rx_disconnected = ctx.rx_disconnected;
    let tmux_last_offset = ctx.tmux_last_offset;
    let watcher_owner_channel_id = ctx.watcher_owner_channel_id;
    let watcher_handoff_claim_outcome = ctx.watcher_handoff_claim_outcome;
    let bridge_created_response_placeholder_msg_id = ctx.bridge_created_response_placeholder_msg_id;
    let bridge_relay_delegated_to_watcher = ctx.bridge_relay_delegated_to_watcher;
    let bridge_output_owner = ctx.bridge_output_owner;
    let should_complete_work_dispatch_after_delivery =
        ctx.should_complete_work_dispatch_after_delivery;
    let should_fail_dispatch_after_delivery = ctx.should_fail_dispatch_after_delivery;
    let can_chain_locally = ctx.can_chain_locally;
    let single_message_panel_footer_mode = ctx.single_message_panel_footer_mode;
    let is_prompt_too_long = ctx.is_prompt_too_long;
    let claude_tui_followup_pre_submit_requeue_candidate =
        ctx.claude_tui_followup_pre_submit_requeue_candidate;
    let had_prior_session_id_at_turn_start = ctx.had_prior_session_id_at_turn_start;
    let session_handshake_seen = ctx.session_handshake_seen;
    let turn_start = ctx.turn_start;
    #[cfg(unix)]
    let bridge_tui_gate_outcome_early = ctx.bridge_tui_gate_outcome_early;

    let shared_owned = state.shared_owned;
    let gateway = state.gateway;
    let provider = state.provider;
    let cancel_token = state.cancel_token;
    let turn_id = state.turn_id;
    let user_text_owned = state.user_text_owned;
    let adk_session_key = state.adk_session_key;
    let adk_cwd = state.adk_cwd;
    let dispatch_id = state.dispatch_id;
    let mut new_session_id = state.new_session_id;
    let mut new_raw_provider_session_id = state.new_raw_provider_session_id;
    let mut full_response = state.full_response;
    let mut active_background_child_session_ids = state.active_background_child_session_ids;
    let mut pending_long_running_open_after_state_save =
        state.pending_long_running_open_after_state_save;
    let mut pending_long_running_retarget_after_state_save =
        state.pending_long_running_retarget_after_state_save;
    let mut long_running_placeholder_active = state.long_running_placeholder_active;
    let mut inflight_state = state.inflight_state;
    let mut api_friction_reports = state.api_friction_reports;
    let review_dispatch_warning = state.review_dispatch_warning;
    let last_edit_text = state.last_edit_text;
    let mut terminal_empty_response_notice = state.terminal_empty_response_notice;
    let mut terminal_full_replay_cleanup_msg_ids = state.terminal_full_replay_cleanup_msg_ids;
    let mut resume_failure_detected = state.resume_failure_detected;
    let mut response_sent_offset = state.response_sent_offset;

    let mut preserve_inflight_for_cleanup_retry = false;
    // #3041 P1-2 (codex P1-2 R3): set ONLY on a delivery-lease `Skip`, where
    // the live HOLDER (the watcher) — a different actor sharing the same
    // per-channel `DeliveryLeaseCell` — owns this turn's delivery AND its
    // inflight lifecycle (the watcher CLEARS inflight on its own success).
    // Distinct from the bridge-owned `preserve_inflight_for_cleanup_retry`
    // sites (EditFailed, PG-cancel-fail, replace-not-committed, send/enqueue
    // failure, TUI quiescence timeout) where the BRIDGE still owns the row
    // and its epilogue `save_inflight_state` is load-bearing. On a Skip the
    // bridge must NOT blindly rewrite inflight: the holder may have already
    // cleared it on success, and a blind re-save would resurrect a STALE
    // inflight row for an already-delivered turn (recovery sees it as
    // delivered and returns without clearing → permanent stale leak). The
    // epilogue's save is therefore made IDENTITY-GUARDED on a Skip
    // (`save_inflight_state_if_identity_unchanged`): it only rewrites if the
    // on-disk row STILL matches this turn's identity, so a watcher-clear
    // (file gone) or a newer turn (identity mismatch) no-ops instead of
    // resurrecting. When the holder FAILS (does not clear), the row is still
    // present + matching, so the bridge refreshes it and retry survives.
    let mut bridge_skip_holder_owns_inflight = false;
    let (mut terminal_delivery_committed, mut terminal_body_visible) = (false, false);
    let mut status_panel_terminal_committed = false;
    let mut completion_footer_terminal_text: Option<String> = None;
    // #2161 (Codex round-2 H1): hoisted into the outer scope so the
    // bridge can run the TUI completion gate BEFORE dispatch completion
    // and reuse the same outcome for the visible status-panel emit
    // below. Default is "emit" for paths that don't reach the gate
    // (e.g. cancelled, prompt_too_long, transport_error) and for
    // non-unix targets where the tmux module is configured out.
    #[allow(unused_mut)]
    let mut bridge_should_emit_completion = true;
    let inflight_generation = inflight_state.born_generation;

    if !bridge_output_owner
        .map(|owner| owner.skips_bridge_spinner_cleanup())
        .unwrap_or(false)
        && let Some(user_msg_id) = user_msg_id
    {
        tv_clear(
            &shared_owned,
            channel_id,
            user_msg_id,
            inflight_generation,
            "clear",
        )
        .await;
    }

    // Recovery auto-retry: session died during restart recovery
    if recovery_retry {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ↻ Recovery session died — triggering auto-retry with history (channel {})",
            channel_id
        );
        reset_session_for_auto_retry(
            &shared_owned,
            channel_id,
            &cancel_token,
            adk_session_key.as_deref(),
            &mut new_session_id,
            &mut new_raw_provider_session_id,
            &mut inflight_state,
            "recovery session died",
        )
        .await;
        // #2452 H6: schedule the auto-retry via the explicit
        // completion path so the dedup lockout is released as soon
        // as scheduling resolves (≤ 120s safety net inside helper).
        // A recovery turn with no anchored user message (user_msg_id == 0)
        // has no message to retry-with-history against, so skip scheduling.
        if let Some(user_msg_id) = user_msg_id {
            spawn_retry_with_history_with_release(
                gateway.clone(),
                channel_id,
                user_msg_id,
                user_text_owned.clone(),
            );
        }
        // Replace placeholder with recovery notice (don't delete — avoids visual gap)
        let _ = gateway
            .edit_message(
                channel_id,
                current_msg_id,
                "↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다.",
            )
            .await;
        full_response = String::new();
    }

    if cancelled {
        close_all_tracked_background_children(
            shared_owned.pg_pool.as_ref(),
            &mut active_background_child_session_ids,
            "aborted",
            "cancel cleanup",
        )
        .await;
        if pending_long_running_open_after_state_save.take().is_some() {
            inflight_state.long_running_placeholder_active = false;
            let _ = crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
                &inflight_state,
                "turn_bridge::cancel_longrun_open_after_state_save@4500",
            );
        }
        // #1255: cancelled turn → drive any active long-running placeholder
        // into Aborted before the rest of the cleanup machinery runs. The
        // controller's idempotent terminal transition guarantees this is
        // safe even if the ToolResult event already fired Completed.
        // #2289 (Codex review): mirror the Done/stream-end outcome handling
        // — only clear the persisted flag when the transition actually
        // committed (Edited / Coalesced / AlreadyTerminal). On
        // `EditFailed`, leave the controller entry Active and the
        // persisted flag set so a later retry path or the placeholder
        // sweeper can finish the teardown. Without this, dropping a
        // raced `Done` here would silently leak a non-evictable Active
        // controller row and clobber the sweeper's repair signal.
        if let Some((key, snapshot, close_trigger, ack_consumed)) =
            long_running_placeholder_active.take()
        {
            let pending_retarget_matches_key = pending_long_running_retarget_after_state_save
                .as_ref()
                .is_some_and(|(pending_key, _, _, _, _)| *pending_key == key);
            if pending_retarget_matches_key {
                let _ = pending_long_running_retarget_after_state_save.take();
                shared_owned.ui.placeholder_controller.detach(&key);
                inflight_state.long_running_placeholder_active = false;
                let _ =
                    crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
                        &inflight_state,
                        "turn_bridge::cancel_longrun_retarget_detach@4524",
                    );
            } else {
                let outcome = shared_owned
                    .ui
                    .placeholder_controller
                    .transition(
                        gateway.as_ref(),
                        key.clone(),
                        super::super::placeholder_controller::PlaceholderLifecycle::Aborted,
                    )
                    .await;
                use super::super::placeholder_controller::PlaceholderControllerOutcome::*;
                if matches!(outcome, Edited | Coalesced | AlreadyTerminal) {
                    inflight_state.long_running_placeholder_active = false;
                    let _ = crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
                        &inflight_state,
                        "turn_bridge::cancel_longrun_placeholder_abort_committed@4537",
                    );
                } else {
                    // EditFailed (or any non-committed outcome): leave the
                    // persisted flag set AND preserve the inflight file for
                    // cleanup retry. The placeholder sweeper relies on the
                    // inflight file existing to discover the stuck row; if
                    // we let the normal cancel cleanup delete it, the
                    // sweeper would lose its repair signal and the
                    // controller entry would stay Active forever. Force
                    // the inflight to be preserved so the next sweeper
                    // pass can finish the teardown.
                    let _ = (key, snapshot, close_trigger, ack_consumed);
                    let _ = crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
                        &inflight_state,
                        "turn_bridge::cancel_longrun_placeholder_abort_edit_failed@4549",
                    );
                    preserve_inflight_for_cleanup_retry = true;
                }
            }
        }

        let cleanup_policy = match cancel_token.restart_mode() {
            Some(restart_mode) => TmuxCleanupPolicy::PreserveSessionAndInflight { restart_mode },
            None => TmuxCleanupPolicy::PreserveSession,
        };
        // #3169 (death #3): the `None`-cancel_source fallback uses the
        // shared `ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON` sentinel so the
        // tmux_runtime SIGINT guard recognises this anonymous internal
        // teardown and suppresses claude's session-killing teardown SIGINT.
        let cancel_source = cancel_token
            .cancel_source()
            .unwrap_or_else(|| tmux_runtime::ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON.to_string());
        stop_active_turn(&provider, &cancel_token, cleanup_policy, &cancel_source).await;

        if let Some(dispatch_id) = dispatch_id.as_deref() {
            if let Some(pg_pool) = shared_owned.pg_pool.as_ref() {
                if let Err(error) = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
                    pg_pool,
                    dispatch_id,
                    Some(cancel_source.as_str()),
                )
                .await
                {
                    // #2044 F8: when the PG cancel fails, the
                    // dispatch row stays in its previous (possibly
                    // "running") state while our local cleanup
                    // proceeds. Without setting
                    // `preserve_inflight_for_cleanup_retry`, the
                    // inflight file would also be deleted, so a
                    // subsequent dispatch_followup could re-use
                    // the dispatch id thinking the turn is still
                    // healthy — producing a dispatch-state ↔
                    // inflight-state inconsistency. Set the retry
                    // flag so the next cleanup pass re-attempts
                    // the cancel, and emit a structured tracing
                    // event so ops can alarm on
                    // `dispatch_cancel_pg_failed`.
                    tracing::warn!(
                        event = "dispatch_cancel_pg_failed",
                        dispatch_id = %dispatch_id,
                        channel_id = channel_id.get(),
                        cancel_source = %cancel_source,
                        error = %error,
                        "[turn_bridge] failed to cancel dispatch in postgres; preserving inflight for cleanup retry",
                    );
                    preserve_inflight_for_cleanup_retry = true;
                }
            }
        }

        let preserved_restart_mode = cancel_token.restart_mode();
        let remaining_response =
            response_portion_after_offset(&full_response, response_sent_offset);
        let terminal_response = if let Some(restart_mode) = preserved_restart_mode {
            handoff_interrupted_message(restart_mode, remaining_response)
        } else if remaining_response.trim().is_empty() {
            "[Stopped]".to_string()
        } else {
            let formatted = if shared_owned.ui.status_panel_v2_enabled {
                super::super::formatting::format_for_discord_with_status_panel(
                    remaining_response,
                    &provider,
                )
            } else {
                super::super::formatting::format_for_discord_with_provider(
                    remaining_response,
                    &provider,
                )
            };
            format!("{}\n\n[Stopped]", formatted)
        };

        // #3041 P1-2 (site 1 — cancel/stop terminal replace): acquire the
        // shared delivery lease BEFORE delivering the `[Stopped]` body; a B2
        // Skip means the holder owns this turn/range → do NOT deliver+advance.
        // (codex P1-a) lease on `watcher_owner_channel_id` — the cell + TurnKey
        // channel the WATCHER uses (a reused watcher can own a channel != this
        // bridge's `channel_id`), so the two CONTEND on one cell (single-holder
        // B2) instead of both delivering = duplicate.
        let stop_lease_acquire = bridge_delivery_lease_for_inflight(
            shared_owned.as_ref(),
            watcher_owner_channel_id,
            shared_owned.restart.current_generation,
            &inflight_state,
            tmux_last_offset,
        );
        if matches!(stop_lease_acquire, BridgeLeaseAcquire::Skip) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                channel = channel_id.get(),
                "  [{ts}] 🌉 #3041 B2: delivery lease held by another holder — bridge skipped duplicate cancel/stop terminal replace (channel {})",
                channel_id
            );
            // #3041 P1-2 (codex P1-c): a B2 Skip means the holder (the watcher)
            // owns this range — the bridge is a TRUE no-op on completion
            // side-effects. PRESERVE inflight so the epilogue does NOT clear it
            // (~9017) / mark `watcher.turn_delivered` (~8356); a still-retry-able
            // turn is re-delivered by ACK-poll if the holder ultimately fails.
            // (codex P1-2 R3) the holder owns the inflight lifecycle → make the
            // epilogue save identity-guarded so it never resurrects a cleared row.
            preserve_inflight_for_cleanup_retry = true;
            bridge_skip_holder_owns_inflight = true;
        } else {
            let stop_lease = match stop_lease_acquire {
                BridgeLeaseAcquire::Held(lease) => Some(lease),
                _ => None,
            };
            let replace_committed = turn_bridge_replace_outcome_committed(
                shared_owned.as_ref(),
                &provider,
                channel_id,
                current_msg_id,
                inflight_state.tmux_session_name.as_deref(),
                gateway
                    .replace_message_with_outcome(channel_id, current_msg_id, &terminal_response)
                    .await,
                dispatch_id.as_deref(),
                adk_session_key.as_deref(),
                Some(turn_id.as_str()),
                "turn_bridge_cancelled_terminal_replace",
            );
            if replace_committed {
                status_panel_terminal_committed = true;
            }
            // B6: the ONLY confirmed_end advance is via a successful lease
            // commit. `Held` → commit (Delivered advances, NotDelivered not).
            // `NoRange` has NO new bytes → no advance outside a lease (codex
            // P1-b: a degenerate equal-nonzero range must not advance).
            if let Some(lease) = stop_lease {
                let lease_range = lease.range();
                let outcome = if replace_committed {
                    crate::services::discord::LeaseOutcome::Delivered
                } else {
                    crate::services::discord::LeaseOutcome::NotDelivered
                };
                let committed = lease.commit_and_advance(
                    shared_owned.as_ref(),
                    watcher_owner_channel_id,
                    inflight_state.tmux_session_name.as_deref(),
                    outcome,
                );
                if replace_committed && committed {
                    terminal_delivery::record_stopped_turn_terminal_replace_delivery(
                        shared_owned.as_ref(),
                        &provider,
                        watcher_owner_channel_id,
                        lease_range,
                        current_msg_id,
                        channel_id,
                        remaining_response,
                    );
                }
            }
            if !replace_committed {
                preserve_inflight_for_cleanup_retry = true;
            }
        }

        if preserved_restart_mode.is_none()
            && let Some(user_msg_id) = user_msg_id
        {
            tv_stop(
                &shared_owned,
                channel_id,
                user_msg_id,
                inflight_generation,
                "stop",
            )
            .await;
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] ■ Stopped");
    } else if is_prompt_too_long {
        let mention = gateway.requester_mention().unwrap_or_default();
        full_response = format!(
            "{} ⚠️ 프롬프트가 너무 깁니다. 대화 컨텍스트가 모델 한도를 초과했습니다.\n\n\
             다음 메시지를 보내면 자동으로 새 턴이 시작됩니다.\n\
             컨텍스트를 줄이려면 `/compact` 또는 `/clear`를 사용해 주세요.",
            mention
        );
        // #3041 P1-2 (site 2 — prompt-too-long terminal replace): same lease
        // routing as site 1 — acquire before replace; B2-skip if held. (codex
        // P1-a) lease on `watcher_owner_channel_id` (shared cell + TurnKey).
        let plt_lease_acquire = bridge_delivery_lease_for_inflight(
            shared_owned.as_ref(),
            watcher_owner_channel_id,
            shared_owned.restart.current_generation,
            &inflight_state,
            tmux_last_offset,
        );
        if matches!(plt_lease_acquire, BridgeLeaseAcquire::Skip) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                channel = channel_id.get(),
                "  [{ts}] 🌉 #3041 B2: delivery lease held by another holder — bridge skipped duplicate prompt-too-long terminal replace (channel {})",
                channel_id
            );
            // #3041 P1-2 (codex P1-c): preserve retry on a B2 Skip — holder owns
            // delivery; do NOT clear inflight / mark the watcher delivered.
            // (codex P1-2 R3) holder owns the inflight lifecycle on a Skip.
            preserve_inflight_for_cleanup_retry = true;
            bridge_skip_holder_owns_inflight = true;
        } else {
            let plt_lease = match plt_lease_acquire {
                BridgeLeaseAcquire::Held(lease) => Some(lease),
                _ => None,
            };
            let replace_committed = turn_bridge_replace_outcome_committed(
                shared_owned.as_ref(),
                &provider,
                channel_id,
                current_msg_id,
                inflight_state.tmux_session_name.as_deref(),
                gateway
                    .replace_message_with_outcome(channel_id, current_msg_id, &full_response)
                    .await,
                dispatch_id.as_deref(),
                adk_session_key.as_deref(),
                Some(turn_id.as_str()),
                "turn_bridge_prompt_too_long_replace",
            );
            if replace_committed {
                status_panel_terminal_committed = true;
            }
            // B6 (codex P1-b): advance ONLY via a successful lease commit.
            // NoRange has no new bytes → no advance outside the lease.
            if let Some(lease) = plt_lease {
                let lease_range = lease.range();
                let outcome = if replace_committed {
                    crate::services::discord::LeaseOutcome::Delivered
                } else {
                    crate::services::discord::LeaseOutcome::NotDelivered
                };
                let committed = lease.commit_and_advance(
                    shared_owned.as_ref(),
                    watcher_owner_channel_id,
                    inflight_state.tmux_session_name.as_deref(),
                    outcome,
                );
                if replace_committed && committed {
                    super::super::outbound::delivery_record::record_delivered_frontier_with_body(
                        shared_owned.as_ref(),
                        &provider,
                        watcher_owner_channel_id,
                        lease_range,
                        current_msg_id.get(),
                        channel_id.get(),
                        &full_response,
                    );
                }
            }
            if !replace_committed {
                preserve_inflight_for_cleanup_retry = true;
            }
        }

        if let Some(user_msg_id) = user_msg_id {
            tv_fail(
                &shared_owned,
                channel_id,
                user_msg_id,
                inflight_generation,
                "fail",
            )
            .await;
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] ⚠ Prompt too long (channel {})", channel_id);
    } else if let Some(owner) = bridge_output_owner {
        let ts = chrono::Local::now().format("%H:%M:%S");
        match owner {
            BridgeOutputOwner::WatcherRelay => {
                tracing::info!(
                    "  [{ts}] 👁 tmux watcher owns assistant relay; bridge skipped direct response delivery (channel {})",
                    channel_id
                );
                if should_delete_bridge_created_watcher_orphan_response(
                    shared_owned.ui.status_panel_v2_enabled,
                    watcher_handoff_claim_outcome,
                    bridge_created_response_placeholder_msg_id,
                    current_msg_id,
                ) {
                    // #3607: terminal-anchor guard + durable delete
                    // observability live in the sibling so the hot file only
                    // dispatches. The guard skips deleting a committed
                    // terminal anchor (the accident this fixes); a genuine
                    // non-terminal orphan is deleted, recorded, and retried.
                    cleanup_or_preserve_watcher_orphan_spinner(
                        shared_owned.clone(),
                        &provider,
                        gateway.clone(),
                        channel_id,
                        current_msg_id,
                        &inflight_state,
                    )
                    .await;
                }
            }
            BridgeOutputOwner::StandbyRelay => tracing::info!(
                "  [{ts}] 👁 standby relay owns assistant relay; bridge skipped direct response delivery (channel {})",
                channel_id
            ),
        }
    } else {
        // Check for stale resume failure BEFORE any other response handling.
        // This path is driven by explicit error/result events, not assistant text.
        if resume_failure_detected {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Resume failed (error in response), clearing session_id (channel {})",
                channel_id
            );
            reset_session_for_auto_retry(
                &shared_owned,
                channel_id,
                &cancel_token,
                adk_session_key.as_deref(),
                &mut new_session_id,
                &mut new_raw_provider_session_id,
                &mut inflight_state,
                "resume failed in response output",
            )
            .await;
            // #2452 H6: explicit completion path — see helper docs.
            // Skip retry-with-history when the recovery turn has no anchored
            // user message (user_msg_id == 0).
            if let Some(user_msg_id) = user_msg_id {
                spawn_retry_with_history_with_release(
                    gateway.clone(),
                    channel_id,
                    user_msg_id,
                    user_text_owned.clone(),
                );
            }
            full_response = String::new(); // Suppress error message to user
        } else if full_response.is_empty() {
            // #2451 H5 graduation: the authoritative resume-failure
            // witness is the absence of `StreamMessage::Init` after a
            // turn that attempted resume. `attempted_resume` is the
            // turn-start snapshot of the provider session_id (taken
            // before any reset_session_for_auto_retry side effect),
            // and `session_handshake_seen` is flipped inside the
            // `Init` handler. The old `quick_exit < 10s` test is kept
            // as a 30s safety backstop for providers whose `Init`
            // emission is unreliable (e.g. gemini may not emit Init
            // on resume success).
            let attempted_resume = had_prior_session_id_at_turn_start;
            let resume_likely_failed_by_handshake =
                attempted_resume && !session_handshake_seen && rx_disconnected;
            // Backstop only — wider threshold to keep false positives
            // away from healthy fast turns.
            let quick_exit_backstop = turn_start.elapsed().as_secs() < 30;
            let quick_empty_resume = resume_likely_failed_by_handshake
                || (quick_exit_backstop && rx_disconnected && attempted_resume);
            // Fallback: try to extract response from tmux output file
            if quick_empty_resume {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⏭ Skipping output file recovery after quick empty resume exit (channel {})",
                    channel_id
                );
            } else if let Some(ref path) = inflight_state.output_path {
                let recovered = super::super::recovery::extract_response_from_output_pub(
                    path,
                    inflight_state.last_offset,
                );
                if !recovered.trim().is_empty() {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ↻ Recovered {} chars from output file for channel {}",
                        recovered.len(),
                        channel_id
                    );
                    full_response = recovered;
                }
            }

            // Check for stale resume failure in recovered output
            let stale_resume_in_output = inflight_state
                .output_path
                .as_deref()
                .map(|path| {
                    output_file_has_stale_resume_error_after_offset(
                        path,
                        inflight_state.last_offset,
                    )
                })
                .unwrap_or(false);
            if stale_resume_in_output {
                resume_failure_detected = true;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ Resume failed (stale session_id in recovered output), auto-retrying (channel {})",
                    channel_id
                );
                reset_session_for_auto_retry(
                    &shared_owned,
                    channel_id,
                    &cancel_token,
                    adk_session_key.as_deref(),
                    &mut new_session_id,
                    &mut new_raw_provider_session_id,
                    &mut inflight_state,
                    "stale session_id in recovered output",
                )
                .await;
                // #2452 H6: explicit completion path — see helper docs.
                if let Some(user_msg_id) = user_msg_id {
                    spawn_retry_with_history_with_release(
                        gateway.clone(),
                        channel_id,
                        user_msg_id,
                        user_text_owned.clone(),
                    );
                }
                full_response = String::new();
            } else {
                // Check for resume failure via other methods
                let mut resume_failed = false;
                // Method 1: check tmux output file
                if let Some(ref path) = inflight_state.output_path
                    && output_file_has_stale_resume_error_after_offset(
                        path,
                        inflight_state.last_offset,
                    )
                {
                    resume_failed = true;
                    resume_failure_detected = true;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Resume failed (stale session_id in output file), auto-retrying (channel {})",
                        channel_id
                    );
                    reset_session_for_auto_retry(
                        &shared_owned,
                        channel_id,
                        &cancel_token,
                        adk_session_key.as_deref(),
                        &mut new_session_id,
                        &mut new_raw_provider_session_id,
                        &mut inflight_state,
                        "stale session_id in output file",
                    )
                    .await;
                    // #2452 H6: explicit completion path — see helper.
                    if let Some(user_msg_id) = user_msg_id {
                        spawn_retry_with_history_with_release(
                            gateway.clone(),
                            channel_id,
                            user_msg_id,
                            user_text_owned.clone(),
                        );
                    }
                    full_response = String::new();
                }
                // #2451 H5 Method 2: authoritative resume-failure
                // classification via the explicit `Init` handshake
                // witness. The legacy `quick_exit < 10s` test now
                // serves only as the 30s safety backstop above. If
                // `attempted_resume` was true AND we never saw `Init`
                // AND rx disconnected, the provider almost certainly
                // failed to bind the prior session_id. The original
                // `core.sessions` re-fetch is replaced by the
                // turn-start snapshot so the recheck cannot race a
                // prior reset_session_for_auto_retry.
                if !resume_failed
                    && rx_disconnected
                    && attempted_resume
                    && (!session_handshake_seen || quick_exit_backstop)
                {
                    {
                        resume_failed = true;
                        resume_failure_detected = true;
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ Empty response with no Init handshake (session_handshake_seen={}, elapsed={}s) — auto-retrying with fresh session (channel {})",
                            session_handshake_seen,
                            turn_start.elapsed().as_secs(),
                            channel_id
                        );
                        reset_session_for_auto_retry(
                            &shared_owned,
                            channel_id,
                            &cancel_token,
                            adk_session_key.as_deref(),
                            &mut new_session_id,
                            &mut new_raw_provider_session_id,
                            &mut inflight_state,
                            "quick exit with empty response",
                        )
                        .await;
                        // #2452 H6: explicit completion path.
                        if let Some(user_msg_id) = user_msg_id {
                            spawn_retry_with_history_with_release(
                                gateway.clone(),
                                channel_id,
                                user_msg_id,
                                user_text_owned.clone(),
                            );
                        }
                        full_response = String::new();
                    }
                }
                if !resume_failed {
                    if rx_disconnected {
                        terminal_empty_response_notice =
                            Some("(No response — 프로세스가 응답 없이 종료됨)".to_string());
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ Empty response: rx disconnected before any text \
                             (channel {}, output_path={:?}, last_offset={})",
                            channel_id,
                            inflight_state.output_path,
                            inflight_state.last_offset
                        );
                    } else {
                        terminal_empty_response_notice = Some("(No response)".to_string());
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ Empty response: done without text (channel {})",
                            channel_id
                        );
                    }
                }
            }
        }

        let late_api_friction =
            crate::services::api_friction::extract_api_friction_reports(&full_response);
        if !late_api_friction.reports.is_empty() {
            api_friction_reports.extend(late_api_friction.reports);
            full_response = late_api_friction.cleaned_response;
            sync_response_delivery_state(
                &full_response,
                &mut response_sent_offset,
                &mut inflight_state,
            );
        }
        for error in late_api_friction.parse_errors {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ invalid API_FRICTION marker: {error}");
        }

        let resume_retry_queued =
            (recovery_retry || resume_failure_detected) && user_msg_id.is_some();
        let mut delivery_response = terminal_delivery_response_after_offset(
            &full_response,
            response_sent_offset,
            terminal_empty_response_notice.as_deref(),
        );
        if let Some(warning) = review_dispatch_warning.as_deref() {
            let warning = warning.trim();
            if !warning.is_empty() {
                if !delivery_response.trim().is_empty() {
                    delivery_response.push_str("\n\n");
                }
                delivery_response.push_str(warning);
            }
        }
        let spoken_delivery_response = delivery_response.clone();

        // Headless silent trigger (metadata.silent=true): suppress assistant
        // text delivery entirely. Lifecycle/error/cancel notifications still
        // flow through their own paths.
        if inflight_state.silent_turn {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🤫 turn_bridge: silent_turn suppressed terminal delivery for channel {} ({} chars)",
                channel_id,
                delivery_response.len()
            );
            terminal_body_visible = true;
            // #3041 P1-2 (site 3 — silent_turn suppression): no Discord post,
            // but the offset STILL advances so the suppressed range is marked
            // consumed (not re-delivered by recovery). Per B6 the advance flows
            // through a lease commit: acquire→commit(Delivered)→release (the
            // bridge OWNS this range; instantaneous "send" → heartbeat formality).
            // (codex P1-a) lease on `watcher_owner_channel_id` (shared cell +
            // TurnKey channel as the watcher). (codex P1-c)
            // `terminal_delivery_committed` is set ONLY when THIS actor resolves
            // the range (`Held`→commit / `NoRange`); on `Skip` the watcher owns
            // delivery → NO-OP on completion side-effects, leave the turn retry-able.
            let lease_acquire = bridge_delivery_lease_for_inflight(
                shared_owned.as_ref(),
                watcher_owner_channel_id,
                shared_owned.restart.current_generation,
                &inflight_state,
                tmux_last_offset,
            );
            // (codex P1-c) one source of truth for "does this acquire outcome
            // mark the silent turn committed": Skip → false (holder owns it,
            // stay retry-able), Held/NoRange → true.
            terminal_delivery_committed = silent_turn_skip_marks_committed(&lease_acquire);
            match lease_acquire {
                BridgeLeaseAcquire::Held(lease) => {
                    lease.commit_and_advance(
                        shared_owned.as_ref(),
                        watcher_owner_channel_id,
                        inflight_state.tmux_session_name.as_deref(),
                        crate::services::discord::LeaseOutcome::Delivered,
                    );
                }
                BridgeLeaseAcquire::Skip => {
                    // B2-skip: the watcher holds the live lease and owns this
                    // range's delivery (codex P1-c). `terminal_delivery_committed
                    // = false` alone is NOT enough — the epilogue still marks
                    // `watcher.turn_delivered` (~8356) and CLEARS inflight (~9017)
                    // unless `preserve_inflight_for_cleanup_retry` is set; set it
                    // so a Skip is a TRUE no-op and the holder's eventual
                    // NotDelivered/Unknown stays re-deliverable.
                    preserve_inflight_for_cleanup_retry = true;
                    // codex P1-2 R3: holder owns the inflight lifecycle on a
                    // Skip — identity-guard the epilogue save (no resurrect).
                    bridge_skip_holder_owns_inflight = true;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        channel = channel_id.get(),
                        "  [{ts}] 🌉 #3041 B2: delivery lease held by another holder — bridge silent_turn skipped offset advance, left turn retry-able (channel {})",
                        channel_id
                    );
                }
                BridgeLeaseAcquire::NoRange => {
                    // No offset to advance (zero/inverted range): the suppression
                    // resolves the (empty) range. B6 holds (no advance).
                }
            }
        } else if delivery_response.trim().is_empty() {
            if empty_sink_commits_fully_consumed_response(&full_response, response_sent_offset) {
                (terminal_delivery_committed, terminal_body_visible) = (true, true);
            } else if empty_sink_preserves_retry(
                &full_response,
                resume_retry_queued,
                response_sent_offset,
                channel_id,
            ) {
                preserve_inflight_for_cleanup_retry = true;
            } else if TurnGateway::edit_message(
                gateway.as_ref(),
                channel_id,
                current_msg_id,
                "↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다.",
            )
            .await
            .is_ok()
            {
                terminal_delivery_committed = true;
                terminal_body_visible = true;
            }
        } else {
            delivery_response = if shared_owned.ui.status_panel_v2_enabled {
                super::super::formatting::format_for_discord_with_status_panel(
                    &delivery_response,
                    &provider,
                )
            } else {
                super::super::formatting::format_for_discord_with_provider(
                    &delivery_response,
                    &provider,
                )
            };
            if can_chain_locally {
                if terminal_delivery_should_send_new_chunks(can_chain_locally, &delivery_response) {
                    let bridge_start = inflight_state.turn_start_offset.unwrap_or(0);
                    let bridge_end = tmux_last_offset.unwrap_or(0);
                    if terminal_controller_cutover::bridge_long_chunks_cutover_decision(
                        can_chain_locally,
                        &delivery_response,
                        bridge_end > bridge_start,
                        true,
                    ) {
                        let bridge_turn = super::super::turn_finalizer::TurnKey::new(
                            watcher_owner_channel_id,
                            inflight_state.user_msg_id,
                            shared_owned.restart.current_generation,
                        );
                        let bridge_lease_key = bridge_delivery_lease_key_for_inflight(
                            watcher_owner_channel_id,
                            shared_owned.restart.current_generation,
                            &inflight_state,
                        );
                        let cell = shared_owned.delivery_lease(watcher_owner_channel_id);
                        terminal_controller_cutover::apply_bridge_long_chunks_controller(
                            gateway.as_ref(),
                            shared_owned.as_ref(),
                            &provider,
                            channel_id,
                            watcher_owner_channel_id,
                            inflight_state.tmux_session_name.as_deref(),
                            &cell,
                            &shared_owned.ui.placeholder_controller,
                            current_msg_id,
                            &delivery_response,
                            &spoken_delivery_response,
                            full_response.len(),
                            bridge_turn,
                            bridge_start,
                            bridge_end,
                            single_message_panel_footer_mode,
                            dispatch_id.as_deref(),
                            adk_session_key.as_deref(),
                            Some(turn_id.as_str()),
                            Some(bridge_lease_key.clone()),
                            terminal_controller_cutover::BridgeLongChunksLocals {
                                terminal_delivery_committed: &mut terminal_delivery_committed,
                                terminal_body_visible: &mut terminal_body_visible,
                                completion_footer_terminal_text:
                                    &mut completion_footer_terminal_text,
                                preserve_inflight_for_cleanup_retry:
                                    &mut preserve_inflight_for_cleanup_retry,
                                bridge_skip_holder_owns_inflight:
                                    &mut bridge_skip_holder_owns_inflight,
                                response_sent_offset: &mut response_sent_offset,
                                inflight_response_sent_offset: &mut inflight_state
                                    .response_sent_offset,
                            },
                        )
                        .await;
                    } else {
                        let lease_acquire = bridge_delivery_lease_for_inflight(
                            shared_owned.as_ref(),
                            watcher_owner_channel_id,
                            shared_owned.restart.current_generation,
                            &inflight_state,
                            tmux_last_offset,
                        );
                        terminal_controller_cutover::apply_bridge_long_chunks_legacy(
                            lease_acquire,
                            gateway.as_ref(),
                            shared_owned.as_ref(),
                            &provider,
                            channel_id,
                            watcher_owner_channel_id,
                            inflight_state.tmux_session_name.as_deref(),
                            current_msg_id,
                            &delivery_response,
                            &spoken_delivery_response,
                            full_response.len(),
                            single_message_panel_footer_mode,
                            dispatch_id.as_deref(),
                            adk_session_key.as_deref(),
                            Some(turn_id.as_str()),
                            terminal_controller_cutover::BridgeLongChunksLocals {
                                terminal_delivery_committed: &mut terminal_delivery_committed,
                                terminal_body_visible: &mut terminal_body_visible,
                                completion_footer_terminal_text:
                                    &mut completion_footer_terminal_text,
                                preserve_inflight_for_cleanup_retry:
                                    &mut preserve_inflight_for_cleanup_retry,
                                bridge_skip_holder_owns_inflight:
                                    &mut bridge_skip_holder_owns_inflight,
                                response_sent_offset: &mut response_sent_offset,
                                inflight_response_sent_offset: &mut inflight_state
                                    .response_sent_offset,
                            },
                        )
                        .await;
                    }
                } else {
                    // #3089 A5/#3998 S1-f2: route structurally eligible
                    // short-replace through the controller
                    // (`terminal_controller_cutover`).
                    let bridge_start = inflight_state.turn_start_offset.unwrap_or(0);
                    let ordered_range = tmux_last_offset.is_some_and(|e| e > bridge_start);
                    let cutover_short_replace =
                        terminal_controller_cutover::bridge_short_replace_cutover_decision(
                            can_chain_locally,
                            &delivery_response,
                            ordered_range,
                            true,
                        );
                    if cutover_short_replace {
                        let bridge_turn = super::super::turn_finalizer::TurnKey::new(
                            watcher_owner_channel_id,
                            inflight_state.user_msg_id,
                            shared_owned.restart.current_generation,
                        );
                        let bridge_lease_key = bridge_delivery_lease_key_for_inflight(
                            watcher_owner_channel_id,
                            shared_owned.restart.current_generation,
                            &inflight_state,
                        );
                        let cell = shared_owned.delivery_lease(watcher_owner_channel_id);
                        terminal_controller_cutover::apply_bridge_short_replace_controller(
                            gateway.as_ref(),
                            shared_owned.as_ref(),
                            &provider,
                            channel_id,
                            watcher_owner_channel_id,
                            inflight_state.tmux_session_name.as_deref(),
                            &cell,
                            &shared_owned.ui.placeholder_controller,
                            current_msg_id,
                            &delivery_response,
                            &spoken_delivery_response,
                            full_response.len(),
                            bridge_turn,
                            bridge_start,
                            tmux_last_offset.unwrap_or(0),
                            single_message_panel_footer_mode,
                            dispatch_id.as_deref(),
                            adk_session_key.as_deref(),
                            Some(turn_id.as_str()),
                            Some(bridge_lease_key.clone()),
                            terminal_controller_cutover::BridgeShortReplaceLocals {
                                terminal_delivery_committed: &mut terminal_delivery_committed,
                                terminal_body_visible: &mut terminal_body_visible,
                                completion_footer_terminal_text:
                                    &mut completion_footer_terminal_text,
                                preserve_inflight_for_cleanup_retry:
                                    &mut preserve_inflight_for_cleanup_retry,
                                bridge_skip_holder_owns_inflight:
                                    &mut bridge_skip_holder_owns_inflight,
                                inflight_response_sent_offset: &mut inflight_state
                                    .response_sent_offset,
                            },
                        )
                        .await;
                    } else {
                        // #3041 P1-2 (site 5 — normal bridge terminal replace):
                        // acquire the shared delivery lease on
                        // `watcher_owner_channel_id` BEFORE delivering so the
                        // watcher and bridge serialize. On B2 Skip the holder owns
                        // this range/turn, so do NOT deliver+advance.
                        let lease_acquire =
                            match terminal_controller_cutover::bridge_terminal_lease_range(
                                Some((bridge_start, tmux_last_offset.unwrap_or(0))),
                                cutover_short_replace,
                            ) {
                                Some(_) => bridge_delivery_lease_for_inflight(
                                    shared_owned.as_ref(),
                                    watcher_owner_channel_id,
                                    shared_owned.restart.current_generation,
                                    &inflight_state,
                                    tmux_last_offset,
                                ),
                                None => BridgeLeaseAcquire::NoRange,
                            };
                        if matches!(lease_acquire, BridgeLeaseAcquire::Skip) {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                channel = channel_id.get(),
                                "  [{ts}] 🌉 #3041 B2: delivery lease held by another holder — bridge skipped duplicate terminal replace (channel {})",
                                channel_id
                            );
                            // #3041 P1-2 (codex P1-c): preserve retry on a B2
                            // Skip — holder owns delivery; do NOT clear inflight /
                            // mark the watcher delivered. (codex P1-2 R3)
                            // identity-guard the save.
                            preserve_inflight_for_cleanup_retry = true;
                            bridge_skip_holder_owns_inflight = true;
                        } else {
                            // `Held(lease)` commits through the lease; `NoRange`
                            // delivers without a lease and without offset advance.
                            let lease = match lease_acquire {
                                BridgeLeaseAcquire::Held(lease) => Some(lease),
                                _ => None,
                            };
                            {
                                let replace_outcome = gateway
                                    .replace_message_with_outcome(
                                        channel_id,
                                        current_msg_id,
                                        &delivery_response,
                                    )
                                    .await;
                                // #2860: delivered if the placeholder was edited OR a
                                // fallback posted the full delivery_response as a fresh
                                // message (edit non-committed); record it delivered so
                                // stall-watchdog recovery does not re-deliver this turn.
                                let fallback_delivered = matches!(
                                    &replace_outcome,
                                    Ok(super::super::formatting::ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. })
                                );
                                let replace_committed = turn_bridge_replace_outcome_committed(
                                    shared_owned.as_ref(),
                                    &provider,
                                    channel_id,
                                    current_msg_id,
                                    inflight_state.tmux_session_name.as_deref(),
                                    replace_outcome,
                                    dispatch_id.as_deref(),
                                    adk_session_key.as_deref(),
                                    Some(turn_id.as_str()),
                                    "turn_bridge_terminal_replace",
                                );
                                // #3041 P1-2 / B6: confirmed_end advance flows ONLY
                                // through the lease commit — `Delivered` on a committed
                                // replace, `NotDelivered` otherwise.
                                let outcome = if let Some(lease) = lease {
                                    let lease_range = lease.range();
                                    let outcome = if replace_committed {
                                        crate::services::discord::LeaseOutcome::Delivered
                                    } else {
                                        crate::services::discord::LeaseOutcome::NotDelivered
                                    };
                                    let committed = lease.commit_and_advance(
                                        shared_owned.as_ref(),
                                        watcher_owner_channel_id,
                                        inflight_state.tmux_session_name.as_deref(),
                                        outcome,
                                    );
                                    // #3630: mirror delivered frontier for post-restart no-inflight dedup.
                                    if replace_committed && committed {
                                        super::super::outbound::delivery_record::record_delivered_frontier_with_body(
                                            shared_owned.as_ref(),
                                            &provider,
                                            watcher_owner_channel_id,
                                            lease_range,
                                            current_msg_id.get(),
                                            channel_id.get(),
                                            &spoken_delivery_response,
                                        );
                                    }
                                    replace_committed
                                } else {
                                    // NoRange: no new bytes, so deliver without a lease
                                    // and without advancing.
                                    replace_committed
                                };
                                if outcome {
                                    terminal_delivery_committed = true;
                                    terminal_body_visible = true;
                                    if single_message_panel_footer_mode {
                                        completion_footer_terminal_text =
                                            Some(delivery_response.clone());
                                    }
                                } else {
                                    preserve_inflight_for_cleanup_retry = true;
                                    if fallback_delivered {
                                        // The fallback carried the whole response; persist
                                        // that offset so recovery never treats it as
                                        // never-delivered.
                                        inflight_state.response_sent_offset = full_response.len();
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                match enqueue_headless_delivery(
                    &shared_owned,
                    channel_id,
                    user_msg_id,
                    adk_session_key.as_deref(),
                    inflight_state.delivery_bot.as_deref(),
                    &delivery_response,
                    Some(cancel_token.as_ref()),
                )
                .await
                {
                    Ok(()) => {
                        cleanup_headless_streaming_placeholder_after_delivery(
                            shared_owned.as_ref(),
                            channel_id,
                            current_msg_id,
                            status_panel_msg_id,
                            &last_edit_text,
                            &provider,
                        )
                        .await;
                        terminal_delivery_committed = true;
                        terminal_body_visible = true;
                        if single_message_panel_footer_mode {
                            completion_footer_terminal_text = Some(delivery_response.clone());
                        }
                    }
                    Err(error) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ headless delivery enqueue failed for channel {}: {} — preserving inflight for retry (full_response not yet delivered)",
                            channel_id,
                            error
                        );
                        // Symmetric with the can_chain_locally failure arm: the answer was NOT delivered (enqueue failed) → do NOT let finalization clear inflight (it is the only persisted full_response). Preserving routes disposition through save_inflight_state so recovery can re-deliver.
                        preserve_inflight_for_cleanup_retry = true;
                    }
                }
            }
        }

        if terminal_delivery_committed {
            response_sent_offset = full_response.len();
            inflight_state.response_sent_offset = response_sent_offset;
            inflight_state.terminal_delivery_committed = true;
            inflight_state.full_response = full_response.clone();
            match crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
                &inflight_state,
                "turn_bridge::terminal_delivery_committed_mirror@5536",
            ) {
                crate::services::discord::inflight::GuardedSaveOutcome::IoError => {
                    tracing::warn!(
                        provider = %provider.as_str(),
                        channel = channel_id.get(),
                        "turn bridge failed to mirror committed terminal delivery before cleanup"
                    );
                }
                crate::services::discord::inflight::GuardedSaveOutcome::Saved
                | crate::services::discord::inflight::GuardedSaveOutcome::Missing
                | crate::services::discord::inflight::GuardedSaveOutcome::IdentityMismatch => {}
            }
            for frozen_msg_id in terminal_full_replay_cleanup_msg_ids.drain(..) {
                // #5413/#3607: current_msg_id is the terminal answer and is
                // already excluded here, so no terminal-anchor guard is
                // needed — every drained id is a non-terminal streamed prefix.
                if frozen_msg_id == current_msg_id {
                    continue;
                }
                let (replay_outcome, replay_detail) = match gateway
                    .delete_message(channel_id, frozen_msg_id)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            target: "agentdesk::codex_rollout_handoff",
                            provider = %provider.as_str(),
                            channel = channel_id.get(),
                            message_id = frozen_msg_id.get(),
                            "turn_bridge removed streamed rollover prefix after full terminal replay"
                        );
                        ("committed", None)
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "agentdesk::codex_rollout_handoff",
                            provider = %provider.as_str(),
                            channel = channel_id.get(),
                            message_id = frozen_msg_id.get(),
                            error = %error,
                            "turn_bridge failed to remove streamed rollover prefix after full terminal replay"
                        );
                        ("failed", Some(error))
                    }
                };
                crate::services::observability::emit_relay_delete(
                    provider.as_str(),
                    channel_id.get(),
                    frozen_msg_id.get(),
                    adk_session_key.as_deref(),
                    Some(turn_id.as_str()),
                    "full_terminal_replay_prefix",
                    "delete_nonterminal",
                    replay_outcome,
                    replay_detail.as_deref(),
                );
            }
            // #2236: look up the typed handoff marker stamped at dispatch
            // time so multi-agent setups with overlapping background
            // channels can be disambiguated by the stored agent_id. The
            // marker is consumed inside voice_background_completion_target
            // below; passing the stored agent_id (cloned, not taken) here
            // keeps both lookups consistent for this single dispatch.
            //
            // #2274: if the in-memory marker is absent (dcserver
            // restarted mid-turn, or rehydration has not yet completed)
            // fall back to the durable PG row for the agent_id lookup.
            // A recovery turn with no anchored user message (user_msg_id ==
            // 0) is never a voice turn — voice turns carry a synthetic,
            // non-zero voice message id — so the voice-handoff completion
            // routing (all keyed on the user message id) does not apply.
            if let Some(user_msg_id) = user_msg_id {
                let pg_pool_for_handoff = shared_owned.pg_pool.as_ref();
                let in_memory_handoff_agent_id = crate::voice::announce_meta::global_store()
                    .get_handoff(user_msg_id)
                    .and_then(|meta| meta.agent_id);
                let stored_handoff_agent_id = match in_memory_handoff_agent_id {
                    Some(agent_id) => Some(agent_id),
                    None => match pg_pool_for_handoff {
                        Some(pool) => {
                            crate::voice::announce_meta::load_handoff_durable(pool, user_msg_id)
                                .await
                                .ok()
                                .flatten()
                                .and_then(|meta| meta.agent_id)
                        }
                        None => None,
                    },
                };
                let mapped_voice_channel_id = shared_owned
                    .voice_barge_in
                    .voice_channel_for_background(channel_id, stored_handoff_agent_id.as_deref())
                    .await;
                if let Some(voice_channel_id) = voice_background_completion_target(
                    mapped_voice_channel_id,
                    dispatch_id.as_deref(),
                    user_msg_id,
                    Some(&turn_id),
                    &user_text_owned,
                    channel_id,
                    pg_pool_for_handoff,
                )
                .await
                {
                    if !inflight_state.silent_turn {
                        let voice_barge_in = shared_owned.voice_barge_in.clone();
                        let shared_for_voice = shared_owned.clone();
                        let summary_source = spoken_delivery_response.clone();
                        let background_channel_id = channel_id;
                        let failed = cancelled
                            || is_prompt_too_long
                            || transport_error
                            || recovery_retry
                            || resume_failure_detected;
                        super::super::task_supervisor::spawn_observed(
                            "voice_background_completion_summary",
                            async move {
                                voice_barge_in
                                    .speak_voice_background_completion_summary(
                                        &shared_for_voice,
                                        voice_channel_id,
                                        background_channel_id,
                                        &summary_source,
                                        failed,
                                    )
                                    .await;
                                voice_barge_in.publish_progress_for_playback(
                                    background_channel_id,
                                    Some(voice_channel_id),
                                    "agent:done",
                                );
                            },
                        );
                    } else {
                        shared_owned.voice_barge_in.publish_progress_for_playback(
                            channel_id,
                            Some(voice_channel_id),
                            "agent:done",
                        );
                    }
                } else if inflight_state.source == crate::dispatch::Source::Voice {
                    if !inflight_state.silent_turn {
                        shared_owned
                            .voice_barge_in
                            .spawn_spoken_result_playback(
                                &shared_owned,
                                channel_id,
                                &spoken_delivery_response,
                            )
                            .await;
                    }
                    shared_owned
                        .voice_barge_in
                        .publish_progress(channel_id, "agent:done");
                }
            }
        }

        // #2161 (Codex round-2 H1): run the TUI completion gate BEFORE
        // dispatch completion / queue drain so a still-busy ClaudeTui
        // pane cannot advance lifecycle state ahead of pane quiescence.
        // The same outcome is reused by the status-panel emit below.
        // The gate lives in the `tmux` module (`#[cfg(unix)]`); on
        // non-unix targets we skip it and emit completion as normal.
        //
        // #2293 — when the early gate (~line 4254, hoisted ABOVE the
        // mailbox release) already ran for this turn, reuse its outcome
        // here instead of polling tmux a second time. The early
        // result is the authoritative one because it was sampled
        // closer to the moment the mailbox would have been released;
        // re-polling here would create a window where the pane could
        // settle into idle AFTER we already deferred mailbox cleanup,
        // producing inconsistent dispatch / status semantics across
        // the two gate sites.
        #[cfg(unix)]
        {
            let tui_transport_error_skip_gate = claude_tui_followup_pre_submit_requeue_candidate
                || (transport_error
                    && bridge_tui_transport_error_should_skip_quiescence(
                        &provider,
                        inflight_state.runtime_kind,
                        &full_response,
                    ));
            let bridge_gate_outcome = if terminal_delivery_committed
                && !preserve_inflight_for_cleanup_retry
                && !tui_transport_error_skip_gate
            {
                if let Some(outcome) = bridge_tui_gate_outcome_early {
                    outcome
                } else if let Some(tmux_session_name) = inflight_state.tmux_session_name.as_deref()
                {
                    super::super::tmux::run_tui_completion_gate(
                        &provider,
                        channel_id,
                        tmux_session_name,
                        inflight_state.task_notification_kind,
                    )
                    .await
                } else {
                    super::super::tmux::TuiCompletionGateOutcome::NotGated
                }
            } else {
                if terminal_delivery_committed && tui_transport_error_skip_gate {
                    tracing::info!(
                        provider = %provider.as_str(),
                        channel = channel_id.get(),
                        runtime_kind = ?inflight_state.runtime_kind,
                        "TUI transport error was already delivered; skipping quiescence gate so inflight cleanup can complete"
                    );
                }
                if claude_tui_followup_pre_submit_requeue_candidate {
                    followup_requeue::requeue_claude_tui_followup_pre_submit_timeout(
                        &shared_owned,
                        &provider,
                        channel_id,
                        &inflight_state,
                        dispatch_id.as_deref(),
                        adk_session_key.as_deref(),
                        turn_id.as_str(),
                    )
                    .await;
                }
                super::super::tmux::TuiCompletionGateOutcome::NotGated
            };

            bridge_should_emit_completion = bridge_gate_outcome.should_emit_completion();
        }

        if should_complete_work_dispatch_after_terminal_delivery(
            should_complete_work_dispatch_after_delivery,
            terminal_delivery_committed,
            preserve_inflight_for_cleanup_retry,
            resume_failure_detected,
            recovery_retry,
            &full_response,
        ) {
            complete_work_dispatch_on_turn_end(
                &shared_owned,
                dispatch_id.as_deref(),
                adk_cwd.as_deref(),
                Some(&full_response),
            )
            .await;
        } else if should_fail_dispatch_after_terminal_delivery(
            should_fail_dispatch_after_delivery,
            terminal_delivery_committed,
            preserve_inflight_for_cleanup_retry,
        ) {
            // Transport error — fail the dispatch only after the terminal
            // error response is deliverable, so auto-queue does not advance
            // ahead of visible turn completion.
            fail_dispatch_with_retry(
                shared_owned.api_port,
                dispatch_id.as_deref(),
                &full_response,
            )
            .await;
        }

        // Signal the watcher that this turn's response was already delivered.
        // Prevents the watcher from relaying the same response when it resumes.
        // #3041 P1-2 (codex P1-c): a B2 Skip set
        // `preserve_inflight_for_cleanup_retry = true`, so this gate (encoded in
        // `bridge_epilogue_marks_watcher_delivered`) does NOT mark the watcher
        // delivered — the bridge never delivered the range; the holder owns it.
        if bridge_epilogue_marks_watcher_delivered(
            preserve_inflight_for_cleanup_retry,
            bridge_relay_delegated_to_watcher,
        ) && let Some(watcher) = shared_owned.tmux_watchers.get(&watcher_owner_channel_id)
        {
            watcher.turn_delivered.store(true, Ordering::Release);
        }

        if can_chain_locally
            && !preserve_inflight_for_cleanup_retry
            && !delivery_response.trim().is_empty()
            && let Some(user_msg_id) = user_msg_id
        {
            tv_done(
                &shared_owned,
                channel_id,
                user_msg_id,
                inflight_generation,
                "done",
            )
            .await;
        }

        td_warn(
            terminal_delivery_committed,
            preserve_inflight_for_cleanup_retry,
            channel_id,
        );
        status_panel_terminal_committed = status_panel_completion_ready_after_terminal_body(
            terminal_delivery_committed,
            terminal_body_visible,
            preserve_inflight_for_cleanup_retry,
        );
        if status_panel_terminal_committed {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ▶ Response sent");
            if let Ok(mut last) = shared_owned.last_turn_at.lock() {
                *last = Some(chrono::Local::now().to_rfc3339());
            }
        }
    }
    TerminalOutcomeDeliveryOutput {
        outcome: TerminalOutcomeDeliveryOutcome::Completed,
        shared_owned,
        gateway,
        provider,
        cancel_token,
        turn_id,
        user_text_owned,
        adk_session_key,
        adk_cwd,
        dispatch_id,
        new_session_id,
        new_raw_provider_session_id,
        full_response,
        active_background_child_session_ids,
        pending_long_running_open_after_state_save,
        pending_long_running_retarget_after_state_save,
        long_running_placeholder_active,
        inflight_state,
        api_friction_reports,
        status_panel_terminal_committed,
        bridge_should_emit_completion,
        completion_footer_terminal_text,
        preserve_inflight_for_cleanup_retry,
        bridge_skip_holder_owns_inflight,
        terminal_delivery_committed,
        resume_failure_detected,
        terminal_empty_response_notice,
        terminal_full_replay_cleanup_msg_ids,
        response_sent_offset,
        turn_start,
    }
}
