//! Headless failure streaks against PostgreSQL: N notices, one N+1 command-bot incident and the
//! keys that keep re-logs, restarts and concurrent closes from adding or dropping one.
use std::sync::Arc;

use chrono::{Duration, Utc};
use serde_json::json;
use sqlx::PgPool;

use super::RoutineDiscordLogger;
use super::execution_mode::{ExecutionDefinition, RoutineExecutionMode};
use super::runtime::RoutineRunOutcome;
use super::store::{NewRoutine, RoutinePatch, RoutineStore};
use crate::db::auto_queue::test_support::TestPostgresDb;

fn store(pool: &PgPool) -> RoutineStore {
    RoutineStore::new_with_timezone_and_checkpoint_limit(
        Arc::new(pool.clone()),
        "Asia/Seoul",
        256 * 1024,
    )
}

fn routine(name: &str) -> NewRoutine {
    NewRoutine {
        agent_id: Some("boot-agent".into()),
        fallback_agent_id: None,
        max_retries: Some(0),
        script_ref: format!("{name}.js"),
        name: name.into(),
        status: None,
        execution_strategy: "fresh".into(),
        schedule: None,
        next_due_at: Some(Utc::now() - Duration::minutes(1)),
        checkpoint: None,
        discord_thread_id: None,
        timeout_secs: None,
    }
}

async fn setup() -> (TestPostgresDb, PgPool, RoutineStore) {
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
    (db, pool, store)
}

async fn headless(store: &RoutineStore, name: &str, limit: i32) -> String {
    let definition = ExecutionDefinition {
        mode: RoutineExecutionMode::Headless,
        runner: json!({"kind": "quickjs", "script_ref": format!("{name}.js")}),
    };
    store
        .attach_routine_with_execution(routine(name), Some(&definition), limit)
        .await
        .unwrap()
        .id
}

async fn claim(store: &RoutineStore) -> String {
    store
        .claim_due_runs(1)
        .await
        .unwrap()
        .pop()
        .expect("due run")
        .run_id
}

/// Fails the run and makes the routine due again.
async fn fail(store: &RoutineStore, run_id: &str) -> bool {
    store
        .fail_run(
            run_id,
            "boom",
            Some(json!({"status": "timeout"})),
            Some(Utc::now() - Duration::minutes(1)),
        )
        .await
        .unwrap()
}

async fn claim_and_fail(store: &RoutineStore) -> String {
    let run_id = claim(store).await;
    assert!(fail(store, &run_id).await);
    run_id
}

/// `(target, bot, reason, session_key)` of every routine-runtime outbox row, oldest first.
async fn rows(pool: &PgPool) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT target, bot, reason_code, session_key FROM message_outbox
         WHERE source = 'routine-runtime' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn row(reason: &str, key: String) -> (String, String, String, String) {
    (
        "agent:boot-agent".into(),
        "announce".into(),
        reason.into(),
        key,
    )
}

const NOTICE: &str = "routine_headless_failure";
const INCIDENT: &str = "routine_failure_incident";

