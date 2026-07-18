/// Queue acceptance is represented by the source-message reaction lifecycle.
/// Posting a separate waiting/retry card duplicates that state and can flood the
/// channel when a busy follow-up is deferred more than once.
pub(in crate::services::discord) const fn queue_status_card_enabled() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compact(source: &str) -> String {
        source.split_whitespace().collect()
    }

    #[test]
    fn queued_user_messages_never_render_status_cards() {
        assert!(
            !queue_status_card_enabled(),
            "queued state must stay reaction-only"
        );
    }

    #[test]
    fn every_busy_queue_card_surface_is_policy_gated() {
        let intake_gate = compact(include_str!("intake_gate.rs"));
        assert!(intake_gate.contains(&compact(
            r#"
                if !is_allowed_bot
                    && super::queue_status_presentation::queue_status_card_enabled()
                {
                    render_visible_queued_ack(
            "#,
        )));

        let race_loss = compact(include_str!("message_handler/intake_turn/race_loss.rs"));
        assert!(race_loss.contains(&compact(
            r#"
                let want_queued_card = want_queued_card
                    && super::super::super::queue_status_presentation::queue_status_card_enabled();
            "#,
        )));
        let race_loss_reaction = compact(include_str!(
            "message_handler/intake_turn/race_loss/mailbox_reaction.rs"
        ));
        assert!(race_loss_reaction.contains(&compact(
            r#"
                crate::services::discord::outbound::reaction_control::ensure_queue_reaction_or_fallback_http(
            "#,
        )));

        let queue_retry_silence = compact(include_str!(
            "../turn_bridge/terminal_outcome_delivery/queue_retry_silence.rs"
        ));
        assert!(queue_retry_silence.contains(&compact(
            r#"
                retry_candidate && !claude_tui_followup_busy_readiness_timeout && !queue_status_card_enabled
            "#,
        )));
        let terminal_delivery =
            compact(include_str!("../turn_bridge/terminal_outcome_delivery.rs"));
        assert!(terminal_delivery.contains(&compact(
            r#"
                queue_retry_silence::apply(
                    claude_tui_followup_pre_submit_requeue_candidate,
                    claude_tui_followup_busy_readiness_timeout,
                    &mut full_response,
                    &mut inflight_state,
                );
            "#,
        )));

        let streaming_edit_text = include_str!("../turn_bridge/streaming_edit_text.rs");
        assert!(
            streaming_edit_text.contains(
                "pub(in crate::services::discord) const CLAUDE_TUI_FOLLOWUP_REQUEUE_DELIVERY_NOTICE: &str = \"\";"
            ),
            "legacy bridge notice must stay empty so retry delivery posts no card"
        );

        let followup_support =
            compact(include_str!("../../claude_tui/hosting/followup_support.rs"));
        assert!(followup_support.contains(&compact(
            r#"
                if requeue_for_retry {
                    return;
                }
            "#,
        )));
    }
}
