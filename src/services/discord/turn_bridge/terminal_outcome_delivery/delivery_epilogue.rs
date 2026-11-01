//! Post-commit terminal delivery epilogue for terminal outcome delivery.

use std::sync::{Arc, atomic::Ordering};

use super::*;

pub(super) enum DeliveryEpilogueMessage {
    PostCommit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryEpilogueOutcome {
    Continue,
}

pub(super) use super::contracts::DeliveryEpilogueContext;

pub(super) struct DeliveryEpilogueState<'a> {
    pub(super) response_sent_offset: &'a mut usize,
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) terminal_full_replay_cleanup_msg_ids: &'a mut Vec<MessageId>,
    pub(super) bridge_should_emit_completion: &'a mut bool,
    pub(super) status_panel_terminal_committed: &'a mut bool,
    pub(super) busy_requeue_outcome: &'a mut Option<followup_requeue::FollowupRequeueOutcome>,
}

#[rustfmt::skip]
pub(super) async fn handle_delivery_epilogue(
    message: DeliveryEpilogueMessage,
    ctx: DeliveryEpilogueContext<'_>,
    state: DeliveryEpilogueState<'_>,
) -> DeliveryEpilogueOutcome {
    let (shared_owned, gateway) = (Arc::clone(ctx.shared_owned), Arc::clone(ctx.gateway));
    let (provider, channel_id) = (ctx.provider.clone(), ctx.channel_id);
    let (user_msg_id, current_msg_id) = (ctx.user_msg_id, ctx.current_msg_id);
    let (adk_session_key, adk_cwd) = (ctx.adk_session_key, ctx.adk_cwd);
    let (dispatch_id, turn_id) = (ctx.dispatch_id, ctx.turn_id);
    let full_response = ctx.full_response.clone();
    let delivery_response = ctx.delivery_response.clone();
    let cancelled = ctx.cancelled;
    let transport_error = ctx.transport_error;
    let recovery_retry = ctx.recovery_retry;
    let resume_failure_detected = ctx.resume_failure_detected;
    let claude_tui_followup_pre_submit_requeue_candidate =
        ctx.claude_tui_followup_pre_submit_requeue_candidate;
    let claude_tui_busy_requeue_pending = ctx.claude_tui_busy_requeue_pending;
    let tui_error_classification = ctx.tui_error_classification;
    #[cfg(unix)]
    let bridge_tui_gate_outcome_early = ctx.bridge_tui_gate_outcome_early;
    let terminal_delivery_committed = ctx.terminal_delivery_committed;
    let terminal_body_visible = ctx.terminal_body_visible;
    let preserve_inflight_for_cleanup_retry = ctx.preserve_inflight_for_cleanup_retry;
    // A protected request settles its dispatch only after an ordinary classification.
    let pool = shared_owned.pg_pool.as_ref();
    let replay = crate::services::discord::replay_policy::ordinary_outcome;
    let ordinary = replay(pool, ctx.cancel_token, state.inflight_state).await;
    let should_complete_work_dispatch_after_delivery =
        ctx.should_complete_work_dispatch_after_delivery && ordinary;
    let should_fail_dispatch_after_delivery = ctx.should_fail_dispatch_after_delivery && ordinary;
    let bridge_relay_delegated_to_watcher = ctx.bridge_relay_delegated_to_watcher;
    let watcher_delivery_pin = ctx.watcher_delivery_pin;
    let can_deliver_directly = gateway.can_deliver_directly();
    let inflight_generation = ctx.inflight_generation;
    let waiting = ctx.claude_tui_followup_pre_submit_requeue_candidate
        || ctx.claude_tui_busy_requeue_pending
        || state.busy_requeue_outcome.is_some()
        || super::completion_proof::waiting(state.inflight_state).is_some();
    let input_confirmed = !waiting && super::completion_proof::input_confirmed(&provider, state.inflight_state, &full_response);
    let projection_allowed = ctx.terminal_delivery_committed && (input_confirmed || (ctx.provider_failure && !waiting));

    let mut response_sent_offset = *state.response_sent_offset;
    let inflight_state = &mut *state.inflight_state;
    let terminal_full_replay_cleanup_msg_ids =
        &mut *state.terminal_full_replay_cleanup_msg_ids;
    #[cfg(unix)]
    let bridge_should_emit_completion;
    #[cfg(not(unix))]
    let bridge_should_emit_completion = *state.bridge_should_emit_completion;
    let status_panel_terminal_committed;
    let busy_requeue_outcome = &mut *state.busy_requeue_outcome;

    match message {
        DeliveryEpilogueMessage::PostCommit => {
        if terminal_delivery_committed {
            response_sent_offset = full_response.len();
            inflight_state.response_sent_offset = response_sent_offset;
            inflight_state.terminal_delivery_committed = true;
            inflight_state.full_response = full_response.clone();
            // The scheduled outcome is mirrored with the commit so a crash before the
            // postlude record still settles this exact turn.
            let scheduled_outcome = crate::services::scheduled_messages::turn_completion::classify(
                &full_response,
                ctx.transcript_terminal_error,
                ctx.provider_failure,
            );
            inflight_state
                .scheduled
                .set_outcome(scheduled_outcome.filter(|_| !cancelled));
            let mirror_outcome =
                crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
                    &mut *inflight_state,
                    "turn_bridge::terminal_delivery_committed_mirror@5536",
                );
            crate::services::discord::inflight::terminal_delivery_evidence_loss::warn_for_bridge_terminal_mirror_outcome(
                mirror_outcome,
                crate::services::discord::inflight::terminal_delivery_evidence_loss::BridgeEvidenceLossContext {
                    provider: &provider,
                    channel_id,
                    current_msg_id,
                    response_sent_offset,
                },
            );
            for frozen_msg_id in terminal_full_replay_cleanup_msg_ids.drain(..).filter(|_| !ctx.already_receipted) {
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
                            channel_id = channel_id.get(),
                            message_id = frozen_msg_id.get(),
                            "turn_bridge removed streamed rollover prefix after full terminal replay"
                        );
                        ("committed", None)
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "agentdesk::codex_rollout_handoff",
                            provider = %provider.as_str(),
                            channel_id = channel_id.get(),
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
            let skip_tui_quiescence = claude_tui_followup_pre_submit_requeue_candidate
                || (transport_error
                    && bridge_tui_transport_error_should_skip_quiescence(
                        &provider,
                        inflight_state.runtime_kind,
                        &full_response,
                        tui_error_classification,
                    ));
            let bridge_gate_outcome = if terminal_delivery_committed
                && !preserve_inflight_for_cleanup_retry
                && !skip_tui_quiescence
                && projection_allowed
            {
                if let Some(outcome) = bridge_tui_gate_outcome_early {
                    outcome
                } else if let Some(tmux_session_name) = inflight_state.tmux_session_name.as_deref()
                {
                    super::super::super::tmux::run_tui_completion_gate(
                        &provider,
                        channel_id,
                        tmux_session_name,
                        inflight_state.task_notification_kind,
                    )
                    .await
                } else {
                    super::super::super::tmux::TuiCompletionGateOutcome::NotGated
                }
            } else {
                if terminal_delivery_committed && skip_tui_quiescence {
                    tracing::info!(
                        provider = %provider.as_str(),
                        channel_id = channel_id.get(),
                        runtime_kind = ?inflight_state.runtime_kind,
                        "TUI transport error was already delivered; skipping quiescence gate so inflight cleanup can complete"
                    );
                }
                followup_requeue::requeue_if_needed(
                    busy_requeue_outcome,
                    (claude_tui_followup_pre_submit_requeue_candidate, claude_tui_busy_requeue_pending),
                    (&shared_owned, &provider, channel_id),
                    &inflight_state,
                    dispatch_id.as_deref(),
                    adk_session_key.as_deref(),
                    followup_requeue::BusyAttempt {
                        turn_id: turn_id.as_str(),
                        cost: ctx.busy_attempt_cost,
                    },
                )
                .await;
                super::super::super::tmux::TuiCompletionGateOutcome::NotGated
            };

            bridge_should_emit_completion = bridge_gate_outcome.should_emit_completion();
        }

        settle_terminal_dispatch(TerminalDispatchSettlement {
            shared: &shared_owned, dispatch_id: dispatch_id.as_deref(), adk_cwd: adk_cwd.as_deref(),
            full_response: &full_response, should_complete: should_complete_work_dispatch_after_delivery && !waiting,
            should_fail: should_fail_dispatch_after_delivery && !waiting, committed: terminal_delivery_committed,
            preserve: preserve_inflight_for_cleanup_retry, resume_failure: resume_failure_detected, recovery_retry,
        }).await;

        // Mark this turn delivered so the watcher will not relay it again when it resumes.
        // #3041 P1-2 (codex P1-c): a B2 Skip set
        // `preserve_inflight_for_cleanup_retry = true`, so this gate (encoded in
        // `bridge_epilogue_marks_watcher_delivered`) does NOT mark the watcher
        // delivered — the bridge never delivered the range; the holder owns it.
        if input_confirmed && terminal_delivery_committed && bridge_epilogue_marks_watcher_delivered(
            preserve_inflight_for_cleanup_retry,
            bridge_relay_delegated_to_watcher,
        ) && let Some(pin) = watcher_delivery_pin
        {
            pin.adopt_if_current(&shared_owned.tmux_watchers, |pin| {
                pin.turn_delivered.store(true, Ordering::Release);
            });
        }

        if !ctx.already_receipted && can_deliver_directly
            && !waiting
            && terminal_delivery_committed
            && !preserve_inflight_for_cleanup_retry
            && !delivery_response.trim().is_empty()
            && let Some(user_msg_id) = user_msg_id
        {
            // A delivered provider failure notice marks the request failed, not done.
            if ctx.provider_failure {
                tv_fail(&shared_owned, channel_id, user_msg_id, inflight_generation, "fail").await;
            } else if input_confirmed {
                tv_done(&shared_owned, channel_id, user_msg_id, inflight_generation, "done").await;
            }
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
        ) && projection_allowed;
        if status_panel_terminal_committed {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ▶ Response sent");
            if let Ok(mut last) = shared_owned.last_turn_at.lock() {
                *last = Some(chrono::Local::now().to_rfc3339());
            }
        }
        }
    }

    *state.response_sent_offset = response_sent_offset;
    *state.bridge_should_emit_completion = bridge_should_emit_completion && projection_allowed && !ctx.already_receipted;
    *state.status_panel_terminal_committed = status_panel_terminal_committed;

    DeliveryEpilogueOutcome::Continue
}

