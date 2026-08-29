/// Lower bound of the synthetic-headless message-id range. Real Discord
/// snowflake ids never reach this value, so any id at or above it is a
/// synthetic placeholder (headless recovery / creation-failed fallback).
/// Keeping the raw identity boundary separate avoids coupling status-panel
/// ownership code to the serenity `MessageId` newtype.
pub(in crate::services::discord) const SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR: u64 =
    8_000_000_000_000_000_000;

/// Raw `u64` form of the synthetic-headless identity predicate.
pub(in crate::services::discord) fn is_synthetic_headless_message_id_raw(value: u64) -> bool {
    value >= SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR
}
