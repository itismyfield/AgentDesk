//! The retained identity of one request's withheld notification.

use super::write::WriteError;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

pub(super) mod outbox;

pub(super) const FIELD: &str = "stale_replay_notice";
pub(super) const TEXT: &str =
    "출력 또는 도구 활동이 있어 자동 재실행을 보류했습니다. 원 요청과 부분 결과를 보존했습니다.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HoldNotice {
    pub(crate) dedupe_key: String,
    pub(crate) outbox_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hold_nonce: Option<String>,
}

#[derive(Clone, Copy)]
pub(crate) struct NoticeRouting<'a> {
    pub(crate) bot: &'a str,
    pub(crate) session_key: Option<&'a str>,
}

pub(super) fn key(receipt_id: i64) -> String {
    format!("stale_replay_withheld:{receipt_id}")
}

pub(super) fn from_preserved(
    receipt_id: i64,
    preserved: Option<&serde_json::Value>,
) -> Result<Option<HoldNotice>, WriteError> {
    if preserved.is_some_and(|value| !value.is_object()) {
        return Err(WriteError::CasMiss);
    }
    let Some(value) = preserved.and_then(|value| value.get(FIELD)) else {
        return Ok(None);
    };
    let notice: HoldNotice =
        serde_json::from_value(value.clone()).map_err(|_| WriteError::CasMiss)?;
    if notice.dedupe_key != key(receipt_id) || notice.outbox_id <= 0 {
        return Err(WriteError::CasMiss);
    }
    Ok(Some(notice))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoticeRead {
    Missing,
    NotNotified,
    Notified(HoldNotice),
}

#[derive(Debug)]
pub(crate) enum NoticeReadError {
    Storage(sqlx::Error),
    Malformed,
}

impl std::fmt::Display for NoticeReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "notice read failed: {error}"),
            Self::Malformed => f.write_str("malformed replay notice"),
        }
    }
}

pub(crate) async fn read(pool: &PgPool, receipt_id: i64) -> Result<NoticeRead, NoticeReadError> {
    let row: Option<(Option<serde_json::Value>, Option<String>)> = sqlx::query_as(
        "SELECT replay_preserved, replay_episode_nonce FROM intake_outbox WHERE id = $1",
    )
    .bind(receipt_id)
    .fetch_optional(pool)
    .await
    .map_err(NoticeReadError::Storage)?;
    let Some((preserved, nonce)) = row else {
        return Ok(NoticeRead::Missing);
    };
    match from_preserved(receipt_id, preserved.as_ref()).map_err(|_| NoticeReadError::Malformed)? {
        None => Ok(NoticeRead::NotNotified),
        Some(notice) => {
            if notice
                .hold_nonce
                .as_deref()
                .is_some_and(|pinned| pinned.is_empty() || Some(pinned) != nonce.as_deref())
            {
                return Err(NoticeReadError::Malformed);
            }
            Ok(NoticeRead::Notified(notice))
        }
    }
}

#[cfg(test)]
#[path = "hold_notice_tests.rs"]
mod tests;
