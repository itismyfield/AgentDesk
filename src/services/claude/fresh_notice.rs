//! A Claude launch that cannot continue its requested session tells the channel once.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{LazyLock, Mutex, PoisonError};

use crate::services::observability::turn_lifecycle::{RecoveryDetails, TurnEvent};

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
    let short: String = requested.chars().take(8).collect();
    let event = TurnEvent::SessionResumeFailedWithRecovery(RecoveryDetails {
        reason: format!("이전 세션 `{short}`의 대화 기록을 이 작업 폴더·계정에서 찾지 못함"),
        recovery_action: "새 세션으로 시작 — 이전 대화 맥락은 이어지지 않습니다".to_string(),
        previous_session_key: None,
        recovered_session_key: None,
        recovery: None,
    });
    let (Some(reason), Some(content)) = (
        event.notification_reason_code(),
        event.notification_content(),
    ) else {
        return;
    };
    #[cfg(test)]
    (POSTED.lock().unwrap_or_else(PoisonError::into_inner)).push(requested.to_string());
    let session_key = format!("claude_resume_lost:{requested}");
    crate::services::message_outbox::enqueue_lifecycle_notification_best_effort(
        pool,
        &format!("channel:{channel}"),
        Some(&session_key),
        reason,
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
