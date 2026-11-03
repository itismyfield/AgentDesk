//! A Claude launch that cannot continue its requested session tells the channel once.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{LazyLock, Mutex, PoisonError};

/// The lifecycle session-notice class this notice posts under, as a failed resume does.
const REASON: &str = "lifecycle.session_resume_failed";

/// Lost sessions this process already announced, by channel; the outbox key covers a restart.
static ANNOUNCED: LazyLock<Mutex<HashSet<(u64, String)>>> = LazyLock::new(Default::default);

/// Logs that `requested` starts fresh because its transcript at `missing` is gone, and posts the
/// channel one notice through the lifecycle outbox; later launches of the same lost session post none.
pub(super) fn announce(
    pool: Option<&sqlx::PgPool>,
    channel: Option<u64>,
    requested: &str,
    missing: &Path,
) {
    tracing::warn!(
        channel_id = channel,
        requested_session_id = requested,
        missing_transcript = %missing.display(),
        "Claude resume transcript missing; launching a fresh session instead"
    );
    let Some(channel) = channel else {
        return;
    };
    let claimed = (ANNOUNCED.lock().unwrap_or_else(PoisonError::into_inner))
        .insert((channel, requested.to_string()));
    if !claimed {
        return;
    }
    // A session notice shows only its first line, so the whole notice is that line.
    let short: String = requested.chars().take(8).collect();
    let content = format!(
        "♻️ 이전 세션 {short} 기록을 찾지 못해 새 세션으로 시작했습니다. 이전 대화 맥락은 이어지지 않습니다."
    );
    #[cfg(test)]
    (POSTED.lock().unwrap_or_else(PoisonError::into_inner)).push(requested.to_string());
    let session_key = format!("claude_resume_lost:{requested}");
    crate::services::message_outbox::enqueue_lifecycle_notification_best_effort(
        pool,
        &format!("channel:{channel}"),
        Some(&session_key),
        REASON,
        &content,
    );
}

#[cfg(test)]
static POSTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// How many notices this process posted for the lost session `requested`.
#[cfg(test)]
pub(crate) fn posted_for_tests(requested: &str) -> usize {
    let posted = POSTED.lock().unwrap_or_else(PoisonError::into_inner);
    posted.iter().filter(|posted| *posted == requested).count()
}

/// Forgets the in-process claim on `requested`, as a restart does.
#[cfg(test)]
pub(crate) fn forget_for_tests(requested: &str) {
    let mut announced = ANNOUNCED.lock().unwrap_or_else(PoisonError::into_inner);
    announced.retain(|(_, lost)| lost != requested);
}

#[cfg(test)]
#[path = "fresh_notice_tests.rs"]
mod tests;
