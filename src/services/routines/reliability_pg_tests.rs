//! Boot-time routine reliability against PostgreSQL: start deferral, the startup grace, and
//! the consecutive-failure alert.
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::PgPool;

use super::store::{ClaimedRoutineRun, NewRoutine, RoutineStore};
use super::{
    RoutineAgentExecutor, RoutineDiscordLogger, RoutineScriptLoader, after_startup_grace,
    run_due_tick, runtime::RoutineRunOutcome,
};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::{HealthRegistry, register_bot_auth_for_tests};

fn store(pool: &PgPool) -> RoutineStore {
    RoutineStore::new_with_timezone_and_checkpoint_limit(
        Arc::new(pool.clone()),
        "Asia/Seoul",
        256 * 1024,
    )
}

fn new_routine(name: &str, agent_id: Option<&str>, thread: Option<&str>) -> NewRoutine {
    NewRoutine {
        agent_id: agent_id.map(str::to_string),
        fallback_agent_id: None,
        max_retries: Some(0),
        script_ref: format!("{name}.js"),
        name: name.to_string(),
        status: None,
        execution_strategy: "persistent".to_string(),
        schedule: Some("0 3 * * *".to_string()),
        next_due_at: Some(Utc::now() - Duration::minutes(5)),
        checkpoint: None,
        discord_thread_id: thread.map(str::to_string),
        timeout_secs: None,
    }
}

async fn run_row(pool: &PgPool, run_id: &str) -> (String, i32, Option<Value>) {
    sqlx::query_as("SELECT status, retry_count, result_json FROM routine_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .expect("routine run row")
}

async fn insert_boot_agent(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_cc)
         VALUES ('boot-agent', 'boot agent', 'claude', '6551001')",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn claim_boot_agent_run(store: &RoutineStore) -> ClaimedRoutineRun {
    store
        .attach_routine(new_routine("memento-hygiene", Some("boot-agent"), None))
        .await
        .unwrap();
    store
        .claim_due_runs(1)
        .await
        .unwrap()
        .pop()
        .expect("due run")
}

