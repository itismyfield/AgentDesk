use super::*;
use std::ops::ControlFlow;

// Edit the authorized status panel before the footer and response phases run.
pub(super) async fn refresh_status_panel(
    ctx: &runtime::TickContext<'_>,
    tick: &mut runtime::TickRuntime<'_>,
    state_dirty: &mut bool,
    status_msg_id: MessageId,
) {
    let runtime::TickContext {
        channel_id,
        gateway,
        provider,
        shared_owned,
        status_panel_started_at,
        ..
    } = *ctx;
    let provider = ProviderRef(provider);
    let panel_text = shared_owned.ui.placeholder_live_events.render_status_panel(
        channel_id,
        &provider,
        status_panel_started_at,
    );
    let panel_cache_invalidation_epoch = shared_owned
        .ui
        .placeholder_live_events
        .panel_cache_invalidation_epoch(channel_id, status_msg_id.get());
    if panel_cache_invalidation_epoch.is_some() || panel_text != *tick.last_status_panel_text {
        match TurnGateway::edit_message(gateway.as_ref(), channel_id, status_msg_id, &panel_text)
            .await
        {
            Ok(()) => {
                *tick.last_status_panel_text = panel_text;
                *tick.last_status_panel_edit = tokio::time::Instant::now();
                if let Some(epoch) = panel_cache_invalidation_epoch {
                    shared_owned
                        .ui
                        .placeholder_live_events
                        .clear_panel_cache_invalidation_if_epoch(
                            channel_id,
                            status_msg_id.get(),
                            epoch,
                        );
                }
                tick.inflight_state.status_message_id = Some(status_msg_id.get());
                *state_dirty = true;
            }
            Err(error) => {
                tracing::warn!(
                    target: "agentdesk::services::discord::turn_bridge::stream_tick",
                    "[turn_bridge] failed to edit status-panel-v2 message {} in channel {}: {}",
                    status_msg_id,
                    channel_id,
                    error
                );
            }
        }
    }
    *tick.status_panel_dirty = false;
}

