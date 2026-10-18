use super::*;
use std::ops::ControlFlow;

pub(super) struct TickContext<'a> {
    pub(super) shared_owned: &'a Arc<SharedData>,
    pub(super) gateway: &'a Arc<dyn TurnGateway>,
    pub(super) channel_id: ChannelId,
    pub(super) provider: &'a ProviderKind,
    pub(super) turn_id: &'a str,
    pub(super) stream_tick_expected: &'a crate::services::discord::inflight::InflightTurnIdentity,
    pub(super) status_interval: std::time::Duration,
    pub(super) single_message_panel_footer_mode: bool,
    pub(super) status_panel_started_at: i64,
    pub(super) done: bool,
    pub(super) dispatch_id: &'a Option<String>,
    pub(super) adk_session_key: &'a Option<String>,
    pub(super) spinner: &'static [&'static str],
}

pub(super) struct TickRuntime<'a> {
    pub(super) spin_idx: &'a mut usize,
    pub(super) last_status_panel_text: &'a mut String,
    pub(super) last_status_panel_edit: &'a mut tokio::time::Instant,
    pub(super) status_panel_dirty: &'a mut bool,
    pub(super) full_response: &'a mut String,
    pub(super) response_sent_offset: &'a mut usize,
    pub(super) bridge_confirmed_response_sent_offset: &'a mut usize,
    pub(super) current_msg_id: &'a mut MessageId,
    pub(super) current_tool_line: &'a mut Option<String>,
    pub(super) prev_tool_status: &'a mut Option<String>,
    pub(super) last_tool_name: &'a mut Option<String>,
    pub(super) last_tool_summary: &'a mut Option<String>,
    pub(super) any_tool_used: &'a mut bool,
    pub(super) has_post_tool_text: &'a mut bool,
    pub(super) streaming_rollover_frozen_msg_ids: &'a mut Vec<MessageId>,
    pub(super) tmux_last_offset: &'a mut Option<u64>,
    pub(super) watcher_owner_channel_id: &'a mut ChannelId,
    pub(super) watcher_owns_assistant_relay: &'a mut bool,
    pub(super) watcher_relay_available_for_turn: &'a mut bool,
    pub(super) watcher_delivery_pin: &'a mut Option<WatcherClaimIncarnation>,
    pub(super) standby_relay_owns_output: &'a mut bool,
    pub(super) status_panel_msg_id: &'a mut Option<MessageId>,
    pub(super) status_panel_generation: &'a mut u64,
    pub(super) last_edit_text: &'a mut String,
    pub(super) last_status_edit: &'a mut tokio::time::Instant,
    pub(super) first_answer_relayed: &'a mut bool,
    pub(super) pending_long_running_open_after_state_save:
        &'a mut PendingLongRunningOpenAfterStateSave,
    pub(super) pending_long_running_retarget_after_state_save:
        &'a mut PendingLongRunningRetargetAfterStateSave,
    pub(super) long_running_placeholder_active: &'a mut LongRunningPlaceholderActive,
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) persisted_inflight_baseline: &'a mut InflightTurnState,
    pub(super) expected_current_message: &'a mut (u64, usize),
    pub(super) pending_current_message_candidate: &'a mut Option<MessageId>,
    pub(super) bridge_created_response_placeholder_msg_id: &'a mut Option<MessageId>,
    pub(super) bridge_spans: &'a mut BridgeLatencySpans,
}

// Reconcile detached locals after a guarded save so subsequent phases see the durable winner.
pub(super) fn reconcile_tick_runtime_from_inflight(
    shared_owned: &SharedData,
    tick: &mut TickRuntime<'_>,
    current_msg_id_before_save: MessageId,
) {
    let mut runtime = super::super::bridge_entry_persist::BridgeEntryRuntimeState {
        inflight_state: tick.inflight_state,
        full_response: tick.full_response,
        response_sent_offset: tick.response_sent_offset,
        bridge_confirmed_response_sent_offset: tick.bridge_confirmed_response_sent_offset,
        current_msg_id: tick.current_msg_id,
        current_tool_line: tick.current_tool_line,
        prev_tool_status: tick.prev_tool_status,
        last_tool_name: tick.last_tool_name,
        last_tool_summary: tick.last_tool_summary,
        any_tool_used: tick.any_tool_used,
        has_post_tool_text: tick.has_post_tool_text,
        streaming_rollover_frozen_msg_ids: tick.streaming_rollover_frozen_msg_ids,
        tmux_last_offset: tick.tmux_last_offset,
        watcher_owner_channel_id: tick.watcher_owner_channel_id,
        watcher_owns_assistant_relay: tick.watcher_owns_assistant_relay,
        watcher_relay_available_for_turn: tick.watcher_relay_available_for_turn,
        watcher_delivery_pin: tick.watcher_delivery_pin,
        standby_relay_owns_output: tick.standby_relay_owns_output,
        status_panel_msg_id: tick.status_panel_msg_id,
        status_panel_generation: tick.status_panel_generation,
    };
    super::super::bridge_entry_persist::reconcile_runtime_locals_from_inflight_state(
        shared_owned,
        &mut runtime,
    );
    super::super::bridge_entry_persist::clear_last_edit_text_if_current_message_changed(
        current_msg_id_before_save,
        *tick.current_msg_id,
        tick.last_edit_text,
    );
}

