//! Commit a withheld request and its one notification without a partial handoff.

use super::{ExactAttempt, WriteError, commit, refused, settle};
use crate::db::replay_disposition::hold_notice::{self, HoldNotice, NoticeRouting, outbox};
use crate::services::message_outbox::OutboxMessage;
use sqlx::{PgPool, Row};

pub(crate) async fn withhold_and_notify(
    pool: &PgPool,
    attempt: &ExactAttempt,
    reason: &str,
    preserved: &serde_json::Value,
    routing: NoticeRouting<'_>,
) -> Result<HoldNotice, WriteError> {
    if !preserved.is_object() || preserved.get(hold_notice::FIELD).is_some() {
        return Err(WriteError::CasMiss);
    }
    let mut tx = pool.begin().await.map_err(WriteError::Storage)?;
    let row = sqlx::query(
        "SELECT channel_id, replay_episode_nonce, replay_owner_incarnation,
                replay_disposition, replay_preserved FROM intake_outbox WHERE id = $1 FOR UPDATE",
    )
    .bind(attempt.receipt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(WriteError::Storage)?
    .ok_or(WriteError::CasMiss)?;
    let nonce: Option<String> = row
        .try_get("replay_episode_nonce")
        .map_err(WriteError::Storage)?;
    let incarnation: Option<String> = row
        .try_get("replay_owner_incarnation")
        .map_err(WriteError::Storage)?;
    if nonce.as_deref() != Some(attempt.nonce.as_str())
        || incarnation.as_deref() != Some(attempt.incarnation.as_str())
    {
        return Err(WriteError::CasMiss);
    }
    let disposition: Option<String> = row
        .try_get("replay_disposition")
        .map_err(WriteError::Storage)?;
    let stored: Option<serde_json::Value> = row
        .try_get("replay_preserved")
        .map_err(WriteError::Storage)?;
    if let Some(notice) = hold_notice::from_preserved(attempt.receipt_id, stored.as_ref())? {
        if disposition.as_deref() != Some("withheld") {
            return Err(WriteError::CasMiss);
        }
        commit(tx).await?;
        return Ok(notice);
    }
    if disposition.as_deref() != Some("withheld") {
        settle::on_tx(&mut tx, attempt, "withheld", Some((reason, preserved))).await?;
    }
    let channel: String = row.try_get("channel_id").map_err(WriteError::Storage)?;
    let target = format!("channel:{channel}");
    let dedupe_key = hold_notice::key(attempt.receipt_id);
    let outbox_id = outbox::enqueue_on_tx(
        &mut tx,
        OutboxMessage {
            target: &target,
            content: hold_notice::TEXT,
            bot: routing.bot,
            source: "lifecycle_notifier",
            reason_code: Some("stale_replay_withheld"),
            session_key: routing.session_key,
        },
        &dedupe_key,
    )
    .await?;
    let notice = HoldNotice {
        dedupe_key,
        outbox_id,
    };
    let saved = sqlx::query(
        "UPDATE intake_outbox
            SET replay_preserved = COALESCE(replay_preserved, '{}'::jsonb)
                                   || jsonb_build_object('stale_replay_notice', $4::JSONB)
          WHERE id = $1 AND replay_episode_nonce = $2 AND replay_owner_incarnation = $3
            AND replay_disposition = 'withheld'
            AND replay_preserved -> 'stale_replay_notice' IS NULL",
    )
    .bind(attempt.receipt_id)
    .bind(&attempt.nonce)
    .bind(&attempt.incarnation)
    .bind(serde_json::to_value(&notice).map_err(|_| WriteError::CasMiss)?)
    .execute(&mut *tx)
    .await
    .map_err(refused)?;
    if saved.rows_affected() != 1 {
        return Err(WriteError::CasMiss);
    }
    commit(tx).await?;
    Ok(notice)
}