/// Makes a deferred run due now, with its deferral window opened at `opened`.
async fn retry_deferred_start(pool: &PgPool, run_id: &str, opened: DateTime<Utc>) {
    let opened = opened.to_rfc3339();
    sqlx::query(
        "UPDATE routine_runs
         SET result_json = jsonb_set(result_json, '{deferred_since}', to_jsonb($2::text)),
             next_retry_at = NOW() - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(run_id)
    .bind(&opened)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn agent_start_before_provider_registers_defers_then_fails_after_window_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    insert_boot_agent(&pool).await;
    let store = store(&pool);
    let claimed = claim_boot_agent_run(&store).await;
    let run_id = claimed.run_id.clone();
    // A registry with no provider registered yet is the state right after a restart.
    let executor = RoutineAgentExecutor::new(
        Arc::new(pool.clone()),
        Some(Arc::new(HealthRegistry::new())),
        60,
    );

    let outcome = executor
        .start_agent_run(&store, claimed, "run".into(), None, None, None, None, false)
        .await
        .unwrap();
    assert_eq!(outcome.status, "deferred", "{outcome:?}");
    let (status, retry_count, result) = run_row(&pool, &run_id).await;
    let result = result.expect("deferred result");
    assert_eq!((status.as_str(), retry_count), ("running", 0));
    assert_eq!(result["deferred_reason"], "provider_not_ready");

    // The runtime never registers: once the deferral window has passed the start fails.
    retry_deferred_start(&pool, &run_id, Utc::now() - Duration::minutes(11)).await;
    let outcomes = executor.poll_agent_runs(&store, 10, false).await.unwrap();
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0].status, "failed", "{outcomes:?}");
    let (status, _, _) = run_row(&pool, &run_id).await;
    assert_eq!(status, "failed");

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn dm_agent_start_before_runtime_is_ready_defers_then_fails_after_window_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    insert_boot_agent(&pool).await;
    let store = store(&pool);
    let claimed = claim_boot_agent_run(&store).await;
    let run_id = claimed.run_id.clone();
    // Registered for the agent's channel but with no Discord client yet: the DM reservation
    // reports a runtime that is not ready.
    let registry = Arc::new(HealthRegistry::new());
    register_bot_auth_for_tests(&registry, "claude", 6551001, None, Vec::new(), false).await;
    let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), Some(registry), 60);

    let outcome = executor
        .start_agent_run(
            &store,
            claimed,
            "run".into(),
            Some("4242".into()),
            None,
            None,
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, "deferred", "{outcome:?}");
    let (status, retry_count, result) = run_row(&pool, &run_id).await;
    let result = result.expect("deferred result");
    assert_eq!((status.as_str(), retry_count), ("running", 0));
    assert_eq!(result["deferred_reason"], "provider_not_ready");
    assert_eq!(result["dm_user_id"], "4242");
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|error| error.contains("DM turn for boot-agent and user 4242")),
        "{result}"
    );

    // The re-attempt inside the window reserves the DM again and stays deferred.
    retry_deferred_start(&pool, &run_id, Utc::now() - Duration::minutes(1)).await;
    let outcomes = executor.poll_agent_runs(&store, 10, false).await.unwrap();
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0].status, "deferred", "{outcomes:?}");
    let (_, retry_count, result) = run_row(&pool, &run_id).await;
    let result = result.expect("deferred result");
    assert_eq!(
        (retry_count, &result["dm_user_id"]),
        (0, &Value::from("4242"))
    );

    // Once the window has passed the same error fails the run.
    retry_deferred_start(&pool, &run_id, Utc::now() - Duration::minutes(11)).await;
    let outcomes = executor.poll_agent_runs(&store, 10, false).await.unwrap();
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0].status, "failed", "{outcomes:?}");
    let (status, _, _) = run_row(&pool, &run_id).await;
    assert_eq!(status, "failed");

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn slot_missed_during_startup_grace_runs_exactly_once_after_it_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let temp = tempfile::tempdir().unwrap();
    let script_path = temp.path().join("grace-slot.js");
    std::fs::write(
        &script_path,
        r#"agentdesk.routines.register({
          name: "Grace slot",
          tick(ctx) { return { action: "complete", result: { ok: true } }; }
        });"#,
    )
    .unwrap();
    let loader = RoutineScriptLoader::new().unwrap();
    let script_ref = loader.load_script(temp.path(), &script_path).unwrap();
    let store = store(&pool);
    let mut routine = new_routine("grace-slot", None, None);
    routine.script_ref = script_ref;
    let routine = store.attach_routine(routine).await.unwrap();
    let dirs = [temp.path().to_path_buf()];
    let tick = |secs: u64| {
        let due_tick = run_due_tick(&store, &loader, &dirs, None, None, 10, false);
        after_startup_grace(std::time::Duration::from_secs(secs), 120, due_tick)
    };

    assert!(tick(30).await.unwrap().is_none(), "grace must hold claims");
    let first = tick(120).await.unwrap().expect("grace over");
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0].status, "succeeded");
    assert!(tick(150).await.unwrap().expect("grace over").is_empty());
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM routine_runs WHERE routine_id = $1")
        .bind(&routine.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(runs, 1);

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn logged_unicode_object_completion_and_next_due_run_persist_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let temp = tempfile::tempdir().unwrap();
    let loader = RoutineScriptLoader::new().unwrap();
    let store = store(&pool);
    let logger = RoutineDiscordLogger::new_with_health_registry(Arc::new(pool.clone()), None);
    let dirs = [temp.path().to_path_buf()];
    for name in ["unicode-result", "next-result"] {
        let script_path = temp.path().join(format!("{name}.js"));
        std::fs::write(
            &script_path,
            r#"agentdesk.routines.register({
              name: "Object result",
              tick(ctx) { return { action: "complete", result: {
                summary: {text: "한".repeat(30)}, status: {text: "🙂".repeat(30)}
              }}; }
            });"#,
        )
        .unwrap();
        let mut routine = new_routine(name, None, None);
        routine.script_ref = loader.load_script(temp.path(), &script_path).unwrap();
        let routine = store.attach_routine(routine).await.unwrap();
        let outcomes = run_due_tick(&store, &loader, &dirs, None, Some(&logger), 1, false)
            .await
            .unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].routine_id, routine.id);
        assert_eq!(outcomes[0].status, "succeeded");
        let (status, _, result) = run_row(&pool, &outcomes[0].run_id).await;
        assert_eq!(status, "succeeded");
        let result = result.unwrap();
        assert_eq!(result["summary"]["text"], "한".repeat(30));
        assert_eq!(result["status"]["text"], "🙂".repeat(30));
        let finished: bool =
            sqlx::query_scalar("SELECT finished_at IS NOT NULL FROM routine_runs WHERE id = $1")
                .bind(&outcomes[0].run_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(finished);
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn consecutive_failure_alert_fires_once_per_streak_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let store = store(&pool);
    let routine = store
        .attach_routine(new_routine(
            "token-daily-report",
            None,
            Some("1479671301387069001"),
        ))
        .await
        .unwrap();
    let logger = RoutineDiscordLogger::new_with_health_registry(Arc::new(pool.clone()), None);
    let past = Utc::now() - Duration::minutes(5);
    let alerts = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM message_outbox WHERE reason_code = 'routine_consecutive_failures'",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let run = |fail: bool| {
        let (store, logger) = (&store, &logger);
        async move {
            let claimed = store
                .claim_due_runs(1)
                .await
                .unwrap()
                .pop()
                .expect("due run");
            let status = if fail {
                store
                    .fail_run(&claimed.run_id, "provider down", None, Some(past))
                    .await
                    .unwrap();
                "failed"
            } else {
                store
                    .finish_run(&claimed.run_id, None, None, None, Some(past))
                    .await
                    .unwrap();
                "succeeded"
            };
            let outcome = RoutineRunOutcome {
                run_id: claimed.run_id,
                routine_id: claimed.routine_id,
                script_ref: claimed.script_ref,
                action: "agent".to_string(),
                status: status.to_string(),
                result_json: None,
                error: fail.then(|| "provider down".to_string()),
                fresh_context_guaranteed: false,
            };
            logger.alert_consecutive_failures(store, &outcome, 3).await;
        }
    };

    for expected in [0, 0, 1, 1] {
        run(true).await;
        assert_eq!(alerts().await, expected);
    }
    run(false).await;
    for expected in [1, 1, 2] {
        run(true).await;
        assert_eq!(alerts().await, expected);
    }
    let content: String = sqlx::query_scalar(
        "SELECT content FROM message_outbox WHERE reason_code = 'routine_consecutive_failures' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        content.contains(&routine.id) && content.contains("provider down"),
        "{content}"
    );

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn checkpoint_rejected_completion_counts_toward_the_failure_alert_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let temp = tempfile::tempdir().unwrap();
    let script_path = temp.path().join("oversized-checkpoint.js");
    std::fs::write(
        &script_path,
        r#"agentdesk.routines.register({
          name: "Oversized checkpoint",
          tick(ctx) { return { action: "complete", checkpoint: { blob: "x".repeat(512) } }; }
        });"#,
    )
    .unwrap();
    let loader = RoutineScriptLoader::new().unwrap();
    let mut routine = new_routine("oversized-checkpoint", None, Some("1479671301387069002"));
    routine.script_ref = loader.load_script(temp.path(), &script_path).unwrap();
    // A 64-byte checkpoint limit makes the store close the requested `complete` as `failed`.
    let store = RoutineStore::new_with_timezone_and_checkpoint_limit(
        Arc::new(pool.clone()),
        "Asia/Seoul",
        64,
    );
    let routine = store.attach_routine(routine).await.unwrap();
    let logger = RoutineDiscordLogger::new_with_health_registry(Arc::new(pool.clone()), None);
    let past = Utc::now() - Duration::minutes(5);
    let make_due = || async {
        sqlx::query("UPDATE routines SET next_due_at = $2 WHERE id = $1")
            .bind(&routine.id)
            .bind(past)
            .execute(&pool)
            .await
            .unwrap();
    };
    let alerts = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM message_outbox WHERE reason_code = 'routine_consecutive_failures'",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let fail_once = || async {
        make_due().await;
        let claimed = store
            .claim_due_runs(1)
            .await
            .unwrap()
            .pop()
            .expect("due run");
        store
            .fail_run(&claimed.run_id, "provider down", None, None)
            .await
            .unwrap();
        let outcome = RoutineRunOutcome {
            run_id: claimed.run_id,
            routine_id: claimed.routine_id,
            script_ref: claimed.script_ref,
            action: "agent".to_string(),
            status: "failed".to_string(),
            result_json: None,
            error: Some("provider down".to_string()),
            fresh_context_guaranteed: false,
        };
        logger.alert_consecutive_failures(&store, &outcome, 3).await;
    };

    fail_once().await;
    fail_once().await;
    make_due().await;
    let dirs = [temp.path().to_path_buf()];
    let outcomes = run_due_tick(&store, &loader, &dirs, None, None, 10, false)
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    let (status, _, _) = run_row(&pool, &outcomes[0].run_id).await;
    assert_eq!(
        status, "failed",
        "the store rejects the oversized checkpoint"
    );
    logger
        .alert_consecutive_failures(&store, &outcomes[0], 3)
        .await;
    assert_eq!(alerts().await, 1);
    fail_once().await;
    assert_eq!(alerts().await, 1);

    pool.close().await;
    db.drop().await;
}

