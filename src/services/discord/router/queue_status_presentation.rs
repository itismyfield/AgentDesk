/// Queue acceptance is represented by the source-message reaction lifecycle.
/// Posting a separate waiting/retry card duplicates that state and can flood the
/// channel when a busy follow-up is deferred more than once.
///
/// **Parked, not retired — do not remove the code this gates.** #4754 (open, P1,
/// `status:landed-partial`) turns the card back on: branch
/// `feat/4754-manual-steer-button-v2` (`f680658189`, PR #5107) already flips this
/// constant to `true` and hangs the manual-steer button off the card, and the
/// issue records the at-most-one-card policy in `intake_gate/queue_effects.rs` as
/// the thing its slice A *inverts*, not deletes. A dead-code census that reads
/// only the `false` here will mis-report the whole card path as unreachable; see
/// `docs/agent-maintenance/t5-t6-removal-inventory.md`
/// §"T6 도달 불가 분기(슬라이스 3) 철거 보류".
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
