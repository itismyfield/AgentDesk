//! Caps the postmortem record tables (archives, audit and termination logs, trend and
//! event rows) so they keep a bounded window instead of growing forever.

use super::*;

const CREATED_AT: &str = "created_at < NOW() - ($1::INT || ' days')::INTERVAL";
// `stat_date` is a zero-padded `YYYY-MM-DD` string, so text order is date order.
const STAT_DATE: &str = "stat_date < to_char(CURRENT_DATE - $1::INT, 'YYYY-MM-DD')";

// `(table, aged-row predicate binding the window as $1, window days)`. Rows whose time
// column is NULL are kept. Runs after the archive copies, so a row archived in this pass
// is judged by its archive window.
const CAPS: [(&str, &str, i32); 10] = [
    (
        "session_transcripts_archive",
        CREATED_AT,
        TRANSCRIPT_ARCHIVE_RETENTION_DAYS,
    ),
    (
        "turns_archive",
        "finished_at < NOW() - ($1::INT || ' days')::INTERVAL",
        TURNS_ARCHIVE_RETENTION_DAYS,
    ),
    (
        "runtime_decisions",
        CREATED_AT,
        RUNTIME_DECISIONS_RETENTION_DAYS,
    ),
    (
        "audit_logs",
        "timestamp < NOW() - ($1::INT || ' days')::INTERVAL",
        AUDIT_RECORD_RETENTION_DAYS,
    ),
    (
        "recovery_audit_records",
        CREATED_AT,
        AUDIT_RECORD_RETENTION_DAYS,
    ),
    (
        "session_termination_events",
        CREATED_AT,
        AUDIT_RECORD_RETENTION_DAYS,
    ),
    ("slo_aggregates", CREATED_AT, SLO_AGGREGATES_RETENTION_DAYS),
    (
        "api_friction_events",
        CREATED_AT,
        API_FRICTION_EVENT_RETENTION_DAYS,
    ),
    (
        "memento_feedback_turn_stats",
        STAT_DATE,
        MEMENTO_FEEDBACK_STATS_RETENTION_DAYS,
    ),
    ("dispatch_events", CREATED_AT, DISPATCH_EVENT_RETENTION_DAYS),
];

pub(super) async fn retain(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    for (table, aged, days) in CAPS {
        delete_aged(pool, dry_run, report, table, aged, days).await?;
    }
    Ok(())
}

