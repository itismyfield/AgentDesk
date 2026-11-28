//! A deferred process run keeps its durable execution mode across executor config changes.
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::routine_ports::build_routine_agent_executor;
use crate::services::routines::store::{NewRoutine, RoutineStore};
use chrono::{Duration, Utc};
use serde_json::Value;
use std::sync::Arc;

#[tokio::test(flavor = "current_thread")]
async fn deferred_process_run_keeps_mode_after_executor_config_changes_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_cc) VALUES ('process-agent', 'process agent', 'claude', '6551001')")
        .execute(&pool).await.unwrap();
    let store = RoutineStore::new_with_timezone_and_checkpoint_limit(
        Arc::new(pool.clone()),
        "Asia/Seoul",
        256 * 1024,
    );
    let routine = store
        .attach_routine(NewRoutine {
            agent_id: Some("process-agent".into()),
            fallback_agent_id: None,
            max_retries: Some(0),
            script_ref: "process-test.js".into(),
            name: "process test".into(),
            status: None,
            execution_strategy: "fresh".into(),
            schedule: None,
            next_due_at: Some(Utc::now() - Duration::seconds(1)),
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: None,
        })
        .await
        .unwrap();
    let claimed = store.claim_run_now(&routine.id).await.unwrap().unwrap();
    let run_id = claimed.run_id.clone();
    let registry = Arc::new(HealthRegistry::new());
    let enabled = build_routine_agent_executor(Arc::new(pool.clone()), Some(registry.clone()), 60)
        .with_headless_fresh_agents(true);
    let outcome = enabled
        .start_agent_run(&store, claimed, "run".into(), None, None, None, None, false)
        .await
        .unwrap();
    assert_eq!(outcome.status, "deferred");
    assert_eq!(outcome.result_json.unwrap()["execution_mode"], "process");
    sqlx::query(
        "UPDATE routine_runs SET next_retry_at = NOW() - INTERVAL '1 second' WHERE id = $1",
    )
    .bind(&run_id)
    .execute(&pool)
    .await
    .unwrap();
    let disabled = build_routine_agent_executor(Arc::new(pool.clone()), Some(registry), 60);
    let outcomes = disabled.poll_agent_runs(&store, 10, false).await.unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].status, "deferred");
    assert_eq!(
        outcomes[0].result_json.as_ref().unwrap()["execution_mode"],
        "process"
    );
    let row: Value = sqlx::query_scalar("SELECT result_json FROM routine_runs WHERE id=$1")
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row["execution_mode"], "process");
    pool.close().await;
    db.drop().await;
}
