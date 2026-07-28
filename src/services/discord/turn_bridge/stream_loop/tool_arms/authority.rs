use std::sync::Arc;

pub(super) use super::super::super::stream_tick::guarded_persist::VisibleMutationAuthority;
use super::super::super::stream_tick::guarded_persist::visible_mutation_authority_after_guarded_save;
use super::super::super::stream_tick::{
    LongRunningPlaceholderActive, PendingLongRunningRetargetAfterStateSave,
};
use super::super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord::turn_bridge::stream_loop) enum StreamToolArmOutcome {
    Continue,
    RetryExactFrame,
    AuthorityLost,
}

pub(super) struct StreamToolAuthorityContext<'a> {
    pub(super) shared_owned: &'a Arc<SharedData>,
    pub(super) gateway: &'a Arc<dyn TurnGateway>,
    pub(super) persisted_inflight_baseline: &'a mut InflightTurnState,
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) stream_tick_expected_identity:
        &'a crate::services::discord::inflight::InflightTurnIdentity,
    pub(super) expected_current_message: &'a mut (u64, usize),
}

pub(super) enum TerminalToolResultFence {
    NoTransition,
    Prefenced(crate::services::discord::placeholder_controller::PlaceholderControllerOutcome),
    Suppressed,
    Stop(StreamToolArmOutcome),
}

pub(super) fn stream_tool_outcome_after_restart_authority(
    authority: Option<VisibleMutationAuthority>,
) -> StreamToolArmOutcome {
    if authority == Some(VisibleMutationAuthority::AuthorityLost) {
        StreamToolArmOutcome::AuthorityLost
    } else {
        StreamToolArmOutcome::Continue
    }
}

pub(super) fn terminal_tool_result_transition_permission(
    authority: VisibleMutationAuthority,
) -> Result<bool, StreamToolArmOutcome> {
    match authority {
        VisibleMutationAuthority::Authorized => Ok(true),
        VisibleMutationAuthority::Suppressed => Ok(false),
        VisibleMutationAuthority::Retry => Err(StreamToolArmOutcome::RetryExactFrame),
        VisibleMutationAuthority::AuthorityLost => Err(StreamToolArmOutcome::AuthorityLost),
    }
}

pub(in crate::services::discord::turn_bridge::stream_loop) fn reconcile_exact_stream_frame_after_tool_outcome(
    pending: &mut std::collections::VecDeque<StreamMessage>,
    frame: StreamMessage,
    outcome: StreamToolArmOutcome,
    retry_retained: &mut bool,
) -> bool {
    if outcome != StreamToolArmOutcome::RetryExactFrame {
        *retry_retained = false;
        return false;
    }
    pending.push_front(frame);
    *retry_retained = true;
    true
}

pub(super) fn fence_restart_visible_mutation(
    context: StreamToolAuthorityContext<'_>,
) -> VisibleMutationAuthority {
    let intended_authority = crate::services::discord::inflight::StreamRelayAuthority::from_state(
        context.inflight_state,
    );
    let outcome = crate::services::discord::inflight::save_stream_tick_state_if_bridge_authority(
        context.persisted_inflight_baseline,
        context.inflight_state,
        context.stream_tick_expected_identity,
        context.expected_current_message.0,
        context.expected_current_message.1,
        "turn_bridge::stream_loop::tool_restart_visible_fence",
    );
    if matches!(
        outcome,
        crate::services::discord::inflight::GuardedSaveOutcome::Saved
    ) {
        *context.expected_current_message = (
            context.inflight_state.current_msg_id,
            context.inflight_state.current_msg_len,
        );
    }
    visible_mutation_authority_after_guarded_save(
        outcome,
        context.inflight_state,
        intended_authority,
    )
}

fn pending_terminal_transition(
    long_running_placeholder_active: &LongRunningPlaceholderActive,
    pending_long_running_retarget_after_state_save: &PendingLongRunningRetargetAfterStateSave,
    is_error: bool,
) -> Option<(
    crate::services::discord::placeholder_controller::PlaceholderKey,
    crate::services::discord::placeholder_controller::PlaceholderLifecycle,
)> {
    long_running_placeholder_active.as_ref().and_then(
        |(key, _, close_trigger, ack_consumed)| {
            let monitor_like = matches!(
                close_trigger,
                crate::services::discord::formatting::LongRunningCloseTrigger::MonitorLike
            );
            let is_dispatch_ack = !monitor_like && !ack_consumed;
            let pending_retarget_matches_key = pending_long_running_retarget_after_state_save
                .as_ref()
                .is_some_and(|(pending_key, _, _, _, _)| pending_key == key);
            (monitor_like || (is_dispatch_ack && is_error))
                .then_some(())
                .filter(|_| !pending_retarget_matches_key)
                .map(|()| {
                    let target = if is_error {
                        crate::services::discord::placeholder_controller::PlaceholderLifecycle::Aborted
                    } else {
                        crate::services::discord::placeholder_controller::PlaceholderLifecycle::Completed
                    };
                    (key.clone(), target)
                })
        },
    )
}

pub(super) async fn fence_terminal_tool_result_transition(
    context: StreamToolAuthorityContext<'_>,
    long_running_placeholder_active: &LongRunningPlaceholderActive,
    pending_long_running_retarget_after_state_save: &PendingLongRunningRetargetAfterStateSave,
    is_error: bool,
) -> TerminalToolResultFence {
    let Some((key, target)) = pending_terminal_transition(
        long_running_placeholder_active,
        pending_long_running_retarget_after_state_save,
        is_error,
    ) else {
        return TerminalToolResultFence::NoTransition;
    };
    let intended_authority = crate::services::discord::inflight::StreamRelayAuthority::from_state(
        context.inflight_state,
    );
    let outcome = crate::services::discord::inflight::save_stream_tick_state_if_bridge_authority(
        context.persisted_inflight_baseline,
        context.inflight_state,
        context.stream_tick_expected_identity,
        context.expected_current_message.0,
        context.expected_current_message.1,
        "turn_bridge::stream_loop::terminal_tool_result_visible_fence",
    );
    if outcome == crate::services::discord::inflight::GuardedSaveOutcome::Saved {
        *context.expected_current_message = (
            context.inflight_state.current_msg_id,
            context.inflight_state.current_msg_len,
        );
    }
    let authority = visible_mutation_authority_after_guarded_save(
        outcome,
        context.inflight_state,
        intended_authority,
    );
    match terminal_tool_result_transition_permission(authority) {
        Ok(true) => TerminalToolResultFence::Prefenced(
            context
                .shared_owned
                .ui
                .placeholder_controller
                .transition(context.gateway.as_ref(), key, target)
                .await,
        ),
        Ok(false) => TerminalToolResultFence::Suppressed,
        Err(outcome) => TerminalToolResultFence::Stop(outcome),
    }
}
