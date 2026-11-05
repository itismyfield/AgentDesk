//! Caps the postmortem record tables (archives, audit and termination logs, trend and
//! event rows) so they keep a bounded window instead of growing forever.

use super::*;

const CREATED_AT: &str = "created_at < NOW() - ($1::INT || ' days')::INTERVAL";
// `stat_date` is a zero-padded `YYYY-MM-DD` string, so text order is date order.
const STAT_DATE: &str = "stat_date < to_char(CURRENT_DATE - $1::INT, 'YYYY-MM-DD')";

// A redrive key stays while its outbox row exists, so the same key keeps replaying.
const REDRIVE_AUDIT: &str = "requested_at < NOW() - ($1::INT || ' days')::INTERVAL \
     AND NOT EXISTS (SELECT 1 FROM message_outbox o \
                     WHERE o.id = message_outbox_redrive_audit.message_outbox_id)";

// Kept: every transition of an entry in an open (or unknown) run, and for any other live
// entry its latest transition and latest `skipped` one, which the phase gate reads.
const ENTRY_TRANSITION: &str = "created_at < NOW() - ($1::INT || ' days')::INTERVAL \
     AND NOT EXISTS ( \
         SELECT 1 FROM auto_queue_entries e \
         LEFT JOIN auto_queue_runs r ON r.id = e.run_id \
         WHERE e.id = auto_queue_entry_transitions.entry_id \
           AND (r.id IS NULL OR r.status NOT IN ('completed', 'cancelled') \
                OR NOT EXISTS (SELECT 1 FROM auto_queue_entry_transitions n \
                    WHERE n.entry_id = e.id \
                      AND (n.created_at, n.id) > (auto_queue_entry_transitions.created_at, \
                                                  auto_queue_entry_transitions.id)) \
                OR (auto_queue_entry_transitions.to_status = 'skipped' \
                    AND NOT EXISTS (SELECT 1 FROM auto_queue_entry_transitions n \
                        WHERE n.entry_id = e.id AND n.to_status = 'skipped' \
                          AND (n.created_at, n.id) > (auto_queue_entry_transitions.created_at, \
                                                      auto_queue_entry_transitions.id)))))";

// Only superseded `failed` attempts go; a key's one non-failed row (0046 unique) is its
// dedupe record. Kept too: in-flight dispatches, each dispatch's latest row.
const DELIVERY_EVENT: &str = "status = 'failed' \
     AND created_at < NOW() - ($1::INT || ' days')::INTERVAL \
     AND NOT EXISTS (SELECT 1 FROM task_dispatches td \
         WHERE td.id = dispatch_delivery_events.dispatch_id \
           AND td.status IN ('pending', 'dispatched')) \
     AND EXISTS (SELECT 1 FROM dispatch_delivery_events n \
         WHERE n.dispatch_id = dispatch_delivery_events.dispatch_id \
           AND n.correlation_id = dispatch_delivery_events.correlation_id \
           AND n.semantic_event_id = dispatch_delivery_events.semantic_event_id \
           AND n.operation = dispatch_delivery_events.operation \
           AND n.target_kind = dispatch_delivery_events.target_kind \
           AND COALESCE(n.target_channel_id, '') \
               = COALESCE(dispatch_delivery_events.target_channel_id, '') \
           AND COALESCE(n.target_thread_id, '') \
               = COALESCE(dispatch_delivery_events.target_thread_id, '') \
           AND (n.attempt, n.updated_at, n.id) > (dispatch_delivery_events.attempt, \
                dispatch_delivery_events.updated_at, dispatch_delivery_events.id)) \
     AND EXISTS (SELECT 1 FROM dispatch_delivery_events n \
         WHERE n.dispatch_id = dispatch_delivery_events.dispatch_id \
           AND n.operation = dispatch_delivery_events.operation \
           AND n.target_kind = dispatch_delivery_events.target_kind \
           AND (n.updated_at, n.id) > (dispatch_delivery_events.updated_at, \
                                       dispatch_delivery_events.id))";

