use super::*;
use crate::services::routines::session_control::{CONTROL_TEST_PAUSE, ControlPause};
use crate::services::routines::store::{NewRoutine, NextDueAtUpdate, RoutineStore};

#[tokio::test(flavor = "current_thread")]
async fn persistent_kill_blocks_fallback_during_its_physical_effect_pg() {
    persistent_kill_fences_turn(ControlPause::BeforeEffect).await;
}

#[tokio::test(flavor = "current_thread")]
async fn persistent_kill_interrupts_before_the_retry_can_replace_its_turn_pg() {
    persistent_kill_fences_turn(ControlPause::AfterCommit).await;
}

async fn persistent_kill_fences_turn(stage: ControlPause) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = TmuxEnv::install(LiveServer);
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let writer_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET lock_timeout = '500ms'")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 4096);
    let writer = RoutineStore::new_with_timezone_and_checkpoint_limit(
        Arc::new(writer_pool.clone()),
        "UTC",
        4096,
    );
    sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_cc) VALUES('actor','Actor','claude','8219101')")
        .execute(&pool).await.unwrap();
    let host = crate::services::platform::hostname_short();
    let mut keys = Vec::new();
    for (name, channel) in [
        ("adk-i8219-turn-one", "8219191"),
        ("adk-i8219-turn-two", "8219192"),
    ] {
        let created =
            crate::services::platform::tmux::create_session(name, None, "sleep 600").unwrap();
        assert!(created.status.success() && tmux.alive(name));
        let key = format!("{host}:{name}");
        sqlx::query("INSERT INTO sessions(session_key,agent_id,provider,status,thread_channel_id) VALUES($1,'actor','claude','turn_active',$2)")
            .bind(&key).bind(channel).execute(&pool).await.unwrap();
        keys.push(key);
    }
    let routine = store
        .attach_routine(NewRoutine {
            agent_id: Some("actor".into()),
            fallback_agent_id: None,
            max_retries: Some(1),
            script_ref: "provider.js".into(),
            name: "turn fence".into(),
            status: None,
            execution_strategy: "persistent".into(),
            schedule: None,
            next_due_at: None,
            checkpoint: None,
            discord_thread_id: Some("8219191".into()),
            timeout_secs: None,
        })
        .await
        .unwrap();
    let run = store.claim_run_now(&routine.id).await.unwrap().unwrap();
    let routing = |turn, channel, key: &str| {
        serde_json::json!({
            "status": "started", "turn_id": turn, "agent_id": "actor",
            "provider": "claude", "channel_id": channel, "session_key": key,
        })
    };
    assert!(
        store
            .mark_agent_turn_started(
                &run.run_id,
                "turn-one",
                Some(routing("turn-one", "8219191", &keys[0])),
                "actor",
                "primary",
            )
            .await
            .unwrap()
    );

    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let kill = CONTROL_TEST_PAUSE.scope(
        (stage, gate.clone()),
        crate::server::routes::routines::kill_routine_session(
            State(test_state(pool.clone())),
            Path(routine.id.clone()),
        ),
    );
    let retry = async {
        gate.wait().await;
        let changed = async {
            if stage == ControlPause::AfterCommit
                && !writer
                    .schedule_agent_retry(
                        &run.run_id,
                        chrono::Utc::now(),
                        None,
                        "retry",
                        Some("actor"),
                        "primary",
                    )
                    .await?
            {
                return Ok(false);
            }
            writer
                .mark_agent_turn_started(
                    &run.run_id,
                    "turn-two",
                    Some(routing("turn-two", "8219192", &keys[1])),
                    "actor",
                    "fallback",
                )
                .await
        }
        .await;
        gate.wait().await;
        changed
    };
    let (kill, retry) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        tokio::join!(kill, retry)
    })
    .await
    .expect("session control and retry must finish");
    let terminal: (String, Option<String>) =
        sqlx::query_as("SELECT status,turn_id FROM routine_runs WHERE id=$1")
            .bind(&run.run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    match stage {
        ControlPause::BeforeEffect => {
            assert!(
                retry
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.to_string().contains("due to lock timeout")),
                "fallback advanced during physical control: {retry:?}, run={terminal:?}",
            );
        }
        ControlPause::AfterCommit => {
            assert!(
                !retry.unwrap(),
                "retry advanced after commit before interruption: run={terminal:?}"
            );
        }
    }
    let Json(kill) = kill.unwrap();
    assert_eq!(kill["interrupted_run_id"], run.run_id);
    assert_eq!(kill["session"]["tmux_killed"], true);
    assert_eq!(terminal, ("interrupted".into(), Some("turn-one".into())));
    assert!(
        store
            .get_routine(&routine.id)
            .await
            .unwrap()
            .unwrap()
            .in_flight_run_id
            .is_none()
    );
    assert!(!tmux.alive("adk-i8219-turn-one"));
    assert!(tmux.alive("adk-i8219-turn-two"));
    assert_eq!(
        field(&pool, "sessions", &keys[0], "status").await,
        "disconnected"
    );
    assert_eq!(
        field(&pool, "sessions", &keys[1], "status").await,
        "turn_active"
    );
    writer_pool.close().await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_persistent_kill_after_provider_or_agent_patch_keeps_the_other_session_pg() {
    persistent_kill_after_patch(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_idle_persistent_kill_after_provider_patch_keeps_the_other_session_pg() {
    persistent_kill_after_patch(Some("provider")).await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_idle_persistent_kill_after_agent_patch_keeps_the_other_session_pg() {
    persistent_kill_after_patch(Some("agent")).await;
}

async fn persistent_kill_after_patch(idle_case: Option<&str>) {
    let idle = idle_case.is_some();
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = TmuxEnv::install(LiveServer);
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_cc,discord_channel_cdx) VALUES('duo','Duo','claude','8219101','8219102'),('next','Next','codex','8219201','8219202')")
        .execute(&pool).await.unwrap();
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 4096);
    let state = test_state(pool.clone());
    for (case, patch, next_agent, actual_agent, actual_provider, other_provider) in [
        (
            "provider",
            serde_json::json!({"provider":"codex"}),
            "duo",
            "duo",
            "claude",
            "codex",
        ),
        (
            "agent",
            serde_json::json!({"agent_id":"next"}),
            "next",
            "duo",
            "claude",
            "codex",
        ),
        (
            "legacy-attempt",
            serde_json::json!({"provider":"codex"}),
            "duo",
            "duo",
            "claude",
            "codex",
        ),
        (
            "fallback-attempt",
            serde_json::json!({"provider":"claude"}),
            "duo",
            "next",
            "codex",
            "claude",
        ),
        (
            "primary-evidence",
            serde_json::json!({"provider":"codex"}),
            "duo",
            "duo",
            "claude",
            "codex",
        ),
    ] {
        if idle_case.is_some_and(|selected| selected != case) {
            continue;
        }
        let thread = (case != "primary-evidence").then_some("8219199");
        let actual_channel = thread.unwrap_or("8219101");
        let old_name = format!("adk-i8219-old-{case}");
        let other_name = format!("adk-i8219-other-{case}");
        for name in [&old_name, &other_name] {
            let made =
                crate::services::platform::tmux::create_session(name, None, "sleep 600").unwrap();
            assert!(made.status.success());
        }
        let host = crate::services::platform::hostname_short();
        let old_key = format!("{host}:{old_name}");
        let other_key = format!("{host}:{other_name}");
        for (key, agent, provider, thread) in [
            (&old_key, actual_agent, actual_provider, thread),
            (&other_key, next_agent, other_provider, None),
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
                discord_thread_id: thread.map(str::to_string),
                timeout_secs: None,
            })
            .await
            .unwrap();
        let run = store.claim_run_now(&routine.id).await.unwrap().unwrap();
        let mut started = serde_json::json!({"status":"started","agent_id":actual_agent,"provider":actual_provider,"channel_id":actual_channel,"discord_thread_id":thread,"session_key":old_key,"turn_id":"claimed-turn"});
        if idle {
            assert!(
                store
                    .mark_agent_turn_started(
                        &run.run_id,
                        "claimed-turn",
                        Some(started.clone()),
                        actual_agent,
                        "primary",
                    )
                    .await
                    .unwrap()
            );
            started["status"] = serde_json::json!("completed");
            if case == "agent" {
                started.as_object_mut().unwrap().remove("session_key");
            }
            assert!(
                store
                    .complete_agent_run(
                        &run.run_id,
                        Some(started),
                        None,
                        None,
                        NextDueAtUpdate::Preserve
                    )
                    .await
                    .unwrap()
            );
        } else {
            sqlx::query("UPDATE routine_runs SET action='agent',turn_id='claimed-turn',result_json=$2 WHERE id=$1")
                .bind(&run.run_id).bind(&started).execute(&pool).await.unwrap();
        }
        if case.ends_with("attempt") {
            let attempt = serde_json::json!([{"event":"started", "agent_id":actual_agent,"provider":actual_provider,"channel_id":actual_channel,"turn_id":"claimed-turn"}]);
            sqlx::query(
                "UPDATE routine_runs SET result_json=NULL,run_spec=NULL,attempts=$2 WHERE id=$1",
            )
            .bind(&run.run_id)
            .bind(attempt)
            .execute(&pool)
            .await
            .unwrap();
        }

        let patched = crate::server::routes::routines::patch_routine(
            State(state.clone()),
            Path(routine.id.clone()),
            Json(serde_json::from_value(patch).unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(patched.0["routine"]["id"], routine.id);
        if !idle {
            let Json(reset) = crate::server::routes::routines::reset_routine_session(
                State(state.clone()),
                Path(routine.id.clone()),
            )
            .await
            .unwrap();
            assert_eq!(reset["session"]["provider"], actual_provider);
            assert_eq!(reset["session"]["agent_id"], actual_agent);
            assert_eq!(reset["session"]["session_key"], old_key);
            assert_eq!(reset["session"]["channel_id"], actual_channel);
        }
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
        if idle {
            assert!(result["interrupted_run_id"].is_null());
        } else {
            assert_eq!(result["interrupted_run_id"], run.run_id);
        }
        let status: String = sqlx::query_scalar("SELECT status FROM routine_runs WHERE id=$1")
            .bind(&run.run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, if idle { "succeeded" } else { "interrupted" });
        let next = store.claim_run_now(&routine.id).await.unwrap().unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert!(
            RoutineStore::interrupt_controlled_turn(
                &mut tx,
                &routine.id,
                &run.run_id,
                "claimed-turn",
                "old control",
                None,
            )
            .await
            .unwrap()
            .is_none()
        );
        tx.commit().await.unwrap();
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
