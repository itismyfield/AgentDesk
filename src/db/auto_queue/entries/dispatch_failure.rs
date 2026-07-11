use super::*;

use crate::services::message_outbox::{OutboxMessage, enqueue_outbox_pg_on_tx_with_ttl};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryDispatchFailureAlert {
    pub target: String,
    pub content: String,
    pub bot: String,
    pub source: String,
    pub reason_code: Option<String>,
    pub session_key: Option<String>,
    pub dedupe_ttl_secs: i64,
}

type AlertBuilder = Box<
    dyn FnOnce(&EntryDispatchFailureResult) -> Result<EntryDispatchFailureAlert, String>
        + Send
        + 'static,
>;

pub async fn record_entry_dispatch_failure_on_pg(
    pool: &PgPool,
    entry_id: &str,
    max_retries: i64,
    trigger_source: &str,
) -> Result<EntryDispatchFailureResult, String> {
    record_entry_dispatch_failure_impl(pool, entry_id, max_retries, trigger_source, None).await
}

pub async fn record_entry_dispatch_failure_with_alert_on_pg<F>(
    pool: &PgPool,
    entry_id: &str,
    max_retries: i64,
    trigger_source: &str,
    build_alert: F,
) -> Result<EntryDispatchFailureResult, String>
where
    F: FnOnce(&EntryDispatchFailureResult) -> Result<EntryDispatchFailureAlert, String>
        + Send
        + 'static,
{
    record_entry_dispatch_failure_impl(
        pool,
        entry_id,
        max_retries,
        trigger_source,
        Some(Box::new(build_alert)),
    )
    .await
}

