//! #4230 S1 stream/status tick helper for the turn bridge.

use std::ops::Deref;
use std::sync::Arc;

use super::*;

pub(super) type LongRunningPlaceholderActive = Option<(
    super::super::placeholder_controller::PlaceholderKey,
    super::super::placeholder_controller::PlaceholderActiveInput,
    super::super::formatting::LongRunningCloseTrigger,
    bool,
)>;
pub(super) type PendingLongRunningOpenAfterStateSave = LongRunningPlaceholderActive;
pub(super) type PendingLongRunningRetargetAfterStateSave = Option<(
    super::super::placeholder_controller::PlaceholderKey,
    super::super::placeholder_controller::PlaceholderActiveInput,
    super::super::formatting::LongRunningCloseTrigger,
    bool,
    super::super::placeholder_controller::PlaceholderKey,
)>;

struct ProviderRef<'a>(&'a ProviderKind);

impl Deref for ProviderRef<'_> {
    type Target = ProviderKind;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl ProviderRef<'_> {
    fn clone(&self) -> ProviderKind {
        self.0.clone()
    }
}

struct TurnIdRef<'a>(&'a str);

impl Deref for TurnIdRef<'_> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl TurnIdRef<'_> {
    fn as_str(&self) -> &str {
        self.0
    }
}

struct FullResponseRef<'a>(&'a str);

impl Deref for FullResponseRef<'_> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl FullResponseRef<'_> {
    fn clone(&self) -> String {
        self.0.to_string()
    }
}

pub(super) struct BridgeStreamTickContext<'a> {
    pub(super) shared_owned: Arc<SharedData>,
    pub(super) gateway: Arc<dyn TurnGateway>,
    pub(super) channel_id: ChannelId,
    pub(super) provider: &'a ProviderKind,
    pub(super) turn_id: &'a str,
    pub(super) status_interval: std::time::Duration,
    pub(super) single_message_panel_footer_mode: bool,
    pub(super) footer_owner: super::super::footer_view_reconciler::CompletionFooterOwner,
    pub(super) status_panel_started_at: i64,
    pub(super) done: bool,
    pub(super) standby_relay_owns_output: bool,
    pub(super) watcher_owner_channel_id: ChannelId,
    pub(super) full_response: &'a str,
    pub(super) dispatch_id: Option<String>,
    pub(super) adk_session_key: Option<String>,
    pub(super) adk_session_name: Option<String>,
    pub(super) adk_session_info: Option<String>,
    pub(super) adk_cwd: Option<String>,
    pub(super) role_binding: Option<RoleBinding>,
    pub(super) current_tool_line: Option<String>,
    pub(super) last_tool_name: Option<String>,
    pub(super) last_tool_summary: Option<String>,
    pub(super) prev_tool_status: Option<String>,
    pub(super) spinner: &'static [&'static str],
    pub(super) live_long_run_heartbeat_interval: std::time::Duration,
}

pub(super) struct BridgeStreamTickState<'a> {
    pub(super) state_dirty: &'a mut bool,
    pub(super) last_session_panel_lifecycle_refresh: &'a mut tokio::time::Instant,
    pub(super) status_panel_dirty: &'a mut bool,
    pub(super) spin_idx: &'a mut usize,
    pub(super) last_status_panel_edit: &'a mut tokio::time::Instant,
    pub(super) last_status_edit: &'a mut tokio::time::Instant,
    pub(super) status_panel_msg_id: &'a mut Option<MessageId>,
    pub(super) last_status_panel_text: &'a mut String,
    pub(super) watcher_owns_assistant_relay: &'a mut bool,
    pub(super) watcher_relay_available_for_turn: &'a mut bool,
    pub(super) response_sent_offset: &'a mut usize,
    pub(super) bridge_confirmed_response_sent_offset: &'a mut usize,
    pub(super) streaming_rollover_frozen_msg_ids: &'a mut Vec<MessageId>,
    pub(super) current_msg_id: &'a mut MessageId,
    pub(super) last_edit_text: &'a mut String,
    pub(super) first_answer_relayed: &'a mut bool,
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) bridge_spans: &'a mut BridgeLatencySpans,
    pub(super) status_panel_generation: &'a mut u64,
    pub(super) pending_long_running_open_after_state_save:
        &'a mut PendingLongRunningOpenAfterStateSave,
    pub(super) pending_long_running_retarget_after_state_save:
        &'a mut PendingLongRunningRetargetAfterStateSave,
    pub(super) long_running_placeholder_active: &'a mut LongRunningPlaceholderActive,
    pub(super) last_adk_heartbeat: &'a mut std::time::Instant,
    pub(super) last_inflight_long_run_heartbeat: &'a mut std::time::Instant,
}