#[tokio::test(flavor = "current_thread")]
async fn headless_streak_notifies_n_times_then_one_incident_then_merged_notices_pg() {
    let (db, pool, store) = setup().await;
    let id = headless(&store, "quiet", 3).await;
    let logger = RoutineDiscordLogger::new_with_health_registry(Arc::new(pool.clone()), None);

    // Two pollers closing the same run store one failure and one event.
    let first = claim(&store).await;
    let (a, b) = tokio::join!(fail(&store, &first), fail(&store, &first));
    assert_eq!(u8::from(a) + u8::from(b), 1);
    let mut runs = vec![first.clone()];
    for _ in 0..5 {
        runs.push(claim_and_fail(&store).await);
    }
    // Re-logging the N+1 outcome and re-closing it add nothing.
    let outcome = RoutineRunOutcome {
        run_id: runs[3].clone(),
        routine_id: id.clone(),
        script_ref: "quiet.js".into(),
        action: "agent".into(),
        status: "failed".into(),
        result_json: None,
        error: Some("boom".into()),
        fresh_context_guaranteed: false,
    };
    logger.log_run_outcome(&store, &outcome).await;
    logger.alert_consecutive_failures(&store, &outcome, 3).await;
    assert!(!fail(&store, &runs[3]).await);

    let key = |kind: &str, run: &str| format!("routine:{id}:{kind}:{run}");
    assert_eq!(
        rows(&pool).await,
        [
            row(NOTICE, key("headless_failure", &runs[0])),
            row(NOTICE, key("headless_failure", &runs[1])),
            row(NOTICE, key("headless_failure", &runs[2])),
            row(INCIDENT, key("failure_incident", &first)),
            row(NOTICE, key("after_incident", &first)),
        ]
    );
    let thread: Option<String> =
        sqlx::query_scalar("SELECT discord_thread_id FROM routines WHERE id = $1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(thread, None, "headless never creates a routine thread");

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn success_resets_interrupted_is_neutral_and_n_stays_pinned_for_the_streak_pg() {
    let (db, pool, store) = setup().await;
    let id = headless(&store, "pinned", 1).await;
    let first = claim_and_fail(&store).await;
    // N edited mid-streak applies to the next streak only.
    let limit_three = RoutinePatch {
        headless_failure_notice_limit: Some(3),
        ..RoutinePatch::default()
    };
    store.patch_routine(&id, limit_three).await.unwrap();
    // A run lost to a restart neither extends nor resets the streak.
    let lost = claim(&store).await;
    sqlx::query(
        "UPDATE routine_runs SET lease_expires_at = NOW() - INTERVAL '1 second' WHERE id = $1",
    )
    .bind(&lost)
    .execute(&pool)
    .await
    .unwrap();
    store.recover_stale_running_runs().await.unwrap();
    sqlx::query("UPDATE routines SET next_due_at = NOW() - INTERVAL '1 minute' WHERE id = $1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    // Second failure of a streak pinned to N = 1: the incident.
    claim_and_fail(&store).await;
    // A success resets; the next streak uses the new N = 3.
    let ok = claim(&store).await;
    let due_again = Some(Utc::now() - Duration::minutes(1));
    assert!(
        store
            .finish_run(&ok, None, None, None, due_again)
            .await
            .unwrap()
    );
    let next_first = claim_and_fail(&store).await;
    let next_second = claim_and_fail(&store).await;

    let key = |kind: &str, run: &str| format!("routine:{id}:{kind}:{run}");
    assert_eq!(
        rows(&pool).await,
        [
            row(NOTICE, key("headless_failure", &first)),
            row(INCIDENT, key("failure_incident", &first)),
            row(NOTICE, key("headless_failure", &next_first)),
            row(NOTICE, key("headless_failure", &next_second)),
        ]
    );

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn thread_mode_runs_reserve_no_headless_events_and_headless_stalls_go_primary_pg() {
    let (db, pool, store) = setup().await;
    // Today's routines are null-mode thread runs: no headless event, whatever the streak.
    store.attach_routine(routine("legacy")).await.unwrap();
    for _ in 0..5 {
        claim_and_fail(&store).await;
    }
    assert_eq!(rows(&pool).await, []);

    // A paused headless routine's stall is announced on the agent's primary channel.
    let id = headless(&store, "stalled", 3).await;
    sqlx::query("UPDATE routines SET status = 'paused', next_due_at = NULL WHERE id = $1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let paused = store.get_routine(&id).await.unwrap().expect("routine");
    let logger = RoutineDiscordLogger::new_with_health_registry(Arc::new(pool.clone()), None);
    let status = logger.log_stale_paused(&store, &paused, 7200, 86_400).await;
    assert_eq!(status.status, "ok", "{status:?}");
    assert_eq!(
        rows(&pool).await,
        [row(
            "routine_paused_stale",
            format!("routine:{id}:paused_stale")
        )]
    );

    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_headless_routine_logs_no_lifecycle_event_and_no_census_verdict_pg() {
    use super::RoutineLifecycleEvent;
    let (db, pool, store) = setup().await;
    let logger = RoutineDiscordLogger::new_with_health_registry(Arc::new(pool.clone()), None);
    let census = super::legacy_body_census::SCRIPT_REF;
    let definition = ExecutionDefinition {
        mode: RoutineExecutionMode::Headless,
        runner: json!({"kind": "quickjs", "script_ref": census}),
    };
    let mut seen = Vec::new();
    // The same census routine: headless, then null-mode with and without an agent as controls.
    // The agentless control resolves no target, so its verdict is recorded but never sent.
    for (name, definition, agent) in [
        ("quiet", Some(&definition), true),
        ("threaded", None, true),
        ("agentless", None, false),
    ] {
        let mut row = routine(name);
        row.script_ref = census.to_string();
        row.agent_id = agent.then(|| "boot-agent".to_string());
        let attached = store.attach_routine_with_execution(row, definition, 3);
        let attached = attached.await.unwrap();
        let lifecycle = logger
            .log_routine_event_with_store(&store, &attached, RoutineLifecycleEvent::Attached)
            .await;
        let run_id = claim(&store).await;
        store.pause_routine(&attached.id).await.unwrap();
        if name != "threaded" {
            let outcome = RoutineRunOutcome {
                run_id: run_id.clone(),
                routine_id: attached.id.clone(),
                script_ref: census.to_string(),
                action: "complete".into(),
                status: "succeeded".into(),
                result_json: None,
                error: None,
                fresh_context_guaranteed: false,
            };
            logger.log_run_outcome(&store, &outcome).await;
        }
        let posts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM message_outbox WHERE session_key LIKE 'routine:' || $1 || ':%'",
        )
        .bind(&attached.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let sections: String =
            sqlx::query_scalar("SELECT discord_log_sections::text FROM routine_runs WHERE id = $1")
                .bind(&run_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        seen.push((lifecycle.status, posts, sections.contains("public_result")));
    }
    assert_eq!(seen[0], ("skipped".to_string(), 0, false), "headless");
    assert_eq!(
        (seen[1].0.as_str(), seen[1].1),
        ("ok", 1),
        "null-mode lifecycle"
    );
    assert!(seen[2].2, "a null-mode census run records its verdict");

    pool.close().await;
    db.drop().await;
}
