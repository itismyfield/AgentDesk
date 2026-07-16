use super::*;

pub(super) fn apply(
    retry_candidate: bool,
    full_response: &mut String,
    inflight_state: &mut InflightTurnState,
) {
    if retry_candidate && !super::super::super::router::queue_status_card_enabled() {
        full_response.clear();
        inflight_state.full_response.clear();
        inflight_state.silent_turn = true;
    }
}
