use std::sync::Arc;

pub(super) use super::super::super::stream_tick::guarded_persist::VisibleMutationAuthority;
use super::super::super::stream_tick::guarded_persist::{
    stream_loop_suppression_cohort_admits, visible_mutation_authority_after_guarded_save,
};
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
    pub(super) any_tool_used: &'a mut bool,
    pub(super) has_post_tool_text: &'a mut bool,
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
    any_tool_used: &mut bool,
    has_post_tool_text: &mut bool,
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
    // re-seeds `current_tool_line`, `prev_tool_status`, `last_tool_name` and
    // `last_tool_summary` (`bridge_entry_persist.rs:96-109`). Exactly those FOUR
    // display fields are excluded here, and the exclusion claim below is about
    // those four ONLY — it does not extend to the two behaviour flags, which
    // this function DOES re-seed (see the `stage_tick_state_for_guard!` note).
    //
    // The two callers below are the ONLY two tool-arm fence sites
    // (`tool_arms.rs:287` restart, `tool_arms.rs:352` terminal ToolResult), and
    // at both of them the loop's copies of those four were derived from the
    // STREAM FRAME the arm is handling, not read from a durable row:
    // `tool_arms.rs:174-181` assigns all four on every ToolUse and runs BEFORE
    // the restart fence in that same arm, and the ToolResult arm re-derives
    // `prev_tool_status` + `current_tool_line` from them again at
    // `tool_arms.rs:551-556`, AFTER its fence. Re-seeding them from `on_disk`
    // would replace the bridge's own frame projection with the watcher's older
    // one and, in the ToolUse arm, erase the tool line for the tool that is
    // starting right now.
    // `tool_arms_derive_the_four_excluded_tool_line_locals_around_their_fences`
    // pins that ordering, so the exclusion cannot rot silently.
    //
    // Everything below IS re-seeded, and for one shared reason: each is a
    // stream-loop local that `stage_tick_state_for_guard!`
    // (`stream_tick.rs:291-304`) pushes back into the row on the NEXT tick.
    // Leaving one behind after a `Saved` leaves the loop holding the pre-save
    // value while `persisted_baseline` holds the merged one, and the next tick's
    // `local != before && durable == before => durable = local` branch
    // (`stream_loop_patch.rs:9` for the flags, `:24` inside
    // `merge_stream_response_progress` for the offset) then REWINDS the durable
    // row to the pre-save value.
    // * `response_sent_offset` — rewinding it resends already-delivered text.
    // * `any_tool_used` / `has_post_tool_text` — the writer that puts a value the
    //   loop never saw onto the row is `persist_watcher_stream_progress_locked`
    //   (`inflight/watcher_state.rs:131-132`), which writes both flags
    //   unconditionally and touches no relay-authority field, so the strict
    //   fence's `authority_changed` never trips and the save returns `Saved`.
    //   `watcher_stamped_tool_flags_survive_the_fence_and_the_next_real_stream_tick`
    //   drives that exact chain through the real watcher writer, the real
    //   restart fence and the real `run_bridge_stream_tick`.
    *response_sent_offset = inflight_state.response_sent_offset;
    *bridge_confirmed_response_sent_offset =
        crate::services::discord::turn_bridge::retry_state::bridge_confirmed_response_sent_offset_seed(
            inflight_state.effective_relay_owner_kind(),
            *response_sent_offset,
        );
    *any_tool_used = inflight_state.any_tool_used;
    *has_post_tool_text = inflight_state.has_post_tool_text;
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
            context.any_tool_used,
            context.has_post_tool_text,
        );
    }
    // Each tool-arm fence is its own entry into the gate — these arms hold no
    // per-tick locals `stream_tick` can lend them — so the cohort is asked here,
    // once, on the way in.
    let cohort_admits = stream_loop_suppression_cohort_admits(context.inflight_state.channel_id);
    visible_mutation_authority_after_guarded_save(
        outcome,
        context.inflight_state,
        intended_authority,
        cohort_admits,
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
            context.any_tool_used,
            context.has_post_tool_text,
        );
    }
    let cohort_admits = stream_loop_suppression_cohort_admits(context.inflight_state.channel_id);
    let authority = visible_mutation_authority_after_guarded_save(
        outcome,
        context.inflight_state,
        intended_authority,
        cohort_admits,
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
