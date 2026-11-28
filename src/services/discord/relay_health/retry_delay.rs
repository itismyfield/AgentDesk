// The sixth attempt runs after the placeholder shield expires at +900 seconds.
// A still-stalled episode re-arms after one hour.
pub(in crate::services::discord) const REDRIVE_MAX_NO_PROGRESS_ATTEMPTS: u8 = 6;
pub(in crate::services::discord) const REDRIVE_BACKOFF_SECS: [i64;
    REDRIVE_MAX_NO_PROGRESS_ATTEMPTS as usize] = [30, 60, 120, 240, 480, 960];
pub(in crate::services::discord) const REDRIVE_CAPPED_REARM_SECS: i64 = 3600;

pub(in crate::services::discord) fn relay_recovery_retry_delay_secs(attempts: u32) -> i64 {
    match attempts {
        0 => 0,
        n if n >= std::primitive::u32::from(REDRIVE_MAX_NO_PROGRESS_ATTEMPTS) => {
            REDRIVE_CAPPED_REARM_SECS
        }
        n => REDRIVE_BACKOFF_SECS[n as usize - 1],
    }
}
