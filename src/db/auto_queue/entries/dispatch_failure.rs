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
    let mut tx = pool.begin().await.map_err(|error| {
        format!("begin postgres auto-queue dispatch failure transaction: {error}")
    })?;
    let result =
        EntryDispatchFailureResult::record_on_pg_tx(&mut tx, entry_id, max_retries, trigger_source)
            .await?;

    if result.changed && result.to_status == ENTRY_STATUS_FAILED {
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
                    attachment: None,
                },
                alert.dedupe_ttl_secs,
            )
            .await
            .map_err(|error| {
                format!("enqueue terminal dispatch failure alert for {entry_id}: {error}")
            })?;
        }
    }

    tx.commit().await.map_err(|error| {
        format!("commit postgres auto-queue dispatch failure {entry_id}: {error}")
    })?;
    Ok(result)
}

impl EntryDispatchFailureResult {
    /// Apply one dispatch failure inside a caller-owned transaction.
    ///
    /// Locking the entry and its run makes the retry decision, transition audit,
    /// and conditional run finalization one ordered write. When the run is not
    /// live, the entry becomes terminal instead of blocking the owning dispatch
    /// terminal write or creating a terminal-run / pending-entry split brain.
    pub async fn record_on_pg_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        entry_id: &str,
        max_retries: i64,
        trigger_source: &str,
    ) -> Result<Self, String> {
        let retry_limit = max_retries.max(1);
        let current = load_entry_status_row_pg_tx_for_dispatch_failure(tx, entry_id).await?;
        if current.status != ENTRY_STATUS_DISPATCHED {
            return Ok(Self {
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
        let run_status = sqlx::query_scalar::<_, String>(
            "SELECT status
             FROM auto_queue_runs
             WHERE id = $1
             FOR UPDATE",
        )
        .bind(&current.run_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|error| {
            format!(
                "lock auto-queue run {} for dispatch failure {entry_id}: {error}",
                current.run_id
            )
        })?
        .ok_or_else(|| format!("auto-queue run not found: {}", current.run_id))?;
        // Dispatch termination must never depend on whether the owning run can
        // accept another attempt. A live run receives the retry while a terminal
        // or otherwise non-live run converts the entry to terminal `failed`.
        // This also closes the real cancel window where the run is already
        // `cancelled` but its dispatched entry has not reached `skipped` yet.
        let target_status = if retry_count >= retry_limit
            || !crate::db::auto_queue::run_status::is_live_run_status(&run_status)
        {
            ENTRY_STATUS_FAILED
        } else {
            ENTRY_STATUS_PENDING
        };

        let rows_affected = sqlx::query(
            "UPDATE auto_queue_entries
             SET status = $1,
                 dispatch_id = NULL,
                 slot_index = NULL,
                 dispatched_at = NULL,
                 completed_at = CASE
                     WHEN $1 = 'failed' THEN NOW()
                     ELSE NULL
                 END,
                 retry_count = $2
             WHERE id = $3
               AND status = 'dispatched'
               AND retry_count = $4",
        )
        .bind(target_status)
        .bind(retry_count)
        .bind(entry_id)
        .bind(current.retry_count)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            format!("update postgres auto-queue dispatch failure {entry_id}: {error}")
        })?
        .rows_affected();
        if rows_affected == 0 {
            return Ok(Self {
                run_id: current.run_id,
                from_status: current.status.clone(),
                to_status: current.status,
                retry_count: current.retry_count,
                retry_limit,
                failure_transition_id: None,
                changed: false,
            });
        }

        let failure_transition_id = record_entry_transition_on_pg(
            tx,
            entry_id,
            ENTRY_STATUS_DISPATCHED,
            target_status,
            trigger_source,
        )
        .await?;
        let result = Self {
            run_id: current.run_id.clone(),
            from_status: ENTRY_STATUS_DISPATCHED.to_string(),
            to_status: target_status.to_string(),
            retry_count,
            retry_limit,
            failure_transition_id: Some(failure_transition_id),
            changed: true,
        };

        if target_status == ENTRY_STATUS_FAILED {
            maybe_finalize_run_after_terminal_entry_pg(tx, &current.run_id, target_status).await?;
        }
        Ok(result)
    }
}