// `(table, aged-row predicate with the window days as $1, days)`; NULL times are kept.
// Runs after the archive copies, so a row archived this pass gets its archive window.
const CAPS: [(&str, &str, i32); 13] = [
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
    (
        "message_outbox_redrive_audit",
        REDRIVE_AUDIT,
        REDRIVE_AUDIT_RETENTION_DAYS,
    ),
    (
        "auto_queue_entry_transitions",
        ENTRY_TRANSITION,
        ENTRY_TRANSITION_RETENTION_DAYS,
    ),
    (
        "dispatch_delivery_events",
        DELIVERY_EVENT,
        DELIVERY_EVENT_RETENTION_DAYS,
    ),
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
                     (id, fingerprint, endpoint, friction_type, summary, payload_json, created_at) \
                     VALUES ($1, $1, '/x', 'test', 'test', '{{}}'::jsonb, {AGE})"
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

    async fn exec(pool: &PgPool, sql: &str) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(pool)
            .await
            .unwrap_or_else(|err| panic!("fixture `{sql}`: {err}"));
    }

    async fn keys(pool: &PgPool, sql: &'static str) -> Vec<String> {
        sqlx::query_scalar(sql)
            .fetch_all(pool)
            .await
            .unwrap_or_else(|err| panic!("read `{sql}`: {err}"))
    }

    /// Ledger tables lose only aged rows no live reader can still pick, and the redrive
    /// and phase-gate readers answer as before.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_keeps_ledger_rows_live_readers_still_use() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_record_caps_excluded",
            "db_retention excluded ledger rows",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let day = |n: i32| format!("NOW() - INTERVAL '{n} days'");

        // Redrive: a real redrive writes the key, then the outbox row fails again.
        exec(
            &pool,
            "INSERT INTO message_outbox (id, target, content, bot, source, status, reason_code, \
             session_key, retry_count, error, dedupe_key, dedupe_expires_at) \
             VALUES (20, 'channel:20', 'notice', 'notify', 'catch_up_too_old', 'failed', \
                     'catch_up_too_old', 'session:20', 5, 'terminal failure', 'once-key', \
                     NOW() - INTERVAL '1 minute')",
        )
        .await;
        let first = crate::services::message_outbox_recovery::redrive_failed_rows(
            &pool,
            &[20],
            "stable-key",
            "incident",
            false,
        )
        .await
        .expect("first redrive");
        assert_eq!(first[0].outcome, "redriven");
        exec(
            &pool,
            &format!(
                "UPDATE message_outbox SET status = 'failed', retry_count = 5 WHERE id = 20; \
                 UPDATE message_outbox_redrive_audit SET requested_at = {}; \
                 INSERT INTO message_outbox_redrive_audit \
                     (message_outbox_id, idempotency_key, reason, requested_at) \
                 VALUES (999, 'gone-old', 'x', {}), (998, 'gone-new', 'x', {})",
                day(100),
                day(91),
                day(89)
            ),
        )
        .await;

        // Entry transitions: `from_status` carries the row label.
        exec(
            &pool,
            "INSERT INTO agents (id, name, provider, discord_channel_id) \
                 VALUES ('agent-rc', 'Agent', 'claude', '999'); \
             INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
                 VALUES ('run-open', 'repo', 'agent-rc', 'active'), \
                        ('run-done', 'repo', 'agent-rc', 'completed'); \
             INSERT INTO kanban_cards (id, title, status, assigned_agent_id) \
                 VALUES ('card-open', 'open', 'in_progress', 'agent-rc'), \
                        ('card-done', 'done', 'done', 'agent-rc'); \
             INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, \
                 priority_rank, thread_group, batch_phase) \
                 VALUES ('e-skip', 'run-open', 'card-open', 'agent-rc', 'skipped', 0, 0, 0), \
                        ('e-next', 'run-open', NULL, 'agent-rc', 'pending', 0, 0, 1), \
                        ('e-done', 'run-done', 'card-done', 'agent-rc', 'done', 0, 0, 0)",
        )
        .await;
        let mut transitions = Vec::new();
        for (label, entry, to, source, age) in [
            ("open-1", "e-skip", "dispatched", "auto", 102),
            ("open-2", "e-skip", "skipped", "manual_skip", 101),
            ("done-0", "e-done", "dispatched", "auto", 104),
            ("done-1", "e-done", "skipped", "manual_skip", 103),
            ("done-2", "e-done", "pending", "auto", 102),
            ("done-3", "e-done", "done", "auto", 101),
            ("gone-old", "e-gone", "done", "auto", 91),
            ("gone-new", "e-gone", "done", "auto", 89),
        ] {
            transitions.push(format!(
                "('{entry}', '{label}', '{to}', '{source}', {})",
                day(age)
            ));
        }
        exec(
            &pool,
            &format!(
                "INSERT INTO auto_queue_entry_transitions \
                 (entry_id, from_status, to_status, trigger_source, created_at) VALUES {}",
                transitions.join(", ")
            ),
        )
        .await;

        // Delivery events: `error` carries the row label; `(created, updated)` ages.
        exec(
            &pool,
            "INSERT INTO task_dispatches (id, status, dispatch_type) \
             VALUES ('d-live', 'pending', 'implementation'), \
                    ('d-fail', 'failed', 'implementation'), \
                    ('d-fresh', 'failed', 'implementation')",
        )
        .await;
        let mut events = Vec::new();
        for (label, dispatch, event, attempt, status, created, updated) in [
            ("sent-1", "d-fail", "n", 1, "sent", 104, 104),
            ("failed-2", "d-fail", "n", 2, "failed", 103, 103),
            ("failed-3", "d-fail", "n", 3, "failed", 102, 102),
            ("dispatch-latest", "d-fail", "m", 1, "failed", 105, 100),
            ("key-latest", "d-fail", "m", 2, "failed", 104, 104),
            ("reserved", "d-fail", "r", 1, "reserved", 106, 106),
            ("live-1", "d-live", "n", 1, "failed", 103, 103),
            ("live-2", "d-live", "n", 2, "failed", 102, 102),
            ("fresh-1", "d-fresh", "n", 1, "failed", 89, 89),
            ("fresh-2", "d-fresh", "n", 2, "failed", 88, 88),
        ] {
            events.push(format!(
                "('{dispatch}', 'dispatch:{dispatch}', '{event}', '{status}', {attempt}, \
                 '{label}', {}, {}, NOW() - INTERVAL '1 day')",
                day(created),
                day(updated)
            ));
        }
        exec(
            &pool,
            &format!(
                "INSERT INTO dispatch_delivery_events (dispatch_id, correlation_id, \
                 semantic_event_id, status, attempt, error, created_at, updated_at, \
                 reserved_until) VALUES {}",
                events.join(", ")
            ),
        )
        .await;

        const REDRIVE: &str = "SELECT idempotency_key FROM message_outbox_redrive_audit ORDER BY 1";
        const TRANSITIONS: &str = "SELECT from_status FROM auto_queue_entry_transitions ORDER BY 1";
        const EVENTS: &str = "SELECT error FROM dispatch_delivery_events ORDER BY 1";
        let snapshot = |pool: PgPool| async move {
            (
                keys(&pool, REDRIVE).await,
                keys(&pool, TRANSITIONS).await,
                keys(&pool, EVENTS).await,
            )
        };
        let seeded = snapshot(pool.clone()).await;

        let dry = db_retention_job(&pool, true).await.expect("dry-run pass");
        let report = |r: &RetentionReport, action: &str| {
            [
                "message_outbox_redrive_audit",
                "auto_queue_entry_transitions",
                "dispatch_delivery_events",
            ]
            .map(|table| r.get(table, action).map(|t| t.rows_affected))
        };
        assert_eq!(report(&dry, "delete_would"), [Some(1), Some(3), Some(1)]);
        assert_eq!(
            snapshot(pool.clone()).await,
            seeded,
            "dry-run deletes nothing"
        );

        let live = db_retention_job(&pool, false).await.expect("live pass");
        assert_eq!(report(&live, "delete"), [Some(1), Some(3), Some(1)]);
        let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            snapshot(pool.clone()).await,
            (
                strings(&["gone-new", "stable-key"]),
                strings(&["done-1", "done-3", "gone-new", "open-1", "open-2"]),
                strings(&[
                    "dispatch-latest",
                    "failed-3",
                    "fresh-1",
                    "fresh-2",
                    "key-latest",
                    "live-1",
                    "live-2",
                    "reserved",
                    "sent-1",
                ]),
            ),
            "only aged rows no reader can pick are deleted"
        );

        // The readers answer as before the pass.
        let replay = crate::services::message_outbox_recovery::redrive_failed_rows(
            &pool,
            &[20],
            "stable-key",
            "incident",
            false,
        )
        .await
        .expect("same-key replay");
        assert_eq!(replay[0].outcome, "idempotent_replay");
        let outbox: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id = 20")
            .fetch_one(&pool)
            .await
            .expect("outbox status");
        assert_eq!(outbox, "failed", "the kept key blocks a second redrive");
        assert_eq!(
            crate::db::auto_queue::current_batch_phase_pg(&pool, "run-open")
                .await
                .expect("phase gate"),
            Some(1),
            "the manual_skip entry still does not hold phase 0 open"
        );

        pool.close().await;
        db.drop().await;
    }
}
