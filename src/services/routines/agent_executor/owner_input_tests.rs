use super::super::store::NewRoutine;
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use std::cell::RefCell;
use std::collections::VecDeque;

tokio::task_local! {
    static STARTS: RefCell<VecDeque<bool>>;
    pub(super) static OWNER: crate::services::turn_orchestrator::ChannelMailboxHandle;
    pub(super) static LEGACY_TEARDOWNS: std::cell::Cell<usize>;
}

#[cfg(unix)]
pub(super) async fn inject(
    executor: &RoutineAgentExecutor,
    store: &RoutineStore,
    start: &start_deferral::PendingAgentStart<'_>,
) -> Option<Result<StartedAgentTurn>> {
    let step = STARTS
        .try_with(|steps| {
            steps
                .borrow_mut()
                .pop_front()
                .expect("unexpected readmission")
        })
        .ok()?;
    if !step {
        return Some(Err(anyhow!("pre-admission refusal")));
    }
    settled_generation();
    let reservation = reserve_headless_agent_turn(poise::serenity_prelude::ChannelId::new(7272));
    let turn_id = reservation.turn_id();
    let claimed = start.claimed;
    let planned = json!({"status":"started","turn_id":turn_id,"run_id":claimed.run_id,"routine_id":claimed.routine_id,
        "agent_id":start.agent_id,"attempt_kind":start.attempt_kind,"provider":"claude","execution_mode":start.execution_mode.as_str(),
        "channel_id":"7272","parent_channel_id":"7272","discord_thread_id":claimed.discord_thread_id,"dm_user_id":start.dm_user_id,
        "is_dm":start.dm_user_id.is_some(),"prompt":start.prompt,"checkpoint":start.checkpoint,
        "next_due_at":start.next_due_at,"script_ref":claimed.script_ref,"fresh_context_guaranteed":false});
    let owned = owner_input::custody::owned(executor, store, start, &planned).await;
    Some(owned.and_then(|started| started.ok_or_else(|| anyhow!("owner left the start"))))
}

/// The isolated runtime root's process generation, so a synthetic original is admissible.
#[cfg(unix)]
pub(super) fn settled_generation() {
    let path = crate::services::discord::runtime_store::generation_path().unwrap();
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "5").unwrap();
    }
}

async fn owner() -> (
    crate::services::turn_orchestrator::ChannelMailboxHandle,
    std::path::PathBuf,
) {
    let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let handle = registry.handle(poise::serenity_prelude::ChannelId::new(7272));
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("routine-tests.json");
    handle
        .v2_adopt(path.clone(), "routine-tests".into())
        .await
        .unwrap();
    (handle, path)
}

