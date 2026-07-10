//! #4230 S5 terminal outcome delivery for `turn_bridge::spawn_turn_bridge`.
//!
//! Moved from the post-finalizer terminal delivery block of `spawn_turn_bridge`:
//! cancel/prompt-too-long/recovery/empty-response delivery, bridge terminal
//! delivery lease commits, terminal-controller cutover, voice completion,
//! TUI completion gate, dispatch completion/fail, watcher-delivered mark,
//! `tv_done`, and terminal status readiness.

use std::sync::Arc;

use super::output_lifecycle::BridgeOutputOwner;
use super::stream_tick::{
    LongRunningPlaceholderActive, PendingLongRunningOpenAfterStateSave,
    PendingLongRunningRetargetAfterStateSave,
};
use cancel_prompt_replace::{
    CancelPromptReplaceContext, CancelPromptReplaceMessage, CancelPromptReplaceOutcome,
    CancelPromptReplaceState, handle_cancel_prompt_replace,
};
use delivery_epilogue::{
    DeliveryEpilogueContext, DeliveryEpilogueMessage, DeliveryEpilogueOutcome,
    DeliveryEpilogueState, handle_delivery_epilogue,
};
use empty_response_recovery::{
    EmptyResponseRecoveryContext, EmptyResponseRecoveryMessage, EmptyResponseRecoveryOutcome,
    EmptyResponseRecoveryState, handle_empty_response_recovery,
};
use recovery_retry::{
    RecoveryRetryContext, RecoveryRetryMessage, RecoveryRetryOutcome, RecoveryRetryState,
    handle_recovery_retry,
};

