use super::JournalEvent;
use uuid::Uuid;

pub(super) enum AppendResult {
    Persisted,
    DuplicateNoOp,
    InvariantConflict,
}

/// The only raw PostgreSQL append entry point for the delivery journal.
pub(in crate::services::discord::session_relay_sink::journal) async fn append_delivery_journal_batch(
    pool: &sqlx::PgPool,
    events: &[JournalEvent],
) -> Result<AppendResult, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let mut inserted = false;
    for event in events {
        let receipt = event.receipt.as_ref();
        let result = sqlx::query(
            "INSERT INTO delivery_journal_events
             (event_id, obligation_id, attempt_id, event_kind, event_seq,
              idempotency_key, canonical_payload, requested_channel_id,
              returned_channel_id, message_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
             ON CONFLICT DO NOTHING",
        )
        .bind(event.event_id)
        .bind(event.obligation_id)
        .bind(event.attempt_id)
        .bind(event.kind)
        .bind(event.seq)
        .bind(&event.idempotency_key)
        .bind(&event.canonical_payload)
        .bind(receipt.map(|value| &value.requested_channel_id))
        .bind(receipt.map(|value| &value.returned_channel_id))
        .bind(receipt.map(|value| &value.message_id))
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() == 1 {
            inserted = true;
            continue;
        }
        let existing = sqlx::query_as::<_, (Uuid, Vec<u8>, serde_json::Value)>(
            "SELECT event_id, idempotency_key, canonical_payload
               FROM delivery_journal_events
              WHERE obligation_id = $1 AND event_seq = $2",
        )
        .bind(event.obligation_id)
        .bind(event.seq)
        .fetch_one(&mut *transaction)
        .await?;
        if existing
            != (
                event.event_id,
                event.idempotency_key.clone(),
                event.canonical_payload.clone(),
            )
        {
            transaction.rollback().await?;
            return Ok(AppendResult::InvariantConflict);
        }
    }
    transaction.commit().await?;
    Ok(if inserted {
        AppendResult::Persisted
    } else {
        AppendResult::DuplicateNoOp
    })
}