/// The nonvisual dispatch settlement used both after direct delivery and after
/// a detached episode's durable retry. Publication authority stays at the caller.
pub(super) struct TerminalDispatchSettlement<'a> {
    pub shared: &'a Arc<SharedData>,
    pub dispatch_id: Option<&'a str>,
    pub adk_cwd: Option<&'a str>,
    pub full_response: &'a str,
    pub should_complete: bool,
    pub should_fail: bool,
    pub committed: bool,
    pub preserve: bool,
    pub resume_failure: bool,
    pub recovery_retry: bool,
}
pub(super) async fn settle_terminal_dispatch(ctx: TerminalDispatchSettlement<'_>) -> bool {
    // The replacement turn retains the dispatch. A recovery notice is neither
    // successful work nor a failure that AutoQueue may independently replay.
    if ctx.recovery_retry || ctx.resume_failure {
        return false;
    }
    if should_complete_work_dispatch_after_terminal_delivery(
        ctx.should_complete,
        ctx.committed,
        ctx.preserve,
        ctx.resume_failure,
        ctx.recovery_retry,
        ctx.full_response,
    ) {
        complete_work_dispatch_on_turn_end(
            ctx.shared,
            ctx.dispatch_id,
            ctx.adk_cwd,
            Some(ctx.full_response),
        )
        .await;
        true
    } else if should_fail_dispatch_after_terminal_delivery(
        ctx.should_fail,
        ctx.committed,
        ctx.preserve,
    ) {
        fail_dispatch_with_retry(ctx.shared.api_port, ctx.dispatch_id, ctx.full_response).await;
        true
    } else {
        false
    }
}
