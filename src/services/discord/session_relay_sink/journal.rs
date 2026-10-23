use super::RelaySinkError;
use crate::services::discord::SharedData;
use crate::services::discord::outbound::DiscordTransportReceipt;
use poise::serenity_prelude::{ChannelId, MessageId};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use serenity::http::Http;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use uuid::Uuid;
mod pg_store;
const JOURNAL_NAMESPACE: Uuid = Uuid::from_u128(0xd9829c0b_8692_4ef0_9396_f7d83aa84dd5);
const MAILBOX_CAPACITY: usize = 256;
#[cfg(not(test))]
static PROCESS_OBSERVER: std::sync::LazyLock<JournalObserver> =
    std::sync::LazyLock::new(JournalObserver::default);
#[cfg(not(test))]
pub(super) fn process_observer() -> &'static JournalObserver {
    &PROCESS_OBSERVER
}
#[derive(Clone)]
pub(in crate::services::discord) struct JournalEvent {
    event_id: Uuid,
    obligation_id: Uuid,
    attempt_id: Option<Uuid>,
    kind: &'static str,
    seq: i16,
    idempotency_key: Vec<u8>,
    canonical_payload: Value,
    receipt: Option<DiscordTransportReceipt>,
}

struct AppendCommand {
    pool: sqlx::PgPool,
    events: Vec<JournalEvent>,
    ack: Option<tokio::sync::oneshot::Sender<Result<pg_store::AppendResult, String>>>,
}

