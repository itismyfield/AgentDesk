//! Only the native turn bound by this managed input may complete without a body.

use super::RolloutParseState;

pub(super) fn bind(
    state: &mut RolloutParseState,
    record: &crate::services::tui_prompt_dedupe::RecordEvidence,
    outcome: crate::services::tui_prompt_dedupe::BindOutcome,
) {
    if outcome == crate::services::tui_prompt_dedupe::BindOutcome::Bound {
        state.managed_turn.clone_from(&record.turn_id);
    }
}

pub(in super::super) fn body_or_verified_empty(state: &RolloutParseState) -> bool {
    state.saw_assistant_text
        || (state.managed_turn.as_deref().is_some_and(|turn| {
            state.started_turn.as_ref().and_then(Option::as_deref) == Some(turn)
                && state.latest_turn.as_deref() == Some(turn)
        }) && state.own_turn_complete_seen
            && !state.mixed_turn
            && !state.has_pending_tool_call()
            && !state.dropped_assistant_content)
}
