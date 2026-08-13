use super::JournalEvent;
use crate::services::discord::outbound::DiscordTransportReceipt;
use uuid::Uuid;

pub(super) enum AppendResult {
    Persisted,
    DuplicateNoOp,
    InvariantConflict,
}

#[rustfmt::skip]
pub(super) enum LoadedObligationWindow { Events(Vec<JournalEvent>), Malformed }
#[derive(sqlx::FromRow)]
#[rustfmt::skip]
struct StoredJournalEvent {
    event_id: Uuid, obligation_id: Uuid, attempt_id: Option<Uuid>, event_kind: String,
    event_seq: i16, idempotency_key: Vec<u8>, canonical_payload: serde_json::Value,
    requested_channel_id: Option<String>, returned_channel_id: Option<String>,
    message_id: Option<String>,
}

pub(super) async fn load_obligation_window(
    connection: &mut sqlx::PgConnection,
    obligation_id: Uuid,
) -> Result<LoadedObligationWindow, sqlx::Error> {
    let rows = sqlx::query_as::<_, StoredJournalEvent>(
        "SELECT event_id, obligation_id, attempt_id, event_kind, event_seq,
                idempotency_key, canonical_payload, requested_channel_id,
                returned_channel_id, message_id FROM public.delivery_journal_events
          WHERE obligation_id = $1 ORDER BY event_seq, event_id",
    )
    .bind(obligation_id)
    .fetch_all(&mut *connection)
    .await?;
    Ok(match rows.into_iter().map(restore_stored_event).collect() {
        Ok(events) => LoadedObligationWindow::Events(events),
        Err(()) => LoadedObligationWindow::Malformed,
    })
}

fn restore_stored_event(row: StoredJournalEvent) -> Result<JournalEvent, ()> {
    let (kind, expected_seq, expects_attempt) = match row.event_kind.as_str() {
        "O" => ("O", 0, false),
        "A" => ("A", 1, true),
        "T" => ("T", 2, true),
        "C" => ("C", 3, true),
        "S" => ("S", 1, false),
        "U" => ("U", 2, true),
        _ => return Err(()),
    };
    if row.event_seq != expected_seq || row.attempt_id.is_some() != expects_attempt {
        return Err(());
    }
    let receipt = match (
        row.requested_channel_id,
        row.returned_channel_id,
        row.message_id,
    ) {
        (Some(requested_channel_id), Some(returned_channel_id), Some(message_id))
            if kind == "T" =>
        {
            Some(DiscordTransportReceipt {
                requested_channel_id,
                returned_channel_id,
                message_id,
            })
        }
        (None, None, None) if kind != "T" => None,
        _ => return Err(()),
    };
    Ok(JournalEvent {
        event_id: row.event_id,
        obligation_id: row.obligation_id,
        attempt_id: row.attempt_id,
        kind,
        seq: row.event_seq,
        idempotency_key: row.idempotency_key,
        canonical_payload: row.canonical_payload,
        receipt,
    })
}

/// The only production raw PostgreSQL append entry point for the delivery journal.
pub(in crate::services::discord::session_relay_sink::journal) async fn append_delivery_journal_batch(
    pool: &sqlx::PgPool,
    events: &[JournalEvent],
) -> Result<AppendResult, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let mut inserted = false;
    for event in events {
        let receipt = event.receipt.as_ref();
        let result = sqlx::query(
            "INSERT INTO public.delivery_journal_events
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
               FROM public.delivery_journal_events
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

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use super::*; use serde_json::json;
    fn stored(kind: &str, seq: i16, attempt_id: Option<Uuid>) -> StoredJournalEvent {
        StoredJournalEvent { event_id:Uuid::from_u128(10), obligation_id:Uuid::from_u128(11), attempt_id, event_kind:kind.into(), event_seq:seq, idempotency_key:vec![12], canonical_payload:json!({}), requested_channel_id:None, returned_channel_id:None, message_id:None }
    }
    fn receipt(mut row: StoredJournalEvent, fields: [Option<&str>; 3]) -> StoredJournalEvent {
        row.requested_channel_id=fields[0].map(str::to_string); row.returned_channel_id=fields[1].map(str::to_string); row.message_id=fields[2].map(str::to_string); row
    }
    #[test]
    fn stored_journal_event_mapping_is_closed_and_fail_closed() {
        let attempt = Uuid::from_u128(13);
        for (kind,seq,id) in [("O",0,None),("A",1,Some(attempt)),("C",3,Some(attempt)),("S",1,None),("U",2,Some(attempt))] { assert!(restore_stored_event(stored(kind,seq,id)).is_ok(),"closed {kind}"); }
        let transport=receipt(stored("T",2,Some(attempt)),[Some("10"),Some("10"),Some("20")]);
        assert_eq!(restore_stored_event(transport).unwrap().receipt.unwrap().message_id,"20");
        for row in [
            stored("future",0,None), stored("O",1,None), stored("A",1,None), stored("O",0,Some(attempt)),
            receipt(stored("T",2,Some(attempt)),[Some("10"),Some("10"),None]),
            receipt(stored("C",3,Some(attempt)),[Some("10"),Some("10"),Some("20")]),
        ] { assert!(restore_stored_event(row).is_err()); }
    }
}
