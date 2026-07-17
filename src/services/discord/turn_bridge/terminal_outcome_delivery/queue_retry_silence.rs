use super::*;

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
        full_response.clear();
        inflight_state.full_response.clear();
        inflight_state.silent_turn = true;
    }
}

#[cfg(test)]
mod tests {
    use super::should_silence_requeued_response;

    #[test]
    fn busy_readiness_timeout_keeps_notice_deliverable() {
        assert!(!should_silence_requeued_response(true, true, false));
    }

    #[test]
    fn other_requeue_paths_keep_legacy_silencing() {
        assert!(should_silence_requeued_response(true, false, false));
        assert!(!should_silence_requeued_response(true, false, true));
    }
}
