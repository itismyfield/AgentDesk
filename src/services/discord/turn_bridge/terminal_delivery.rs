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

fn replace_outcome_commits_terminal_delivery(outcome: &ReplaceLongMessageOutcome) -> bool {
    matches!(outcome, ReplaceLongMessageOutcome::EditedOriginal)
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
        Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
            sent_chunks,
            total_chunks,
            failed_chunk_index,
            sent_continuation_message_ids,
            cleanup_errors,
            error,
        }) => {
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::failed(format!(
                    "partial continuation failure: sent_chunks={sent_chunks}, total_chunks={total_chunks}, failed_chunk_index={failed_chunk_index}, cleaned_continuations={}, cleanup_errors={}, error={error}",
                    sent_continuation_message_ids.len(),
                    cleanup_errors.len()
                )),
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
        replace_outcome_commits_terminal_delivery,
        should_complete_work_dispatch_after_terminal_delivery,
        should_fail_dispatch_after_terminal_delivery,
    };
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;

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
    fn partial_continuation_failure_does_not_commit_terminal_delivery() {
        let outcome = ReplaceLongMessageOutcome::PartialContinuationFailure {
            sent_chunks: 1,
            total_chunks: 3,
            failed_chunk_index: 1,
            sent_continuation_message_ids: Vec::new(),
            cleanup_errors: Vec::new(),
            error: "HTTP 500".to_string(),
        };

        assert!(!replace_outcome_commits_terminal_delivery(&outcome));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            replace_outcome_commits_terminal_delivery(&outcome),
            false,
            false,
            false,
            "final response with missing continuation",
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            true,
            replace_outcome_commits_terminal_delivery(&outcome),
            false,
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
