//! Retention keeps the rows the resume and recovery readers still pick.
use crate::services::maintenance::jobs::db_retention::db_retention_job;
use sqlx::PgPool;

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

/// Audit, tuning and checkpoint rows go only when no reader can pick them; the resume and
/// recovery readers answer as before (the reopen reader is checked in `tuning_aggregate`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_retention_keeps_rows_the_resume_and_recovery_readers_use() {
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
        (prior, events)
    };
    let before = readers(pool.clone()).await;
    assert_eq!(
        before.0,
        [Some("in_progress"), Some("b"), Some("null")].map(|s| s.map(String::from))
    );

    let live = db_retention_job(&pool, false).await.expect("live pass");
    for (table, deleted) in [
        ("kanban_audit_logs", 3),
        ("review_tuning_outcomes", 4),
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
        ["fn-new", "recent", "tn-r1", "tn-r2a", "tn-r2b"].map(String::from)
    );

    pool.close().await;
    db.drop().await;
}
