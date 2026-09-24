//! Durable, bounded machine samples for timeline rendering and later agent use.
//! Recording follows a successful node heartbeat but never gates liveness.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use tokio::sync::mpsc;

pub(crate) const HISTORY_WINDOW_MS: i64 = 15 * 60 * 1_000;
pub(crate) const HISTORY_DEFAULT_LIMIT: i64 = 120;
pub(crate) const HISTORY_MAX_LIMIT: i64 = 240;
pub(crate) const HISTORY_RETENTION_DAYS: i64 = 90;
pub(crate) const HISTORY_CLEANUP_BATCH: i64 = 1_000;
const HISTORY_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);
const PENDING_SAMPLES: usize = 8;
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Optional telemetry I/O must not delay heartbeat or hub lease updates.
pub(crate) fn spawn_recorder(
    pool: PgPool,
    instance_id: String,
    hub_active: Arc<AtomicBool>,
) -> mpsc::Sender<Value> {
    let (sender, mut receiver) = mpsc::channel(PENDING_SAMPLES);
    tokio::spawn(async move {
        let mut cleanup = tokio::time::interval(HISTORY_CLEANUP_INTERVAL);
        cleanup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_error: Option<Instant> = None;
        loop {
            tokio::select! {
                sample = receiver.recv() => {
                    let Some(sample) = sample else { break; };
                    if crate::db::postgres::background_should_yield(&pool) { continue; }
                    let result = tokio::time::timeout(WRITE_TIMEOUT, record(&pool, &instance_id, &sample)).await;
                    let failure = match result {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(error.to_string()),
                        Err(_) => Some("database write timed out".to_owned()),
                    };
                    if let Some(message) = failure
                        && last_error.is_none_or(|last| last.elapsed() >= ERROR_LOG_INTERVAL)
                    {
                        tracing::warn!(instance_id, "[cluster] machine sample persistence failed: {message}");
                        last_error = Some(Instant::now());
                    }
                }
                _ = cleanup.tick() => {
                    if !hub_active.load(Ordering::Acquire)
                        || crate::db::postgres::background_should_yield(&pool) { continue; }
                    match tokio::time::timeout(CLEANUP_TIMEOUT, async {
                        loop {
                            if !hub_active.load(Ordering::Acquire)
                                || crate::db::postgres::background_should_yield(&pool)
                                || purge_expired(&pool).await? < HISTORY_CLEANUP_BATCH as u64
                            {
                                return Ok::<(), sqlx::Error>(());
                            }
                            tokio::task::yield_now().await;
                        }
                    }).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!("[cluster] machine sample retention failed: {error}"),
                        Err(_) => tracing::warn!("[cluster] machine sample retention timed out"),
                    }
                }
            }
        }
    });
    sender
}

pub(crate) async fn record(
    pool: &PgPool,
    instance_id: &str,
    resources: &Value,
) -> Result<(), sqlx::Error> {
    if !resources.is_object() {
        return Ok(());
    }
    let Some(observed_at_ms) = resources.get("observed_at_ms").and_then(Value::as_i64) else {
        return Ok(());
    };
    let Some(expires_at_ms) = resources.get("expires_at_ms").and_then(Value::as_i64) else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO machine_resource_samples (instance_id, observed_at_ms, expires_at_ms, resources)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (instance_id, observed_at_ms) DO NOTHING",
    )
    .bind(instance_id)
    .bind(observed_at_ms)
    .bind(expires_at_ms)
    .bind(resources)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn history(
    pool: &PgPool,
    instance_id: &str,
    from_ms: i64,
    to_ms: i64,
    limit: i64,
) -> Result<Vec<Value>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT resources FROM machine_resource_samples
         WHERE instance_id = $1 AND observed_at_ms >= $2 AND observed_at_ms <= $3
         ORDER BY observed_at_ms DESC LIMIT $4",
    )
    .bind(instance_id)
    .bind(from_ms)
    .bind(to_ms)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut samples: Vec<Value> = rows
        .into_iter()
        .filter_map(|row| row.try_get("resources").ok())
        .collect();
    samples.reverse();
    Ok(samples)
}

pub(crate) async fn purge_expired(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let cutoff_ms = chrono::Utc::now()
        .timestamp_millis()
        .saturating_sub(HISTORY_RETENTION_DAYS * 24 * 60 * 60 * 1_000);
    sqlx::query(
        "WITH expired AS (
             SELECT ctid FROM machine_resource_samples
             WHERE observed_at_ms < $1 ORDER BY observed_at_ms LIMIT $2
         ) DELETE FROM machine_resource_samples AS samples
         USING expired WHERE samples.ctid = expired.ctid",
    )
    .bind(cutoff_ms)
    .bind(HISTORY_CLEANUP_BATCH)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
}

pub(crate) fn history_response(instance_id: &str, samples: Vec<Value>) -> Value {
    json!({"instance_id": instance_id, "samples": samples})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recorder_queue_is_bounded_without_waiting_for_the_database() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost:1/unused")
            .unwrap();
        let sender = spawn_recorder(pool, "offline-db".into(), Arc::new(AtomicBool::new(false)));
        // This current-thread test has not yielded, so the recorder cannot consume
        // the queue. Saturation must return immediately, without a database await.
        for _ in 0..PENDING_SAMPLES {
            sender.try_send(json!({})).unwrap();
        }
        assert!(matches!(
            sender.try_send(json!({})),
            Err(mpsc::error::TrySendError::Full(_))
        ));
    }

    #[tokio::test]
    async fn postgres_machine_history_is_durable_ordered_deduplicated_and_retained() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "machine_history",
            "machine resource history",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let now = chrono::Utc::now().timestamp_millis();
        let sample = |at| {
            json!({"observed_at_ms": at, "expires_at_ms": at + 30_000,
            "cpu": {"usage_percent": null}, "network": null})
        };
        for at in [now, now - 10_000, now - 20_000, now] {
            record(&pool, "runner-a", &sample(at)).await.unwrap();
        }
        record(&pool, "runner-b", &sample(now)).await.unwrap();
        record(&pool, "runner-a", &json!(null)).await.unwrap();
        let rows = history(&pool, "runner-a", now - 30_000, now, 2)
            .await
            .unwrap();
        assert_eq!(rows, vec![sample(now - 10_000), sample(now)]);
        assert_eq!(
            history(&pool, "runner-a", now - 30_000, now, 10)
                .await
                .unwrap()
                .len(),
            3
        );
        let old = now - (HISTORY_RETENTION_DAYS + 1) * 86_400_000;
        record(&pool, "runner-a", &sample(old)).await.unwrap();
        assert_eq!(purge_expired(&pool).await.unwrap(), 1);
        assert_eq!(
            history(&pool, "runner-b", now - 1, now, 10).await.unwrap(),
            vec![sample(now)]
        );
        assert!(
            history(&pool, "runner-a", old, old, 10)
                .await
                .unwrap()
                .is_empty()
        );
        pool.close().await;
        db.drop().await;
    }
}
