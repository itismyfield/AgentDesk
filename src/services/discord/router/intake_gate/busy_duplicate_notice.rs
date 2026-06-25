use crate::services::turn_orchestrator::EnqueueRefusalReason;
use std::fmt::Display;

pub(super) fn silence_if_already_queued(
    reason: Option<EnqueueRefusalReason>,
    message_id: impl Display,
    channel_id: impl Display,
) -> bool {
    if !should_silence(reason) {
        return false;
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        reason = reason.map(|reason| reason.as_str()),
        "  [{ts}] ↪ BUSY-QUEUE: silently ignored already-queued source message {message_id} in channel {channel_id}"
    );
    true
}

fn should_silence(reason: Option<EnqueueRefusalReason>) -> bool {
    matches!(reason, Some(EnqueueRefusalReason::SourceIdAlreadyQueued))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silences_already_queued_source_id_duplicate_notice() {
        assert!(should_silence(Some(
            EnqueueRefusalReason::SourceIdAlreadyQueued
        )));
    }

    #[test]
    fn keeps_recent_resend_duplicate_notice_visible() {
        assert!(!should_silence(Some(EnqueueRefusalReason::LastItemDedup)));
    }

    #[test]
    fn keeps_queue_error_notice_visible() {
        assert!(!should_silence(Some(
            EnqueueRefusalReason::ActorUnreachable
        )));
        assert!(!should_silence(Some(EnqueueRefusalReason::MailboxClosed)));
        assert!(!should_silence(None));
    }
}
