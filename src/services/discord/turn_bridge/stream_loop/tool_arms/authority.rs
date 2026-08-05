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
    /// Stream-loop runtime locals that must follow `inflight_state` whenever the
    /// guarded save adopts a durable row the loop did not stage. `stream_tick`
    /// does this through `reconcile_tick_runtime_from_inflight!`; these arms had
    /// no equivalent and kept editing/reporting a stale Discord anchor and body.
    pub(super) current_msg_id: &'a mut MessageId,
    pub(super) full_response: &'a mut String,
    pub(super) response_sent_offset: &'a mut usize,
    /// The stream loop's `bridge_confirmed_response_sent_offset`, abbreviated to
    /// keep the call sites within the `stream_loop.rs` namespace size cap. NOT
    /// the unrelated `confirmed_end_offset` used by the relay sink.
    pub(super) confirmed_offset: &'a mut usize,
}

/// Mirror of `stream_tick`'s post-save reconcile for the tool arms: after a
/// `Saved` the durable row is authoritative, so the loop locals this module's
/// callers still hold must be re-seeded from it. Without this the arms write a
/// stale anchor back through `edit_bound_current_message` and a stale body back
/// through `inflight_state.full_response`, which the next merge flush then
/// pushes onto the durable row (epoch and body regression).
pub(super) fn reconcile_tool_arm_locals_after_guarded_save(
    inflight_state: &InflightTurnState,
    expected_current_message: &mut (u64, usize),
    current_msg_id: &mut MessageId,
    full_response: &mut String,
    response_sent_offset: &mut usize,
    bridge_confirmed_response_sent_offset: &mut usize,
) {
    *expected_current_message = (
        inflight_state.current_msg_id,
        inflight_state.current_msg_len,
    );
    *current_msg_id = crate::services::discord::turn_bridge::current_message_anchor::detached_current_msg_id_from_durable(
        inflight_state.current_msg_id,
    );
    full_response.clone_from(&inflight_state.full_response);
    // A SUBSET of what `bridge_entry_persist::reconcile_runtime_locals_from_inflight_state`
    // (the `stream_tick` side) re-seeds — not an isomorphism. That function also
    // re-seeds `current_tool_line`, `prev_tool_status`, `last_tool_name`,
    // `last_tool_summary`, `any_tool_used` and `has_post_tool_text`
    // (`bridge_entry_persist.rs:96-109`), and this one deliberately does not.
    // Those six are staged back the same way and `any_tool_used` /
    // `has_post_tool_text` are not display-only (they feed `resolve_done_response`
    // via `content_arms.rs:341-346`), but their merge goes through
    // `apply_local_change_if_durable_unchanged`, where durable wins, and no
    // damage path through it has been demonstrated. They are left out until one
    // is, rather than widened pre-emptively.
    //
    // `response_sent_offset` IS demonstrated: it is one stream-loop local shared
    // with `stream_tick`, whose `stage_tick_state_for_guard!` stages it back on
    // the NEXT tick; leaving it behind lets the
    // `durable == before => return local` arm of `merge_stream_response_progress`
    // rewind the durable offset and resend already-delivered text.
    *response_sent_offset = inflight_state.response_sent_offset;
    *bridge_confirmed_response_sent_offset =
        crate::services::discord::turn_bridge::retry_state::bridge_confirmed_response_sent_offset_seed(
            inflight_state.effective_relay_owner_kind(),
            *response_sent_offset,
        );
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
    mut context: StreamToolAuthorityContext<'_>,
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
        reconcile_tool_arm_locals_after_guarded_save(
            context.inflight_state,
            context.expected_current_message,
            context.current_msg_id,
            context.full_response,
            context.response_sent_offset,
            context.confirmed_offset,
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
    mut context: StreamToolAuthorityContext<'_>,
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
        reconcile_tool_arm_locals_after_guarded_save(
            context.inflight_state,
            context.expected_current_message,
            context.current_msg_id,
            context.full_response,
            context.response_sent_offset,
            context.confirmed_offset,
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
