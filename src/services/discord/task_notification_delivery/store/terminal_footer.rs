//! Footer-owned persistence for source-identified terminal events (#4295).

use sqlx::PgPool;

use super::{TaskCardScope, cleanup_old_rows_pg, db_id, find_terminal_delivery_pg, stable_nonce};

pub(super) async fn record_footer_only_pg(
    pool: &PgPool,
    scope: &TaskCardScope,
    content: &str,
    content_hash: &str,
) -> Result<(), String> {
    if find_terminal_delivery_pg(pool, scope, content_hash)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let channel_id = db_id(scope.channel_id, "channel_id")?;
    let nonce = stable_nonce(scope, 1);
    sqlx::query(
        "INSERT INTO task_notification_card_state
             (channel_id, provider, session_key, event_key, surface_owner,
              delivery_state, bot_key, discord_nonce, revision, update_count,
              rendered_content, content_hash, terminal_delivery_fingerprint)
         VALUES ($1, $2, $3, $4, 'footer_only', 'footer_only', '', $5, 1, 1, $6, $7, $8)
         ON CONFLICT DO NOTHING",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(nonce)
    .bind(content)
    .bind(content_hash)
    .bind(&scope.terminal_delivery_fingerprint)
    .execute(pool)
    .await
    .map_err(|error| format!("record footer-only task card state: {error}"))?;
    sqlx::query(
        "UPDATE task_notification_card_state
         SET rendered_content = $6,
             content_hash = $7,
             terminal_delivery_fingerprint = COALESCE(
                 terminal_delivery_fingerprint, $5
             ),
             updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2
           AND ((session_key = $3 AND event_key = $4)
                OR ($5::VARCHAR IS NOT NULL AND terminal_delivery_fingerprint = $5))
           AND delivery_state = 'footer_only'",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(&scope.terminal_delivery_fingerprint)
    .bind(content)
    .bind(content_hash)
    .execute(pool)
    .await
    .map_err(|error| format!("refresh footer-only task card state: {error}"))?;
    cleanup_old_rows_pg(pool).await;
    Ok(())
}
