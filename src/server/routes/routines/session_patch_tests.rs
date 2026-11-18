use super::*;
use crate::services::routines::store::{NewRoutine, RoutineStore};

#[tokio::test(flavor = "current_thread")]
async fn a_persistent_kill_after_provider_or_agent_patch_keeps_the_other_session_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = TmuxEnv::install(LiveServer);
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_cc,discord_channel_cdx) VALUES('duo','Duo','claude','8219101','8219102'),('next','Next','codex','8219201','8219202')")
        .execute(&pool).await.unwrap();
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 4096);
    let state = test_state(pool.clone());
    for (case, patch, next_agent) in [
        ("provider", serde_json::json!({"provider":"codex"}), "duo"),
        ("agent", serde_json::json!({"agent_id":"next"}), "next"),
    ] {
        let old_name = format!("adk-i8219-old-{case}");
        let other_name = format!("adk-i8219-other-{case}");
        tmux.start(&old_name);
        tmux.start(&other_name);
        let host = crate::services::platform::hostname_short();
        let old_key = format!("{host}:{old_name}");
        let other_key = format!("{host}:{other_name}");
        for (key, agent, provider, thread) in [
            (&old_key, "duo", "claude", Some("8219199")),
            (&other_key, next_agent, "codex", None),
        ] {
            sqlx::query("INSERT INTO sessions(session_key,agent_id,provider,status,thread_channel_id,last_heartbeat) VALUES($1,$2,$3,'turn_active',$4,NOW())")
                .bind(key).bind(agent).bind(provider).bind(thread).execute(&pool).await.unwrap();
        }
        let routine = store
            .attach_routine(NewRoutine {
                agent_id: Some("duo".into()),
                fallback_agent_id: None,
                max_retries: Some(0),
                script_ref: "provider.js".into(),
                name: case.into(),
                status: None,
                execution_strategy: "persistent".into(),
                schedule: None,
                next_due_at: None,
                checkpoint: None,
                discord_thread_id: Some("8219199".into()),
                timeout_secs: None,
            })
            .await
            .unwrap();
        let run = store.claim_run_now(&routine.id).await.unwrap().unwrap();
        let started = serde_json::json!({"agent_id":"duo","provider":"claude","channel_id":"8219199","discord_thread_id":"8219199","session_key":old_key,"turn_id":"claimed-turn"});
        sqlx::query("UPDATE routine_runs SET action='agent',turn_id='claimed-turn',result_json=$2 WHERE id=$1")
            .bind(&run.run_id).bind(started).execute(&pool).await.unwrap();
        let patched = crate::server::routes::routines::patch_routine(
            State(state.clone()),
            Path(routine.id.clone()),
            Json(serde_json::from_value(patch).unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(patched.0["routine"]["id"], routine.id);
        let Json(result) = crate::server::routes::routines::kill_routine_session(
            State(state.clone()),
            Path(routine.id.clone()),
        )
        .await
        .unwrap();
        assert!(
            tmux.alive(&other_name),
            "{case}: another session was killed: {result}"
        );
        assert_eq!(
            field(&pool, "sessions", &other_key, "status").await,
            "turn_active",
            "{case}: {result}"
        );
        assert!(
            !tmux.alive(&old_name),
            "{case}: claimed session survived: {result}"
        );
        assert_eq!(result["interrupted_run_id"], run.run_id);
        let status: String = sqlx::query_scalar("SELECT status FROM routine_runs WHERE id=$1")
            .bind(&run.run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "interrupted");
        let next = store.claim_run_now(&routine.id).await.unwrap().unwrap();
        assert!(
            store
                .interrupt_in_flight_run(&routine.id, &run.run_id, "old control", None)
                .await
                .unwrap()
                .is_none()
        );
        let current = store.get_routine(&routine.id).await.unwrap().unwrap();
        assert_eq!(
            current.in_flight_run_id.as_deref(),
            Some(next.run_id.as_str())
        );
        let denied = crate::server::routes::routines::kill_routine_session(
            State(state.clone()),
            Path(routine.id.clone()),
        )
        .await;
        assert!(
            denied.is_err(),
            "unstarted run cannot guess another primary session"
        );
        assert_eq!(
            field(&pool, "sessions", &other_key, "status").await,
            "turn_active"
        );
    }
    pool.close().await;
    db.drop().await;
}
