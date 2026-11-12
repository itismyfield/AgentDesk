pub(crate) use crate::domain::outbound::outbox::identity::{
    ACTIONABLE_OPS_ALERT_BOT, dedupe_key_for_message, delivery_bot_for_target_session,
    is_actionable_ops_alert, is_non_turn_operational_alert, normalized_reason_code,
    normalized_session_key,
};

#[cfg(test)]
mod tests {
    use super::{dedupe_key_for_message, normalized_session_key};

    #[test]
    fn missing_empty_and_padded_session_keep_the_legacy_fallback() {
        for (raw, expected) in [
            (None, "channel:123"),
            (Some(""), "channel:123"),
            (Some("   "), "channel:123"),
            (Some(" route "), "route"),
        ] {
            assert_eq!(
                normalized_session_key(" channel:123 ", raw).as_deref(),
                Some(expected)
            );
        }
        assert_eq!(normalized_session_key("  ", None), None);
    }

    #[test]
    fn dedupe_wire_keys_keep_the_legacy_v1_bytes() {
        assert_eq!(
            dedupe_key_for_message(" channel:123 ", "body", None, Some(" session-abc ")).as_deref(),
            Some(
                "message_outbox:v1:a3dd137aeed6c9e9aa81a0943e4b57cfd54e6f77d85de2a23494a826a998ef0d"
            )
        );
        assert_eq!(
            dedupe_key_for_message(
                "channel:123",
                "ignored body",
                Some(" lifecycle.done "),
                Some("session-abc")
            )
            .as_deref(),
            Some(
                "message_outbox:v1:0eccb846df8d5f39cba65cd21b7a642a390d38d46a848015fb96d9feff54e917"
            )
        );
    }
}
