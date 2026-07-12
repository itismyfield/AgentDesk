//! Recurrence and retry timing policy for scheduled-message deliveries.

use chrono::{DateTime, Utc};

use crate::db::scheduled_messages as db;
use crate::services::scheduling::next_due_after_anchor;

/// A fire slot is retried this many times after interruptions before the
/// definition is failed outright. The delivery retry_count counts re-arms of
/// the same fire slot.
pub(super) const MAX_FIRE_RETRIES: i32 = 3;
const FIRE_RETRY_BACKOFF_SECS: [i64; 3] = [60, 300, 900];

/// A live future slot (manual trigger-now case) resumes as-is. Otherwise the
/// next occurrence is calculated from the shared schedule grammar.
pub(super) fn compute_resume(
    message_schedule: Option<&str>,
    timezone: &str,
    current_scheduled_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> (Option<DateTime<Utc>>, Option<&'static str>) {
    let Some(schedule) = message_schedule.filter(|value| !value.trim().is_empty()) else {
        return (None, None);
    };
    let next = if current_scheduled_at > now {
        current_scheduled_at
    } else {
        match next_due_after_anchor(schedule, timezone, current_scheduled_at, now) {
            Ok(next) => next,
            Err(error) => {
                tracing::warn!("[smsg] recurrence computation failed: {error}");
                return (None, Some(db::STATUS_FAILED));
            }
        }
    };
    if let Some(expires_at) = expires_at
        && next >= expires_at
    {
        return (None, Some(db::STATUS_EXPIRED));
    }
    (Some(next), None)
}

pub(super) fn fire_retry_next_at(
    retry_count_before_increment: i32,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    usize::try_from(retry_count_before_increment)
        .ok()
        .and_then(|index| FIRE_RETRY_BACKOFF_SECS.get(index))
        .map(|delay_secs| now + chrono::Duration::seconds(*delay_secs))
}
