//! Boot-time routine reliability against PostgreSQL: start deferral, the startup grace, and
//! the consecutive-failure alert.
use std::sync::Arc;

use chrono::{Duration, Utc};
use serde_json::Value;
use sqlx::PgPool;

use super::store::{NewRoutine, RoutineStore};
use super::{
    RoutineAgentExecutor, RoutineDiscordLogger, RoutineScriptLoader, after_startup_grace,
    run_due_tick, runtime::RoutineRunOutcome,
};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::HealthRegistry;

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

#[tokio::test(flavor = "current_thread")]
async fn agent_start_before_provider_registers_defers_then_fails_after_window_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_cc)
         VALUES ('boot-agent', 'boot agent', 'claude', '6551001')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = store(&pool);
    store
        .attach_routine(new_routine("memento-hygiene", Some("boot-agent"), None))
        .await
        .unwrap();
    let claimed = store
        .claim_due_runs(1)
        .await
        .unwrap()
        .pop()
        .expect("due run");
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
    let opened = (Utc::now() - Duration::minutes(11)).to_rfc3339();
    sqlx::query(
        "UPDATE routine_runs
         SET result_json = jsonb_set(result_json, '{deferred_since}', to_jsonb($2::text)),
             next_retry_at = NOW() - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(&run_id)
    .bind(&opened)
    .execute(&pool)
    .await
    .unwrap();
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
