use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

type RetryPolicyState = (
    String,
    String,
    i64,
    Option<String>,
    Option<i64>,
    String,
    String,
    Option<String>,
);

struct RuntimeFixture {
    _config_guard: crate::config::TestEnvVarGuard,
    _guard: crate::config::TestEnvVarGuard,
    _root: tempfile::TempDir,
}

fn runtime_fixture() -> RuntimeFixture {
    let root = tempfile::tempdir().expect("create isolated failure runtime root");
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

async fn setup() -> (TestPostgresDb, sqlx::PgPool) {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    let version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(&pool)
        .await
        .expect("failure assertions require real PostgreSQL");
    eprintln!("replay_hold failure reducer real-PG fixture: {version}");
    (fixture, pool)
}

async fn seed_failure(
    pool: &sqlx::PgPool,
    suffix: &str,
    dispatch_status: &str,
    entry_status: &str,
    disposition: Option<&str>,
) -> (String, String, String) {
    let run = format!("run-{suffix}");
    let entry = format!("entry-{suffix}");
    let dispatch = format!("dispatch-{suffix}");
    let receipt = if disposition.is_some() {
        Some(
            sqlx::query_scalar::<_, i64>(
                r#"INSERT INTO intake_outbox (target_instance_id, forwarded_by_instance_id,
                channel_id, user_msg_id, request_owner_id, user_text, turn_kind, agent_id,
                provider, status, replay_only, replay_disposition, replay_source_message_ids,
                replay_episode_nonce, replay_request_key, replay_preserved)
             VALUES ('worker', 'leader', $1, $1, 'user', 'original request', 'foreground',
                'agent', 'claude', 'unknown', TRUE, 'registered_not_started', ARRAY[$1],
                'episode', $1, '{"partial_body":"retained result"}') RETURNING id"#,
            )
            .bind(suffix)
            .fetch_one(pool)
            .await
            .unwrap(),
        )
    } else {
        None
    };
    sqlx::query("INSERT INTO auto_queue_runs (id, status) VALUES ($1, 'active')")
        .bind(&run)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"INSERT INTO task_dispatches (id, status, dispatch_type, replay_receipt_id, result,
            claim_owner, claimed_at, claim_expires_at)
         VALUES ($1, $2, 'implementation', $3, 'retained result', 'original-node',
            NOW() - INTERVAL '2 hours', NOW() - INTERVAL '1 hour')"#,
    )
    .bind(&dispatch)
    .bind(dispatch_status)
    .bind(receipt)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO auto_queue_entries (id, run_id, agent_id, status, retry_count,
            dispatch_id, slot_index, dispatched_at)
         VALUES ($1, $2, $1, $4, 0, $3, 0, NOW())"#,
    )
    .bind(&entry)
    .bind(&run)
    .bind(&dispatch)
    .bind(entry_status)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO auto_queue_slots (agent_id, slot_index, assigned_run_id,
            assigned_thread_group, thread_id_map) VALUES ($1, 0, $2, 0, '{"thread":"retained"}')"#,
    )
    .bind(&entry)
    .bind(&run)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id, trigger_source)
         VALUES ($1, $2, 'initial')"#,
    )
    .bind(&entry)
    .bind(&dispatch)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO sessions (session_key, provider, status, active_dispatch_id,
            current_replay_receipt_id, replay_episode_nonce, channel_id,
            raw_provider_session_id, session_info)
         VALUES ($1, 'claude', 'turn_active', $2, $3, 'episode', $4,
            'provider-session', 'partial output pending delivery')"#,
    )
    .bind(&dispatch)
    .bind(&dispatch)
    .bind(receipt)
    .bind(suffix)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO dispatch_semaphore_holdings (semaphore_name, scope, scope_key,
            slot_index, holder_instance_id, dispatch_id, expires_at)
         VALUES ('gpu', 'per-cluster', $1, 0, 'original-node', $1, NOW() + INTERVAL '1 hour')"#,
    )
    .bind(&dispatch)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO kv_meta (key, value) VALUES ('runtime-config', '{"maxEntryRetries":3}')
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"#,
    )
    .execute(pool)
    .await
    .unwrap();
    if let Some(disposition) = disposition {
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
    (run, entry, dispatch)
}

