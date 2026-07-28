use super::authority::{
    VisibleMutationAuthority, stream_tool_outcome_after_restart_authority,
    terminal_tool_result_transition_permission,
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