// `table` and `aged` come only from `CAPS`, never from input.
async fn delete_aged(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
    table: &'static str,
    aged: &'static str,
    days: i32,
) -> Result<()> {
    if dry_run {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*)::BIGINT FROM {table} WHERE {aged}"
        )))
        .bind(days)
        .fetch_one(pool)
        .await?;
        report.push(TableReport {
            table_name: table,
            action: "delete_would",
            rows_affected: n,
        });
        return Ok(());
    }

    let del = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {table} WHERE {aged}"
    )))
    .bind(days)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: table,
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ids(pool: &PgPool, sql: String) -> Vec<String> {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.clone()))
            .fetch_all(pool)
            .await
            .unwrap_or_else(|err| panic!("read survivors `{sql}`: {err}"))
    }

    // `(table, key column, insert binding key as $1 and age in days as $2, window days)`.
    // The windows are the agreed policy, written out rather than read from the constants.
    const AGE: &str = "NOW() - ($2::INT || ' days')::INTERVAL";
    fn capped_tables() -> Vec<(&'static str, &'static str, String, i32)> {
        vec![
            (
                "runtime_decisions",
                "signal",
                format!(
                    "INSERT INTO runtime_decisions \
                     (signal, evidence_json, chosen_action, actor, created_at) \
                     VALUES ($1, '{{}}'::jsonb, 'Escalate', 'supervisor', {AGE})"
                ),
                90,
            ),
            (
                "slo_aggregates",
                "metric",
                format!(
                    "INSERT INTO slo_aggregates \
                     (window_start_ms, window_end_ms, metric, value, created_at) \
                     VALUES (0, 1, $1, 1.0, {AGE})"
                ),
                90,
            ),
            (
                "audit_logs",
                "entity_id",
                format!(
                    "INSERT INTO audit_logs (entity_type, entity_id, action, timestamp) \
                     VALUES ('kanban_card', $1, 'update', {AGE})"
                ),
                180,
            ),
            (
                "recovery_audit_records",
                "channel_id",
                format!(
                    "INSERT INTO recovery_audit_records (channel_id, source, message_count, \
                     max_chars_per_message, redacted_preview, content_sha256, created_at) \
                     VALUES ($1, 'test', 1, 1, '', '', {AGE})"
                ),
                180,
            ),
            (
                "session_termination_events",
                "session_key",
                format!(
                    "INSERT INTO session_termination_events \
                     (session_key, killer_component, reason_code, created_at) \
                     VALUES ($1, 'test', 'test', {AGE})"
                ),
                180,
            ),
            (
                "api_friction_events",
                "fingerprint",
                format!(
                    "INSERT INTO api_friction_events \
                     (fingerprint, endpoint, friction_type, summary, payload_json, created_at) \
                     VALUES ($1, '/x', 'test', 'test', '{{}}'::jsonb, {AGE})"
                ),
                90,
            ),
            (
                "memento_feedback_turn_stats",
                "turn_id",
                "INSERT INTO memento_feedback_turn_stats (turn_id, stat_date, agent_id, provider) \
                 VALUES ($1, to_char(CURRENT_DATE - $2::INT, 'YYYY-MM-DD'), 'agent', 'codex')"
                    .to_string(),
                90,
            ),
            (
                "turns_archive",
                "turn_id",
                format!(
                    "INSERT INTO turns_archive (turn_id, channel_id, started_at, finished_at) \
                     VALUES ($1, 'chan', {AGE}, {AGE})"
                ),
                365,
            ),
            (
                "dispatch_events",
                "transition_source",
                format!(
                    "INSERT INTO dispatch_events \
                     (dispatch_id, to_status, transition_source, created_at) \
                     VALUES ('live-dispatch', 'dispatched', $1, {AGE})"
                ),
                90,
            ),
        ]
    }

    /// Each capped table keeps the row a day inside its window and loses the row a day
    /// past it; a transcript archived in the same pass stays in the archive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_caps_record_tables_to_their_windows() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_record_caps",
            "db_retention record table caps",
        )
        .await;
        let pool = db.connect_and_migrate().await;

        // A dispatch that is still in flight: its events are capped regardless of status.
        sqlx::query(
            "INSERT INTO task_dispatches (id, status, dispatch_type) \
             VALUES ('live-dispatch', 'pending', 'implementation')",
        )
        .execute(&pool)
        .await
        .expect("seed live dispatch");
        let tables = capped_tables();
        for (table, _, insert, days) in &tables {
            for (key, age) in [("old", days + 1), ("new", days - 1)] {
                sqlx::query(sqlx::AssertSqlSafe(insert.clone()))
                    .bind(key)
                    .bind(age)
                    .execute(&pool)
                    .await
                    .unwrap_or_else(|err| panic!("seed {table} {key}: {err}"));
            }
        }
        // 179 days is past the hot window but inside the archive window. The ids stay
        // clear of the `session_transcripts` sequence the hot row draws from.
        for (id, key, age) in [(1_000_001_i64, "old", 181), (1_000_002, "new", 179)] {
            sqlx::query(
                "INSERT INTO session_transcripts_archive (id, turn_id, created_at) \
                 VALUES ($3, $1, NOW() - ($2::INT || ' days')::INTERVAL)",
            )
            .bind(key)
            .bind(age)
            .bind(id)
            .execute(&pool)
            .await
            .expect("seed archive row");
        }
        sqlx::query(
            "INSERT INTO session_transcripts (turn_id, created_at) \
             VALUES ('hot-100', NOW() - INTERVAL '100 days')",
        )
        .execute(&pool)
        .await
        .expect("seed hot transcript");

        let mut keyed: Vec<(&'static str, &'static str)> =
            tables.iter().map(|t| (t.0, t.1)).collect();
        keyed.push(("session_transcripts_archive", "turn_id"));
        let survivors = |pool: PgPool, keyed: Vec<(&'static str, &'static str)>| async move {
            let mut all = Vec::new();
            for (table, key) in keyed {
                let rows = ids(&pool, format!("SELECT {key} FROM {table} ORDER BY 1")).await;
                all.push((table, rows));
            }
            all
        };
        let seeded = survivors(pool.clone(), keyed.clone()).await;

        let dry = db_retention_job(&pool, true).await.expect("dry-run pass");
        for (table, _) in &keyed {
            assert_eq!(
                dry.get(table, "delete_would").map(|t| t.rows_affected),
                Some(1),
                "{table}: dry-run counts the one aged row"
            );
        }
        assert_eq!(
            survivors(pool.clone(), keyed.clone()).await,
            seeded,
            "dry-run deletes nothing"
        );

        let report = db_retention_job(&pool, false).await.expect("live pass");
        for (table, _) in &keyed {
            assert_eq!(
                report.get(table, "delete").map(|t| t.rows_affected),
                Some(1),
                "{table}: live pass deletes the one aged row"
            );
        }
        for (table, rows) in survivors(pool.clone(), keyed.clone()).await {
            let expected = if table == "session_transcripts_archive" {
                vec!["hot-100", "new"]
            } else {
                vec!["new"]
            };
            assert_eq!(
                rows, expected,
                "{table}: only the row past the window is deleted"
            );
        }

        pool.close().await;
        db.drop().await;
    }
}
