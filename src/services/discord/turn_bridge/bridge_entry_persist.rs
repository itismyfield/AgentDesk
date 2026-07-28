//! Bridge-entry inflight persistence plus local-state reconciliation (#4259 R4).

use super::*;

pub(super) struct BridgeEntryRuntimeState<'a> {
    pub(super) inflight_state: &'a mut InflightTurnState,
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
    pub(super) standby_relay_owns_output: &'a mut bool,
    pub(super) status_panel_msg_id: &'a mut Option<MessageId>,
    pub(super) status_panel_generation: &'a mut u64,
}

fn relay_owner_flags(
    owner_kind: crate::services::discord::inflight::RelayOwnerKind,
    watcher_registered: bool,
) -> (bool, bool, bool) {
    use crate::services::discord::inflight::RelayOwnerKind;

    let watcher_owns_assistant_relay = matches!(owner_kind, RelayOwnerKind::Watcher);
    let watcher_relay_available_for_turn = watcher_owns_assistant_relay && watcher_registered;
    let standby_relay_owns_output = matches!(
        owner_kind,
        RelayOwnerKind::StandbyRelay | RelayOwnerKind::SessionBoundRelay | RelayOwnerKind::Unknown
    );
    (
        watcher_owns_assistant_relay,
        watcher_relay_available_for_turn,
        standby_relay_owns_output,
    )
}

pub(super) fn bridge_stream_relay_suppressed(
    watcher_owns_assistant_relay: bool,
    standby_relay_owns_output: bool,
) -> bool {
    watcher_owns_assistant_relay || standby_relay_owns_output
}

fn reconcile_runtime_locals_after_saved_patch(
    shared: &SharedData,
    state: BridgeEntryRuntimeState<'_>,
) {
    let BridgeEntryRuntimeState {
        inflight_state,
        full_response,
        response_sent_offset,
        bridge_confirmed_response_sent_offset,
        current_msg_id,
        current_tool_line,
        prev_tool_status,
        last_tool_name,
        last_tool_summary,
        any_tool_used,
        has_post_tool_text,
        streaming_rollover_frozen_msg_ids,
        tmux_last_offset,
        watcher_owner_channel_id,
        watcher_owns_assistant_relay,
        watcher_relay_available_for_turn,
        standby_relay_owns_output,
        status_panel_msg_id,
        status_panel_generation,
    } = state;

    full_response.clone_from(&inflight_state.full_response);
    *response_sent_offset = inflight_state.response_sent_offset;
    *bridge_confirmed_response_sent_offset = bridge_confirmed_response_sent_offset_seed(
        inflight_state.effective_relay_owner_kind(),
        *response_sent_offset,
    );
    if let Some(merged_current_msg_id) =
        crate::services::discord::inflight::optional_message_id(inflight_state.current_msg_id)
    {
        *current_msg_id = merged_current_msg_id;
    }
    current_tool_line.clone_from(&inflight_state.current_tool_line);
    prev_tool_status.clone_from(&inflight_state.prev_tool_status);
    last_tool_name.clone_from(&inflight_state.last_tool_name);
    last_tool_summary.clone_from(&inflight_state.last_tool_summary);
    *any_tool_used = inflight_state.any_tool_used;
    *has_post_tool_text = inflight_state.has_post_tool_text;
    *streaming_rollover_frozen_msg_ids = inflight_state
        .streaming_rollover_frozen_msg_ids
        .iter()
        .filter_map(|id| crate::services::discord::inflight::optional_message_id(*id))
        .collect();
    if tmux_last_offset.is_some() {
        *tmux_last_offset = Some(inflight_state.last_offset);
    }
    if let Some(merged_owner_channel_id) = inflight_state
        .watcher_owner_channel_id
        .and_then(crate::services::discord::inflight::opt_channel_id)
    {
        *watcher_owner_channel_id = merged_owner_channel_id;
    }
    let watcher_registered = live_watcher_registered_for_relay(shared, *watcher_owner_channel_id);
    (
        *watcher_owns_assistant_relay,
        *watcher_relay_available_for_turn,
        *standby_relay_owns_output,
    ) = relay_owner_flags(
        inflight_state.effective_relay_owner_kind(),
        watcher_registered,
    );
    *status_panel_msg_id = inflight_state
        .status_message_id
        .and_then(crate::services::discord::inflight::optional_message_id);
    *status_panel_generation = inflight_state.status_panel_generation;
}

/// Saves bridge-entry mutations without recreating or overwriting a row this
/// turn no longer owns. A successful store patch replaces `inflight_state` with
/// the lock-held merge; mirror that merge into detached loop locals so the next
/// stream tick cannot flush the pre-await snapshot back over watcher progress.
pub(super) fn persist_bridge_entry_inflight_state(
    before: &InflightTurnState,
    shared: &SharedData,
    mut runtime: BridgeEntryRuntimeState<'_>,
) -> crate::services::discord::inflight::GuardedSaveOutcome {
    use crate::services::discord::inflight::{
        GuardedSaveOutcome, patch_bridge_entry_state_if_identity_unchanged,
    };

    const CALLER: &str = "turn_bridge::spawn_turn_bridge::bridge_entry";
    let outcome = patch_bridge_entry_state_if_identity_unchanged(
        before,
        &mut *runtime.inflight_state,
        CALLER,
    );
    match outcome {
        GuardedSaveOutcome::Saved => {
            reconcile_runtime_locals_after_saved_patch(shared, runtime);
        }
        GuardedSaveOutcome::Missing => tracing::warn!(
            channel_id = before.channel_id,
            caller = CALLER,
            "bridge-entry inflight patch skipped: durable row missing; row was not recreated"
        ),
        GuardedSaveOutcome::IdentityMismatch => tracing::warn!(
            channel_id = before.channel_id,
            caller = CALLER,
            "bridge-entry inflight patch skipped: durable row belongs to another turn"
        ),
        GuardedSaveOutcome::IoError => tracing::warn!(
            channel_id = before.channel_id,
            caller = CALLER,
            "bridge-entry inflight patch failed: inflight store I/O error"
        ),
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::inflight::RelayOwnerKind;

    #[test]
    fn same_turn_owner_advancement_suppresses_bridge_stream_relay() {
        for owner_kind in [
            RelayOwnerKind::Watcher,
            RelayOwnerKind::StandbyRelay,
            RelayOwnerKind::SessionBoundRelay,
            RelayOwnerKind::Unknown,
        ] {
            let (watcher_owns, watcher_available, standby_owns) =
                relay_owner_flags(owner_kind, true);
            assert!(
                bridge_stream_relay_suppressed(watcher_owns, standby_owns),
                "merged owner {owner_kind:?} must suppress bridge stream delivery"
            );
            assert_eq!(watcher_available, owner_kind == RelayOwnerKind::Watcher);
        }

        let (watcher_owns, watcher_available, standby_owns) =
            relay_owner_flags(RelayOwnerKind::Watcher, false);
        assert_eq!(
            (watcher_owns, watcher_available, standby_owns),
            (true, false, false)
        );
        assert!(bridge_stream_relay_suppressed(watcher_owns, standby_owns));
    }
}
