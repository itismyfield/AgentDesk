//! A routine's pinned provider and model through the real start: the chosen channel, never
//! another provider's, and a mid-run PATCH that waits for the next claim.
use super::agent_executor_owner_input::settled_generation;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::{HealthRegistry, register_bot_auth_for_tests};
use crate::services::engine_runtime::health_port::owner_input::OwnerInput;
use crate::services::routines::agent_executor::owner_hook_tests::OWNER;
use crate::services::routines::agent_executor::*;
use crate::services::routines::provider_choice::ProviderChoice;
use crate::services::routines::runtime::RoutineRunOutcome;
use crate::services::routines::store::RoutineStore;
use crate::services::routines::store::{NewRoutine, RoutinePatch};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use serde_json::{Value, json};
use std::sync::Arc;

/// Agents as (id, provider, primary, cc, cdx) channel columns.
type Agent<'a> = (
    &'a str,
    &'a str,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
);

async fn setup(agents: &[Agent<'_>]) -> (TestPostgresDb, sqlx::PgPool, RoutineStore) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (id, provider, primary, cc, cdx) in agents {
        sqlx::query(
            "INSERT INTO agents(id,name,provider,discord_channel_id,discord_channel_cc,discord_channel_cdx)
             VALUES ($1,$1,$2,$3,$4,$5)",
        )
        .bind(id)
        .bind(provider)
        .bind(primary)
        .bind(cc)
        .bind(cdx)
        .execute(&pool)
        .await
        .unwrap();
    }
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 262144);
    (db, pool, store)
}

async fn attach(
    store: &RoutineStore,
    agent: &str,
    strategy: &str,
    choice: ProviderChoice,
) -> String {
    let routine = NewRoutine {
        agent_id: Some(agent.into()),
        fallback_agent_id: None,
        max_retries: Some(0),
        script_ref: format!("{strategy}.js"),
        name: strategy.into(),
        status: None,
        execution_strategy: strategy.into(),
        schedule: None,
        next_due_at: None,
        checkpoint: None,
        discord_thread_id: None,
        timeout_secs: Some(1),
    };
    let routine = store.attach_routine_with_choice(routine, None, 3, &choice);
    routine.await.unwrap().id
}

fn choice(provider: &str, model: Option<&str>) -> ProviderChoice {
    ProviderChoice {
        provider: Some(provider.into()),
        model: model.map(str::to_string),
    }
}

async fn start(
    executor: &RoutineAgentExecutor,
    store: &RoutineStore,
    id: &str,
) -> RoutineRunOutcome {
    let claimed = store.claim_run_now(id).await.unwrap().unwrap();
    let run =
        executor.start_agent_run(store, claimed, "work".into(), None, None, None, None, false);
    run.await.unwrap()
}

