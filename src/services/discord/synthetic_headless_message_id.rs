/// Lower bound of the synthetic-headless message-id range. Real Discord
/// snowflake ids never reach this value, so any id at or above it is a
/// synthetic placeholder (headless recovery / creation-failed fallback).
/// Centralized here so both `turn_bridge::is_synthetic_headless_message_id`
/// and the typed `inflight` status-panel ownership ops (#3077) agree on the
/// boundary without coupling `inflight` to the serenity `MessageId` newtype.
pub(in crate::services::discord) const SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR: u64 =
    8_000_000_000_000_000_000;

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
