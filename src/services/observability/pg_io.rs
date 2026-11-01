//! Postgres insert primitives for events and snapshots, `pub(super)` so the
//! worker can share them without touching the global runtime.

use anyhow::{Result, anyhow};
use sqlx::PgPool;

use super::helpers::saturating_i64;
use super::{QueuedEvent, QueuedQualityEvent, SnapshotRow};

pub(super) async fn insert_events_pg(pool: &PgPool, events: &[QueuedEvent]) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin postgres observability event tx: {error}"))?;
    for event in events {
        sqlx::query(
            "INSERT INTO observability_events (
                event_type,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, CAST($8 AS jsonb))",
        )
        .bind(&event.event_type)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.dispatch_id)
        .bind(&event.session_key)
        .bind(&event.turn_id)
        .bind(&event.status)
        .bind(&event.payload_json)
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow!("insert postgres observability event: {error}"))?;
    }
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit postgres observability event tx: {error}"))?;
    Ok(())
}

/// #2049 Finding 1 / Finding 5: When bulk insert rolls back the whole tx,
/// re-attempt each row in its own short tx so good rows still land. Returns
/// the rows that still failed (candidates for dead-letter JSONL or queue
/// push-back).
pub(super) async fn insert_events_pg_row_isolated(
    pool: &PgPool,
    events: &[QueuedEvent],
) -> Vec<QueuedEvent> {
    let mut failed = Vec::new();
    for event in events {
        let result = sqlx::query(
            "INSERT INTO observability_events (
                event_type,
                provider,
                channel_id,
                dispatch_id,
                session_key,
                turn_id,
                status,
                payload_json
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, CAST($8 AS jsonb))",
        )
        .bind(&event.event_type)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.dispatch_id)
        .bind(&event.session_key)
        .bind(&event.turn_id)
        .bind(&event.status)
        .bind(&event.payload_json)
        .execute(pool)
        .await;
        if let Err(error) = result {
            tracing::warn!(
                "[observability] per-row insert failed (event_type={}, dispatch_id={:?}): {error}",
                event.event_type,
                event.dispatch_id
            );
            failed.push(event.clone());
        }
    }
    failed
}

pub(super) async fn insert_quality_events_pg(
    pool: &PgPool,
    events: &[QueuedQualityEvent],
) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin postgres agent quality event tx: {error}"))?;
    for event in events {
        sqlx::query(
            "INSERT INTO agent_quality_event (
                source_event_id,
                correlation_id,
                agent_id,
                provider,
                channel_id,
                card_id,
                dispatch_id,
                event_type,
                payload
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8::agent_quality_event_type, CAST($9 AS jsonb))",
        )
        .bind(&event.source_event_id)
        .bind(&event.correlation_id)
        .bind(&event.agent_id)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.card_id)
        .bind(&event.dispatch_id)
        .bind(&event.event_type)
        .bind(&event.payload_json)
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow!("insert postgres agent quality event: {error}"))?;
    }
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit postgres agent quality event tx: {error}"))?;
    Ok(())
}

/// #2049 Finding 1 / Finding 5: Row-isolated retry for `agent_quality_event`
/// so one enum-cast failure cannot lose the whole 64-row batch.
pub(super) async fn insert_quality_events_pg_row_isolated(
    pool: &PgPool,
    events: &[QueuedQualityEvent],
) -> Vec<QueuedQualityEvent> {
    let mut failed = Vec::new();
    for event in events {
        let result = sqlx::query(
            "INSERT INTO agent_quality_event (
                source_event_id,
                correlation_id,
                agent_id,
                provider,
                channel_id,
                card_id,
                dispatch_id,
                event_type,
                payload
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8::agent_quality_event_type, CAST($9 AS jsonb))",
        )
        .bind(&event.source_event_id)
        .bind(&event.correlation_id)
        .bind(&event.agent_id)
        .bind(&event.provider)
        .bind(&event.channel_id)
        .bind(&event.card_id)
        .bind(&event.dispatch_id)
        .bind(&event.event_type)
        .bind(&event.payload_json)
        .execute(pool)
        .await;
        if let Err(error) = result {
            tracing::warn!(
                "[quality] per-row insert failed (event_type={}, agent_id={:?}): {error}",
                event.event_type,
                event.agent_id
            );
            failed.push(event.clone());
        }
    }
    failed
}

pub(super) async fn insert_snapshots_pg(pool: &PgPool, snapshots: &[SnapshotRow]) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin postgres observability snapshot tx: {error}"))?;
    for snapshot in snapshots {
        sqlx::query(
            "INSERT INTO observability_counter_snapshots (
                provider,
                channel_id,
                turn_attempts,
                guard_fires,
                watcher_replacements,
                recovery_fires,
                turn_successes,
                turn_failures
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&snapshot.provider)
        .bind(&snapshot.channel_id)
        .bind(saturating_i64(snapshot.values.turn_attempts))
        .bind(saturating_i64(snapshot.values.guard_fires))
        .bind(saturating_i64(snapshot.values.watcher_replacements))
        .bind(saturating_i64(snapshot.values.recovery_fires))
        .bind(saturating_i64(snapshot.values.turn_successes))
        .bind(saturating_i64(snapshot.values.turn_failures))
        .execute(&mut *tx)
        .await
        .map_err(|error| anyhow!("insert postgres observability snapshot: {error}"))?;
    }
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit postgres observability snapshot tx: {error}"))?;
    Ok(())
}