async fn scenario(retry: bool, fallback: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (handle, path) = owner().await;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_cc) VALUES('primary','Primary','claude','7272'),('fallback','Fallback','claude','7273')").execute(&pool).await.unwrap();
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 262144);
    store
        .attach_routine(NewRoutine {
            agent_id: Some("primary".into()),
            fallback_agent_id: Some("fallback".into()),
            max_retries: Some(0),
            script_ref: "accepted.js".into(),
            name: "accepted".into(),
            status: None,
            execution_strategy: "fresh".into(),
            schedule: None,
            next_due_at: Some(Utc::now() - Duration::minutes(1)),
            checkpoint: Some(json!({"old":true})),
            discord_thread_id: None,
            timeout_secs: Some(1),
        })
        .await
        .unwrap();
    let claimed = store.claim_due_runs(1).await.unwrap().pop().unwrap();
    let run_id = claimed.run_id.clone();
    let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), None, 1);
    let mut steps = VecDeque::new();
    if fallback {
        steps.push_back(false);
    }
    steps.push_back(true);
    OWNER
        .scope(
            handle,
            STARTS.scope(RefCell::new(steps), async {
                if retry {
                    store.schedule_agent_retry(&run_id, Utc::now()-Duration::seconds(1),
                Some(json!({"status":"retry_scheduled","prompt":"work","checkpoint":{"new":true}})),
                "previous failure",Some("primary"),"primary").await.unwrap();
                    let result = executor.poll_agent_runs(&store, 10, false).await.unwrap();
                    assert_eq!(result.len(), 1);
                    assert_eq!(result[0].status, "running");
                } else {
                    let result = executor
                        .start_agent_run(
                            &store,
                            claimed,
                            "work".into(),
                            None,
                            Some(json!({"new":true})),
                            None,
                            None,
                            false,
                        )
                        .await
                        .unwrap();
                    assert_eq!(result.status, "running");
                }
                assert!(STARTS.with(|s| s.borrow().is_empty()));
            }),
        )
        .await;
    let row: (String, Option<chrono::DateTime<Utc>>, Option<String>) = sqlx::query_as(
        "SELECT status,finished_at,owned_tmux_session FROM routine_runs WHERE id=$1",
    )
    .bind(&run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, ("running".into(), None, None));
    let checkpoint:Option<Value>=sqlx::query_scalar("SELECT checkpoint FROM routines WHERE id=(SELECT routine_id FROM routine_runs WHERE id=$1)")
        .bind(&run_id).fetch_one(&pool).await.unwrap();
    assert_eq!(checkpoint, Some(json!({"old":true})));
    let snapshot: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(snapshot["admission_sequence"], 1);
    if fallback {
        let attempts: Value = sqlx::query_scalar("SELECT attempts FROM routine_runs WHERE id=$1")
            .bind(&run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            attempts_include_fallback(&attempts),
            "owner attempts must preserve fallback metadata"
        );
        assert_eq!(
            attempts.as_array().unwrap().last().unwrap()["agent_id"],
            "fallback"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn accepted_primary_does_not_complete_or_readmit_pg() {
    scenario(false, false).await;
}
#[tokio::test]
async fn accepted_fallback_does_not_complete_or_readmit_pg() {
    scenario(false, true).await;
}
#[tokio::test]
async fn accepted_retry_does_not_complete_or_readmit_pg() {
    scenario(true, false).await;
}
#[tokio::test]
async fn accepted_retry_fallback_does_not_complete_or_readmit_pg() {
    scenario(true, true).await;
}

#[tokio::test]
async fn custody_projection_cut_and_stale_completion_stay_on_the_same_ticket_pg() {
    use crate::services::turn_orchestrator::ChannelMailboxHandle;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (handle, path) = owner().await;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_cc) VALUES('primary','Primary','claude','7272')").execute(&pool).await.unwrap();
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 262144);
    store
        .attach_routine(NewRoutine {
            agent_id: Some("primary".into()),
            fallback_agent_id: None,
            max_retries: Some(3),
            script_ref: "cut.js".into(),
            name: "cut".into(),
            status: None,
            execution_strategy: "fresh".into(),
            schedule: None,
            next_due_at: Some(Utc::now() - Duration::minutes(1)),
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: Some(60),
        })
        .await
        .unwrap();
    let claimed = store.claim_due_runs(1).await.unwrap().pop().unwrap();
    let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), None, 60);
    // The owner commit succeeds while the accepted PG projection is rejected.
    sqlx::query("CREATE FUNCTION reject_accepted() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.result_json->>'status'='accepted' THEN RAISE EXCEPTION 'projection fault'; END IF; RETURN NEW; END $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_accepted BEFORE UPDATE ON routine_runs FOR EACH ROW EXECUTE FUNCTION reject_accepted()").execute(&pool).await.unwrap();
    OWNER
        .scope(
            handle.clone(),
            STARTS.scope(RefCell::new(VecDeque::from([true])), async {
                let result = executor
                    .start_agent_run(
                        &store,
                        claimed.clone(),
                        "raw prompt".into(),
                        None,
                        None,
                        None,
                        None,
                        false,
                    )
                    .await
                    .unwrap();
                assert_eq!(result.status, "running");
            }),
        )
        .await;
    let run = store
        .list_running_agent_runs(10)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(run.result_json.as_ref().unwrap()["status"], "input_pending");
    let saved = ChannelMailboxHandle::restore_owner_input(
        run.result_json.as_ref().unwrap()["owner_input"].clone(),
    )
    .unwrap();
    let turn = run.turn_id.clone().unwrap();
    let original = serde_json::from_value(saved.original.clone()).unwrap();
    let restarted = crate::services::turn_orchestrator::ChannelMailboxRegistry::default()
        .handle(poise::serenity_prelude::ChannelId::new(7272));
    restarted
        .v2_adopt(path.clone(), "routine-tests".into())
        .await
        .unwrap();
    assert_eq!(
        restarted
            .v2_admission_context(original, saved.ticket.clone())
            .await
            .unwrap(),
        Some(saved.context.clone())
    );
    sqlx::query("DROP TRIGGER reject_accepted ON routine_runs")
        .execute(&pool)
        .await
        .unwrap();
    let intent_at = run
        .attempts
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event"] == "input_pending")
        .unwrap()["at"]
        .as_str()
        .unwrap()
        .parse::<DateTime<Utc>>()
        .unwrap();
    sqlx::query("INSERT INTO session_transcripts(turn_id,assistant_message,created_at) VALUES($1,'stale before intent',$2)")
        .bind(&turn).bind(intent_at-Duration::seconds(1)).execute(&pool).await.unwrap();
    sqlx::query("UPDATE routine_runs SET started_at=NOW()-INTERVAL '2 hours',lease_expires_at=NOW()-INTERVAL '1 second',next_retry_at=NOW()-INTERVAL '1 second' WHERE id=$1").bind(&run.run_id).execute(&pool).await.unwrap();
    LEGACY_TEARDOWNS.scope(std::cell::Cell::new(0), OWNER.scope(restarted.clone(),async {
        assert!(executor.poll_agent_runs(&store,10,false).await.unwrap().is_empty());
        let repaired=store.list_running_agent_runs(10).await.unwrap().pop().unwrap();
        assert_eq!(repaired.result_json.as_ref().unwrap()["owner_input"]["ticket"],saved.ticket);
        assert_eq!(repaired.result_json.as_ref().unwrap()["status"],"accepted");
        let renewed:bool=sqlx::query_scalar("SELECT lease_expires_at>NOW() FROM routine_runs WHERE id=$1").bind(&run.run_id).fetch_one(&pool).await.unwrap();assert!(renewed);
        restarted.v2_arm_head_for_tests("actual-attempt").await.unwrap();
        assert!(!owner_input::custody::bind_observer(&store,&repaired,&restarted,"wrong-attempt",None).await.unwrap_or(false));
        assert!(owner_input::custody::bind_observer(&store,&repaired,&restarted,"actual-attempt",None).await.unwrap());
        assert!(executor.poll_agent_runs(&store,10,false).await.unwrap().is_empty(),"pre-bind completion cannot settle the actual attempt");
        sqlx::query("UPDATE session_transcripts SET assistant_message='actual observed answer',created_at=NOW() WHERE turn_id=$1").bind(&turn).execute(&pool).await.unwrap();
        let completed=executor.poll_agent_runs(&store,10,false).await.unwrap();assert_eq!(completed.len(),1);assert_eq!(completed[0].status,"succeeded");
        assert_eq!(LEGACY_TEARDOWNS.with(|count|count.get()),0,"owner input cannot enter legacy session cleanup");
        executor.teardown_fresh_agent_session(&store,&run.routine_id,Some(&json!({"execution_mode":"tmux"})),"legacy cleanup control").await;
        assert_eq!(LEGACY_TEARDOWNS.with(|count|count.get()),1,"the legacy cleanup control reaches the spy");
    })).await;
    let snapshot: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(snapshot["admission_sequence"], 1);
    let newer = json!({"status":"accepted","owner_input":{"ticket":"different"}});
    assert!(
        !store
            .project_agent_input(&run.run_id, &turn, &saved.ticket, &newer)
            .await
            .unwrap()
    );
    pool.close().await;
    db.drop().await;
}

#[path = "owner_custody_tests.rs"]
mod custody_tests;

#[path = "owner_review_tests.rs"]
mod review_tests;
