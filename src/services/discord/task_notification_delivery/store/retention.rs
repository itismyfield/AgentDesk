//! Bounded retention for task-card and terminal-delivery state.

use super::*;

pub(super) async fn cleanup_old_rows_pg(pool: &PgPool) {
    if let Err(error) = cleanup_old_rows_pg_impl(pool).await {
        tracing::debug!(error = %error, "task notification bounded retention cleanup failed");
    }
}

#[cfg(test)]
pub(in crate::services::discord::task_notification_delivery) async fn cleanup_old_rows_pg_checked(
    pool: &PgPool,
) -> Result<(), String> {
    cleanup_old_rows_pg_impl(pool).await
}

async fn cleanup_old_rows_pg_impl(pool: &PgPool) -> Result<(), String> {
    sqlx::query(
        "DELETE FROM task_notification_terminal_delivery
         WHERE id IN (
             SELECT id FROM task_notification_terminal_delivery
             WHERE delivered_at < NOW() - make_interval(days => $1::int)
             ORDER BY delivered_at ASC
             LIMIT $2
         )",
    )
    .bind(RETENTION_DAYS)
    .bind(RETENTION_DELETE_LIMIT)
    .execute(pool)
    .await
    .map_err(|error| format!("terminal delivery retention cleanup failed: {error}"))?;
    sqlx::query(
        "DELETE FROM task_notification_response_delivery
         WHERE id IN (
             SELECT id FROM task_notification_response_delivery
             WHERE updated_at < NOW() - make_interval(days => $1::int)
               AND delivery_state = 'delivered'
             ORDER BY updated_at ASC
             LIMIT $2
         )",
    )
    .bind(RETENTION_DAYS)
    .bind(RETENTION_DELETE_LIMIT)
    .execute(pool)
    .await
    .map_err(|error| format!("task response bounded retention cleanup failed: {error}"))?;
    sqlx::query(
        "DELETE FROM task_notification_card_state
         WHERE id IN (
             SELECT card.id FROM task_notification_card_state AS card
             WHERE card.updated_at < NOW() - make_interval(days => $1::int)
               AND card.lease_owner IS NULL
               AND NOT (card.delivery_state = 'posting' AND card.post_started_at IS NOT NULL)
               AND NOT EXISTS (
                   SELECT 1 FROM task_notification_response_delivery AS response
                   WHERE response.channel_id = card.channel_id
                     AND response.provider = card.provider
                     AND response.session_key = card.session_key
                     AND response.event_key = card.event_key
                     AND response.delivery_state <> 'delivered'
               )
             ORDER BY card.updated_at ASC
             LIMIT $2
         )",
    )
    .bind(RETENTION_DAYS)
    .bind(RETENTION_DELETE_LIMIT)
    .execute(pool)
    .await
    .map_err(|error| format!("task card bounded retention cleanup failed: {error}"))?;
    Ok(())
}
