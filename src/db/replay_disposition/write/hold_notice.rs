//! Commit a withheld request and its one notification without a partial handoff.

#[cfg(test)]
use super::{ExactAttempt, settle};
use super::{WriteError, commit, refused};
use crate::db::replay_disposition::hold_notice::{self, HoldNotice, NoticeRouting, outbox};
use crate::services::message_outbox::OutboxMessage;
use sqlx::PgPool;
#[cfg(test)]
use sqlx::Row;

#[cfg(test)]
pub(crate) async fn withhold_and_notify(
    pool: &PgPool,
    attempt: &ExactAttempt,
    reason: &str,
    preserved: &serde_json::Value,
    routing: NoticeRouting<'_>,
) -> Result<HoldNotice, WriteError> {
    let preserved_object = preserved.as_object().ok_or(WriteError::CasMiss)?;
    if preserved.get(hold_notice::FIELD).is_some() {
        return Err(WriteError::CasMiss);
    }
    let mut tx = pool.begin().await.map_err(WriteError::Storage)?;
    let row = sqlx::query(
        "SELECT channel_id, replay_episode_nonce, replay_owner_incarnation,
                replay_disposition, replay_hold_reason, replay_preserved FROM intake_outbox WHERE id = $1 FOR UPDATE",
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
    if disposition.as_deref() == Some("withheld") {
        let stored_reason: Option<String> = row
            .try_get("replay_hold_reason")
            .map_err(WriteError::Storage)?;
        if stored_reason.as_deref() != Some(reason)
            || preserved_object
                .iter()
                .any(|(key, value)| stored.as_ref().and_then(|saved| saved.get(key)) != Some(value))
        {
            return Err(WriteError::CasMiss);
        }
    }
    if let Some(notice) = hold_notice::from_preserved(attempt.receipt_id, stored.as_ref())? {
        if disposition.as_deref() != Some("withheld")
            || notice
                .hold_nonce
                .as_deref()
                .is_some_and(|nonce| nonce != attempt.nonce())
        {
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
        hold_nonce: Some(attempt.nonce().to_string()),
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

/// Restores delivery custody only: the locked canonical tuple never becomes an execution permit.
pub(crate) async fn recover_hold_notice(
    pool: &PgPool,
    receipt_id: i64,
    provider: &str,
    channel: &str,
    capture: Option<&serde_json::Value>,
    routing: NoticeRouting<'_>,
) -> Result<HoldNotice, WriteError> {
    let mut tx = pool.begin().await.map_err(WriteError::Storage)?;
    let row: (String, Option<String>, Option<String>, Option<serde_json::Value>) = sqlx::query_as(
        "SELECT replay_disposition, replay_episode_nonce, replay_owner_incarnation, replay_preserved
           FROM intake_outbox WHERE id=$1 AND lower(btrim(provider))=lower(btrim($2))
             AND channel_id=$3 FOR UPDATE",
    ).bind(receipt_id).bind(provider).bind(channel).fetch_optional(&mut *tx).await
        .map_err(WriteError::Storage)?.ok_or(WriteError::CasMiss)?;
    let (disposition, nonce, incarnation, stored) = row;
    if !matches!(
        disposition.as_str(),
        "started_unclassified" | "startup_failed_no_effect" | "withheld"
    ) {
        return Err(WriteError::CasMiss);
    }
    if nonce.as_deref() == Some("") || (disposition != "withheld" && nonce.is_none()) {
        return Err(WriteError::CasMiss);
    }
    if let Some(notice) = hold_notice::from_preserved(receipt_id, stored.as_ref())? {
        if disposition != "withheld"
            || notice
                .hold_nonce
                .as_deref()
                .is_some_and(|pinned| pinned.is_empty() || Some(pinned) != nonce.as_deref())
        {
            return Err(WriteError::CasMiss);
        }
        commit(tx).await?;
        return Ok(notice);
    }
    let mut preserved = stored.unwrap_or_else(|| serde_json::json!({}));
    let object = preserved.as_object_mut().ok_or(WriteError::CasMiss)?;
    if let Some(capture) = capture {
        object
            .entry("recovery_snapshot")
            .or_insert_with(|| capture.clone());
    }
    let target = format!("channel:{channel}");
    let dedupe_key = hold_notice::key(receipt_id);
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
        hold_nonce: nonce.clone(),
    };
    object.insert(
        hold_notice::FIELD.into(),
        serde_json::to_value(&notice).map_err(|_| WriteError::CasMiss)?,
    );
    let saved = sqlx::query(
        "UPDATE intake_outbox SET replay_disposition='withheld', replay_preserved=$5,
             replay_hold_reason=COALESCE(replay_hold_reason, 'restart without a live continuation')
          WHERE id=$1 AND replay_episode_nonce IS NOT DISTINCT FROM $2
            AND replay_owner_incarnation IS NOT DISTINCT FROM $3 AND replay_disposition=$4",
    )
    .bind(receipt_id)
    .bind(nonce)
    .bind(incarnation)
    .bind(disposition)
    .bind(preserved)
    .execute(&mut *tx)
    .await
    .map_err(refused)?;
    if saved.rows_affected() != 1 {
        return Err(WriteError::CasMiss);
    }
    commit(tx).await?;
    Ok(notice)
}
