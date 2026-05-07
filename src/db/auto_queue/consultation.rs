use sqlx::PgPool;
use thiserror::Error;

use super::entries::{
    ENTRY_STATUS_DISPATCHED, EntryStatusUpdateOptions, update_entry_status_on_pg_tx,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsultationDispatchRecordResult {
    pub metadata_json: String,
    pub entry_status_changed: bool,
}

#[derive(Debug, Error)]
pub enum ConsultationDispatchRecordError {
    #[error("consultation dispatch id is required")]
    MissingDispatchId,
    #[error("consultation trigger source is required")]
    MissingSource,
    #[error("consultation card not found: {card_id}")]
    CardNotFound { card_id: String },
}

fn consultation_metadata_object(
    base_metadata_json: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let trimmed = base_metadata_json.trim();
    if trimmed.is_empty() {
        return serde_json::Map::new();
    }

    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

pub async fn record_consultation_dispatch_on_pg(
    pool: &PgPool,
    entry_id: &str,
    card_id: &str,
    dispatch_id: &str,
    trigger_source: &str,
    base_metadata_json: &str,
) -> Result<ConsultationDispatchRecordResult, String> {
    let dispatch_id = dispatch_id.trim();
    if dispatch_id.is_empty() {
        return Err(ConsultationDispatchRecordError::MissingDispatchId.to_string());
    }
    let trigger_source = trigger_source.trim();
    if trigger_source.is_empty() {
        return Err(ConsultationDispatchRecordError::MissingSource.to_string());
    }

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres consultation dispatch transaction: {error}"))?;
    let mut metadata = consultation_metadata_object(base_metadata_json);
    metadata.insert(
        "consultation_status".to_string(),
        serde_json::json!("pending"),
    );
    metadata.insert(
        "consultation_dispatch_id".to_string(),
        serde_json::json!(dispatch_id),
    );
    let metadata_json = serde_json::Value::Object(metadata).to_string();

    let updated = sqlx::query(
        "UPDATE kanban_cards
         SET metadata = $1::jsonb,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(&metadata_json)
    .bind(card_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("update postgres consultation metadata for {card_id}: {error}"))?
    .rows_affected();
    if updated == 0 {
        tx.rollback().await.map_err(|error| {
            format!("rollback missing postgres consultation card {card_id}: {error}")
        })?;
        return Err(ConsultationDispatchRecordError::CardNotFound {
            card_id: card_id.to_string(),
        }
        .to_string());
    }

    let entry_result = update_entry_status_on_pg_tx(
        &mut tx,
        entry_id,
        ENTRY_STATUS_DISPATCHED,
        trigger_source,
        &EntryStatusUpdateOptions {
            dispatch_id: Some(dispatch_id.to_string()),
            slot_index: None,
        },
    )
    .await?;
    if !entry_result.changed {
        tx.rollback().await.map_err(|error| {
            format!("rollback stale postgres consultation dispatch entry {entry_id}: {error}")
        })?;
        return Err(format!(
            "stale postgres consultation dispatch entry {entry_id}: status update was not applied"
        ));
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres consultation dispatch transaction: {error}"))?;

    Ok(ConsultationDispatchRecordResult {
        metadata_json,
        entry_status_changed: entry_result.changed,
    })
}
