//! Cancelled/prompt-too-long terminal replacement arms for terminal outcome delivery.

use std::sync::Arc;

use super::*;
use crate::services::discord::session_banner::DiscordTurnSessionBanner;

pub(super) enum CancelPromptReplaceMessage {
    Cancelled,
    PromptTooLong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CancelPromptReplaceOutcome {
    Continue,
}

pub(super) struct CancelPromptReplaceContext<'a> {
    pub(super) shared_owned: &'a Arc<SharedData>,
    pub(super) gateway: &'a Arc<dyn TurnGateway>,
    pub(super) provider: &'a ProviderKind,
    pub(super) cancel_token: &'a Arc<crate::services::provider::CancelToken>,
    pub(super) channel_id: ChannelId,
    pub(super) user_msg_id: Option<MessageId>,
    pub(super) current_msg_id: MessageId,
    pub(super) dispatch_id: &'a Option<String>,
    pub(super) adk_session_key: &'a Option<String>,
    pub(super) turn_id: &'a String,
    pub(super) watcher_owner_channel_id: ChannelId,
    pub(super) tmux_last_offset: Option<u64>,
    pub(super) response_sent_offset: usize,
    pub(super) inflight_generation: u64,
}

pub(super) struct CancelPromptReplaceState<'a> {
    pub(super) full_response: &'a mut String,
    pub(super) active_background_child_session_ids: &'a mut Vec<i64>,
    pub(super) pending_long_running_open_after_state_save:
        &'a mut PendingLongRunningOpenAfterStateSave,
    pub(super) pending_long_running_retarget_after_state_save:
        &'a mut PendingLongRunningRetargetAfterStateSave,
    pub(super) long_running_placeholder_active: &'a mut LongRunningPlaceholderActive,
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) preserve_inflight_for_cleanup_retry: &'a mut bool,
    pub(super) bridge_skip_holder_owns_inflight: &'a mut bool,
    pub(super) status_panel_terminal_committed: &'a mut bool,
}

#[rustfmt::skip]
pub(super) async fn handle_cancel_prompt_replace(
    message: CancelPromptReplaceMessage,
    ctx: CancelPromptReplaceContext<'_>,
    state: CancelPromptReplaceState<'_>,
) -> CancelPromptReplaceOutcome {
    let shared_owned = Arc::clone(ctx.shared_owned);
    let gateway = Arc::clone(ctx.gateway);
    let provider = ctx.provider.clone();
    let cancel_token = Arc::clone(ctx.cancel_token);
    let channel_id = ctx.channel_id;
    let user_msg_id = ctx.user_msg_id;
    let current_msg_id = ctx.current_msg_id;
    let dispatch_id = ctx.dispatch_id;
    let adk_session_key = ctx.adk_session_key;
    let turn_id = ctx.turn_id;
    let watcher_owner_channel_id = ctx.watcher_owner_channel_id;
    let tmux_last_offset = ctx.tmux_last_offset;
    let response_sent_offset = ctx.response_sent_offset;
    let inflight_generation = ctx.inflight_generation;

    let mut full_response = std::mem::take(state.full_response);
    let mut active_background_child_session_ids =
        std::mem::take(state.active_background_child_session_ids);
    let mut pending_long_running_open_after_state_save =
        state.pending_long_running_open_after_state_save.take();
    let mut pending_long_running_retarget_after_state_save =
        state.pending_long_running_retarget_after_state_save.take();
    let mut long_running_placeholder_active = state.long_running_placeholder_active.take();
    let inflight_state = &mut *state.inflight_state;
    let mut preserve_inflight_for_cleanup_retry = *state.preserve_inflight_for_cleanup_retry;
    let mut bridge_skip_holder_owns_inflight = *state.bridge_skip_holder_owns_inflight;
    let mut status_panel_terminal_committed = *state.status_panel_terminal_committed;
    let banner = DiscordTurnSessionBanner::new_with_turn_key(
        shared_owned.as_ref(),
        channel_id,
        &provider,
        inflight_state.user_msg_id,
        Some(&inflight_state.started_at),
        inflight_state.turn_start_offset,
    );

    match message {
        CancelPromptReplaceMessage::Cancelled => {
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
        // Safe even if the idempotent ToolResult transition already completed.
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
                        super::super::super::placeholder_controller::PlaceholderLifecycle::Aborted,
                    )
                    .await;
                use super::super::super::placeholder_controller::PlaceholderControllerOutcome::*;
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
            let formatted = banner.format_discord_body(remaining_response);
            format!("{}\n\n[Stopped]", formatted)
        };
        let terminal_response = banner.prefix(response_sent_offset == 0, terminal_response);

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
                channel_id = channel_id.get(),
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
        }
        CancelPromptReplaceMessage::PromptTooLong => {
        let mention = gateway.requester_mention().unwrap_or_default();
        full_response = super::prompt_too_long_guidance::render_terminal_guidance(&full_response);
        if !mention.is_empty() {
            full_response = format!("{mention} {full_response}");
        }
        let display_response = banner.prefix(response_sent_offset == 0, full_response.clone());
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
                channel_id = channel_id.get(),
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
                    .replace_message_with_outcome(channel_id, current_msg_id, &display_response)
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
                    super::super::super::outbound::delivery_record::record_delivered_frontier_with_body(
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
        }
    }

    *state.full_response = full_response;
    *state.active_background_child_session_ids = active_background_child_session_ids;
    *state.pending_long_running_open_after_state_save =
        pending_long_running_open_after_state_save;
    *state.pending_long_running_retarget_after_state_save =
        pending_long_running_retarget_after_state_save;
    *state.long_running_placeholder_active = long_running_placeholder_active;
    *state.preserve_inflight_for_cleanup_retry = preserve_inflight_for_cleanup_retry;
    *state.bridge_skip_holder_owns_inflight = bridge_skip_holder_owns_inflight;
    *state.status_panel_terminal_committed = status_panel_terminal_committed;

    CancelPromptReplaceOutcome::Continue
}