async fn record_entry_dispatch_failure_impl(
    pool: &PgPool,
    entry_id: &str,
    max_retries: i64,
    trigger_source: &str,
    mut alert_builder: Option<AlertBuilder>,
) -> Result<EntryDispatchFailureResult, String> {
    let retry_limit = max_retries.max(1);
    loop {
        let current = load_entry_status_row_pg(pool, entry_id).await?;
        if current.status != ENTRY_STATUS_DISPATCHED {
            return Ok(EntryDispatchFailureResult {
                run_id: current.run_id,
                from_status: current.status.clone(),
                to_status: current.status,
                retry_count: current.retry_count,
                retry_limit,
                failure_transition_id: None,
                changed: false,
            });
        }

        let retry_count = current.retry_count.saturating_add(1);
        let target_status = if retry_count >= retry_limit {
            ENTRY_STATUS_FAILED
        } else {
            ENTRY_STATUS_PENDING
        };

        let mut tx = pool.begin().await.map_err(|error| {
            format!("begin postgres auto-queue dispatch failure transaction: {error}")
        })?;
        let rows_affected = sqlx::query(
            "UPDATE auto_queue_entries
             SET status = CASE
                     WHEN retry_count + 1 >= $1 THEN 'failed'
                     ELSE 'pending'
                 END,
                 dispatch_id = NULL,
                 slot_index = NULL,
                 dispatched_at = NULL,
                 completed_at = CASE
                     WHEN retry_count + 1 >= $1 THEN NOW()
                     ELSE NULL
                 END,
                 retry_count = retry_count + 1
             WHERE id = $2
               AND status = 'dispatched'
               AND retry_count = $3",
        )
        .bind(retry_limit)
        .bind(entry_id)
        .bind(current.retry_count)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            format!("update postgres auto-queue dispatch failure {entry_id}: {error}")
        })?
        .rows_affected();

        if rows_affected == 0 {
            tx.rollback().await.map_err(|error| {
                format!("rollback stale postgres auto-queue dispatch failure {entry_id}: {error}")
            })?;

            let latest = load_entry_status_row_pg(pool, entry_id).await?;
            if latest.status != ENTRY_STATUS_DISPATCHED {
                return Ok(EntryDispatchFailureResult {
                    run_id: latest.run_id,
                    from_status: latest.status.clone(),
                    to_status: latest.status,
                    retry_count: latest.retry_count,
                    retry_limit,
                    failure_transition_id: None,
                    changed: false,
                });
            }
            continue;
        }
        let failure_transition_id = record_entry_transition_on_pg(
            &mut tx,
            entry_id,
            ENTRY_STATUS_DISPATCHED,
            target_status,
            trigger_source,
        )
        .await?;
        let result = EntryDispatchFailureResult {
            run_id: current.run_id.clone(),
            from_status: ENTRY_STATUS_DISPATCHED.to_string(),
            to_status: target_status.to_string(),
            retry_count,
            retry_limit,
            failure_transition_id: Some(failure_transition_id),
            changed: true,
        };

        if target_status == ENTRY_STATUS_FAILED {
            if let Some(build_alert) = alert_builder.take() {
                let alert = build_alert(&result)?;
                enqueue_outbox_pg_on_tx_with_ttl(
                    &mut tx,
                    OutboxMessage {
                        target: &alert.target,
                        content: &alert.content,
                        bot: &alert.bot,
                        source: &alert.source,
                        reason_code: alert.reason_code.as_deref(),
                        session_key: alert.session_key.as_deref(),
                    },
                    alert.dedupe_ttl_secs,
                )
                .await
                .map_err(|error| {
                    format!("enqueue terminal dispatch failure alert for {entry_id}: {error}")
                })?;
            }
            maybe_finalize_run_after_terminal_entry_pg(&mut tx, &current.run_id, target_status)
                .await?;
        }

        tx.commit().await.map_err(|error| {
            format!("commit postgres auto-queue dispatch failure {entry_id}: {error}")
        })?;
        return Ok(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use sqlx::Row;

    fn alert(source: &str, transition_id: i64) -> EntryDispatchFailureAlert {
        EntryDispatchFailureAlert {
            target: "channel:123".to_string(),
            content: "terminal dispatch failure".to_string(),
            bot: "notify".to_string(),
            source: source.to_string(),
            reason_code: Some("auto_queue.entry_dispatch_failed".to_string()),
            session_key: Some(format!("entry-atomic:{transition_id}")),
            dedupe_ttl_secs: 30 * 60,
        }
    }

    #[tokio::test]
    async fn terminal_failure_rolls_back_when_alert_obligation_cannot_enqueue_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query("INSERT INTO auto_queue_runs (id, status) VALUES ('run-atomic', 'active')")
            .execute(&pool)
            .await
            .expect("seed run");
        sqlx::query(
            "INSERT INTO auto_queue_entries (id, run_id, agent_id, status, retry_count)
             VALUES ('entry-atomic', 'run-atomic', 'agent-1', 'dispatched', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let rejected = record_entry_dispatch_failure_with_alert_on_pg(
            &pool,
            "entry-atomic",
            1,
            "test_invalid_alert",
            |failure| {
                let transition_id = failure
                    .failure_transition_id
                    .ok_or_else(|| "missing failure transition".to_string())?;
                Ok(alert("not-a-registered-source", transition_id))
            },
        )
        .await;
        assert!(
            rejected
                .as_ref()
                .is_err_and(|error| error.contains("source `not-a-registered-source`")),
            "unexpected result: {rejected:?}"
        );

        let entry = sqlx::query(
            "SELECT status, retry_count, completed_at IS NULL AS completion_is_null
             FROM auto_queue_entries WHERE id = 'entry-atomic'",
        )
        .fetch_one(&pool)
        .await
        .expect("load rolled-back entry");
        assert_eq!(entry.get::<String, _>("status"), ENTRY_STATUS_DISPATCHED);
        assert_eq!(entry.get::<i64, _>("retry_count"), 0);
        assert!(entry.get::<bool, _>("completion_is_null"));
        let transitions = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = 'entry-atomic'",
        )
        .fetch_one(&pool)
        .await
        .expect("count rolled-back transitions");
        let outbox = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM message_outbox")
            .fetch_one(&pool)
            .await
            .expect("count rolled-back outbox");
        assert_eq!((transitions, outbox), (0, 0));

        let committed = record_entry_dispatch_failure_with_alert_on_pg(
            &pool,
            "entry-atomic",
            1,
            "test_valid_alert",
            |failure| {
                let transition_id = failure
                    .failure_transition_id
                    .ok_or_else(|| "missing failure transition".to_string())?;
                Ok(alert("auto-queue", transition_id))
            },
        )
        .await
        .expect("commit terminal failure and alert");
        assert_eq!(committed.to_status, ENTRY_STATUS_FAILED);
        let committed_counts = sqlx::query_as::<_, (i64, i64)>(
            "SELECT
                 (SELECT COUNT(*) FROM auto_queue_entry_transitions
                   WHERE entry_id = 'entry-atomic'),
                 (SELECT COUNT(*) FROM message_outbox)",
        )
        .fetch_one(&pool)
        .await
        .expect("count committed transition and outbox");
        assert_eq!(committed_counts, (1, 1));

        pool.close().await;
        pg_db.drop().await;
    }
}
