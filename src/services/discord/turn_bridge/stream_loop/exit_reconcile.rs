use super::super::stream_tick::guarded_persist::{
    StreamTickCandidateSaveContext, settle_pending_current_message_candidate_on_loop_exit,
};
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord::turn_bridge) enum StreamLoopOutcome {
    Completed,
    AuthorityLost,
}

pub(super) fn stream_loop_should_continue(
    done: bool,
    terminal_control_drain_until: Option<std::time::Instant>,
    runtime_handoff_retry_retained: bool,
    guarded_tool_frame_retry_retained: bool,
    now: std::time::Instant,
) -> bool {
    !done
        || runtime_handoff_retry_retained
        || guarded_tool_frame_retry_retained
        || terminal_control_drain_until.is_some_and(|deadline| now < deadline)
}

pub(super) fn should_exit_completed_turn_on_cancel(
    done: bool,
    cancel_requested: bool,
    guarded_tool_frame_retry_retained: bool,
) -> bool {
    done && cancel_requested && !guarded_tool_frame_retry_retained
}

pub(super) const RETAINED_STREAM_RETRY_BACKOFF: std::time::Duration =
    std::time::Duration::from_millis(100);

pub(super) fn retained_stream_retry_backoff(
    runtime_handoff_retry_pending: bool,
    guarded_tool_frame_retry_pending: bool,
) -> std::time::Duration {
    debug_assert!(guarded_tool_frame_retry_pending || runtime_handoff_retry_pending);
    RETAINED_STREAM_RETRY_BACKOFF
}

/// A successful exit-candidate flush replaces `inflight_state` with the exact
/// lock-held merge. Mirror every merged stream field back into the caller-owned
/// loop state before terminal handling can observe the detached pre-await view.
pub(super) fn reconcile_saved_exit_candidate(
    shared: &SharedData,
    state: &mut StreamLoopState<'_>,
    current_msg_id_before_settle: MessageId,
) {
    let mut runtime = super::super::bridge_entry_persist::BridgeEntryRuntimeState {
        inflight_state: &mut *state.inflight_state,
        full_response: &mut *state.full_response,
        response_sent_offset: &mut *state.response_sent_offset,
        bridge_confirmed_response_sent_offset: &mut *state.bridge_confirmed_response_sent_offset,
        current_msg_id: &mut *state.current_msg_id,
        current_tool_line: &mut *state.current_tool_line,
        prev_tool_status: &mut *state.prev_tool_status,
        last_tool_name: &mut *state.last_tool_name,
        last_tool_summary: &mut *state.last_tool_summary,
        any_tool_used: &mut *state.any_tool_used,
        has_post_tool_text: &mut *state.has_post_tool_text,
        streaming_rollover_frozen_msg_ids: &mut *state.streaming_rollover_frozen_msg_ids,
        tmux_last_offset: &mut *state.tmux_last_offset,
        watcher_owner_channel_id: &mut *state.watcher_owner_channel_id,
        watcher_owns_assistant_relay: &mut *state.watcher_owns_assistant_relay,
        watcher_relay_available_for_turn: &mut *state.watcher_relay_available_for_turn,
        standby_relay_owns_output: &mut *state.standby_relay_owns_output,
        status_panel_msg_id: &mut *state.status_panel_msg_id,
        status_panel_generation: &mut *state.status_panel_generation,
    };
    super::super::bridge_entry_persist::reconcile_runtime_locals_from_inflight_state(
        shared,
        &mut runtime,
    );
    super::super::bridge_entry_persist::clear_last_edit_text_if_current_message_changed(
        current_msg_id_before_settle,
        *state.current_msg_id,
        state.last_edit_text,
    );
}

pub(super) struct StreamLoopExitCandidateContext<'context, 'state, G: TurnGateway + ?Sized> {
    pub(super) shared: &'context SharedData,
    pub(super) gateway: &'context G,
    pub(super) provider: &'context ProviderKind,
    pub(super) token_hash: &'context str,
    pub(super) channel_id: ChannelId,
    pub(super) persisted_inflight_baseline: &'context mut InflightTurnState,
    pub(super) expected_identity:
        &'context crate::services::discord::inflight::InflightTurnIdentity,
    pub(super) pending_current_message_candidate: &'context mut Option<MessageId>,
    pub(super) state: StreamLoopState<'state>,
}

pub(super) async fn settle_and_reconcile_exit_candidate<G: TurnGateway + ?Sized>(
    mut context: StreamLoopExitCandidateContext<'_, '_, G>,
) {
    let current_msg_id_before_exit_settle = *context.state.current_msg_id;
    if settle_pending_current_message_candidate_on_loop_exit(StreamTickCandidateSaveContext {
        gateway: context.gateway,
        provider: context.provider,
        token_hash: context.token_hash,
        channel_id: context.channel_id,
        persisted_baseline: context.persisted_inflight_baseline,
        inflight_state: &mut *context.state.inflight_state,
        expected_identity: context.expected_identity,
        expected_current_message: &mut *context.state.expected_current_message,
        current_msg_id: &mut *context.state.current_msg_id,
        pending_current_message_candidate: context.pending_current_message_candidate,
        bridge_created_response_placeholder_msg_id: &mut *context
            .state
            .bridge_created_response_placeholder_msg_id,
    })
    .await
    {
        reconcile_saved_exit_candidate(
            context.shared,
            &mut context.state,
            current_msg_id_before_exit_settle,
        );
    }
}