async fn started_agent_run(
    pool: &PgPool,
    store: &RoutineStore,
    name: &str,
    strategy: &str,
    turn: &str,
) -> String {
    let mut routine = new_routine(name, None, None);
    routine.execution_strategy = strategy.to_string();
    store.attach_routine(routine).await.unwrap();
    let claimed = store
        .claim_due_runs(1)
        .await
        .unwrap()
        .pop()
        .expect("due run");
    assert!(
        store
            .mark_agent_turn_started(&claimed.run_id, turn, None, "agent", "primary")
            .await
            .unwrap()
    );
    // The turn started before any of its quality events.
    sqlx::query("UPDATE routine_runs SET started_at = NOW() - INTERVAL '10 minutes' WHERE id = $1")
        .bind(&claimed.run_id)
        .execute(pool)
        .await
        .unwrap();
    claimed.run_id
}

async fn quality_event(pool: &PgPool, turn: &str, event_type: &str, outcome: &str, age_secs: i64) {
    sqlx::query(
        "INSERT INTO agent_quality_event (source_event_id, correlation_id, event_type, payload, created_at)
         VALUES ($1, $1, $2::agent_quality_event_type,
                 jsonb_build_object('details', jsonb_build_object('outcome', $3::text)),
                 NOW() - ($4::bigint * INTERVAL '1 second'))",
    )
    .bind(turn)
    .bind(event_type)
    .bind(outcome)
    .bind(age_secs)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn fresh_terminal_turn_error_ends_run_as_failure_before_timeout_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let store = store(&pool);
    let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), None, 1800);
    let cancelled =
        started_agent_run(&pool, &store, "fresh-cancelled", "fresh", "discord:7001:1").await;
    quality_event(&pool, "discord:7001:1", "turn_error", "cancelled", 60).await;
    let late = started_agent_run(&pool, &store, "fresh-late", "fresh", "discord:7002:1").await;
    quality_event(&pool, "discord:7002:1", "turn_error", "transport_error", 0).await;
    let persistent = started_agent_run(&pool, &store, "kept", "persistent", "discord:7003:1").await;
    quality_event(&pool, "discord:7003:1", "turn_error", "cancelled", 60).await;
    let retrying = started_agent_run(&pool, &store, "fresh-retry", "fresh", "discord:7004:1").await;
    quality_event(&pool, "discord:7004:1", "turn_error", "recovery_retry", 60).await;
    let ok = started_agent_run(&pool, &store, "fresh-ok", "fresh", "discord:7005:1").await;
    quality_event(&pool, "discord:7005:1", "turn_complete", "completed", 60).await;

    let outcomes = executor.poll_agent_runs(&store, 10, false).await.unwrap();
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(
        (outcomes[0].run_id.as_str(), outcomes[0].status.as_str()),
        (cancelled.as_str(), "failed")
    );
    assert!(
        outcomes[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("cancelled")),
        "{outcomes:?}"
    );
    assert_eq!(run_row(&pool, &cancelled).await.0, "failed");
    for run in [&late, &persistent, &retrying, &ok] {
        assert_eq!(run_row(&pool, run).await.0, "running", "{run}");
    }

    // A transcript persisted after the error event still completes the run normally.
    sqlx::query(
        "INSERT INTO session_transcripts (turn_id, assistant_message)
         VALUES ('discord:7002:1', 'partial briefing'), ('discord:7005:1', 'daily briefing')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let outcomes = executor.poll_agent_runs(&store, 10, false).await.unwrap();
    let mut done: Vec<_> = outcomes
        .iter()
        .map(|o| (o.run_id.clone(), o.status.clone()))
        .collect();
    done.sort();
    let mut expected = vec![
        (late.clone(), "succeeded".to_string()),
        (ok.clone(), "succeeded".to_string()),
    ];
    expected.sort();
    assert_eq!(done, expected);

    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn latest_runs_return_each_routines_newest_run_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let store = store(&pool);
    let ran = store
        .attach_routine(new_routine("ran-twice", None, None))
        .await
        .unwrap();
    let idle = store
        .attach_routine(new_routine("never-ran", None, None))
        .await
        .unwrap();
    for (run_id, minutes_ago) in [("run-old", 10), ("run-new", 1)] {
        sqlx::query(
            "INSERT INTO routine_runs (id, routine_id, status, started_at)
             VALUES ($1, $2, 'succeeded', NOW() - ($3::bigint * INTERVAL '1 minute'))",
        )
        .bind(run_id)
        .bind(&ran.id)
        .bind(minutes_ago)
        .execute(&pool)
        .await
        .unwrap();
    }

    let latest = store
        .latest_runs(&[ran.id.clone(), idle.id.clone()])
        .await
        .unwrap();
    assert_eq!(latest, store.list_runs(&ran.id, 1).await.unwrap());
    assert_eq!(latest[0].id, "run-new");

    pool.close().await;
    db.drop().await;
}