// Create and bind rollover candidates in order before another candidate can be sent.
pub(super) async fn run_rollover_loop(
    ctx: &runtime::TickContext<'_>,
    tick: &mut runtime::TickRuntime<'_>,
    state_dirty: &mut bool,
    body_claim: super::super::terminal_controller_cutover::BodyClaim<'static>,
) -> ControlFlow<(), bool> {
    let runtime::TickContext {
        adk_session_key,
        channel_id,
        dispatch_id,
        done,
        gateway,
        provider,
        shared_owned,
        spinner,
        status_interval,
        status_panel_started_at,
        turn_id,
        ..
    } = *ctx;
    let provider = ProviderRef(provider);
    let turn_id = TurnIdRef(turn_id);
    // #3805 P2 (PR-D): track whether an answer rollover created a fresh
    // tail message this interval, so the two-message status panel is
    // re-anchored BELOW it exactly once (not on quiet intervals).
    let mut rolled_over_this_interval = false;
    loop {
        if !runtime::authorize_tick_mutation!(
            ctx,
            tick,
            state_dirty,
            "turn_bridge::stream_tick::rollover_freeze"
        ) {
            break;
        }
        let raw_current_portion =
            response_portion_after_offset(tick.full_response, *tick.response_sent_offset);
        // #3813 AC#1 tail: mark first-output pre-rollover (first_output<=first_relay).
        tick.bridge_spans
            .mark_first_output(!raw_current_portion.is_empty());
        if done || raw_current_portion.is_empty() {
            break;
        }

        let indicator = spinner[*tick.spin_idx % spinner.len()];
        let status_block = build_bridge_single_message_panel_status_block(
            shared_owned.as_ref(),
            channel_id,
            &provider,
            status_panel_started_at,
            indicator,
            tick.prev_tool_status.as_deref(),
            tick.current_tool_line.as_deref(),
            tick.full_response,
        );
        if bridge_streaming_rollover_should_skip(raw_current_portion) {
            break;
        }
        let rendered_current_portion =
            crate::services::discord::session_banner::with_discord_turn_session_banner_identity_prefix(
                shared_owned.as_ref(),
                channel_id,
                &provider,
                tick.inflight_state.user_msg_id,
                Some(&tick.inflight_state.started_at),
                tick.inflight_state.turn_start_offset,
                *tick.response_sent_offset == 0,
                raw_current_portion.to_string(),
            );
        let Some(plan) = super::super::super::formatting::plan_streaming_rollover(
            &rendered_current_portion,
            &status_block,
        ) else {
            break;
        };
        let raw_split_at =
            crate::services::discord::session_banner::raw_split_after_session_banner_prefix(
                plan.split_at,
                rendered_current_portion.len(),
                raw_current_portion.len(),
            );
        if raw_split_at == 0 {
            break;
        }
        let rollover_source_epoch = (
            *tick.current_msg_id,
            *tick.response_sent_offset,
            tick.inflight_state.current_msg_len,
        );

        match guarded_bridge_rollover_edit(
            gateway.as_ref(),
            &provider,
            channel_id,
            *tick.current_msg_id,
            raw_current_portion,
            &plan.frozen_chunk,
            Some(body_claim),
        )
        .await
        {
            Ok(GuardedRolloverEditOutcome::Clean) => {
                if !runtime::authorize_tick_mutation!(
                    ctx,
                    tick,
                    state_dirty,
                    "turn_bridge::stream_tick::rollover_send"
                ) || rollover_source_epoch
                    != (
                        *tick.current_msg_id,
                        *tick.response_sent_offset,
                        tick.inflight_state.current_msg_len,
                    )
                {
                    break;
                }
                let visible_anchor =
                    crate::services::discord::footer_retirement::visible_anchor(&status_block);
                match TurnGateway::send_message(gateway.as_ref(), channel_id, visible_anchor).await
                {
                    Ok(next_msg_id) => {
                        let next_response_sent_offset = *tick.response_sent_offset + raw_split_at;
                        assert_response_sent_offset_progress(
                            &provider,
                            channel_id,
                            dispatch_id.as_deref(),
                            adk_session_key.as_deref(),
                            &turn_id,
                            *tick.response_sent_offset,
                            next_response_sent_offset,
                            tick.full_response,
                            "src/services/discord/turn_bridge/mod.rs:rollover_response_sent_offset",
                        );
                        *tick.response_sent_offset = next_response_sent_offset;
                        *tick.bridge_confirmed_response_sent_offset = *tick.response_sent_offset;
                        tick.streaming_rollover_frozen_msg_ids
                            .push(*tick.current_msg_id);
                        mirror_frozen_prefix_ids(
                            tick.streaming_rollover_frozen_msg_ids,
                            tick.inflight_state,
                        );
                        *tick.current_msg_id = next_msg_id;
                        *tick.pending_current_message_candidate = Some(next_msg_id);
                        *tick.bridge_created_response_placeholder_msg_id = Some(next_msg_id);
                        rolled_over_this_interval = true;
                        *tick.last_edit_text = visible_anchor.to_string();
                        *tick.last_status_edit = tokio::time::Instant::now() - status_interval;
                        tick.inflight_state.current_msg_id =
                            durable_current_msg_id_from_detached(*tick.current_msg_id);
                        tick.inflight_state.current_msg_len = tick.last_edit_text.len();
                        tick.inflight_state.response_sent_offset = *tick.response_sent_offset;
                        tick.inflight_state.full_response = tick.full_response.clone();
                        // #3813 AC#1 tail: rollover send = bridge first relay.
                        tick.bridge_spans.mark_first_relay(true);
                        if let Some((_, _, _, _, pending_new_key)) =
                            tick.pending_long_running_retarget_after_state_save.as_mut()
                        {
                            *pending_new_key =
                                super::super::super::placeholder_controller::PlaceholderKey {
                                    provider: provider.clone(),
                                    channel_id,
                                    message_id: *tick.current_msg_id,
                                };
                        }
                        if let Some((pending_key, _, _, _)) =
                            tick.pending_long_running_open_after_state_save.as_mut()
                        {
                            pending_key.message_id = *tick.current_msg_id;
                        }
                        // #1255: rollover retargets the controller to the
                        // new message and detaches the old key first.
                        if let Some((old_key, snapshot, close_trigger, ack_consumed)) =
                            tick.long_running_placeholder_active.as_ref()
                        {
                            let new_key =
                                super::super::super::placeholder_controller::PlaceholderKey {
                                    provider: provider.clone(),
                                    channel_id,
                                    message_id: *tick.current_msg_id,
                                };
                            *tick.pending_long_running_retarget_after_state_save = Some((
                                old_key.clone(),
                                snapshot.clone(),
                                *close_trigger,
                                *ack_consumed,
                                new_key,
                            ));
                        }
                        // A rollover candidate must become durable before
                        // this loop may create its successor.  On retryable
                        // store failure the candidate remains the sole
                        // pending message; loop-exit settlement either
                        // binds or deterministically deletes it.
                        if !runtime::authorize_tick_mutation!(
                            ctx,
                            tick,
                            state_dirty,
                            "turn_bridge::stream_tick::rollover_candidate_bind"
                        ) {
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "agentdesk::services::discord::turn_bridge::stream_tick",
                            "[discord] failed to create rollover placeholder in channel {}: {}",
                            channel_id,
                            error
                        );
                        if runtime::authorize_tick_mutation!(
                            ctx,
                            tick,
                            state_dirty,
                            "turn_bridge::stream_tick::rollover_send_failure_restore"
                        ) && rollover_source_epoch
                            == (
                                *tick.current_msg_id,
                                *tick.response_sent_offset,
                                tick.inflight_state.current_msg_len,
                            )
                        {
                            let _ = TurnGateway::edit_message(
                                gateway.as_ref(),
                                channel_id,
                                *tick.current_msg_id,
                                &plan.display_snapshot,
                            )
                            .await;
                            *tick.last_edit_text = plan.display_snapshot;
                        }
                        break;
                    }
                }
            }
            Ok(GuardedRolloverEditOutcome::Held | GuardedRolloverEditOutcome::Blocked) => {
                break;
            }
            Err(error) => {
                tracing::warn!(
                    target: "agentdesk::services::discord::turn_bridge::stream_tick",
                    "[discord] failed to freeze rollover chunk for message {} in channel {}: {}",
                    *tick.current_msg_id,
                    channel_id,
                    error
                );
                break;
            }
        }
    }

    ControlFlow::Continue(rolled_over_this_interval)
}