async fn preserved_snapshot(
    pool: &sqlx::PgPool,
    run: &str,
    entry: &str,
    dispatch: &str,
) -> serde_json::Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'dispatch', to_jsonb(d), 'entry', to_jsonb(e), 'run', to_jsonb(r),
            'session', to_jsonb(s), 'slot', to_jsonb(slot),
            'receipt', (SELECT to_jsonb(o) FROM intake_outbox o WHERE o.id = d.replay_receipt_id),
            'semaphores', (SELECT jsonb_agg(to_jsonb(h) ORDER BY h.semaphore_name, h.scope, h.scope_key, h.slot_index) FROM dispatch_semaphore_holdings h WHERE h.dispatch_id = d.id),
            'history', (SELECT jsonb_agg(to_jsonb(h) ORDER BY h.id) FROM auto_queue_entry_dispatch_history h WHERE h.entry_id = e.id),
            'dispatch_events', (SELECT COUNT(*) FROM dispatch_events WHERE dispatch_id = d.id),
            'entry_transitions', (SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = e.id),
            'outbox', (SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = d.id))
         FROM task_dispatches d JOIN auto_queue_entries e ON e.id = $2
         JOIN auto_queue_runs r ON r.id = $3 JOIN sessions s ON s.active_dispatch_id = d.id
         JOIN auto_queue_slots slot ON slot.agent_id = e.id AND slot.slot_index = 0
         WHERE d.id = $1",
    ).bind(dispatch).bind(entry).bind(run).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn replay_hold_generic_failure_preserves_request_entry_links_slots_and_delivery_owner_pg() {
    let _runtime = runtime_fixture();
    let (fixture, pool) = setup().await;
    for disposition in [
        "started_unclassified",
        "startup_failed_no_effect",
        "withheld",
    ] {
        for (dispatch_status, entry_status) in [
            ("pending", "dispatched"),
            ("dispatched", "dispatched"),
            ("dispatched", "pending"),
        ] {
            let suffix = format!("{disposition}-{dispatch_status}-{entry_status}");
            let (run, entry, dispatch) = seed_failure(
                &pool,
                &suffix,
                dispatch_status,
                entry_status,
                Some(disposition),
            )
            .await;
            let before = preserved_snapshot(&pool, &run, &entry, &dispatch).await;
            let mut tx = pool.begin().await.unwrap();
            let (outcome, post_commit) = fail_runtime_dispatch_on_pg_tx(
                &mut tx,
                &dispatch,
                &dispatch_failure_result("worker disappeared", None).to_string(),
                true,
                "replay_hold_test_generic_failure",
            )
            .await
            .expect("held generic failure must return a disposition, not fail its transaction");
            tx.commit().await.unwrap();
            assert_eq!(outcome, DispatchFailureWriteOutcome::ReplayHeld, "{suffix}");
            assert!(
                post_commit.is_none(),
                "held generic failure must not emit failure effects"
            );
            assert_eq!(
                preserved_snapshot(&pool, &run, &entry, &dispatch).await,
                before,
                "{suffix}"
            );
            let state: (String, Option<String>, Option<i64>, i64) = sqlx::query_as(
                "SELECT status, dispatch_id, slot_index, retry_count FROM auto_queue_entries WHERE id = $1",
            ).bind(&entry).fetch_one(&pool).await.unwrap();
            assert_eq!(state, (entry_status.into(), Some(dispatch), Some(0), 0));
        }
    }
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn replay_hold_dormant_and_classified_generic_failure_preserve_retry_policy_pg() {
    let _runtime = runtime_fixture();
    let (fixture, pool) = setup().await;
    for (suffix, disposition) in [("legacy", None), ("classified", Some("classified_normal"))] {
        let (run, entry, dispatch) =
            seed_failure(&pool, suffix, "dispatched", "dispatched", disposition).await;
        let mut tx = pool.begin().await.unwrap();
        let (outcome, post_commit) = fail_runtime_dispatch_on_pg_tx(
            &mut tx,
            &dispatch,
            &dispatch_failure_result("ordinary transport failure", None).to_string(),
            true,
            "replay_hold_test_normal_failure",
        )
        .await
        .expect("ordinary failure remains retryable");
        tx.commit().await.unwrap();
        assert_eq!(outcome, DispatchFailureWriteOutcome::Updated);
        assert!(post_commit.is_some());
        let state: RetryPolicyState = sqlx::query_as(
            "SELECT d.status, e.status, e.retry_count, e.dispatch_id, e.slot_index,
                    r.status, s.status, s.active_dispatch_id
             FROM task_dispatches d JOIN auto_queue_entries e ON e.id = $2
             JOIN auto_queue_runs r ON r.id = $3 JOIN sessions s ON s.session_key = d.id
             WHERE d.id = $1",
        )
        .bind(&dispatch)
        .bind(&entry)
        .bind(&run)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            state,
            (
                "failed".into(),
                "pending".into(),
                1,
                None,
                None,
                "active".into(),
                "idle".into(),
                None
            )
        );
        let effects: (i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM dispatch_events WHERE dispatch_id = $1 AND to_status = 'failed'),
                    (SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = $2 AND to_status = 'pending'),
                    (SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = $1 AND action = 'status_reaction'),
                    (SELECT COUNT(*) FROM dispatch_semaphore_holdings WHERE dispatch_id = $1),
                    (SELECT COUNT(*) FROM auto_queue_entry_dispatch_history WHERE entry_id = $2 AND dispatch_id = $1)",
        ).bind(&dispatch).bind(&entry).fetch_one(&pool).await.unwrap();
        assert_eq!(effects, (1, 1, 1, 0, 1));
        let slot: Option<String> = sqlx::query_scalar(
            "SELECT assigned_run_id FROM auto_queue_slots WHERE agent_id = $1 AND slot_index = 0",
        )
        .bind(&entry)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(slot, Some(run));
    }
    pool.close().await;
    fixture.drop().await;
}
