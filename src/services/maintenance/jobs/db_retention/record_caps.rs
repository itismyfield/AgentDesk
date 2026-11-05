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
           AND (r.id IS NULL OR r.status IS NULL OR r.status NOT IN ('completed', 'cancelled') \
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

// Keeps each card's latest row into each status (NULL times never count as newer), which the
// rework resume reads; the kept set is computed once so the pass stays one scan.
const KANBAN_AUDIT: &str = "created_at < NOW() - ($1::INT || ' days')::INTERVAL \
     AND id NOT IN (SELECT DISTINCT ON (card_id, to_status) id FROM kanban_audit_logs \
         WHERE card_id IS NOT NULL AND to_status IS NOT NULL \
         ORDER BY card_id, to_status, created_at DESC NULLS LAST, id DESC)";

// Keeps each card's true negatives at its top round and its latest false negative, the rows a
// reopen reclassifies and backfills.
const REVIEW_TUNING: &str = "created_at < NOW() - ($1::INT || ' days')::INTERVAL \
     AND id NOT IN ( \
         SELECT id FROM (SELECT id, review_round, \
                 MAX(review_round) OVER (PARTITION BY card_id) AS top_round \
             FROM review_tuning_outcomes \
             WHERE card_id IS NOT NULL AND outcome = 'true_negative') tn \
         WHERE review_round = top_round \
         UNION ALL \
         SELECT * FROM (SELECT DISTINCT ON (card_id) id FROM review_tuning_outcomes \
             WHERE card_id IS NOT NULL AND outcome = 'false_negative' \
             ORDER BY card_id, id DESC) fn)";

// Keeps an in-flight review's items, and a carded item unless its dispatch still names that
// card for the rework lookup to fall back on.
const REVIEW_DECISION: &str = "decided_at < NOW() - ($1::INT || ' days')::INTERVAL \
     AND NOT EXISTS (SELECT 1 FROM task_dispatches td \
         WHERE td.id = review_decisions.dispatch_id AND td.status IN ('pending', 'dispatched')) \
     AND (kanban_card_id IS NULL \
          OR EXISTS (SELECT 1 FROM task_dispatches td \
              WHERE td.id = review_decisions.dispatch_id \
                AND td.kanban_card_id = review_decisions.kanban_card_id))";

// Keeps each channel's newest events by `seq` (unique per channel) up to the count recovery
// reloads; the literal below must match that count.
const CHECKPOINT_EVENT: &str = "at < NOW() - ($1::INT || ' days')::INTERVAL \
     AND id NOT IN (SELECT id FROM (SELECT id, \
             ROW_NUMBER() OVER (PARTITION BY channel_id ORDER BY seq DESC) AS newest \
         FROM agent_recovery_checkpoint_events) ranked WHERE newest <= 8)";
const _: () = assert!(crate::services::agent_recovery::checkpoint::DEFAULT_READ_EVENT_LIMIT == 8);

// `(table, aged-row predicate with the window days as $1, days)`; NULL times are kept.
// Runs after the archive copies, so a row archived this pass gets its archive window.
const CAPS: [(&str, &str, i32); 17] = [
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
    (
        "kanban_audit_logs",
        KANBAN_AUDIT,
        AUDIT_RECORD_RETENTION_DAYS,
    ),
    (
        "review_tuning_outcomes",
        REVIEW_TUNING,
        REVIEW_TUNING_RETENTION_DAYS,
    ),
    (
        "review_decisions",
        REVIEW_DECISION,
        REVIEW_DECISION_RETENTION_DAYS,
    ),
    (
        "agent_recovery_checkpoint_events",
        CHECKPOINT_EVENT,
        CHECKPOINT_EVENT_RETENTION_DAYS,
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
        let outbox_id = crate::services::message_outbox::enqueue_outbox_pg_returning_id_with_ttl(
            &pool,
            crate::services::message_outbox::OutboxMessage {
                target: "channel:20",
                content: "notice",
                bot: "notify",
                source: "catch_up_too_old",
                reason_code: Some("catch_up_too_old"),
                session_key: Some("session:20"),
            },
            0,
        )
        .await
        .expect("enqueue outbox row")
        .expect("outbox row inserted");
        let fail = format!(
            "UPDATE message_outbox SET status = 'failed', retry_count = 5, \
             error = 'terminal failure' WHERE id = {outbox_id}"
        );
        exec(&pool, &fail).await;
        let redrive = || {
            crate::services::message_outbox_recovery::redrive_failed_rows(
                &pool,
                std::slice::from_ref(&outbox_id),
                "stable-key",
                "incident",
                false,
            )
        };
        let first = redrive().await.expect("first redrive");
        assert_eq!(first[0].outcome, "redriven");
        exec(
            &pool,
            &format!(
                "{fail}; \
                 UPDATE message_outbox_redrive_audit SET requested_at = {}; \
                 INSERT INTO message_outbox_redrive_audit \
                     (message_outbox_id, idempotency_key, reason, requested_at) \
                 VALUES (999999, 'gone-old', 'x', {}), (999998, 'gone-new', 'x', {})",
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
        let replay = redrive().await.expect("same-key replay");
        assert_eq!(replay[0].outcome, "idempotent_replay");
        let outbox: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id = $1")
            .bind(outbox_id)
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

    /// A finished run's skipped entry keeps the transition the phase gate reads when two
    /// skips share a timestamp, and runs of unknown status keep every transition.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_keeps_the_phase_gate_skip_on_ties_and_null_runs() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_record_caps_ties",
            "db_retention transition ties and null runs",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        // Run `tie-a` writes the manual skip first (smaller id), `tie-b` writes it last.
        exec(
            &pool,
            "INSERT INTO agents (id, name, provider, discord_channel_id) \
                 VALUES ('agent-t', 'Agent', 'claude', '998'); \
             INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
                 VALUES ('tie-a', 'repo', 'agent-t', 'completed'), \
                        ('tie-b', 'repo', 'agent-t', 'completed'), \
                        ('run-null', 'repo', 'agent-t', NULL); \
             INSERT INTO kanban_cards (id, title, status, assigned_agent_id) \
                 VALUES ('card-a', 'a', 'in_progress', 'agent-t'), \
                        ('card-b', 'b', 'in_progress', 'agent-t'); \
             INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, \
                 priority_rank, thread_group, batch_phase) \
                 VALUES ('e-a', 'tie-a', 'card-a', 'agent-t', 'skipped', 0, 0, 0), \
                        ('e-a-next', 'tie-a', NULL, 'agent-t', 'pending', 0, 0, 1), \
                        ('e-b', 'tie-b', 'card-b', 'agent-t', 'skipped', 0, 0, 0), \
                        ('e-b-next', 'tie-b', NULL, 'agent-t', 'pending', 0, 0, 1), \
                        ('e-null', 'run-null', NULL, 'agent-t', 'done', 0, 0, 0), \
                        ('e-norun', NULL, NULL, 'agent-t', 'done', 0, 0, 0); \
             INSERT INTO auto_queue_entry_transitions \
                 (entry_id, from_status, to_status, trigger_source, created_at) VALUES \
                 ('e-a', 'a-old', 'dispatched', 'auto', NOW() - INTERVAL '110 days'), \
                 ('e-a', 'a-manual', 'skipped', 'manual_skip', NOW() - INTERVAL '100 days'), \
                 ('e-a', 'a-auto', 'skipped', 'auto', NOW() - INTERVAL '100 days'), \
                 ('e-a', 'a-null', 'pending', 'auto', NULL), \
                 ('e-b', 'b-old', 'dispatched', 'auto', NOW() - INTERVAL '110 days'), \
                 ('e-b', 'b-auto', 'skipped', 'auto', NOW() - INTERVAL '100 days'), \
                 ('e-b', 'b-manual', 'skipped', 'manual_skip', NOW() - INTERVAL '100 days'), \
                 ('e-null', 'null-1', 'dispatched', 'auto', NOW() - INTERVAL '110 days'), \
                 ('e-null', 'null-2', 'done', 'auto', NOW() - INTERVAL '100 days'), \
                 ('e-norun', 'norun-1', 'dispatched', 'auto', NOW() - INTERVAL '110 days'), \
                 ('e-norun', 'norun-2', 'done', 'auto', NOW() - INTERVAL '100 days')",
        )
        .await;
        // The two `100 days` values above are one statement's `NOW()`, so they tie exactly.
        let phases = |pool: PgPool| async move {
            let mut out = Vec::new();
            for run in ["tie-a", "tie-b"] {
                out.push(
                    crate::db::auto_queue::current_batch_phase_pg(&pool, run)
                        .await
                        .expect("phase gate"),
                );
            }
            out
        };
        // `tie-a`'s latest skip is the automatic one, so its entry holds phase 0.
        assert_eq!(phases(pool.clone()).await, vec![Some(0), Some(1)]);

        let report = db_retention_job(&pool, false).await.expect("live pass");
        assert_eq!(
            report
                .get("auto_queue_entry_transitions", "delete")
                .map(|t| t.rows_affected),
            Some(4),
            "the old dispatch rows and the losing tied skips go"
        );
        assert_eq!(
            keys(
                &pool,
                "SELECT from_status FROM auto_queue_entry_transitions ORDER BY 1"
            )
            .await,
            [
                "a-auto", "a-null", "b-manual", "norun-1", "norun-2", "null-1", "null-2"
            ]
            .map(String::from),
            "tied skips keep the larger id; unknown-status and run-less entries keep all"
        );
        assert_eq!(
            phases(pool.clone()).await,
            vec![Some(0), Some(1)],
            "the phase gate answers as before the pass"
        );

        pool.close().await;
        db.drop().await;
    }

    /// Audit, tuning and checkpoint rows go only when the resume, reopen and recovery readers
    /// no longer pick them, and those readers answer as before.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_keeps_rows_the_resume_reopen_and_recovery_readers_use() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_record_caps_r2c",
            "db_retention audit tuning checkpoint rows",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        // Labels ride in `result`, `verdict` and `writer_agent_id`. Rows of one statement
        // share `NOW()`, so equal ages tie exactly.
        exec(
            &pool,
            "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, result, created_at) \
             VALUES ('k1', 'review', 'pending_decision', 'pd-old', NOW() - INTERVAL '200 days'), \
                    ('k1', 'in_progress', 'pending_decision', 'pd-latest', NOW() - INTERVAL '190 days'), \
                    ('k1', 'a', 'review', 'tie-small-id', NOW() - INTERVAL '185 days'), \
                    ('k1', 'b', 'review', 'tie-large-id', NOW() - INTERVAL '185 days'), \
                    (NULL, 'x', 'done', 'no-card', NOW() - INTERVAL '200 days'), \
                    ('k2', 'old', 'done', 'k2-aged', NOW() - INTERVAL '200 days'), \
                    ('k2', 'null', 'done', 'k2-null-time', NULL), \
                    ('k3', 'c', 'pending_decision', 'in-window', NOW() - INTERVAL '170 days'); \
             INSERT INTO review_tuning_outcomes \
                 (card_id, review_round, verdict, outcome, created_at) \
             VALUES ('t1', 1, 'tn-r1', 'true_negative', NOW() - INTERVAL '100 days'), \
                    ('t1', 2, 'tn-r2a', 'true_negative', NOW() - INTERVAL '100 days'), \
                    ('t1', 2, 'tn-r2b', 'true_negative', NOW() - INTERVAL '100 days'), \
                    ('t1', NULL, 'tn-no-round', 'true_negative', NOW() - INTERVAL '100 days'), \
                    ('t1', 1, 'fn-old', 'false_negative', NOW() - INTERVAL '100 days'), \
                    ('t1', 1, 'fn-new', 'false_negative', NOW() - INTERVAL '95 days'), \
                    ('t1', 1, 'tp-aged', 'true_positive', NOW() - INTERVAL '100 days'), \
                    (NULL, 3, 'no-card', 'true_negative', NOW() - INTERVAL '100 days'), \
                    ('t1', 3, 'recent', 'true_positive', NOW() - INTERVAL '10 days'); \
             INSERT INTO agent_recovery_channel_state \
                 (channel_id, status, owner_agent_id, fallback_agent_id, active_writer_agent_id) \
             VALUES ('c1', 'owner', 'a', 'b', 'a'), ('c2', 'owner', 'a', 'b', 'a'); \
             INSERT INTO agent_recovery_checkpoint_events \
                 (id, channel_id, seq, at, writer_agent_id, kind, payload, payload_bytes) \
             SELECT channel || '-' || seq, channel, seq, \
                    NOW() - CASE WHEN seq = 11 THEN INTERVAL '10 days' ELSE INTERVAL '100 days' END, \
                    channel || '-' || seq, 'owner_progress', \
                    '{\"schema_version\":1,\"identity_label\":\"t\",\"goal\":\"\",\"progress\":\"\", \
                      \"decisions\":\"\",\"files\":[],\"next\":\"\",\"last_user_message\":\"\"}'::jsonb, 0 \
             FROM (SELECT 'c1' AS channel, generate_series(1, 11) AS seq \
                   UNION ALL SELECT 'c2', generate_series(1, 3)) s",
        )
        .await;

        let readers = |pool: PgPool| async move {
            let mut prior = Vec::new();
            for (card, to) in [("k1", "pending_decision"), ("k1", "review"), ("k2", "done")] {
                prior.push(
                    crate::server::routes::resume::prior_status_into_pg(&pool, card, to)
                        .await
                        .expect("prior status"),
                );
            }
            let mut events = Vec::new();
            for channel in ["c1", "c2"] {
                let loaded = crate::services::agent_recovery::checkpoint::load_checkpoint_events(
                    &pool,
                    channel,
                    crate::services::agent_recovery::checkpoint::DEFAULT_READ_EVENT_LIMIT as i64,
                )
                .await
                .expect("checkpoint events");
                events.push(loaded.into_iter().map(|e| e.id).collect::<Vec<_>>());
            }
            // The reopen reclassifies exactly the true negatives at the card's top round.
            let reopened: Vec<String> = sqlx::query_scalar(
                "SELECT verdict FROM review_tuning_outcomes \
                 WHERE card_id = 't1' AND outcome = 'true_negative' AND review_round = \
                     (SELECT MAX(review_round) FROM review_tuning_outcomes \
                      WHERE card_id = 't1' AND outcome = 'true_negative') ORDER BY 1",
            )
            .fetch_all(&pool)
            .await
            .expect("reopen targets");
            (prior, events, reopened)
        };
        let before = readers(pool.clone()).await;
        assert_eq!(
            before.0,
            [Some("in_progress"), Some("b"), Some("null")].map(|s| s.map(String::from))
        );

        let live = db_retention_job(&pool, false).await.expect("live pass");
        for (table, deleted) in [
            ("kanban_audit_logs", 3),
            ("review_tuning_outcomes", 5),
            ("agent_recovery_checkpoint_events", 3),
        ] {
            assert_eq!(
                live.get(table, "delete").map(|t| t.rows_affected),
                Some(deleted),
                "{table}: rows no reader picks are deleted"
            );
        }
        assert_eq!(
            readers(pool.clone()).await,
            before,
            "the readers answer as before"
        );
        assert_eq!(
            keys(&pool, "SELECT result FROM kanban_audit_logs ORDER BY 1").await,
            [
                "in-window",
                "k2-aged",
                "k2-null-time",
                "pd-latest",
                "tie-large-id"
            ]
            .map(String::from)
        );
        assert_eq!(
            keys(
                &pool,
                "SELECT verdict FROM review_tuning_outcomes ORDER BY 1"
            )
            .await,
            ["fn-new", "recent", "tn-r2a", "tn-r2b"].map(String::from)
        );

        crate::kanban::correct_tn_to_fn_on_reopen(Some(&pool), "t1");
        assert_eq!(
            keys(
                &pool,
                "SELECT verdict FROM review_tuning_outcomes WHERE outcome = 'false_negative' ORDER BY 1"
            )
            .await,
            ["fn-new", "tn-r2a", "tn-r2b"].map(String::from),
            "the reopen still turns the top-round true negatives into false negatives"
        );

        pool.close().await;
        db.drop().await;
    }
}
