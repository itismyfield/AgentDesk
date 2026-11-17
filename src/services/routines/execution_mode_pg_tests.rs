//! Execution-mode definitions against PostgreSQL: claim snapshot, legacy provenance and the
//! DB constraints behind the API checks.
use std::sync::Arc;

use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::execution_mode::{
    ExecutionDefinition, RoutineExecutionMode, is_definition_constraint_violation,
};
use super::store::{NewRoutine, RoutinePatch, RoutineStore};
use super::{RoutineAgentExecutor, RoutineScriptLoader, run_due_tick};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::HealthRegistry;

fn store(pool: &PgPool) -> RoutineStore {
    RoutineStore::new_with_timezone_and_checkpoint_limit(
        Arc::new(pool.clone()),
        "Asia/Seoul",
        256 * 1024,
    )
}

fn routine(script_ref: &str, strategy: &str, thread: Option<&str>) -> NewRoutine {
    NewRoutine {
        agent_id: Some("boot-agent".into()),
        fallback_agent_id: None,
        max_retries: Some(0),
        script_ref: script_ref.into(),
        name: script_ref.trim_end_matches(".js").into(),
        status: None,
        execution_strategy: strategy.into(),
        schedule: None,
        next_due_at: Some(Utc::now() - Duration::minutes(1)),
        checkpoint: None,
        discord_thread_id: thread.map(str::to_string),
        timeout_secs: None,
    }
}

