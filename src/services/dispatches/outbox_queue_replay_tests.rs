use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use std::sync::Mutex;

#[derive(Default)]
struct RecordingNotifier(Mutex<Vec<(String, String)>>);

impl OutboxNotifier for RecordingNotifier {
    async fn notify_dispatch(
        &self,
        _agent_id: String,
        _title: String,
        _card_id: String,
        dispatch_id: String,
    ) -> Result<DispatchNotifyDeliveryResult, String> {
        self.0
            .lock()
            .unwrap()
            .push((dispatch_id.clone(), "notify".into()));
        Ok(DispatchNotifyDeliveryResult::success(
            &dispatch_id,
            "notify",
            "sent",
        ))
    }

    async fn handle_followup(&self, dispatch_id: String) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .push((dispatch_id, "followup".into()));
        Ok(())
    }

    async fn sync_status_reaction(&self, dispatch_id: String) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .push((dispatch_id, "status_reaction".into()));
        Ok(())
    }
}

struct RuntimeFixture {
    _config_guard: crate::config::TestEnvVarGuard,
    _guard: crate::config::TestEnvVarGuard,
    _root: tempfile::TempDir,
}

fn runtime_fixture() -> RuntimeFixture {
    let root = tempfile::tempdir().expect("create isolated outbox runtime root");
    let config = root.path().join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join("agentdesk.yaml"), "server: {}\n").unwrap();
    let guard = crate::config::set_agentdesk_root_for_test(root.path());
    let config_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config.join("agentdesk.yaml"),
    );
    RuntimeFixture {
        _config_guard: config_guard,
        _guard: guard,
        _root: root,
    }
}

async fn setup() -> (TestPostgresDb, PgPool) {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    let version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(&pool)
        .await
        .expect("outbox assertions require real PostgreSQL");
    eprintln!("replay_hold outbox real-PG fixture: {version}");
    (fixture, pool)
}

async fn seed_dispatch(pool: &PgPool, id: &str, status: &str, linked: bool) -> Option<i64> {
    let receipt = if linked {
        Some(
            sqlx::query_scalar::<_, i64>(
                r#"INSERT INTO intake_outbox (target_instance_id, forwarded_by_instance_id,
                channel_id, user_msg_id, request_owner_id, user_text, turn_kind, agent_id,
                provider, status, replay_only, replay_disposition, replay_source_message_ids,
                replay_episode_nonce, replay_request_key, replay_preserved)
             VALUES ('worker', 'leader', $1, $1, 'user', 'original request', 'foreground',
                'agent', 'claude', 'unknown', TRUE, 'registered_not_started', ARRAY[$1],
                'episode', $1, '{"partial_body":"preserved output"}') RETURNING id"#,
            )
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("seed registered outbox receipt"),
        )
    } else {
        None
    };
    sqlx::query(
        r#"INSERT INTO task_dispatches (id, status, replay_receipt_id, result)
         VALUES ($1, $2, $3, 'preserved output')"#,
    )
    .bind(id)
    .bind(status)
    .bind(receipt)
    .execute(pool)
    .await
    .unwrap();
    for action in ["notify", "followup", "status_reaction"] {
        sqlx::query(
            r#"INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title)
             VALUES ($1, $2, 'agent', 'card', 'original title')"#,
        )
        .bind(id)
        .bind(action)
        .execute(pool)
        .await
        .expect("seed old pending outbox row before hold");
    }
    receipt
}

