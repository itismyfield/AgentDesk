//! Empty-response recovery and silent-turn terminal delivery for terminal outcome delivery.
mod guidance;
use {super::*, std::sync::Arc};

pub(super) enum EmptyResponseRecoveryMessage {
    ResumeFailureAlreadyHandled,
    InspectEmptyResponse,
}

pub(super) enum EmptyResponseRecoveryOutcome {
    ContinueDelivery {
        delivery_response: String,
        spoken_delivery_response: String,
        resume_retry_queued: bool,
    },
    SilentTurnHandled {
        delivery_response: String,
        spoken_delivery_response: String,
        resume_retry_queued: bool,
    },
}

pub(super) struct EmptyResponseRecoveryContext<'a> {
    pub(super) shared_owned: &'a Arc<SharedData>,
    pub(super) gateway: &'a Arc<dyn TurnGateway>,
    pub(super) cancel_token: &'a Arc<crate::services::provider::CancelToken>,
    pub(super) channel_id: ChannelId,
    pub(super) user_msg_id: Option<MessageId>,
    pub(super) adk_session_key: &'a Option<String>,
    pub(super) user_text_owned: &'a String,
    pub(super) had_prior_session_id_at_turn_start: bool,
    pub(super) session_handshake_seen: bool,
    pub(super) rx_disconnected: bool,
    pub(super) turn_start: std::time::Instant,
    pub(super) recovery_retry: bool,
    pub(super) review_dispatch_warning: &'a Option<String>,
    pub(super) watcher_owner_channel_id: ChannelId,
    pub(super) tmux_last_offset: Option<u64>,
}

pub(super) struct EmptyResponseRecoveryState<'a> {
    pub(super) full_response: &'a mut String,
    pub(super) new_session_id: &'a mut Option<String>,
    pub(super) new_raw_provider_session_id: &'a mut Option<String>,
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) api_friction_reports: &'a mut Vec<crate::services::api_friction::ApiFrictionReport>,
    pub(super) terminal_empty_response_notice: &'a mut Option<String>,
    pub(super) resume_failure_detected: &'a mut bool,
    pub(super) response_sent_offset: &'a mut usize,
    pub(super) terminal_delivery_committed: &'a mut bool,
    pub(super) terminal_body_visible: &'a mut bool,
    pub(super) preserve_inflight_for_cleanup_retry: &'a mut bool,
    pub(super) bridge_skip_holder_owns_inflight: &'a mut bool,
}

#[rustfmt::skip]
pub(super) async fn handle_empty_response_recovery(
    message: EmptyResponseRecoveryMessage,
    ctx: EmptyResponseRecoveryContext<'_>,
    state: EmptyResponseRecoveryState<'_>,
) -> EmptyResponseRecoveryOutcome {
    let shared_owned = Arc::clone(ctx.shared_owned);
    let gateway = Arc::clone(ctx.gateway);
    let cancel_token = Arc::clone(ctx.cancel_token);
    let channel_id = ctx.channel_id;
    let user_msg_id = ctx.user_msg_id;
    let adk_session_key = ctx.adk_session_key;
    let user_text_owned = ctx.user_text_owned;
    let had_prior_session_id_at_turn_start = ctx.had_prior_session_id_at_turn_start;
    let session_handshake_seen = ctx.session_handshake_seen;
    let rx_disconnected = ctx.rx_disconnected;
    let turn_start = ctx.turn_start;
    let recovery_retry = ctx.recovery_retry;
    let review_dispatch_warning = ctx.review_dispatch_warning;
    let watcher_owner_channel_id = ctx.watcher_owner_channel_id;
    let tmux_last_offset = ctx.tmux_last_offset;

    let mut full_response = std::mem::take(state.full_response);
    let mut new_session_id = state.new_session_id.take();
    let mut new_raw_provider_session_id = state.new_raw_provider_session_id.take();
    let mut inflight_state = &mut *state.inflight_state;
    let mut api_friction_reports = std::mem::take(state.api_friction_reports);
    let mut terminal_empty_response_notice = state.terminal_empty_response_notice.take();
    let mut resume_failure_detected = *state.resume_failure_detected;
    let mut response_sent_offset = *state.response_sent_offset;
    let mut terminal_delivery_committed = *state.terminal_delivery_committed;
    let mut terminal_body_visible = *state.terminal_body_visible;
    let mut preserve_inflight_for_cleanup_retry = *state.preserve_inflight_for_cleanup_retry;
    let mut bridge_skip_holder_owns_inflight = *state.bridge_skip_holder_owns_inflight;

    match message {
        EmptyResponseRecoveryMessage::ResumeFailureAlreadyHandled => {}
        EmptyResponseRecoveryMessage::InspectEmptyResponse => {
        if full_response.is_empty() {
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
                            Some(guidance::empty_response_guidance(true).to_string());
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ Empty response: rx disconnected before any text \
                             (channel {}, output_path={:?}, last_offset={})",
                            channel_id,
                            inflight_state.output_path,
                            inflight_state.last_offset
                        );
                    } else {
                        terminal_empty_response_notice =
                            Some(guidance::empty_response_guidance(false).to_string());
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ Empty response: done without text (channel {})",
                            channel_id
                        );
                    }
                }
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
        let silent_turn_handled = if inflight_state.silent_turn {
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
                        channel_id = channel_id.get(),
                        "  [{ts}] 🌉 #3041 B2: delivery lease held by another holder — bridge silent_turn skipped offset advance, left turn retry-able (channel {})",
                        channel_id
                    );
                }
                BridgeLeaseAcquire::NoRange => {
                    // No offset to advance (zero/inverted range): the suppression
                    // resolves the (empty) range. B6 holds (no advance).
                }
            }
            true
        } else {
            false
        };

    *state.full_response = full_response;
    *state.new_session_id = new_session_id;
    *state.new_raw_provider_session_id = new_raw_provider_session_id;
    *state.api_friction_reports = api_friction_reports;
    *state.terminal_empty_response_notice = terminal_empty_response_notice;
    *state.resume_failure_detected = resume_failure_detected;
    *state.response_sent_offset = response_sent_offset;
    *state.terminal_delivery_committed = terminal_delivery_committed;
    *state.terminal_body_visible = terminal_body_visible;
    *state.preserve_inflight_for_cleanup_retry = preserve_inflight_for_cleanup_retry;
    *state.bridge_skip_holder_owns_inflight = bridge_skip_holder_owns_inflight;

    if silent_turn_handled {
        EmptyResponseRecoveryOutcome::SilentTurnHandled {
            delivery_response,
            spoken_delivery_response,
            resume_retry_queued,
        }
    } else {
        EmptyResponseRecoveryOutcome::ContinueDelivery {
            delivery_response,
            spoken_delivery_response,
            resume_retry_queued,
        }
    }
}
