use super::authority::{
    VisibleMutationAuthority, reconcile_tool_arm_locals_after_guarded_save,
    stream_tool_outcome_after_restart_authority, terminal_tool_result_transition_permission,
};
use super::*;
use crate::services::discord::turn_bridge::stream_tick::guarded_persist::visible_mutation_authority_after_guarded_save;

fn bridge_state(channel_id: u64) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        Some("adk-4259-r8".to_string()),
        343_742_347_365_974_026,
        77_010,
        18,
        "queued restart".to_string(),
        Some("session".to_string()),
        Some("AgentDesk-codex-r8-restart".to_string()),
        Some("/tmp/AgentDesk-codex-r8-restart.jsonl".to_string()),
        None,
        512,
    )
}

#[test]
fn queued_restart_foreign_authority_propagates_loss_while_self_delegation_continues() {
    let bridge = bridge_state(42_593_120);
    let intended = crate::services::discord::inflight::StreamRelayAuthority::from_state(&bridge);
    let mut foreign = bridge.clone();
    foreign.set_watcher_owner_channel_id(foreign.channel_id + 1);
    foreign.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
    let foreign_authority = visible_mutation_authority_after_guarded_save(
        crate::services::discord::inflight::GuardedSaveOutcome::IdentityMismatch,
        &foreign,
        intended,
    );
    assert_eq!(foreign_authority, VisibleMutationAuthority::AuthorityLost);
    assert_eq!(
        stream_tool_outcome_after_restart_authority(Some(foreign_authority)),
        StreamToolArmOutcome::AuthorityLost,
    );

    let delegated = crate::services::discord::inflight::StreamRelayAuthority::from_state(&foreign);
    let self_delegated = visible_mutation_authority_after_guarded_save(
        crate::services::discord::inflight::GuardedSaveOutcome::Saved,
        &foreign,
        delegated,
    );
    assert_eq!(self_delegated, VisibleMutationAuthority::Suppressed);
    assert_eq!(
        stream_tool_outcome_after_restart_authority(Some(self_delegated)),
        StreamToolArmOutcome::Continue,
    );
}

#[test]
fn terminal_tool_result_fence_maps_handoff_loss_and_io_retry_fail_closed() {
    assert_eq!(
        terminal_tool_result_transition_permission(VisibleMutationAuthority::Authorized),
        Ok(true),
    );
    assert_eq!(
        terminal_tool_result_transition_permission(VisibleMutationAuthority::Suppressed),
        Ok(false),
    );
    assert_eq!(
        terminal_tool_result_transition_permission(VisibleMutationAuthority::AuthorityLost),
        Err(StreamToolArmOutcome::AuthorityLost),
    );
    assert_eq!(
        terminal_tool_result_transition_permission(VisibleMutationAuthority::Retry),
        Err(StreamToolArmOutcome::RetryExactFrame),
    );
}

#[test]
fn transient_terminal_tool_result_fence_requeues_the_exact_frame_at_front() {
    let mut pending = std::collections::VecDeque::from([StreamMessage::Text {
        content: "later frame".to_string(),
    }]);
    let frame = StreamMessage::ToolResult {
        content: "exact terminal payload".to_string(),
        is_error: true,
        tool_use_id: Some("tool-4259-r9".to_string()),
    };
    let mut retry_retained = false;
    assert!(reconcile_exact_stream_frame_after_tool_outcome(
        &mut pending,
        frame,
        StreamToolArmOutcome::RetryExactFrame,
        &mut retry_retained,
    ));
    assert!(retry_retained);
    let Some(StreamMessage::ToolResult {
        content,
        is_error,
        tool_use_id,
    }) = pending.pop_front()
    else {
        panic!("exact ToolResult must remain at queue front");
    };
    assert_eq!(content, "exact terminal payload");
    assert!(is_error);
    assert_eq!(tool_use_id.as_deref(), Some("tool-4259-r9"));
    assert!(matches!(
        pending.pop_front(),
        Some(StreamMessage::Text { content }) if content == "later frame"
    ));
}

/// The tool arms consume `current_msg_id` and `full_response` as loop locals and
/// write both back — `edit_bound_current_message` puts the anchor back into
/// `inflight_state`, and the ToolUse arm assigns `inflight_state.full_response`.
/// When a guarded save reports `Saved` after resolving against a durable row the
/// loop never staged, those locals are stale, and writing them back makes the
/// NEXT merge flush push a rewound epoch and body onto the durable row.
/// `stream_tick` avoids this with `reconcile_tick_runtime_from_inflight!`; these
/// arms had no equivalent until this reconcile was added.
#[test]
fn tool_arm_locals_follow_the_durable_row_after_a_guarded_save() {
    let mut durable = bridge_state(42_593_122);
    durable.current_msg_id = 1_534_511_625_615_311_000;
    durable.current_msg_len = 21;
    durable.full_response = "base plus shared plus watcher tail".to_string();
    durable.response_sent_offset = durable.full_response.len();

    // What the loop still holds: the epoch and body it staged before the save.
    let stale_anchor = MessageId::new(1_534_511_598_012_600_371);
    let mut current_msg_id = stale_anchor;
    let mut full_response = "base plus shared".to_string();
    let mut expected_current_message = (1_534_511_598_012_600_371_u64, 21_usize);
    let stale_offset = "base plus shared".len();
    let mut response_sent_offset = stale_offset;
    let mut bridge_confirmed_response_sent_offset = stale_offset;

    reconcile_tool_arm_locals_after_guarded_save(
        &durable,
        &mut expected_current_message,
        &mut current_msg_id,
        &mut full_response,
        &mut response_sent_offset,
        &mut bridge_confirmed_response_sent_offset,
    );

    assert_eq!(
        expected_current_message,
        (durable.current_msg_id, durable.current_msg_len),
        "the fence baseline must follow the durable epoch",
    );
    assert_ne!(
        current_msg_id, stale_anchor,
        "the arm must not keep editing the pre-save anchor",
    );
    assert_eq!(
        crate::services::discord::turn_bridge::current_message_anchor::durable_current_msg_id_from_detached(
            current_msg_id,
        ),
        durable.current_msg_id,
        "the arm's anchor must resolve to the durable current message",
    );
    assert_eq!(
        full_response, durable.full_response,
        "the arm must not write a rewound body back into inflight_state",
    );
    // `response_sent_offset` is one stream-loop local shared with `stream_tick`,
    // which stages it back on the next tick. Leaving it behind lets
    // `merge_stream_response_progress`'s `durable == before => return local` arm
    // rewind the durable offset and resend already-delivered text.
    assert_ne!(
        response_sent_offset, stale_offset,
        "the arm must not keep the pre-save delivery watermark",
    );
    assert_eq!(
        response_sent_offset, durable.response_sent_offset,
        "the delivery watermark must follow the durable row",
    );
    assert_eq!(
        bridge_confirmed_response_sent_offset,
        crate::services::discord::turn_bridge::retry_state::bridge_confirmed_response_sent_offset_seed(
            durable.effective_relay_owner_kind(),
            durable.response_sent_offset,
        ),
        "the bridge-confirmed watermark must be re-seeded exactly as stream_tick re-seeds it",
    );
}
