//! Permanent domain keys stay independent of the notification's routing session.

use crate::db::replay_disposition::write::{WriteError, refused};
use crate::services::message_outbox::{OutboxMessage, validate_outbox_source};
use sqlx::{Postgres, Transaction};

pub(in crate::db::replay_disposition) async fn enqueue_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    message: OutboxMessage<'_>,
    dedupe_key: &str,
) -> Result<i64, WriteError> {
    if dedupe_key.trim().is_empty() {
        return Err(WriteError::CasMiss);
    }
    validate_outbox_source(message.source)
        .map_err(|_| WriteError::Fenced("notification source is not allowed".into()))?;
    // A collision may reuse the same event only when its original route and payload agree.
    sqlx::query_scalar(
        "INSERT INTO message_outbox
             (target, content, bot, source, reason_code, session_key, dedupe_key, dedupe_expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, NULL)
         ON CONFLICT (dedupe_key)
             WHERE dedupe_key IS NOT NULL AND status NOT IN ('failed', 'cancelled')
         DO UPDATE SET dedupe_expires_at = NULL
             WHERE message_outbox.target = EXCLUDED.target
               AND message_outbox.content = EXCLUDED.content
               AND message_outbox.bot = EXCLUDED.bot
               AND message_outbox.source = EXCLUDED.source
               AND message_outbox.reason_code IS NOT DISTINCT FROM EXCLUDED.reason_code
               AND message_outbox.session_key IS NOT DISTINCT FROM EXCLUDED.session_key
         RETURNING id",
    )
    .bind(message.target)
    .bind(message.content)
    .bind(message.bot)
    .bind(message.source)
    .bind(message.reason_code)
    .bind(message.session_key)
    .bind(dedupe_key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(refused)?
    .ok_or(WriteError::CasMiss)
}
