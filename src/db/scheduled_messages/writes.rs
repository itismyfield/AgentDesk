//! #4658: definition insert (pool + transactional). Split from the parent
//! module to keep it under the giant-file threshold. The snapshot-strategy
//! create path uses `insert_scheduled_message_tx` so the snapshot capture and
//! the definition insert share one transaction.

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::{DEFINITION_COLUMNS, NewScheduledMessage, ScheduledMessageRow};

pub async fn insert_scheduled_message_pg(
    pool: &PgPool,
    new: &NewScheduledMessage,
) -> Result<ScheduledMessageRow, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row = insert_scheduled_message_tx(&mut tx, new).await?;
    tx.commit().await?;
    Ok(row)
}

/// Insert a definition on a caller-owned transaction. #4658 snapshot creation
/// captures the immutable snapshot row on this same `tx` first (so the FK and
/// `chk_smsg_snapshot_required` are satisfied) and passes its id via
/// `new.context_snapshot_id`.
pub async fn insert_scheduled_message_tx(
    tx: &mut Transaction<'_, Postgres>,
    new: &NewScheduledMessage,
) -> Result<ScheduledMessageRow, sqlx::Error> {
    let id = format!("smsg_{}", Uuid::new_v4());
    sqlx::query_as::<_, ScheduledMessageRow>(&format!(
        "INSERT INTO scheduled_messages
            (id, content, title, target_channel_id, bot, delivery_kind, agent_id,
             agent_instruction, on_agent_failure, scheduled_at, schedule, timezone,
             expires_at, source, created_by, dedupe_key, context_strategy,
             context_snapshot_id, on_context_failure)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                 $17, $18, $19)
         RETURNING {DEFINITION_COLUMNS}"
    ))
    .bind(&id)
    .bind(&new.content)
    .bind(&new.title)
    .bind(&new.target_channel_id)
    .bind(&new.bot)
    .bind(&new.delivery_kind)
    .bind(&new.agent_id)
    .bind(&new.agent_instruction)
    .bind(&new.on_agent_failure)
    .bind(new.scheduled_at)
    .bind(&new.schedule)
    .bind(&new.timezone)
    .bind(new.expires_at)
    .bind(&new.source)
    .bind(&new.created_by)
    .bind(&new.dedupe_key)
    .bind(&new.context_strategy)
    .bind(&new.context_snapshot_id)
    .bind(&new.on_context_failure)
    .fetch_one(&mut **tx)
    .await
}