pub(super) async fn run_bridge_stream_tick(
    ctx: BridgeStreamTickContext<'_>,
    state: BridgeStreamTickState<'_>,
) {
    let shared_owned = ctx.shared_owned;
    let gateway = ctx.gateway;
    let channel_id = ctx.channel_id;
    let provider = ProviderRef(ctx.provider);
    let turn_id = TurnIdRef(ctx.turn_id);
    let status_interval = ctx.status_interval;
    let single_message_panel_footer_mode = ctx.single_message_panel_footer_mode;
    let footer_owner = ctx.footer_owner;
    let status_panel_started_at = ctx.status_panel_started_at;
    let done = ctx.done;
    let standby_relay_owns_output = ctx.standby_relay_owns_output;
    let watcher_owner_channel_id = ctx.watcher_owner_channel_id;
    let full_response = FullResponseRef(ctx.full_response);
    let dispatch_id = ctx.dispatch_id;
    let adk_session_key = ctx.adk_session_key;
    let adk_session_name = ctx.adk_session_name;
    let adk_session_info = ctx.adk_session_info;
    let adk_cwd = ctx.adk_cwd;
    let role_binding = ctx.role_binding;
    let current_tool_line = ctx.current_tool_line;
    let last_tool_name = ctx.last_tool_name;
    let last_tool_summary = ctx.last_tool_summary;
    let prev_tool_status = ctx.prev_tool_status;
    let spinner = ctx.spinner;
    let live_long_run_heartbeat_interval = ctx.live_long_run_heartbeat_interval;

    let mut state_dirty = *state.state_dirty;
    let mut last_session_panel_lifecycle_refresh = *state.last_session_panel_lifecycle_refresh;
    let mut status_panel_dirty = *state.status_panel_dirty;
    let mut spin_idx = *state.spin_idx;
    let mut last_status_panel_edit = *state.last_status_panel_edit;
    let mut last_status_edit = *state.last_status_edit;
    let mut status_panel_msg_id = *state.status_panel_msg_id;
    let mut last_status_panel_text = std::mem::take(state.last_status_panel_text);
    let mut watcher_owns_assistant_relay = *state.watcher_owns_assistant_relay;
    let mut watcher_relay_available_for_turn = *state.watcher_relay_available_for_turn;
    let mut response_sent_offset = *state.response_sent_offset;
    let mut bridge_confirmed_response_sent_offset = *state.bridge_confirmed_response_sent_offset;
    let mut streaming_rollover_frozen_msg_ids =
        std::mem::take(state.streaming_rollover_frozen_msg_ids);
    let mut current_msg_id = *state.current_msg_id;
    let mut last_edit_text = std::mem::take(state.last_edit_text);
    let mut first_answer_relayed = *state.first_answer_relayed;
    let inflight_state = &mut *state.inflight_state;
    let bridge_spans = &mut *state.bridge_spans;
    let mut status_panel_generation = *state.status_panel_generation;
    let mut pending_long_running_open_after_state_save =
        state.pending_long_running_open_after_state_save.take();
    let mut pending_long_running_retarget_after_state_save =
        state.pending_long_running_retarget_after_state_save.take();
    let mut long_running_placeholder_active = state.long_running_placeholder_active.take();
    let mut last_adk_heartbeat = *state.last_adk_heartbeat;
    let mut last_inflight_long_run_heartbeat = *state.last_inflight_long_run_heartbeat;

    if shared_owned.ui.status_panel_v2_enabled
        && last_session_panel_lifecycle_refresh.elapsed() >= status_interval
    {
        last_session_panel_lifecycle_refresh = tokio::time::Instant::now();
        status_panel_dirty |= refresh_session_panel_line_from_lifecycle(
            shared_owned.as_ref(),
            channel_id,
            turn_id.as_str(),
            inflight_state.tmux_session_name.as_deref(),
            &provider, // #3983 item4: one-shot session banner render
        )
        .await;
    }

    let indicator = spinner[spin_idx % spinner.len()];
    spin_idx += 1;

    // #3813 Phase 2: hold the status-panel / footer edit off the shared
    // rate lane while the opening answer is pending so the #4006 fast lane
    // wins it. `status_panel_dirty` stays set → renders next interval. See
    // status_panel_edit_defer_for_first_answer for the #3477 guard.
    let defer_status_panel_for_first_answer = status_panel_edit_defer_for_first_answer(
        first_answer_relayed,
        !response_portion_after_offset(&full_response, response_sent_offset).is_empty(),
    );

    if shared_owned.ui.status_panel_v2_enabled
        && bridge_status_panel_dirty_should_edit_separate_panel(
            status_panel_dirty,
            single_message_panel_footer_mode,
        )
        && !defer_status_panel_for_first_answer
        && last_status_panel_edit.elapsed() >= status_interval
        && let Some(status_msg_id) = status_panel_msg_id
    {
        let panel_text = shared_owned.ui.placeholder_live_events.render_status_panel(
            channel_id,
            &provider,
            status_panel_started_at,
        );
        if panel_text != last_status_panel_text {
            match gateway
                .edit_message(channel_id, status_msg_id, &panel_text)
                .await
            {
                Ok(()) => {
                    last_status_panel_text = panel_text;
                    last_status_panel_edit = tokio::time::Instant::now();
                    inflight_state.status_message_id = Some(status_msg_id.get());
                    state_dirty = true;
                }
                Err(error) => {
                    tracing::warn!(
                        "[turn_bridge] failed to edit status-panel-v2 message {} in channel {}: {}",
                        status_msg_id,
                        channel_id,
                        error
                    );
                }
            }
        }
        status_panel_dirty = false;
    }
    if single_message_panel_footer_mode
        && status_panel_dirty
        && !defer_status_panel_for_first_answer
        && last_status_panel_edit.elapsed() >= status_interval
    {
        refresh_bridge_footer(shared_owned.as_ref(), channel_id, footer_owner, indicator).await;
        last_status_panel_edit = tokio::time::Instant::now();
        status_panel_dirty = false;
    }
    if !watcher_owns_assistant_relay && !standby_relay_owns_output {
        // #3805 P2 (PR-D): track whether an answer rollover created a fresh
        // tail message this interval, so the two-message status panel is
        // re-anchored BELOW it exactly once (not on quiet intervals).
        let mut rolled_over_this_interval = false;
        loop {
            let current_portion =
                response_portion_after_offset(&full_response, response_sent_offset);
            // #3813 AC#1 tail: mark first-output pre-rollover (first_output<=first_relay).
            bridge_spans.mark_first_output(!current_portion.is_empty());
            if done || current_portion.is_empty() {
                break;
            }

            let indicator = spinner[spin_idx % spinner.len()];
            let status_block = build_bridge_single_message_panel_status_block(
                shared_owned.as_ref(),
                channel_id,
                &provider,
                status_panel_started_at,
                indicator,
                prev_tool_status.as_deref(),
                current_tool_line.as_deref(),
                &full_response,
            );
            if bridge_streaming_rollover_should_skip(current_portion) {
                break;
            }
            let Some(plan) =
                super::super::formatting::plan_streaming_rollover(current_portion, &status_block)
            else {
                break;
            };

            match gateway
                .edit_message(channel_id, current_msg_id, &plan.frozen_chunk)
                .await
            {
                Ok(()) => match gateway.send_message(channel_id, &status_block).await {
                    Ok(next_msg_id) => {
                        let next_response_sent_offset = response_sent_offset + plan.split_at;
                        assert_response_sent_offset_progress(
                            &provider,
                            channel_id,
                            dispatch_id.as_deref(),
                            adk_session_key.as_deref(),
                            &turn_id,
                            response_sent_offset,
                            next_response_sent_offset,
                            &full_response,
                            "src/services/discord/turn_bridge/mod.rs:rollover_response_sent_offset",
                        );
                        response_sent_offset = next_response_sent_offset;
                        bridge_confirmed_response_sent_offset = response_sent_offset;
                        streaming_rollover_frozen_msg_ids.push(current_msg_id);
                        mirror_frozen_prefix_ids(
                            &streaming_rollover_frozen_msg_ids,
                            &mut *inflight_state,
                        );
                        current_msg_id = next_msg_id;
                        rolled_over_this_interval = true;
                        last_edit_text = status_block;
                        last_status_edit = tokio::time::Instant::now() - status_interval;
                        inflight_state.current_msg_id = current_msg_id.get();
                        inflight_state.current_msg_len = last_edit_text.len();
                        inflight_state.response_sent_offset = response_sent_offset;
                        inflight_state.full_response = full_response.clone();
                        state_dirty = true;
                        // #3813 AC#1 tail: rollover send = bridge first relay.
                        bridge_spans.mark_first_relay(true);
                        if let Some((_, _, _, _, pending_new_key)) =
                            pending_long_running_retarget_after_state_save.as_mut()
                        {
                            *pending_new_key =
                                super::super::placeholder_controller::PlaceholderKey {
                                    provider: provider.clone(),
                                    channel_id,
                                    message_id: current_msg_id,
                                };
                        }
                        if let Some((pending_key, _, _, _)) =
                            pending_long_running_open_after_state_save.as_mut()
                        {
                            pending_key.message_id = current_msg_id;
                        }
                        // #1255: rollover retargets the controller to the
                        // new message and detaches the old key first.
                        if let Some((old_key, snapshot, close_trigger, ack_consumed)) =
                            long_running_placeholder_active.as_ref()
                        {
                            let new_key = super::super::placeholder_controller::PlaceholderKey {
                                provider: provider.clone(),
                                channel_id,
                                message_id: current_msg_id,
                            };
                            pending_long_running_retarget_after_state_save = Some((
                                old_key.clone(),
                                snapshot.clone(),
                                *close_trigger,
                                *ack_consumed,
                                new_key,
                            ));
                            state_dirty = true;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            "[discord] failed to create rollover placeholder in channel {}: {}",
                            channel_id,
                            error
                        );
                        let _ = gateway
                            .edit_message(channel_id, current_msg_id, &plan.display_snapshot)
                            .await;
                        last_edit_text = plan.display_snapshot;
                        break;
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        "[discord] failed to freeze rollover chunk for message {} in channel {}: {}",
                        current_msg_id,
                        channel_id,
                        error
                    );
                    break;
                }
            }
        }

        // #3805 P2 (PR-D): after a mid-turn answer rollover the live status
        // panel is now stranded ABOVE the new tail answer chunk. Under the
        // two-message flag, re-anchor it BELOW the new answer (send new,
        // retire old, bump the generation epoch) so it stays pinned to the
        // latest chunk. Gate is OFF-inert → the rollover path is
        // byte-identical when the flag is off.
        if rolled_over_this_interval
            && two_message_panel::two_message_should_reanchor_panel_on_rollover(
                shared_owned.ui.two_message_panel_enabled,
                status_panel_msg_id.is_some(),
            )
        {
            let panel_text = shared_owned.ui.placeholder_live_events.render_status_panel(
                channel_id,
                &provider,
                status_panel_started_at,
            );
            let reanchored =
                two_message_panel::reanchor_bridge_two_message_status_panel_below_answer(
                    gateway.as_ref(),
                    shared_owned.as_ref(),
                    channel_id,
                    &provider,
                    &panel_text,
                    current_msg_id,
                    &mut status_panel_msg_id,
                    &mut *inflight_state,
                    &mut status_panel_generation,
                    &mut last_status_panel_text,
                )
                .await;
            if reanchored {
                state_dirty = true;
            }
        }

        let current_portion = response_portion_after_offset(&full_response, response_sent_offset);
        let status_block = build_bridge_single_message_panel_status_block(
            shared_owned.as_ref(),
            channel_id,
            &provider,
            status_panel_started_at,
            indicator,
            prev_tool_status.as_deref(),
            current_tool_line.as_deref(),
            &full_response,
        );
        let stable_display_text = build_turn_bridge_streaming_edit_text(
            shared_owned.ui.status_panel_v2_enabled,
            current_portion,
            &status_block,
            &provider,
        );

        if super::super::single_message_panel::streaming_footer_text_changed(
            single_message_panel_footer_mode,
            &last_edit_text,
            &stable_display_text,
        ) && !done
            && bridge_streaming_edit_gate_open(
                last_status_edit.elapsed() >= status_interval,
                first_answer_relayed,
                current_portion.is_empty(),
            )
            && long_running_placeholder_active.is_none()
            && pending_long_running_open_after_state_save.is_none()
            && pending_long_running_retarget_after_state_save.is_none()
        {
            let edit_ok = TurnGateway::edit_message(
                gateway.as_ref(),
                channel_id,
                current_msg_id,
                &stable_display_text,
            )
            .await
            .is_ok();
            last_status_edit = tokio::time::Instant::now();
            if edit_ok {
                first_answer_relayed |= !current_portion.is_empty();
                // #3813 AC#1 tail: first bridge-owned relay delivered.
                bridge_spans.mark_first_relay(!current_portion.is_empty());
                last_edit_text = stable_display_text;
                inflight_state.current_msg_id = current_msg_id.get();
                inflight_state.current_msg_len = last_edit_text.len();
                inflight_state.response_sent_offset = response_sent_offset;
                inflight_state.full_response = full_response.clone();
                state_dirty = true;
            }
        }
    }

    if shared_owned.ui.placeholder_live_events_enabled
        && watcher_owns_assistant_relay
        && let Some((key, input, _, _)) = long_running_placeholder_active.as_ref()
        && let Some(block) = shared_owned
            .ui
            .placeholder_live_events
            .render_block(channel_id)
    {
        let outcome = shared_owned
            .ui
            .placeholder_controller
            .ensure_active_with_live_events(gateway.as_ref(), key.clone(), input.clone(), block)
            .await;
        if matches!(
            outcome,
            super::super::placeholder_controller::PlaceholderControllerOutcome::Edited
        ) {
            state_dirty = true;
        }
    }

    if bridge_should_reclaim_relay_from_missing_watcher(
        watcher_owns_assistant_relay,
        standby_relay_owns_output,
        live_watcher_registered_for_relay(shared_owned.as_ref(), watcher_owner_channel_id),
    ) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ turn_bridge reclaiming assistant relay for channel {} after watcher disappeared",
            channel_id.get()
        );
        watcher_owns_assistant_relay = false;
        watcher_relay_available_for_turn = false;
        inflight_state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::None);
        rewind_and_persist_delivery_on_reclaim(
            &full_response,
            bridge_confirmed_response_sent_offset,
            &mut response_sent_offset,
            &mut *inflight_state,
            channel_id,
        );
        state_dirty = true;
    }

    if state_dirty
        || pending_long_running_open_after_state_save.is_some()
        || pending_long_running_retarget_after_state_save.is_some()
        || inflight_state.current_tool_line != current_tool_line
        || inflight_state.last_tool_name != last_tool_name
        || inflight_state.last_tool_summary != last_tool_summary
        || inflight_state.prev_tool_status != prev_tool_status
    {
        inflight_state.current_tool_line = current_tool_line.clone();
        inflight_state.last_tool_name = last_tool_name.clone();
        inflight_state.last_tool_summary = last_tool_summary.clone();
        inflight_state.prev_tool_status = prev_tool_status.clone();
        match save_inflight_state(&*inflight_state) {
            Ok(()) => {
                if let Some((key, snapshot, close_trigger, ack_consumed)) =
                    pending_long_running_open_after_state_save.take()
                {
                    if key.message_id == current_msg_id && long_running_placeholder_active.is_none()
                    {
                        let outcome = ensure_active_placeholder_card(
                            shared_owned.as_ref(),
                            gateway.as_ref(),
                            key.clone(),
                            snapshot.clone(),
                        )
                        .await;
                        use super::super::placeholder_controller::PlaceholderControllerOutcome::*;
                        if matches!(outcome, Edited | Coalesced) {
                            long_running_placeholder_active =
                                Some((key, snapshot, close_trigger, ack_consumed));
                        } else {
                            inflight_state.long_running_placeholder_active = false;
                            if let Err(error) = save_inflight_state(&*inflight_state) {
                                tracing::warn!(
                                    "[turn_bridge] failed to persist long-running placeholder open failure in channel {}: {}",
                                    channel_id,
                                    error
                                );
                            }
                        }
                    } else {
                        inflight_state.long_running_placeholder_active = false;
                        if let Err(error) = save_inflight_state(&*inflight_state) {
                            tracing::warn!(
                                "[turn_bridge] failed to persist stale long-running placeholder open drop in channel {}: {}",
                                channel_id,
                                error
                            );
                        }
                    }
                }
                if let Some((old_key, snapshot, close_trigger, ack_consumed, new_key)) =
                    pending_long_running_retarget_after_state_save.take()
                {
                    let active_still_matches_old_key = long_running_placeholder_active
                        .as_ref()
                        .is_some_and(|(active_key, _, _, _)| *active_key == old_key);
                    if active_still_matches_old_key {
                        shared_owned.ui.placeholder_controller.detach(&old_key);
                        let outcome = ensure_active_placeholder_card(
                            shared_owned.as_ref(),
                            gateway.as_ref(),
                            new_key.clone(),
                            snapshot.clone(),
                        )
                        .await;
                        use super::super::placeholder_controller::PlaceholderControllerOutcome::*;
                        if matches!(outcome, Edited | Coalesced) {
                            long_running_placeholder_active =
                                Some((new_key, snapshot, close_trigger, ack_consumed));
                        } else {
                            // Retarget edit failed — drop the flag so the
                            // regular streaming loop and sweeper resume
                            // normal handling.
                            long_running_placeholder_active = None;
                            inflight_state.long_running_placeholder_active = false;
                            if let Err(error) = save_inflight_state(&*inflight_state) {
                                tracing::warn!(
                                    "[turn_bridge] failed to persist long-running placeholder retarget failure in channel {}: {}",
                                    channel_id,
                                    error
                                );
                            }
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    "[turn_bridge] failed to persist inflight state before moving placeholder pin in channel {}: {}",
                    channel_id,
                    error
                );
            }
        }
    }

    if last_adk_heartbeat.elapsed() >= std::time::Duration::from_secs(30) {
        post_adk_session_status(
            adk_session_key.as_deref(),
            adk_session_name.as_deref(),
            Some(provider.as_str()),
            "working",
            &provider,
            adk_session_info.as_deref(),
            None,
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
        last_adk_heartbeat = std::time::Instant::now();
    }

    // codex round-8 P1: keep `placeholder_sweeper` from abandoning a
    // healthy long-running tool wait by bumping inflight mtime every
    // 30s while a placeholder is owned. If the turn dies, this loop
    // stops firing → mtime stops advancing → sweeper can abandon
    // normally past `ABANDON_THRESHOLD_SECS`.
    if long_running_placeholder_active.is_some()
        && last_inflight_long_run_heartbeat.elapsed() >= live_long_run_heartbeat_interval
    {
        inflight_state.updated_at = chrono::Utc::now().to_rfc3339();
        let _ = save_inflight_state(&*inflight_state);
        last_inflight_long_run_heartbeat = std::time::Instant::now();
    }

    *state.state_dirty = state_dirty;
    *state.last_session_panel_lifecycle_refresh = last_session_panel_lifecycle_refresh;
    *state.status_panel_dirty = status_panel_dirty;
    *state.spin_idx = spin_idx;
    *state.last_status_panel_edit = last_status_panel_edit;
    *state.last_status_edit = last_status_edit;
    *state.status_panel_msg_id = status_panel_msg_id;
    *state.last_status_panel_text = last_status_panel_text;
    *state.watcher_owns_assistant_relay = watcher_owns_assistant_relay;
    *state.watcher_relay_available_for_turn = watcher_relay_available_for_turn;
    *state.response_sent_offset = response_sent_offset;
    *state.bridge_confirmed_response_sent_offset = bridge_confirmed_response_sent_offset;
    *state.streaming_rollover_frozen_msg_ids = streaming_rollover_frozen_msg_ids;
    *state.current_msg_id = current_msg_id;
    *state.last_edit_text = last_edit_text;
    *state.first_answer_relayed = first_answer_relayed;
    *state.status_panel_generation = status_panel_generation;
    *state.pending_long_running_open_after_state_save = pending_long_running_open_after_state_save;
    *state.pending_long_running_retarget_after_state_save =
        pending_long_running_retarget_after_state_save;
    *state.long_running_placeholder_active = long_running_placeholder_active;
    *state.last_adk_heartbeat = last_adk_heartbeat;
    *state.last_inflight_long_run_heartbeat = last_inflight_long_run_heartbeat;
}
