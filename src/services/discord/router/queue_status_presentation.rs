/// Queue acceptance has one channel-scoped card. The card exposes the explicit
/// manual-steer control; queue coalescing remains responsible for preventing
/// duplicate cards.
pub(in crate::services::discord) const fn queue_status_card_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compact(source: &str) -> String {
        source.split_whitespace().collect()
    }

    #[test]
    fn queued_user_messages_render_one_manual_steer_card() {
        assert!(
            queue_status_card_enabled(),
            "the queue card is the explicit manual steering control"
        );
    }

    #[test]
    fn manual_steer_custom_id_and_intake_injection_boundary_are_wired() {
        let interaction = include_str!("../manual_steer_interaction.rs");
        assert!(interaction.contains("manual-steer:v1:"));
        assert!(interaction.contains("mailbox_claim_manual_steer"));
        assert!(interaction.contains("mailbox_restore_manual_steer_claim"));
        assert!(interaction.contains("inject_with_bounded_retry"));

        let intake = include_str!("message_handler/intake_turn.rs");
        assert!(
            !intake.contains("inject_with_bounded_retry") && !intake.contains("steering_hook::"),
            "busy intake must enqueue only; native TUI injection is button-only"
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
