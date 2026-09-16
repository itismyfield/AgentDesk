use super::*;
use crate::services::discord::turn_bridge::chunk_compose::body_mutation_telemetry::{
    BodyMutationCorrelation, BodyMutationSite, observe_body_mutation,
};

fn should_silence_requeued_response(
    retry_candidate: bool,
    claude_tui_followup_busy_readiness_timeout: bool,
    queue_status_card_enabled: bool,
) -> bool {
    retry_candidate && !claude_tui_followup_busy_readiness_timeout && !queue_status_card_enabled
}

pub(super) fn apply(
    retry_candidate: bool,
    claude_tui_followup_busy_readiness_timeout: bool,
    full_response: &mut String,
    inflight_state: &mut InflightTurnState,
) {
    if should_silence_requeued_response(
        retry_candidate,
        claude_tui_followup_busy_readiness_timeout,
        super::super::super::router::queue_status_card_enabled(),
    ) {
        // #5938 r2 P1-2: isomorphic to `retry_state::clear_response_delivery_state`
        // — local body and durable row body blanked in the same breath — so it
        // falls in the same recorded class. Without the record the readout shows
        // `after_len=N` and then a later `before_len=0` with nothing between
        // them, which is indistinguishable from the loss this instrumentation is
        // hunting.
        observe_body_mutation(
            BodyMutationSite::SilenceRequeuedResponse,
            BodyMutationCorrelation::from_inflight_row(inflight_state),
            full_response.as_str(),
            "",
        );
        full_response.clear();
        inflight_state.full_response.clear();
        inflight_state.silent_turn = true;
    }
}

#[cfg(test)]
mod tests {
    use super::should_silence_requeued_response;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::discord::turn_bridge::chunk_compose::body_mutation_telemetry::body_mutation_telemetry_tests::captured_logs;
    use crate::services::provider::ProviderKind;

    fn silenced_row() -> InflightTurnState {
        let mut row = InflightTurnState::new(
            ProviderKind::Codex,
            5_938_012,
            None,
            343_742_347_365_974_026,
            77_010,
            18,
            String::new(),
            None,
            None,
            None,
            None,
            0,
        );
        row.full_response = "COUNT-001\nCOUNT-002\n".to_string();
        row
    }

    #[test]
    fn busy_readiness_timeout_keeps_notice_deliverable() {
        assert!(!should_silence_requeued_response(true, true, false));
    }

    #[test]
    fn other_requeue_paths_keep_legacy_silencing() {
        assert!(should_silence_requeued_response(true, false, false));
        assert!(!should_silence_requeued_response(true, false, true));
    }

    /// #5938 r2 P1-2. Driven through the REAL `apply`, so deleting its
    /// observation fails here rather than quietly re-opening the gap.
    #[test]
    fn silencing_a_requeued_response_emits_a_body_mutation_record() {
        let mut row = silenced_row();
        let mut local = String::from("COUNT-001\nCOUNT-002\n");
        let logs = captured_logs(|| {
            super::apply(true, false, &mut local, &mut row);
        });

        // The silencing itself is untouched by the observation.
        assert!(local.is_empty());
        assert!(row.full_response.is_empty());
        assert!(row.silent_turn);

        assert!(
            logs.contains("site=\"queue_retry_silence::apply\""),
            "the requeue silencer must publish a body-mutation record; got: {logs}"
        );
        assert!(logs.contains("before_len=20"), "got: {logs}");
        assert!(logs.contains("after_len=0"), "got: {logs}");
    }

    /// The other side: a turn that is NOT silenced must not manufacture a record.
    #[test]
    fn a_response_that_is_not_silenced_emits_nothing() {
        let mut row = silenced_row();
        let mut local = String::from("COUNT-001\nCOUNT-002\n");
        let logs = captured_logs(|| {
            super::apply(false, false, &mut local, &mut row);
        });
        assert_eq!(local, "COUNT-001\nCOUNT-002\n");
        assert!(logs.is_empty(), "got: {logs}");
    }
}