// Fence each visible mutation and report authority loss for the caller to write back and return.
pub(super) async fn authorize_visible_mutation(
    ctx: &TickContext<'_>,
    tick: &mut TickRuntime<'_>,
    caller: &str,
) -> (bool, ControlFlow<(), bool>) {
    let TickContext {
        channel_id,
        gateway,
        provider,
        shared_owned,
        stream_tick_expected,
        ..
    } = *ctx;
    let provider = ProviderRef(provider);
    tick.inflight_state
        .full_response
        .clone_from(tick.full_response);
    tick.inflight_state.response_sent_offset = *tick.response_sent_offset;
    tick.inflight_state.any_tool_used = *tick.any_tool_used;
    tick.inflight_state.has_post_tool_text = *tick.has_post_tool_text;
    sync_stream_tick_tool_fields(
        tick.inflight_state,
        tick.current_tool_line,
        tick.prev_tool_status,
        tick.last_tool_name,
        tick.last_tool_summary,
    );
    let intended_authority =
        crate::services::discord::inflight::StreamRelayAuthority::from_state(tick.inflight_state);
    let current_msg_id_before_fence = *tick.current_msg_id;
    let guarded_outcome = fence_stream_tick_visible_mutation_with_candidate_cleanup(
        StreamTickCandidateSaveContext {
            gateway: gateway.as_ref(),
            provider: &provider,
            token_hash: &shared_owned.token_hash,
            channel_id,
            persisted_baseline: tick.persisted_inflight_baseline,
            inflight_state: tick.inflight_state,
            expected_identity: stream_tick_expected,
            expected_current_message: tick.expected_current_message,
            current_msg_id: tick.current_msg_id,
            pending_current_message_candidate: tick.pending_current_message_candidate,
            bridge_created_response_placeholder_msg_id: tick
                .bridge_created_response_placeholder_msg_id,
        },
        caller,
    )
    .await;
    let state_dirty = dirty_after_guarded_save(guarded_outcome);
    if guarded_outcome == GuardedSaveOutcome::Saved
        || (guarded_outcome.is_identity_mismatch_legacy()
            && stream_tick_expected.matches_state(tick.inflight_state))
    {
        reconcile_tick_runtime_from_inflight(
            shared_owned.as_ref(),
            tick,
            current_msg_id_before_fence,
        );
    }
    let allowed = match visible_mutation_authority_after_guarded_save(
        guarded_outcome,
        tick.inflight_state,
        intended_authority,
    )
    .mutation_permission()
    {
        Some(allowed) => allowed,
        None => {
            // `durable_relay_owner_kind="none"` means "the BRIDGE owns
            // the relay" (`StreamRelayAuthority::bridge_owns_relay`), NOT
            // "nobody owns it" — read `bridge_owns_relay` for the
            // disposition and never treat `none` as a missing owner.
            tracing::warn!(
                target: "agentdesk::services::discord::turn_bridge::stream_tick",
                channel_id = channel_id.get(),
                caller = caller,
                ?guarded_outcome,
                durable_relay_owner_kind =
                    tick.inflight_state.effective_relay_owner_kind().as_str(),
                bridge_owns_relay =
                    crate::services::discord::inflight::StreamRelayAuthority::from_state(
                        tick.inflight_state,
                    )
                    .bridge_owns_relay(),
                "stream tick lost durable Discord mutation authority"
            );
            return (state_dirty, ControlFlow::Break(()));
        }
    };
    (state_dirty, ControlFlow::Continue(allowed))
}

macro_rules! authorize_tick_mutation {
    ($ctx:expr, $tick:expr, $state_dirty:expr, $caller:expr) => {{
        let (dirty, permission) = runtime::authorize_visible_mutation($ctx, $tick, $caller).await;
        *$state_dirty = dirty;
        match permission {
            ControlFlow::Continue(allowed) => allowed,
            ControlFlow::Break(()) => return ControlFlow::Break(()),
        }
    }};
}
pub(super) use authorize_tick_mutation;
