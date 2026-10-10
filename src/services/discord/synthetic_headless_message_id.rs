/// Lower bound of the synthetic-headless message-id range. Real Discord
/// snowflake ids never reach this value, so any id at or above it is a
/// synthetic id (headless placeholders, voice, external input-ledger keys).
/// Centralized here so both `turn_bridge::is_synthetic_headless_message_id`
/// and the typed `inflight` status-panel ownership ops (#3077) agree on the
/// boundary without coupling `inflight` to the serenity `MessageId` newtype.
pub(in crate::services::discord) const SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR: u64 =
    crate::services::tui_input::input_key::SYNTHETIC_KEY_BASE;

/// Raw `u64` form of `turn_bridge::is_synthetic_headless_message_id`.
pub(in crate::services::discord) fn is_synthetic_headless_message_id_raw(value: u64) -> bool {
    value >= SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_preserves_floor_boundary() {
        assert!(!is_synthetic_headless_message_id_raw(
            SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR - 1
        ));
        assert!(is_synthetic_headless_message_id_raw(
            SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR
        ));
        assert!(is_synthetic_headless_message_id_raw(u64::MAX));
    }
}
