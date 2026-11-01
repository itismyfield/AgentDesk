//! A routine start through the real launch: the run row it plans reaches the owner as the
//! admitted context, so a persistent routine stays persistent and a fresh one fresh.
use super::*;
use crate::services::discord::health::{HealthRegistry, register_bot_auth_for_tests};
use crate::services::turn_orchestrator::ChannelMailboxHandle;

/// The context the owner holds for a routine of `strategy` that the launch handed it.
async fn admitted_context(strategy: &str) -> Value {
    let _root = crate::config::TestRuntimeRootGuard::new();
    settled_generation();
    let (handle, _) = owner().await;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_cc) VALUES('primary','Primary','claude','7272')")
        .execute(&pool)
        .await
        .unwrap();
    let registry = Arc::new(HealthRegistry::new());
    register_bot_auth_for_tests(&registry, "claude", 7272, None, Vec::new(), false).await;
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 262144);
    store
        .attach_routine(NewRoutine {
            agent_id: Some("primary".into()),
            fallback_agent_id: None,
            max_retries: Some(0),
            script_ref: format!("{strategy}.js"),
            name: strategy.into(),
            status: None,
            execution_strategy: strategy.into(),
            schedule: None,
            next_due_at: Some(Utc::now() - Duration::minutes(1)),
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: Some(1),
        })
        .await
        .unwrap();
    let claimed = store.claim_due_runs(1).await.unwrap().pop().unwrap();
    let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), Some(registry), 60);
    let start = executor.start_agent_run(
        &store,
        claimed,
        "work".into(),
        None,
        None,
        None,
        None,
        false,
    );
    let result = OWNER.scope(handle.clone(), start).await.unwrap();
    assert_eq!(result.status, "running", "{result:?}");
    let run = store
        .list_running_agent_runs(10)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let stored = run.result_json.as_ref().unwrap();
    assert_eq!(stored["status"], "accepted", "{stored}");
    let saved = ChannelMailboxHandle::restore_owner_input(stored["owner_input"].clone()).unwrap();
    let original = serde_json::from_value(saved.original.clone()).unwrap();
    let context = handle.v2_admission_context(original, saved.ticket.clone());
    let context = context.await.unwrap().expect("admitted context");
    pool.close().await;
    db.drop().await;
    context
}

#[tokio::test]
async fn a_persistent_routine_reaches_the_owner_persistent_and_a_fresh_one_fresh_pg() {
    let context = admitted_context("persistent").await;
    // What the legacy start would be given, so the owner's start resolves the same way.
    assert_eq!(context["source"], "routine", "{context}");
    assert!(context["channel_name_hint"].is_string(), "{context}");
    assert!(context["synthetic_label"].is_string(), "{context}");
    let persistent = context["metadata"].clone();
    assert_eq!(
        persistent["execution_strategy"], "persistent",
        "{persistent}"
    );
    assert!(persistent["routine_id"].is_string(), "{persistent}");
    let fresh = admitted_context("fresh").await["metadata"].clone();
    assert_eq!(fresh["execution_strategy"], "fresh", "{fresh}");
}