#[tokio::test]
async fn a_chosen_provider_and_model_reach_the_launch_on_that_providers_channel_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    settled_generation();
    let handle =
        ChannelMailboxRegistry::default().handle(poise::serenity_prelude::ChannelId::new(7272));
    let queue = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    handle
        .v2_adopt(queue.join("routine-tests.json"), "routine-tests".into())
        .await
        .unwrap();
    let (db, pool, store) = setup(&[("pc", "codex", None, Some("7272"), Some("7270"))]).await;
    let registry = Arc::new(HealthRegistry::new());
    register_bot_auth_for_tests(&registry, "claude", 7272, None, Vec::new(), false).await;
    register_bot_auth_for_tests(&registry, "codex", 7270, None, Vec::new(), false).await;
    let id = attach(
        &store,
        "pc",
        "persistent",
        choice("claude", Some("claude-test")),
    )
    .await;
    let claimed = store.claim_run_now(&id).await.unwrap().unwrap();
    // No bot can prepare a thread here; a run without the fallback pin starts on the route channel.
    sqlx::query("UPDATE routine_runs SET run_spec = run_spec - 'primary_fallback'")
        .execute(&pool)
        .await
        .unwrap();
    let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), Some(registry), 60);
    let run = executor.start_agent_run(
        &store,
        claimed,
        "work".into(),
        None,
        None,
        None,
        None,
        false,
    );
    let result = OWNER.scope(handle.clone(), run).await.unwrap();
    assert_eq!(result.status, "running", "{result:?}");
    let run = store
        .list_running_agent_runs(10)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let stored = run.result_json.unwrap();
    let saved = OwnerInput::restore(stored["owner_input"].clone()).unwrap();
    let original = serde_json::from_value(saved.original.clone()).unwrap();
    let context = handle
        .v2_admission_context(original, saved.ticket.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context["provider"], "claude", "{context}");
    assert_eq!(context["parent_channel_id"], "7272", "{context}");
    assert_eq!(context["metadata"]["model"], "claude-test", "{context}");
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_provider_the_agent_has_no_channel_for_fails_the_run_instead_of_running_elsewhere_pg() {
    let (db, pool, store) = setup(&[("solo", "claude", None, Some("7272"), None)]).await;
    let registry = Arc::new(HealthRegistry::new());
    register_bot_auth_for_tests(&registry, "claude", 7272, None, Vec::new(), false).await;
    let id = attach(&store, "solo", "persistent", ProviderChoice::default()).await;
    // The agent lost its codex channel after the choice was stored.
    sqlx::query("UPDATE routines SET provider = 'codex'")
        .execute(&pool)
        .await
        .unwrap();
    let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), Some(registry), 60);
    let outcome = start(&executor, &store, &id).await;
    assert_eq!(outcome.status, "failed", "{outcome:?}");
    let error = outcome.error.unwrap_or_default();
    assert!(error.contains("has no codex provider channel"), "{error}");
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_patch_applies_from_the_next_claim_and_an_unset_choice_keeps_the_primary_pg() {
    let pc = ("pc", "claude", None, Some("7270"), Some("7271"));
    let (db, pool, store) =
        setup(&[pc, ("other", "claude", None, Some("7280"), Some("7281"))]).await;
    let executor = RoutineAgentExecutor::new(
        Arc::new(pool.clone()),
        Some(Arc::new(HealthRegistry::new())),
        60,
    );
    let id = attach(&store, "pc", "persistent", ProviderChoice::default()).await;
    let in_flight = store.claim_run_now(&id).await.unwrap().unwrap();
    let patch = RoutinePatch {
        agent_id: Some("other".into()),
        provider_choice: Some(choice("codex", Some("gpt-next"))),
        ..RoutinePatch::default()
    };
    store.patch_routine(&id, patch).await.unwrap().unwrap();
    let run = executor.start_agent_run(
        &store,
        in_flight,
        "work".into(),
        None,
        None,
        None,
        None,
        false,
    );
    let outcome = run.await.unwrap();
    let result = outcome.result_json.unwrap();
    assert_eq!(outcome.status, "deferred", "{result}");
    assert_eq!(result["error"], "provider runtime not registered: claude");
    assert_eq!(result["agent_id"], "pc");
    sqlx::query("UPDATE routine_runs SET status = 'succeeded', finished_at = NOW()")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE routines SET in_flight_run_id = NULL")
        .execute(&pool)
        .await
        .unwrap();
    let outcome = start(&executor, &store, &id).await;
    let result = outcome.result_json.unwrap();
    assert_eq!(
        result["error"], "provider runtime not registered: codex",
        "{result}"
    );
    assert_eq!(result["agent_id"], "other");
    let spec: Value =
        sqlx::query_scalar("SELECT run_spec FROM routine_runs WHERE status = 'running'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (&spec["provider"], &spec["model"]),
        (&json!("codex"), &json!("gpt-next"))
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_fresh_routine_on_a_provider_without_process_turns_starts_a_channel_session_pg() {
    let (db, pool, store) = setup(&[("ag", "antigravity", Some("7290"), None, None)]).await;
    let registry = Some(Arc::new(HealthRegistry::new()));
    let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), registry, 60)
        .with_headless_fresh_agents(true);
    let id = attach(&store, "ag", "fresh", ProviderChoice::default()).await;
    let outcome = start(&executor, &store, &id).await;
    let result = outcome.result_json.unwrap();
    // Past the process check: only the unregistered runtime holds it back, as for any provider.
    assert_eq!(outcome.status, "deferred", "{result}");
    assert_eq!(
        result["error"],
        "provider runtime not registered: antigravity"
    );
    pool.close().await;
    db.drop().await;
}
#[tokio::test(flavor = "current_thread")]
async fn a_late_old_run_thread_stays_with_its_claim_after_provider_patch_pg() {
    let (db, pool, store) = setup(&[("pc", "claude", None, Some("7270"), Some("7271"))]).await;
    let id = attach(&store, "pc", "persistent", ProviderChoice::default()).await;
    let claimed = store.claim_run_now(&id).await.unwrap().unwrap();
    store
        .patch_routine(
            &id,
            RoutinePatch {
                provider_choice: Some(choice("codex", None)),
                ..RoutinePatch::default()
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .update_run_discord_thread_id(&claimed.run_id, "7279")
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .get_routine(&id)
            .await
            .unwrap()
            .unwrap()
            .discord_thread_id,
        None
    );
    assert_eq!(
        store
            .routine_as_claimed(&id, &claimed.run_id)
            .await
            .unwrap()
            .unwrap()
            .discord_thread_id
            .as_deref(),
        Some("7279")
    );
    pool.close().await;
    db.drop().await;
}