fn thread_definition(script_ref: &str) -> ExecutionDefinition {
    ExecutionDefinition {
        mode: RoutineExecutionMode::Thread,
        runner: json!({"kind": "quickjs", "script_ref": script_ref}),
    }
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

async fn run_spec(pool: &PgPool, run_id: &str) -> Value {
    sqlx::query_scalar("SELECT run_spec FROM routine_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn in_flight_run_keeps_its_claim_snapshot_while_the_definition_changes_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents (id, name, provider) VALUES ('fallback-agent', 'fb', 'codex')")
        .execute(&pool)
        .await
        .unwrap();
    let store = store(&pool);
    let routine = store
        .attach_routine(routine("legacy.js", "persistent", Some("111")))
        .await
        .unwrap();
    let claimed = store
        .claim_due_runs(1)
        .await
        .unwrap()
        .pop()
        .expect("due run");
    let run_id = claimed.run_id.clone();
    let spec = run_spec(&pool, &run_id).await;
    assert_eq!(spec["execution_mode"], Value::Null, "{spec}");
    assert_eq!(spec["execution_compatibility"], "legacy_persistent");
    assert_eq!(spec["execution_strategy"], "persistent");
    assert_eq!(spec["headless_failure_notice_limit"], 3);

    // The first start pins the backend; a later retry on another backend does not move it.
    for backend in ["tmux", "process"] {
        assert!(
            store
                .mark_agent_turn_started(
                    &run_id,
                    &format!("turn-{backend}"),
                    Some(json!({"execution_mode": backend})),
                    "boot-agent",
                    "primary",
                )
                .await
                .unwrap()
        );
    }
    assert_eq!(run_spec(&pool, &run_id).await["execution_backend"], "tmux");

    // The run's own thread replacement follows the run; an operator PATCH does not.
    assert!(
        store
            .update_discord_thread_id(&routine.id, "333")
            .await
            .unwrap()
    );
    let patched = store
        .patch_routine(
            &routine.id,
            RoutinePatch {
                fallback_agent_id: Some(Some("fallback-agent".into())),
                max_retries: Some(3),
                discord_thread_id: Some(Some("222".into())),
                timeout_secs: Some(Some(77)),
                execution_definition: Some(thread_definition("legacy.js")),
                headless_failure_notice_limit: Some(9),
                ..RoutinePatch::default()
            },
        )
        .await
        .unwrap()
        .expect("patched routine");
    assert_eq!(patched.execution_strategy, "fresh");

    let running = store.list_running_agent_runs(10).await.unwrap();
    let running = running
        .iter()
        .find(|run| run.run_id == run_id)
        .expect("running run");
    assert_eq!(
        (
            running.execution_strategy.as_str(),
            running.discord_thread_id.as_deref(),
            running.timeout_secs,
            running.max_retries,
            running.fallback_agent_id.as_deref(),
        ),
        ("persistent", Some("333"), None, 0, None)
    );

    // The next claim takes the patched definition.
    assert!(
        store
            .fail_run(
                &run_id,
                "done",
                None,
                Some(Utc::now() - Duration::minutes(1))
            )
            .await
            .unwrap()
    );
    let next = store
        .claim_due_runs(1)
        .await
        .unwrap()
        .pop()
        .expect("next run");
    assert_eq!(
        (
            next.execution_strategy.as_str(),
            next.discord_thread_id.as_deref(),
            next.timeout_secs
        ),
        ("fresh", Some("222"), Some(77))
    );
    let spec = run_spec(&pool, &next.run_id).await;
    assert_eq!(spec["execution_mode"], "thread", "{spec}");
    assert_eq!(spec["execution_compatibility"], "explicit");
    assert_eq!(spec["headless_failure_notice_limit"], 9);
    assert_eq!(spec["execution_backend"], Value::Null);

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_mode_refuses_dynamic_dm_while_legacy_null_mode_keeps_it_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    insert_boot_agent(&pool).await;
    let temp = tempfile::tempdir().unwrap();
    let script_path = temp.path().join("dm.js");
    std::fs::write(
        &script_path,
        r#"agentdesk.routines.register({
          name: "DM",
          tick(ctx) { return { action: "agent", prompt: "brief", dmUserId: "4242" }; }
        });"#,
    )
    .unwrap();
    let loader = RoutineScriptLoader::new().unwrap();
    let script_ref = loader.load_script(temp.path(), &script_path).unwrap();
    let store = store(&pool);
    let legacy = store
        .attach_routine(routine(&script_ref, "fresh", None))
        .await
        .unwrap();
    let explicit = store
        .attach_routine_with_execution(
            routine(&script_ref, "fresh", None),
            Some(&thread_definition(&script_ref)),
            3,
        )
        .await
        .unwrap();
    // No provider is registered, so an admitted agent start defers instead of launching.
    let executor = RoutineAgentExecutor::new(
        Arc::new(pool.clone()),
        Some(Arc::new(HealthRegistry::new())),
        60,
    );
    let dirs = [temp.path().to_path_buf()];

    let outcomes = run_due_tick(&store, &loader, &dirs, Some(&executor), None, 10, false)
        .await
        .unwrap();
    let status_of = |routine_id: &str| {
        let outcome = outcomes
            .iter()
            .find(|outcome| outcome.routine_id == routine_id)
            .expect("outcome per routine");
        (outcome.status.clone(), outcome.error.clone())
    };
    assert_eq!(status_of(&legacy.id).0, "deferred", "{outcomes:?}");
    let (status, error) = status_of(&explicit.id);
    assert_eq!(status, "failed", "{outcomes:?}");
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("dmUserId")),
        "{error:?}"
    );

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn definition_constraints_reject_writes_that_race_past_the_api_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let store = store(&pool);
    let explicit = store
        .attach_routine_with_execution(
            routine("explicit.js", "persistent", None),
            Some(&thread_definition("explicit.js")),
            3,
        )
        .await
        .unwrap();
    assert_eq!(explicit.execution_strategy, "fresh");

    let error = store
        .patch_routine(
            &explicit.id,
            RoutinePatch {
                execution_strategy: Some("persistent".into()),
                ..RoutinePatch::default()
            },
        )
        .await
        .expect_err("explicit mode stays fresh");
    assert!(is_definition_constraint_violation(&error), "{error}");
    let error = store
        .patch_routine(
            &explicit.id,
            RoutinePatch {
                headless_failure_notice_limit: Some(0),
                ..RoutinePatch::default()
            },
        )
        .await
        .expect_err("limit below range");
    assert!(is_definition_constraint_violation(&error), "{error}");

    // A client that predates the new columns still gets the null-mode contract and N = 3.
    sqlx::query(
        "INSERT INTO routines (id, script_ref, name, execution_strategy)
         VALUES ('old-client', 'old.js', 'old', 'fresh')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let projection = store
        .load_execution_definitions(&["old-client".to_string()])
        .await
        .unwrap()
        .remove("old-client")
        .expect("old row")
        .projection();
    assert_eq!(
        (
            projection.execution_mode,
            projection.effective_execution_mode,
            projection.execution_compatibility,
            projection.headless_failure_notice_limit,
        ),
        (None, Some("thread"), "legacy_fresh", 3)
    );

    // The runner CHECK must refuse rows whose kind or mode is missing, not let them pass as UNKNOWN.
    let cases = [
        ("{}", None, false),
        (r#"{"kind": null}"#, None, false),
        (r#"{"kind": "command"}"#, None, false),
        (r#"{"kind": "command"}"#, Some("thread"), false),
        (r#"{"kind": "quickjs"}"#, Some("thread"), true),
        (r#"{"kind": "command"}"#, Some("script"), true),
    ];
    for (index, (runner, mode, accepted)) in cases.into_iter().enumerate() {
        let inserted = sqlx::query(
            "INSERT INTO routines (id, script_ref, name, execution_strategy, execution_mode, runner)
             VALUES ($1, 'm.js', 'm', 'fresh', $2, $3::jsonb)",
        )
        .bind(format!("runner-{index}"))
        .bind(mode)
        .bind(runner)
        .execute(&pool)
        .await;
        match inserted {
            Ok(_) => assert!(accepted, "{runner} / {mode:?} must be refused"),
            Err(error) => {
                assert!(!accepted, "{runner} / {mode:?}: {error}");
                assert!(
                    error.to_string().contains("routines_runner_shape_check"),
                    "{error}"
                );
            }
        }
    }

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_cleanup_reads_the_routine_as_its_run_was_claimed_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let store = store(&pool);
    let claim = || {
        let store = store.clone();
        async move {
            store
                .claim_due_runs(1)
                .await
                .unwrap()
                .pop()
                .expect("due run")
        }
    };

    // persistent -> fresh during the run: cleanup still sees the persistent run and skips it.
    let persistent = store
        .attach_routine(routine("persistent.js", "persistent", Some("1001")))
        .await
        .unwrap();
    let persistent_run = claim().await;
    let fresh_patch = RoutinePatch {
        execution_strategy: Some("fresh".into()),
        ..RoutinePatch::default()
    };
    store
        .patch_routine(&persistent.id, fresh_patch)
        .await
        .unwrap();

    // fresh thread A -> B during the run: cleanup still targets A.
    let fresh = store
        .attach_routine(routine("fresh.js", "fresh", Some("2001")))
        .await
        .unwrap();
    let fresh_run = claim().await;
    let retarget = |thread: &str| RoutinePatch {
        discord_thread_id: Some(Some(thread.into())),
        ..RoutinePatch::default()
    };
    store
        .patch_routine(&fresh.id, retarget("2002"))
        .await
        .unwrap();

    // A run claimed before snapshots existed keeps following the live row.
    let legacy = store
        .attach_routine(routine("legacy.js", "fresh", Some("3001")))
        .await
        .unwrap();
    let legacy_run = claim().await;
    sqlx::query("UPDATE routine_runs SET run_spec = NULL WHERE id = $1")
        .bind(&legacy_run.run_id)
        .execute(&pool)
        .await
        .unwrap();
    store
        .patch_routine(&legacy.id, retarget("3002"))
        .await
        .unwrap();

    let as_claimed = |routine_id: String, run_id: String| {
        let store = store.clone();
        async move {
            let routine = store
                .routine_as_claimed(&routine_id, &run_id)
                .await
                .unwrap()
                .expect("routine");
            (routine.execution_strategy, routine.discord_thread_id)
        }
    };
    let expected = [
        (&persistent.id, &persistent_run.run_id, "persistent", "1001"),
        (&fresh.id, &fresh_run.run_id, "fresh", "2001"),
        (&legacy.id, &legacy_run.run_id, "fresh", "3002"),
    ];
    for (routine_id, run_id, strategy, thread) in expected {
        assert_eq!(
            as_claimed(routine_id.clone(), run_id.clone()).await,
            (strategy.to_string(), Some(thread.to_string())),
            "{routine_id}"
        );
    }

    // Boot recovery reads the same claim-time definition after closing the expired runs.
    sqlx::query("UPDATE routine_runs SET lease_expires_at = NOW() - INTERVAL '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let recovered = store.recover_stale_running_runs().await.unwrap();
    assert_eq!(recovered.len(), 3, "{recovered:?}");
    for (routine_id, run_id, strategy, thread) in expected {
        let row = recovered
            .iter()
            .find(|row| &row.run_id == run_id)
            .expect("recovered run");
        assert_eq!(
            (
                row.execution_strategy.as_str(),
                row.discord_thread_id.as_deref()
            ),
            (strategy, Some(thread)),
            "{routine_id}"
        );
        assert_eq!(
            as_claimed(routine_id.clone(), run_id.clone()).await,
            (strategy.to_string(), Some(thread.to_string()))
        );
    }

    pool.close().await;
    db.drop().await;
}