#[derive(Default)]
pub(super) struct JournalObserver {
    sender: Mutex<Option<mpsc::Sender<AppendCommand>>>,
}
impl JournalObserver {
    fn sender(&self) -> mpsc::Sender<AppendCommand> {
        let mut slot = self
            .sender
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        slot.get_or_insert_with(|| {
            let (sender, mut receiver) = mpsc::channel::<AppendCommand>(MAILBOX_CAPACITY);
            super::super::task_supervisor::spawn_observed(
                "delivery_journal_observer",
                async move {
                    while let Some(command) = receiver.recv().await {
                        let result =
                            pg_store::append_delivery_journal_batch(&command.pool, &command.events)
                                .await;
                        if let Some(ack) = command.ack {
                            let reply = match &result {
                                Ok(
                                    value @ (pg_store::AppendResult::Persisted
                                    | pg_store::AppendResult::DuplicateNoOp),
                                ) => Ok(*value),
                                Ok(pg_store::AppendResult::InvariantConflict) => {
                                    Err("strict invariant conflict".into())
                                }
                                Err(error) => Err(error.to_string()),
                            };
                            let _ = ack.send(reply);
                        }
                        if let Err(error) = &result {
                            tracing::warn!(error = %error, "delivery journal append failed");
                        }
                    }
                },
            );
            sender
        })
        .clone()
    }
}
pub(super) fn anchor_receipt(
    receipts: &[DiscordTransportReceipt],
) -> Option<DiscordTransportReceipt> {
    receipts.last().cloned()
}
pub(super) async fn send_long_chunks_with_anchor_receipt(
    gateway: Option<&dyn super::super::gateway::TurnGateway>,
    http: &Http,
    channel: ChannelId,
    anchor: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(Vec<MessageId>, Option<DiscordTransportReceipt>), RelaySinkError> {
    if let Some(gateway) = gateway {
        return gateway
            .send_long_message_with_rollback(channel, anchor, text)
            .await
            .map(|message_ids| (message_ids, None))
            .map_err(RelaySinkError::Transient);
    }
    let receipts =
        super::super::formatting::send_long_message_raw_with_rollback_returning_receipts(
            http, channel, anchor, text, shared,
        )
        .await
        .map_err(|error| RelaySinkError::Transient(error.to_string()))?;
    let message_ids = super::super::formatting::message_ids_from_receipts(receipts.clone())
        .map_err(|error| RelaySinkError::Transient(error.to_string()))?;
    Ok((message_ids, anchor_receipt(&receipts)))
}
fn event(
    obligation_id: Uuid,
    attempt_id: Option<Uuid>,
    kind: &'static str,
    seq: i16,
    payload: Value,
) -> JournalEvent {
    let event_id = Uuid::new_v5(&obligation_id, format!("event:{kind}:{seq}").as_bytes());
    let mut key = Sha256::new();
    key.update(obligation_id.as_bytes());
    key.update(kind.as_bytes());
    key.update(seq.to_be_bytes());
    let idempotency_key = key.finalize().to_vec();
    JournalEvent {
        event_id,
        obligation_id,
        attempt_id,
        kind,
        seq,
        idempotency_key,
        canonical_payload: payload,
        receipt: None,
    }
}
pub(crate) async fn append_exact_metadata(
    pool: sqlx::PgPool,
    metadata: &crate::services::tui_o::exact_episode::EpisodeMetadata,
) -> Result<(), String> {
    if !metadata.supported() {
        return Err("unsupported strict metadata".into());
    }
    let payload = serde_json::to_value(metadata).map_err(|e| e.to_string())?;
    let obligation = Uuid::new_v5(
        &JOURNAL_NAMESPACE,
        format!(
            "strict:{}:{}",
            metadata.episode,
            if matches!(
                metadata.evidence,
                crate::services::tui_o::exact_episode::EpisodeEvidence::Settled { .. }
            ) {
                "settled".to_string()
            } else {
                metadata.record.to_string()
            }
        )
        .as_bytes(),
    );
    let envelope = event(obligation, None, "O", 0, payload);
    let (ack, receiver) = tokio::sync::oneshot::channel();
    #[cfg(test)]
    let observer = JournalObserver::default();
    #[cfg(not(test))]
    let observer = process_observer();
    observer
        .sender()
        .send(AppendCommand {
            pool,
            events: vec![envelope],
            ack: Some(ack),
        })
        .await
        .map_err(|e| e.to_string())?;
    receiver.await.map_err(|e| e.to_string())?.map(|_| ())
}
#[cfg(test)]
pub(crate) use pg_store::exact_tests::{
    exact_duplicate_pg_full_fields_and_legacy_same_key_other_attempt,
    exact_namespace_pg_old_reader_and_legacy_binding_bytes_unchanged,
};
pub(in crate::services::discord) struct ObligationWindowJudgment {
    delivered_outbox_id: Option<i64>,
    #[cfg(test)]
    malformed: bool,
}
#[cfg(test)]
fn admission_events(
    obligation_id: Uuid,
    attempt_id: Uuid,
    canonical_payload: Value,
    frontier: (u64, u64),
) -> Vec<JournalEvent> {
    vec![
        event(obligation_id, None, "O", 0, canonical_payload),
        event(
            obligation_id,
            Some(attempt_id),
            "A",
            1,
            json!({
                "attempt": 0,
                "frontier_start": frontier.0,
                "frontier_end": frontier.1,
            }),
        ),
    ]
}
#[cfg(test)]
fn transport_event(
    obligation_id: Uuid,
    attempt_id: Uuid,
    receipt: DiscordTransportReceipt,
) -> JournalEvent {
    let payload = json!({"requested_channel_id": receipt.requested_channel_id,
        "returned_channel_id": receipt.returned_channel_id, "message_id": receipt.message_id});
    let mut event = event(obligation_id, Some(attempt_id), "T", 2, payload);
    event.receipt = Some(receipt);
    event
}
fn judge_loaded_obligation_window(
    loaded: pg_store::LoadedObligationWindow,
) -> ObligationWindowJudgment {
    match loaded {
        pg_store::LoadedObligationWindow::Events(events) => {
            let (delivered, binding, _malformed) = exact_delivery_predicate(&events);
            ObligationWindowJudgment {
                delivered_outbox_id: delivered.then_some(binding).flatten(),
                #[cfg(test)]
                malformed: _malformed,
            }
        }
        pg_store::LoadedObligationWindow::Malformed => malformed_judgment(),
    }
}
fn malformed_judgment() -> ObligationWindowJudgment {
    ObligationWindowJudgment {
        delivered_outbox_id: None,
        #[cfg(test)]
        malformed: true,
    }
}
fn exact_delivery_predicate(events: &[JournalEvent]) -> (bool, Option<i64>, bool) {
    let events = legacy_events(events);
    let events = events.as_slice();
    let Some(first) = events.first() else {
        return (false, None, false);
    };
    if events.iter().enumerate().any(|(index, event)| {
        event.obligation_id != first.obligation_id
            || !event_shape_is_valid(event)
            || index > 0 && events[index - 1].seq >= event.seq
            || events[..index]
                .iter()
                .any(|prior| prior.kind == event.kind || prior.seq == event.seq)
    }) {
        return (false, None, true);
    }

    let mut intake_outbox_id = None;
    for event in events {
        let Some(value) = event.canonical_payload.get("intake_outbox_id") else {
            continue;
        };
        let Some(value) = value.as_i64().filter(|value| *value > 0) else {
            return (false, None, true);
        };
        if event.kind != "O" || intake_outbox_id.replace(value).is_some() {
            return (false, None, true);
        }
    }

    let mut attempt_id = None;
    for event in events.iter().filter(|event| event.attempt_id.is_some()) {
        if attempt_id.is_some_and(|attempt| event.attempt_id != Some(attempt)) {
            return (false, None, true);
        }
        attempt_id = event.attempt_id;
    }
    let mut frontier = None;
    for event in events
        .iter()
        .filter(|event| matches!(event.kind, "A" | "C"))
    {
        let Some(value) = event_frontier(event) else {
            return (false, None, true);
        };
        if frontier.is_some_and(|existing| existing != value) {
            return (false, None, true);
        }
        frontier = Some(value);
    }
    if events
        .iter()
        .find(|event| event.kind == "T")
        .is_some_and(|event| !receipt_is_exact(event))
    {
        return (false, None, true);
    }

    let has = |kind| events.iter().any(|event| event.kind == kind);
    let delivered = events.len() == 4
        && ["O", "A", "T", "C"].iter().all(|kind| has(*kind))
        && !has("S")
        && !has("U");
    (delivered, intake_outbox_id, false)
}
fn event_shape_is_valid(event: &JournalEvent) -> bool {
    let (seq, attempt, receipt) = match event.kind {
        "O" => (0, false, false),
        "A" => (1, true, false),
        "T" => (2, true, true),
        "C" => (3, true, false),
        "S" => (1, false, false),
        "U" => (2, true, false),
        _ => return false,
    };
    event.seq == seq && event.attempt_id.is_some() == attempt && event.receipt.is_some() == receipt
}
fn event_frontier(event: &JournalEvent) -> Option<(u64, u64)> {
    let start = event.canonical_payload.get("frontier_start")?.as_u64()?;
    let end = event.canonical_payload.get("frontier_end")?.as_u64()?;
    (start <= end).then_some((start, end))
}
fn receipt_is_exact(event: &JournalEvent) -> bool {
    let Some(receipt) = event.receipt.as_ref() else {
        return false;
    };
    !receipt.requested_channel_id.is_empty()
        && receipt.requested_channel_id == receipt.returned_channel_id
        && !receipt.message_id.is_empty()
        && event
            .canonical_payload
            .get("requested_channel_id")
            .and_then(Value::as_str)
            == Some(receipt.requested_channel_id.as_str())
        && event
            .canonical_payload
            .get("returned_channel_id")
            .and_then(Value::as_str)
            == Some(receipt.returned_channel_id.as_str())
        && event
            .canonical_payload
            .get("message_id")
            .and_then(Value::as_str)
            == Some(receipt.message_id.as_str())
}
fn legacy_events(events: &[JournalEvent]) -> Vec<JournalEvent> {
    events
        .iter()
        .filter(|event| {
            event
                .canonical_payload
                .get("namespace")
                .and_then(Value::as_str)
                != Some(crate::services::tui_o::exact_episode::STRICT_NAMESPACE)
        })
        .cloned()
        .collect()
}

impl ObligationWindowJudgment {
    pub(in crate::services::discord) fn delivered_outbox_id(&self) -> Option<i64> {
        self.delivered_outbox_id
    }
    #[cfg(test)]
    pub(in crate::services::discord) fn malformed(&self) -> bool {
        self.malformed
    }
}

pub(in crate::services::discord) async fn judge_obligation_window(
    connection: &mut sqlx::PgConnection,
    obligation_id: Uuid,
) -> Result<ObligationWindowJudgment, sqlx::Error> {
    let loaded = pg_store::load_obligation_window(connection, obligation_id).await?;
    Ok(judge_loaded_obligation_window(loaded))
}