mod cancel_prompt_replace;
mod delivery_epilogue;
mod empty_response_recovery;
mod recovery_retry;

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

    if recovery_retry {
        let outcome = handle_recovery_retry(
            RecoveryRetryMessage::SessionDiedDuringRecovery,
            RecoveryRetryContext {
                shared_owned: &shared_owned,
                gateway: &gateway,
                cancel_token: &cancel_token,
                channel_id,
                user_msg_id,
                current_msg_id,
                adk_session_key: &adk_session_key,
                user_text_owned: &user_text_owned,
            },
            RecoveryRetryState {
                full_response: &mut full_response,
                new_session_id: &mut new_session_id,
                new_raw_provider_session_id: &mut new_raw_provider_session_id,
                inflight_state: &mut inflight_state,
            },
        )
        .await;
        match outcome {
            RecoveryRetryOutcome::Continue => {}
        }
    }

    if cancelled || is_prompt_too_long {
        let message = if cancelled {
            CancelPromptReplaceMessage::Cancelled
        } else {
            CancelPromptReplaceMessage::PromptTooLong
        };
        let outcome = handle_cancel_prompt_replace(
            message,
            CancelPromptReplaceContext {
                shared_owned: &shared_owned,
                gateway: &gateway,
                provider: &provider,
                cancel_token: &cancel_token,
                channel_id,
                user_msg_id,
                current_msg_id,
                dispatch_id: &dispatch_id,
                adk_session_key: &adk_session_key,
                turn_id: &turn_id,
                watcher_owner_channel_id,
                tmux_last_offset,
                response_sent_offset,
                inflight_generation,
            },
            CancelPromptReplaceState {
                full_response: &mut full_response,
                active_background_child_session_ids: &mut active_background_child_session_ids,
                pending_long_running_open_after_state_save:
                    &mut pending_long_running_open_after_state_save,
                pending_long_running_retarget_after_state_save:
                    &mut pending_long_running_retarget_after_state_save,
                long_running_placeholder_active: &mut long_running_placeholder_active,
                inflight_state: &mut inflight_state,
                preserve_inflight_for_cleanup_retry: &mut preserve_inflight_for_cleanup_retry,
                bridge_skip_holder_owns_inflight: &mut bridge_skip_holder_owns_inflight,
                status_panel_terminal_committed: &mut status_panel_terminal_committed,
            },
        )
        .await;
        match outcome {
            CancelPromptReplaceOutcome::Continue => {}
        }
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
        let empty_response_recovery_message = if resume_failure_detected {
            EmptyResponseRecoveryMessage::ResumeFailureAlreadyHandled
        } else {
            EmptyResponseRecoveryMessage::InspectEmptyResponse
        };
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
        }

        let outcome = handle_empty_response_recovery(
            empty_response_recovery_message,
            EmptyResponseRecoveryContext {
                shared_owned: &shared_owned,
                gateway: &gateway,
                cancel_token: &cancel_token,
                channel_id,
                user_msg_id,
                adk_session_key: &adk_session_key,
                user_text_owned: &user_text_owned,
                had_prior_session_id_at_turn_start,
                session_handshake_seen,
                rx_disconnected,
                turn_start,
                recovery_retry,
                review_dispatch_warning: &review_dispatch_warning,
                watcher_owner_channel_id,
                tmux_last_offset,
            },
            EmptyResponseRecoveryState {
                full_response: &mut full_response,
                new_session_id: &mut new_session_id,
                new_raw_provider_session_id: &mut new_raw_provider_session_id,
                inflight_state: &mut inflight_state,
                api_friction_reports: &mut api_friction_reports,
                terminal_empty_response_notice: &mut terminal_empty_response_notice,
                resume_failure_detected: &mut resume_failure_detected,
                response_sent_offset: &mut response_sent_offset,
                terminal_delivery_committed: &mut terminal_delivery_committed,
                terminal_body_visible: &mut terminal_body_visible,
                preserve_inflight_for_cleanup_retry: &mut preserve_inflight_for_cleanup_retry,
                bridge_skip_holder_owns_inflight: &mut bridge_skip_holder_owns_inflight,
            },
        )
        .await;
        let (
            mut delivery_response,
            spoken_delivery_response,
            resume_retry_queued,
            silent_turn_handled,
        ) = match outcome {
            EmptyResponseRecoveryOutcome::ContinueDelivery {
                delivery_response,
                spoken_delivery_response,
                resume_retry_queued,
            } => (
                delivery_response,
                spoken_delivery_response,
                resume_retry_queued,
                false,
            ),
            EmptyResponseRecoveryOutcome::SilentTurnHandled {
                delivery_response,
                spoken_delivery_response,
                resume_retry_queued,
            } => (
                delivery_response,
                spoken_delivery_response,
                resume_retry_queued,
                true,
            ),
        };
        if silent_turn_handled {
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
                                channel_id = channel_id.get(),
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

        let outcome = handle_delivery_epilogue(
            DeliveryEpilogueMessage::PostCommit,
            DeliveryEpilogueContext {
                shared_owned: &shared_owned,
                gateway: &gateway,
                provider: &provider,
                channel_id,
                user_msg_id,
                current_msg_id,
                adk_session_key: &adk_session_key,
                adk_cwd: &adk_cwd,
                dispatch_id: &dispatch_id,
                turn_id: &turn_id,
                user_text_owned: &user_text_owned,
                full_response: &full_response,
                delivery_response: &delivery_response,
                spoken_delivery_response: &spoken_delivery_response,
                cancelled,
                is_prompt_too_long,
                transport_error,
                recovery_retry,
                resume_failure_detected,
                claude_tui_followup_pre_submit_requeue_candidate,
                #[cfg(unix)]
                bridge_tui_gate_outcome_early,
                terminal_delivery_committed,
                terminal_body_visible,
                preserve_inflight_for_cleanup_retry,
                should_complete_work_dispatch_after_delivery,
                should_fail_dispatch_after_delivery,
                bridge_relay_delegated_to_watcher,
                watcher_owner_channel_id,
                can_chain_locally,
                inflight_generation,
            },
            DeliveryEpilogueState {
                response_sent_offset: &mut response_sent_offset,
                inflight_state: &mut inflight_state,
                terminal_full_replay_cleanup_msg_ids: &mut terminal_full_replay_cleanup_msg_ids,
                bridge_should_emit_completion: &mut bridge_should_emit_completion,
                status_panel_terminal_committed: &mut status_panel_terminal_committed,
            },
        )
        .await;
        match outcome {
            DeliveryEpilogueOutcome::Continue => {}
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
