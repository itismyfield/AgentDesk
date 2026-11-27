use super::*;
use crate::services::discord::health::routine_ports::build_routine_session_controller;
use crate::services::routines::provider_choice::ProviderChoice;
use crate::services::routines::store::{NewRoutine, RoutinePatch, RoutineStore};

#[tokio::test(flavor = "current_thread")]
async fn recovery_target_keeps_the_claimed_provider_after_definition_patch_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_cc,discord_channel_cdx) VALUES('duo','Duo','claude','8219101','8219102')")
        .execute(&pool).await.unwrap();
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 4096);
    let routine = store
        .attach_routine(NewRoutine {
            agent_id: Some("duo".into()),
            fallback_agent_id: None,
            max_retries: Some(0),
            script_ref: "provider.js".into(),
            name: "provider".into(),
            status: None,
            execution_strategy: "fresh".into(),
            schedule: None,
            next_due_at: None,
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: None,
        })
        .await
        .unwrap();
    let claimed = store.claim_run_now(&routine.id).await.unwrap().unwrap();
    let tmux = "AgentDesk-routine-provider-fixture";
    let token = format!("other-host:{tmux}");
    assert!(
        store
            .set_run_owned_tmux_session(&claimed.run_id, &token)
            .await
            .unwrap()
    );
    store
        .patch_routine(
            &routine.id,
            RoutinePatch {
                provider_choice: Some(ProviderChoice {
                    provider: Some("codex".into()),
                    model: None,
                }),
                ..RoutinePatch::default()
            },
        )
        .await
        .unwrap();
    let claimed_routine = store
        .routine_as_claimed(&routine.id, &claimed.run_id)
        .await
        .unwrap()
        .unwrap();
    let controller = build_routine_session_controller(Arc::new(pool.clone()), None);
    let target = controller
        .resolve_fresh_target_for_name(&claimed_routine, &token, tmux)
        .await
        .unwrap();
    assert_eq!(target.provider, ProviderKind::Claude);
    assert_eq!(target.channel_id.get(), 8_219_101);
    pool.close().await;
    db.drop().await;
}