async fn load_entry_status_row_pg_tx_for_dispatch_failure(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry_id: &str,
) -> Result<EntryStatusRow, String> {
    let row = sqlx::query(
        "SELECT run_id,
                COALESCE(kanban_card_id, '') AS kanban_card_id,
                COALESCE(agent_id, '') AS agent_id,
                status,
                dispatch_id,
                COALESCE(retry_count, 0)::BIGINT AS retry_count,
                slot_index::BIGINT AS slot_index,
                COALESCE(thread_group, 0)::BIGINT AS thread_group,
                COALESCE(batch_phase, 0)::BIGINT AS batch_phase,
                completed_at::text AS completed_at
         FROM auto_queue_entries
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(entry_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("lock postgres auto-queue entry {entry_id}: {error}"))?
    .ok_or_else(|| format!("auto-queue entry not found: {entry_id}"))?;

    Ok(EntryStatusRow {
        run_id: row
            .try_get("run_id")
            .map_err(|error| format!("decode auto-queue entry {entry_id} run_id: {error}"))?,
        card_id: row.try_get("kanban_card_id").map_err(|error| {
            format!("decode auto-queue entry {entry_id} kanban_card_id: {error}")
        })?,
        agent_id: row
            .try_get("agent_id")
            .map_err(|error| format!("decode auto-queue entry {entry_id} agent_id: {error}"))?,
        status: row
            .try_get("status")
            .map_err(|error| format!("decode auto-queue entry {entry_id} status: {error}"))?,
        dispatch_id: row
            .try_get("dispatch_id")
            .map_err(|error| format!("decode auto-queue entry {entry_id} dispatch_id: {error}"))?,
        retry_count: row
            .try_get("retry_count")
            .map_err(|error| format!("decode auto-queue entry {entry_id} retry_count: {error}"))?,
        slot_index: row
            .try_get("slot_index")
            .map_err(|error| format!("decode auto-queue entry {entry_id} slot_index: {error}"))?,
        thread_group: row
            .try_get("thread_group")
            .map_err(|error| format!("decode auto-queue entry {entry_id} thread_group: {error}"))?,
        batch_phase: row
            .try_get("batch_phase")
            .map_err(|error| format!("decode auto-queue entry {entry_id} batch_phase: {error}"))?,
        completed_at: row
            .try_get("completed_at")
            .map_err(|error| format!("decode auto-queue entry {entry_id} completed_at: {error}"))?,
    })
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

    async fn seed_failure_entry(
        pool: &PgPool,
        run_id: &str,
        entry_id: &str,
        run_status: &str,
        retry_count: i64,
    ) {
        sqlx::query("INSERT INTO auto_queue_runs (id, status) VALUES ($1, $2)")
            .bind(run_id)
            .bind(run_status)
            .execute(pool)
            .await
            .expect("seed run");
        sqlx::query(
            "INSERT INTO auto_queue_entries (id, run_id, agent_id, status, retry_count)
             VALUES ($1, $2, 'agent-1', 'dispatched', $3)",
        )
        .bind(entry_id)
        .bind(run_id)
        .bind(retry_count)
        .execute(pool)
        .await
        .expect("seed entry");
    }

    #[tokio::test]
    async fn retry_failure_keeps_run_runnable_and_records_one_transition_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_failure_entry(&pool, "run-retry", "entry-retry", "active", 0).await;

        let result =
            record_entry_dispatch_failure_on_pg(&pool, "entry-retry", 3, "test_retry_failure")
                .await
                .expect("record retryable dispatch failure");
        assert!(result.changed);
        assert_eq!(result.to_status, ENTRY_STATUS_PENDING);
        assert_eq!(result.retry_count, 1);

        let state = sqlx::query_as::<_, (String, i64, String, bool)>(
            "SELECT e.status, e.retry_count, r.status, r.completed_at IS NULL
             FROM auto_queue_entries e
             JOIN auto_queue_runs r ON r.id = e.run_id
             WHERE e.id = 'entry-retry'",
        )
        .fetch_one(&pool)
        .await
        .expect("load retry state");
        assert_eq!(
            state,
            ("pending".to_string(), 1, "active".to_string(), true)
        );
        let transitions = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM auto_queue_entry_transitions
             WHERE entry_id = 'entry-retry'
               AND from_status = 'dispatched'
               AND to_status = 'pending'",
        )
        .fetch_one(&pool)
        .await
        .expect("count retry transition");
        assert_eq!(transitions, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn terminal_failure_finalizes_once_and_duplicate_is_noop_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_failure_entry(&pool, "run-terminal", "entry-terminal", "active", 0).await;

        let first = record_entry_dispatch_failure_on_pg(
            &pool,
            "entry-terminal",
            1,
            "test_terminal_failure",
        )
        .await
        .expect("record terminal failure");
        let completion = sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
            "SELECT completed_at FROM auto_queue_runs WHERE id = 'run-terminal'",
        )
        .fetch_one(&pool)
        .await
        .expect("load first completion timestamp");
        let duplicate = record_entry_dispatch_failure_on_pg(
            &pool,
            "entry-terminal",
            1,
            "test_terminal_failure_duplicate",
        )
        .await
        .expect("duplicate failure is idempotent");
        let state = sqlx::query_as::<_, (String, i64, String, chrono::DateTime<chrono::Utc>, i64)>(
            "SELECT e.status, e.retry_count, r.status, r.completed_at,
                    (SELECT COUNT(*) FROM auto_queue_entry_transitions
                     WHERE entry_id = e.id)
             FROM auto_queue_entries e
             JOIN auto_queue_runs r ON r.id = e.run_id
             WHERE e.id = 'entry-terminal'",
        )
        .fetch_one(&pool)
        .await
        .expect("load duplicate terminal state");
        assert!(first.changed);
        assert!(!duplicate.changed);
        assert_eq!(
            state,
            (
                "failed".to_string(),
                1,
                "completed".to_string(),
                completion,
                1,
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn restoring_run_accepts_retry_without_orphaning_entry_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_failure_entry(
            &pool,
            "run-restoring-retry",
            "entry-restoring-retry",
            "restoring",
            0,
        )
        .await;

        let result = record_entry_dispatch_failure_on_pg(
            &pool,
            "entry-restoring-retry",
            3,
            "test_restoring_retry",
        )
        .await
        .expect("restore-window failure remains reducible");
        assert!(result.changed);
        assert_eq!(result.to_status, ENTRY_STATUS_PENDING);
        let state = sqlx::query_as::<_, (String, i64, String, i64)>(
            "SELECT e.status, e.retry_count, r.status,
                    (SELECT COUNT(*) FROM auto_queue_entry_transitions
                     WHERE entry_id = e.id AND to_status = 'pending')
             FROM auto_queue_entries e
             JOIN auto_queue_runs r ON r.id = e.run_id
             WHERE e.id = 'entry-restoring-retry'",
        )
        .fetch_one(&pool)
        .await
        .expect("load restoring retry state");
        assert_eq!(
            state,
            ("pending".to_string(), 1, "restoring".to_string(), 1)
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn restoring_terminal_failure_retains_run_slot_ownership_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_failure_entry(
            &pool,
            "run-restoring-terminal",
            "entry-restoring-terminal",
            "restoring",
            0,
        )
        .await;
        sqlx::query(
            "INSERT INTO auto_queue_slots (
                 agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
             ) VALUES ('agent-1', 0, 'run-restoring-terminal', 0, '{}'::jsonb)",
        )
        .execute(&pool)
        .await
        .expect("seed restoring slot");

        let result = record_entry_dispatch_failure_on_pg(
            &pool,
            "entry-restoring-terminal",
            1,
            "test_restoring_terminal",
        )
        .await
        .expect("terminal failure during restore");
        assert_eq!(result.to_status, ENTRY_STATUS_FAILED);
        let state = sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT e.status, r.status, s.assigned_run_id
             FROM auto_queue_entries e
             JOIN auto_queue_runs r ON r.id = e.run_id
             JOIN auto_queue_slots s ON s.agent_id = e.agent_id AND s.slot_index = 0
             WHERE e.id = 'entry-restoring-terminal'",
        )
        .fetch_one(&pool)
        .await
        .expect("load restoring terminal state");
        assert_eq!(
            state,
            (
                "failed".to_string(),
                "restoring".to_string(),
                Some("run-restoring-terminal".to_string())
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn non_live_runs_terminalize_entry_instead_of_blocking_dispatch_failure_pg() {
        for run_status in ["cancelled", "completed", "generated", "unknown-status"] {
            let pg_db = TestPostgresDb::create().await;
            let pool = pg_db.connect_and_migrate().await;
            let run_id = format!("run-non-live-{run_status}");
            let entry_id = format!("entry-non-live-{run_status}");
            seed_failure_entry(&pool, &run_id, &entry_id, run_status, 0).await;
            if run_status == "completed" {
                sqlx::query("UPDATE auto_queue_runs SET completed_at = NOW() WHERE id = $1")
                    .bind(&run_id)
                    .execute(&pool)
                    .await
                    .expect("stamp completed run");
            }

            let result = record_entry_dispatch_failure_on_pg(
                &pool,
                &entry_id,
                3,
                "test_non_live_run_failure",
            )
            .await
            .expect("non-live run must not block entry terminalization");
            assert_eq!(result.to_status, ENTRY_STATUS_FAILED);
            let state = sqlx::query_as::<_, (String, i64, String, i64)>(
                "SELECT e.status, e.retry_count, r.status,
                        (SELECT COUNT(*) FROM auto_queue_entry_transitions
                         WHERE entry_id = e.id AND to_status = 'failed')
                 FROM auto_queue_entries e
                 JOIN auto_queue_runs r ON r.id = e.run_id
                 WHERE e.id = $1",
            )
            .bind(&entry_id)
            .fetch_one(&pool)
            .await
            .expect("load non-live terminal state");
            let expected_run_status = if run_status == "generated" {
                // An eligible non-live generated run can be finalized once its
                // last entry becomes terminal. Unknown or terminal states are
                // intentionally preserved by the run-status CAS.
                "completed"
            } else {
                run_status
            };
            assert_eq!(
                state,
                ("failed".to_string(), 1, expected_run_status.to_string(), 1),
                "run status {run_status} must not block entry terminalization"
            );

            pool.close().await;
            pg_db.drop().await;
        }
    }

    #[tokio::test]
    async fn concurrent_failure_has_one_cas_winner_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_failure_entry(&pool, "run-race", "entry-race", "active", 0).await;

        let left_pool = pool.clone();
        let right_pool = pool.clone();
        let (left, right) = tokio::join!(
            async move {
                record_entry_dispatch_failure_on_pg(&left_pool, "entry-race", 3, "race_left").await
            },
            async move {
                record_entry_dispatch_failure_on_pg(&right_pool, "entry-race", 3, "race_right")
                    .await
            },
        );
        let left = left.expect("left failure invocation");
        let right = right.expect("right failure invocation");
        assert_eq!(
            [left.changed, right.changed]
                .into_iter()
                .filter(|changed| *changed)
                .count(),
            1
        );
        let state = sqlx::query_as::<_, (String, i64, String, i64)>(
            "SELECT e.status, e.retry_count, r.status,
                    (SELECT COUNT(*) FROM auto_queue_entry_transitions
                     WHERE entry_id = e.id)
             FROM auto_queue_entries e
             JOIN auto_queue_runs r ON r.id = e.run_id
             WHERE e.id = 'entry-race'",
        )
        .fetch_one(&pool)
        .await
        .expect("load raced failure state");
        assert_eq!(state, ("pending".to_string(), 1, "active".to_string(), 1));

        pool.close().await;
        pg_db.drop().await;
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
