use super::super::formatting::ReplaceLongMessageOutcome;
use super::*;

fn record_turn_bridge_terminal_replace_cleanup(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: Option<&str>,
    outcome: super::super::placeholder_cleanup::PlaceholderCleanupOutcome,
    source: &'static str,
) {
    if let super::super::placeholder_cleanup::PlaceholderCleanupOutcome::Failed { class, detail } =
        &outcome
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ placeholder cleanup {} failed ({}) for channel {} msg {}: {}",
            super::super::placeholder_cleanup::PlaceholderCleanupOperation::EditTerminal.as_str(),
            class.as_str(),
            channel_id.get(),
            message_id.get(),
            detail
        );
    }
    shared.placeholder_cleanup.record(
        super::super::placeholder_cleanup::PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id,
            tmux_session_name: tmux_session_name.map(str::to_string),
            operation: super::super::placeholder_cleanup::PlaceholderCleanupOperation::EditTerminal,
            outcome,
            source,
        },
    );
}

pub(super) fn turn_bridge_replace_outcome_committed(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: Option<&str>,
    replace_result: Result<ReplaceLongMessageOutcome, String>,
    source: &'static str,
) -> bool {
    match replace_result {
        Ok(ReplaceLongMessageOutcome::EditedOriginal) => {
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
                source,
            );
            true
        }
        Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { edit_error }) => {
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::failed(edit_error),
                source,
            );
            false
        }
        Err(error) => {
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::failed(error),
                source,
            );
            false
        }
    }
}

pub(super) fn should_complete_work_dispatch_after_terminal_delivery(
    completion_candidate: bool,
    terminal_delivery_committed: bool,
    preserve_inflight_for_cleanup_retry: bool,
    resume_failure_detected: bool,
    recovery_retry: bool,
    full_response: &str,
) -> bool {
    completion_candidate
        && terminal_delivery_committed
        && !preserve_inflight_for_cleanup_retry
        && !resume_failure_detected
        && !recovery_retry
        && !full_response.trim().is_empty()
}

pub(super) fn should_fail_dispatch_after_terminal_delivery(
    fail_candidate: bool,
    terminal_delivery_committed: bool,
    preserve_inflight_for_cleanup_retry: bool,
) -> bool {
    fail_candidate && terminal_delivery_committed && !preserve_inflight_for_cleanup_retry
}

#[cfg(test)]
mod tests {
    use super::{
        should_complete_work_dispatch_after_terminal_delivery,
        should_fail_dispatch_after_terminal_delivery,
    };

    #[test]
    fn work_dispatch_completion_requires_terminal_delivery_commit() {
        assert!(should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            false,
            false,
            false,
            "visible final response",
        ));

        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            false,
            false,
            false,
            false,
            "visible final response",
        ));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            true,
            false,
            false,
            "visible final response",
        ));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            false,
            true,
            false,
            "visible final response",
        ));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            false,
            false,
            true,
            "visible final response",
        ));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true, true, false, false, false, "   ",
        ));
    }

    #[test]
    fn final_completion_delivery_stays_blocked_until_terminal_message_commits() {
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            false,
            false,
            false,
            false,
            "final response waiting for Discord delivery",
        ));
        assert!(should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            false,
            false,
            false,
            "final response delivered",
        ));
    }

    #[test]
    fn transport_error_dispatch_failure_requires_terminal_delivery_commit() {
        assert!(should_fail_dispatch_after_terminal_delivery(
            true, true, false,
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            true, false, false,
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            true, true, true,
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            false, true, false,
        ));
    }
}
