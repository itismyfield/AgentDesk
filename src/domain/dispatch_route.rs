/// Reviews use the counterpart channel; agent-owned stages use the primary channel.
pub(crate) fn use_counter_model_channel(dispatch_type: Option<&str>) -> bool {
    matches!(
        dispatch_type,
        Some("review") | Some("e2e-test") | Some("consultation") | Some("plan-review")
    )
}
