/// Queue acceptance is represented by the source-message reaction lifecycle.
/// Posting a separate waiting/retry card duplicates that state and can flood the
/// channel when a busy follow-up is deferred more than once.
pub(in crate::services::discord) const fn queue_status_card_enabled() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_user_messages_never_render_status_cards() {
        assert!(
            !queue_status_card_enabled(),
            "queued state must stay reaction-only"
        );
    }
}
