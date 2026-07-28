use super::*;

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
