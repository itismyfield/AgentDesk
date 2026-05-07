use sqlx::PgPool;
use thiserror::Error;

use super::core::{
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use sqlx::{PgPool, Row};

    async fn setup_pool(pg_db: &TestPostgresDb) -> PgPool {
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-1', 'repo-1', 'agent-1', 'active')",
        )
        .execute(&pool)
        .await
        .expect("seed run");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-1', 'Agent 1', 'claude', '123')",
        )
        .execute(&pool)
        .await
        .expect("seed agent");

        pool
    }

    #[tokio::test]
    async fn record_consultation_dispatch_preserves_metadata_and_marks_entry_dispatched_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, metadata)
             VALUES ('card-consult', 'Card Consult', 'requested', CAST($1 AS jsonb))",
        )
        .bind(
            serde_json::json!({
                "keep": "yes",
                "preflight_status": "consult_required"
            })
            .to_string(),
        )
        .execute(&pool)
        .await
        .expect("seed kanban card");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-consult', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-consult', 'run-1', 'card-consult', 'agent-1', 'pending', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let result = record_consultation_dispatch_on_pg(
            &pool,
            "entry-consult",
            "card-consult",
            "dispatch-consult",
            "test_consultation_dispatch",
            r#"{"keep":"yes","preflight_status":"consult_required"}"#,
        )
        .await
        .expect("consultation dispatch");
        assert!(result.entry_status_changed);

        let metadata: serde_json::Value =
            sqlx::query_scalar("SELECT metadata::TEXT FROM kanban_cards WHERE id = 'card-consult'")
                .fetch_one(&pool)
                .await
                .ok()
                .and_then(|raw: String| serde_json::from_str(&raw).ok())
                .expect("metadata json");
        assert_eq!(metadata["keep"], "yes");
        assert_eq!(metadata["preflight_status"], "consult_required");
        assert_eq!(metadata["consultation_status"], "pending");
        assert_eq!(metadata["consultation_dispatch_id"], "dispatch-consult");

        let row = sqlx::query(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-consult'",
        )
        .fetch_one(&pool)
        .await
        .expect("entry row");
        let status: String = row.try_get("status").expect("status");
        let dispatch_id: Option<String> = row.try_get("dispatch_id").expect("dispatch_id");
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-consult"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn record_consultation_dispatch_requires_dispatch_id_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        let error = record_consultation_dispatch_on_pg(
            &pool,
            "entry-missing",
            "card-missing",
            "   ",
            "test_consultation_dispatch",
            "{}",
        )
        .await
        .expect_err("missing dispatch id must fail");
        assert!(
            error.contains("consultation dispatch id is required"),
            "expected missing-dispatch-id error, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
