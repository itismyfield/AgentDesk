//! Cohesive intake-to-inflight state construction.
use super::*;

pub(super) fn build_intake_inflight_state(
    intake_outbox_id: Option<i64>,
    source_message_ids: Vec<serenity::MessageId>,
    construct: impl FnOnce() -> InflightTurnState,
) -> InflightTurnState {
    let mut state = construct();
    state.adopt_intake_outbox(intake_outbox_id);
    state.source_message_ids = source_message_ids
        .into_iter()
        .map(serenity::MessageId::get)
        .collect();
    state
}