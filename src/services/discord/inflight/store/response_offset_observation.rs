use super::InflightTurnState;
use serde_json::{Value, json};

#[derive(Clone, Copy)]
pub(in crate::services::discord::inflight) struct StreamResponseOffsetInputs<'a> {
    pub(in crate::services::discord::inflight) entrypoint: &'static str,
    pub(in crate::services::discord::inflight) merge_branch: &'static str,
    pub(in crate::services::discord::inflight) before: &'a InflightTurnState,
    pub(in crate::services::discord::inflight) local: &'a InflightTurnState,
    pub(in crate::services::discord::inflight) durable: &'a InflightTurnState,
}

pub(super) fn response_offset_details(
    candidate: &InflightTurnState,
    candidate_in_bounds: bool,
    existing: Option<&InflightTurnState>,
    code_location: &'static str,
    inputs: Option<StreamResponseOffsetInputs<'_>>,
) -> Value {
    let (entrypoint, merge_branch, before, local, durable) = match inputs {
        Some(inputs) => (
            inputs.entrypoint,
            Some(inputs.merge_branch),
            Some(inputs.before),
            Some(inputs.local),
            Some(inputs.durable),
        ),
        None => (code_location, None, None, None, existing),
    };
    json!({
        "entrypoint": entrypoint,
        "merge_branch": merge_branch,
        "finalizer_turn_id": candidate.effective_finalizer_turn_id().to_string(),
        "response_sent_offset": candidate.response_sent_offset,
        "full_response_len": candidate.full_response.len(),
        "candidate_in_bounds": candidate_in_bounds,
        "before": before.map(snapshot),
        "local": local.map(snapshot),
        "durable": durable.map(snapshot),
    })
}

fn snapshot(state: &InflightTurnState) -> Value {
    let offset = state.response_sent_offset;
    let length = state.full_response.len();
    json!({
        "response_sent_offset": offset,
        "full_response_len": length,
        "in_bounds": offset <= length && state.full_response.is_char_boundary(offset),
    })
}
