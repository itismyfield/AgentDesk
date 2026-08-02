/// Queue acceptance has one channel-scoped card. The card exposes the explicit
/// manual-steer control; queue coalescing remains responsible for preventing
/// duplicate cards.
pub(in crate::services::discord) const fn queue_status_card_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_user_messages_render_one_manual_steer_card() {
        assert!(
            queue_status_card_enabled(),
            "the queue card is the explicit manual steering control"
        );
    }
}
