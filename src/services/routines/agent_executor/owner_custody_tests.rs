use super::*;
use crate::services::discord::runtime_store::durable_probe;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

async fn cutpoint(pending_owner: bool) {
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
            script_ref: "intent.js".into(),
            name: "intent".into(),
            status: None,
            execution_strategy: "persistent".into(),
            schedule: None,
            next_due_at: Some(Utc::now() - Duration::minutes(1)),
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: Some(1),
        })
        .await
        .unwrap();
    let claimed = store.claim_due_runs(1).await.unwrap().pop().unwrap();
    let turn = "discord:7272:8000000000000002121";
    let original=serde_json::from_value(json!({"author_id":1,"author_is_bot":true,"message_id":8000000000000002121u64,"channel_id":7272,"queued_generation":5,"text":"intent original"})).unwrap();
    let context = json!({"status":"input_pending","run_id":claimed.run_id,"routine_id":claimed.routine_id,"turn_id":turn,"agent_id":"primary","provider":"claude","execution_mode":"tmux","attempt_kind":"primary","prompt":"intent original","script_ref":"intent.js","fresh_context_guaranteed":false});
    let input = ChannelMailboxHandle::new_owner_input(
        &original,
        "routine-tests",
        "claude",
        context.clone(),
    );
    let mut result = context;
    result["owner_input"] = serde_json::to_value(&input).unwrap();
    assert!(
        store
            .record_agent_input_intent(&claimed.run_id, None, turn, &result)
            .await
            .unwrap()
    );
    if pending_owner {
        let fault = durable_probe::Watch::new(
            &path,
            durable_probe::Plan {
                fail: vec![durable_probe::Step::ParentSync],
                ..Default::default()
            },
        );
        assert!(!input.confirm_admission(&handle).await.unwrap());
        drop(fault);
    }
    drop(handle);
    let restarted = crate::services::turn_orchestrator::ChannelMailboxRegistry::default()
        .handle(poise::serenity_prelude::ChannelId::new(7272));
    restarted
        .v2_adopt(path.clone(), "routine-tests".into())
        .await
        .unwrap();
    sqlx::query("INSERT INTO session_transcripts(turn_id,assistant_message) VALUES($1,'unrelated old completion')").bind(turn).execute(&pool).await.unwrap();
    sqlx::query("UPDATE routine_runs SET started_at=NOW()-INTERVAL '2 hours',next_retry_at=NOW()-INTERVAL '1 second',lease_expires_at=NOW()-INTERVAL '1 second' WHERE id=$1").bind(&claimed.run_id).execute(&pool).await.unwrap();
    let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), None, 1);
    OWNER
        .scope(
            restarted,
            STARTS.scope(RefCell::new(VecDeque::new()), async {
                assert!(
                    executor
                        .poll_agent_runs(&store, 10, false)
                        .await
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    executor
                        .poll_agent_runs(&store, 10, false)
                        .await
                        .unwrap()
                        .is_empty()
                );
            }),
        )
        .await;
    let (status,finished,renewed,saved):(String,Option<DateTime<Utc>>,bool,Value)=sqlx::query_as("SELECT status,finished_at,lease_expires_at>NOW(),result_json FROM routine_runs WHERE id=$1").bind(&claimed.run_id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "running");
    assert!(finished.is_none() && renewed);
    assert_eq!(saved["owner_input"]["ticket"], input.ticket);
    assert_eq!(saved["status"], "accepted");
    assert!(
        !store
            .project_agent_input(&claimed.run_id, "wrong-turn", &input.ticket, &saved)
            .await
            .unwrap()
    );
    assert!(
        !store
            .project_agent_input(&claimed.run_id, turn, "wrong-ticket", &saved)
            .await
            .unwrap()
    );
    let mut foreign = saved.clone();
    foreign["owner_input"]["channel"] = json!(7273);
    assert!(
        !store
            .project_agent_input(&claimed.run_id, turn, &input.ticket, &foreign)
            .await
            .unwrap()
    );
    let snapshot: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(snapshot["admission_sequence"], 1);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn intent_only_restart_recovers_original_ticket_before_completion_or_retry_pg() {
    cutpoint(false).await;
}
#[tokio::test]
async fn pending_owner_restart_recovers_original_ticket_before_completion_or_retry_pg() {
    cutpoint(true).await;
}