async fn classify(pool: &PgPool, receipt: i64, disposition: &str) {
    sqlx::query(
        "UPDATE intake_outbox SET replay_disposition = 'started_unclassified' WHERE id = $1",
    )
    .bind(receipt)
    .execute(pool)
    .await
    .unwrap();
    if disposition != "started_unclassified" {
        sqlx::query("UPDATE intake_outbox SET replay_disposition = $2 WHERE id = $1")
            .bind(receipt)
            .bind(disposition)
            .execute(pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn replay_hold_old_notify_and_followup_never_call_notifier_but_status_reaction_does_pg() {
    let _runtime = runtime_fixture();
    let (fixture, pool) = setup().await;
    for disposition in [
        "started_unclassified",
        "startup_failed_no_effect",
        "withheld",
    ] {
        let receipt = seed_dispatch(&pool, disposition, "pending", true)
            .await
            .unwrap();
        classify(&pool, receipt, disposition).await;
    }
    let notifier = RecordingNotifier::default();
    assert_eq!(
        process_outbox_batch_with_pg(Some(&pool), &notifier, Some("replacement-node")).await,
        9
    );
    let calls = notifier.0.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    assert!(
        calls.iter().all(|(_, action)| action == "status_reaction"),
        "held execution calls: {calls:?}"
    );
    let retained: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM task_dispatches WHERE status = 'pending' AND result = 'preserved output'),
                (SELECT COUNT(*) FROM intake_outbox WHERE replay_preserved->>'partial_body' = 'preserved output'),
                (SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'done')",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(retained, (3, 3, 9));
    let suppressed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM dispatch_outbox WHERE action IN ('notify', 'followup')
           AND delivery_result->>'detail' LIKE '%held%' AND retry_count = 0",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(suppressed, 6);
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn replay_hold_dormant_batch_preserves_legacy_notify_and_classified_terminal_followup_pg() {
    let _runtime = runtime_fixture();
    let (fixture, pool) = setup().await;
    seed_dispatch(&pool, "legacy-pending", "pending", false).await;
    let receipt = seed_dispatch(&pool, "classified-completed", "completed", true)
        .await
        .unwrap();
    classify(&pool, receipt, "classified_normal").await;
    let notifier = RecordingNotifier::default();
    assert_eq!(
        process_outbox_batch_with_pg(Some(&pool), &notifier, Some("normal-node")).await,
        6
    );
    let mut calls = notifier.0.lock().unwrap().clone();
    calls.sort();
    let mut expected = vec![
        ("legacy-pending".into(), "notify".into()),
        ("legacy-pending".into(), "followup".into()),
        ("legacy-pending".into(), "status_reaction".into()),
        ("classified-completed".into(), "followup".into()),
        ("classified-completed".into(), "status_reaction".into()),
    ];
    expected.sort();
    assert_eq!(calls, expected);
    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT id, status FROM task_dispatches ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        statuses,
        vec![
            ("classified-completed".into(), "completed".into()),
            ("legacy-pending".into(), "dispatched".into())
        ]
    );
    let done: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM dispatch_outbox WHERE status = 'done'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(done, 6);
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn replay_hold_unreadable_batch_gate_fails_closed_without_spending_retry_budget_pg() {
    let _runtime = runtime_fixture();
    let (fixture, pool) = setup().await;
    seed_dispatch(&pool, "unreadable-gate", "pending", false).await;
    sqlx::raw_sql(
        "CREATE OR REPLACE FUNCTION replay_disposition_blocks_rerun(disposition TEXT)
         RETURNS BOOLEAN LANGUAGE plpgsql IMMUTABLE AS $$
         BEGIN RAISE EXCEPTION 'test replay gate unreadable'; END $$",
    )
    .execute(&pool)
    .await
    .unwrap();
    let notifier = RecordingNotifier::default();
    assert_eq!(
        process_outbox_batch_with_pg(Some(&pool), &notifier, Some("gate-node")).await,
        3
    );
    assert_eq!(
        notifier.0.lock().unwrap().as_slice(),
        &[("unreadable-gate".into(), "status_reaction".into())]
    );
    let preserved: (i64, String, String) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM dispatch_outbox WHERE action IN ('notify', 'followup')
                 AND status IN ('pending', 'processing') AND retry_count = 0 AND processed_at IS NULL),
                status, result FROM task_dispatches WHERE id = 'unreadable-gate'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(preserved, (2, "pending".into(), "preserved output".into()));
    pool.close().await;
    fixture.drop().await;
}
