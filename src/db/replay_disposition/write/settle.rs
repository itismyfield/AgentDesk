//! Exact receipt classification within the caller's transaction.

use super::{ExactAttempt, WriteError, refused};

pub(super) async fn on_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    attempt: &ExactAttempt,
    disposition: &str,
    hold: Option<(&str, &serde_json::Value)>,
) -> Result<(), WriteError> {
    let settled = sqlx::query(
        "UPDATE intake_outbox
            SET replay_disposition = $4, replay_hold_reason = COALESCE($5, replay_hold_reason),
                replay_preserved = COALESCE(replay_preserved, '{}'::jsonb) || COALESCE($6, '{}'::jsonb)
          WHERE id = $1 AND replay_episode_nonce = $2 AND replay_owner_incarnation = $3
            AND (replay_disposition = 'started_unclassified'
                 OR (replay_disposition = 'startup_failed_no_effect' AND $4 = 'withheld'))",
    )
    .bind(attempt.receipt_id)
    .bind(&attempt.nonce)
    .bind(&attempt.incarnation)
    .bind(disposition)
    .bind(hold.map(|(reason, _)| reason))
    .bind(hold.map(|(_, preserved)| preserved))
    .execute(&mut **tx)
    .await
    .map_err(refused)?;
    if settled.rows_affected() != 1 {
        return Err(WriteError::CasMiss);
    }
    Ok(())
}