// Refresh the panel and current response after rollover using the same mutation fences.
pub(super) async fn refresh_streaming_response(
    ctx: &runtime::TickContext<'_>,
    tick: &mut runtime::TickRuntime<'_>,
    state_dirty: &mut bool,
    rolled_over_this_interval: bool,
    indicator: &str,
    body_claim: super::super::terminal_controller_cutover::BodyClaim<'static>,
) -> ControlFlow<(), ()> {
    let runtime::TickContext {
        channel_id,
        done,
        gateway,
        provider,
        shared_owned,
        single_message_panel_footer_mode,
        status_interval,
        status_panel_started_at,
        ..
    } = *ctx;
    let provider = ProviderRef(provider);
    // #3805 P2 (PR-D): after a mid-turn answer rollover the live status
    // panel is now stranded ABOVE the new tail answer chunk. Under the
    // two-message flag, re-anchor it BELOW the new answer (send new,
    // retire old, bump the generation epoch) so it stays pinned to the
    // latest chunk. Gate is OFF-inert → the rollover path is
    // byte-identical when the flag is off.
    if rolled_over_this_interval
        && two_message_panel::two_message_should_reanchor_panel_on_rollover(
            shared_owned.ui.two_message_panel_enabled,
            tick.status_panel_msg_id.is_some(),
        )
        && runtime::authorize_tick_mutation!(
            ctx,
            tick,
            state_dirty,
            "turn_bridge::stream_tick::panel_reanchor"
        )
        && tick.status_panel_msg_id.is_some()
    {
        let panel_text = shared_owned.ui.placeholder_live_events.render_status_panel(
            channel_id,
            &provider,
            status_panel_started_at,
        );
        let _ = two_message_panel::reanchor_bridge_two_message_status_panel_below_answer(
            gateway.as_ref(),
            shared_owned.as_ref(),
            channel_id,
            &provider,
            &panel_text,
            *tick.current_msg_id,
            tick.status_panel_msg_id,
            tick.inflight_state,
            tick.status_panel_generation,
            tick.last_status_panel_text,
        )
        .await;
    }

    let streaming_edit_authorized = runtime::authorize_tick_mutation!(
        ctx,
        tick,
        state_dirty,
        "turn_bridge::stream_tick::streaming_edit"
    );
    let raw_current_portion =
        response_portion_after_offset(tick.full_response, *tick.response_sent_offset);
    let status_block = build_bridge_single_message_panel_status_block(
        shared_owned.as_ref(),
        channel_id,
        &provider,
        status_panel_started_at,
        indicator,
        tick.prev_tool_status.as_deref(),
        tick.current_tool_line.as_deref(),
        tick.full_response,
    );
    let rendered_current_portion =
        crate::services::discord::session_banner::with_discord_turn_session_banner_identity_prefix(
            shared_owned.as_ref(),
            channel_id,
            &provider,
            tick.inflight_state.user_msg_id,
            Some(&tick.inflight_state.started_at),
            tick.inflight_state.turn_start_offset,
            *tick.response_sent_offset == 0,
            raw_current_portion.to_string(),
        );
    let stable_display_text = build_turn_bridge_streaming_edit_text(
        shared_owned.ui.status_panel_v2_enabled,
        &rendered_current_portion,
        &status_block,
        &provider,
    );

    if streaming_edit_authorized
        && super::super::super::single_message_panel::streaming_footer_text_changed(
            single_message_panel_footer_mode,
            tick.last_edit_text,
            &stable_display_text,
        )
        && !done
        && bridge_streaming_edit_gate_open(
            tick.last_status_edit.elapsed() >= status_interval,
            *tick.first_answer_relayed,
            raw_current_portion.is_empty(),
        )
        && tick.long_running_placeholder_active.is_none()
        && tick.pending_long_running_open_after_state_save.is_none()
        && tick
            .pending_long_running_retarget_after_state_save
            .is_none()
    {
        let edit = || {
            let text = &stable_display_text;
            TurnGateway::edit_message(gateway.as_ref(), channel_id, *tick.current_msg_id, text)
        };
        let claim = (!raw_current_portion.trim().is_empty()).then_some(body_claim);
        let edit_ok = matches!(
            claim_then_send(claim, edit).await,
            Ok(BodySend::Sent(Ok(())))
        );
        *tick.last_status_edit = tokio::time::Instant::now();
        if edit_ok {
            *tick.first_answer_relayed |= !raw_current_portion.is_empty();
            // #3813 AC#1 tail: first bridge-owned relay delivered.
            tick.bridge_spans
                .mark_first_relay(!raw_current_portion.is_empty());
            *tick.last_edit_text = stable_display_text;
            tick.inflight_state.current_msg_id =
                durable_current_msg_id_from_detached(*tick.current_msg_id);
            tick.inflight_state.current_msg_len = tick.last_edit_text.len();
            tick.inflight_state.response_sent_offset = *tick.response_sent_offset;
            tick.inflight_state.full_response = tick.full_response.clone();
            *state_dirty = true;
        }
    }
    ControlFlow::Continue(())
}

#[cfg(test)]
#[path = "footer_retirement_raw_history_tests.rs"]
mod footer_retirement_raw_history_tests;
