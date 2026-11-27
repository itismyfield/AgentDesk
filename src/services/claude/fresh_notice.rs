//! A Claude turn that cannot continue its requested session tells the channel once.

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
        resumed = false,
        reason = %"transcript_missing",
        "Claude resume transcript missing; launching a fresh session instead"
    );
    let Some(channel) = channel else {
        return;
    };
    let Some(short) = claim(channel, requested) else {
        return;
    };
    // A session notice shows only its first line, so the whole notice is that line.
    let content = format!(
        "♻️ 이전 세션 {short} 기록을 찾지 못해 새 세션으로 시작했습니다. 이전 대화 맥락은 이어지지 않습니다."
    );
    let session_key = format!("claude_resume_lost:{requested}");
    crate::services::message_outbox::enqueue_lifecycle_notification_best_effort(
        pool,
        &format!("channel:{channel}"),
        Some(&session_key),
        REASON,
        &content,
    );
}

/// Logs that the thread's stored session `requested` has no hook-verified binding to resume under,
/// and posts the channel one notice that the turn starts fresh with the thread's recent messages.
pub(crate) async fn announce_unverified(
    pool: Option<&sqlx::PgPool>,
    channel: u64,
    requested: &str,
) {
    tracing::warn!(
        channel_id = channel,
        requested_session_id = requested,
        resumed = false,
        reason = %"thread_unverified_provider_session",
        "Claude thread session has no verified binding; launching a fresh session instead"
    );
    let (Some(short), Some(pool)) = (claim(channel, requested), pool) else {
        return;
    };
    let content = format!(
        "♻️ 이전 세션({short})은 이 스레드에서 이어받을 수 없어 새 세션으로 시작했습니다. 스레드의 최근 대화만 전달됩니다."
    );
    let (target, session_key) = (
        format!("channel:{channel}"),
        format!("claude_resume_unverified:{requested}"),
    );
    let queued = crate::services::message_outbox::enqueue_lifecycle_notification_pg(
        pool,
        &target,
        Some(&session_key),
        REASON,
        &content,
    );
    if let Err(error) = queued.await {
        tracing::warn!(channel_id = channel, %error, "unverified thread resume notice was not queued");
    }
}

/// Claims the one notice of `requested` in this channel; its short id when this call won it.
fn claim(channel: u64, requested: &str) -> Option<String> {
    let claimed = (ANNOUNCED.lock().unwrap_or_else(PoisonError::into_inner))
        .insert((channel, requested.to_string()));
    if !claimed {
        return None;
    }
    #[cfg(any(test, feature = "test-support"))]
    (POSTED.lock().unwrap_or_else(PoisonError::into_inner)).push(requested.to_string());
    Some(requested.chars().take(8).collect())
}

#[cfg(any(test, feature = "test-support"))]
static POSTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// How many notices this process posted for the lost session `requested`.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn posted_for_tests(requested: &str) -> usize {
    let posted = POSTED.lock().unwrap_or_else(PoisonError::into_inner);
    posted.iter().filter(|posted| *posted == requested).count()
}

/// Forgets the in-process claim on `requested`, as a restart does.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn forget_for_tests(requested: &str) {
    let mut announced = ANNOUNCED.lock().unwrap_or_else(PoisonError::into_inner);
    announced.retain(|(_, lost)| lost != requested);
}

#[cfg(test)]
#[path = "fresh_notice_tests.rs"]
mod tests;
