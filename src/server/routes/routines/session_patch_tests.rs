use super::*;
use crate::services::routines::store::{NewRoutine, RoutineStore};
use serde_json::{Value, json};

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
        let started = serde_json::json!({"agent_id":actual_agent,"provider":actual_provider,"channel_id":actual_channel,"discord_thread_id":thread,"session_key":old_key,"turn_id":"claimed-turn"});
        sqlx::query("UPDATE routine_runs SET action='agent',turn_id='claimed-turn',result_json=$2 WHERE id=$1")
            .bind(&run.run_id).bind(started).execute(&pool).await.unwrap();
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

async fn patch_model_request(router: &axum::Router, id: &str, body: Value) -> (StatusCode, Value) {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    let response = router
        .clone()
        .oneshot(
            Request::patch(format!("/api/routines/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

struct ModelPatchFixture {
    db: TestPostgresDb,
    pool: sqlx::PgPool,
    store: RoutineStore,
    router: axum::Router,
}

impl ModelPatchFixture {
    async fn new(parent_named: bool) -> Self {
        use crate::domain::provider::ProviderKind;
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_cc,discord_channel_cdx) VALUES ('original','Original','codex','8200','8210'), ('other','Other','codex','8220','8230')")
            .execute(&pool).await.unwrap();
        let home = crate::services::provider_auth_profile::extra_account_home(
            &ProviderKind::Codex,
            "work",
        )
        .unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let org = json!({
            "version": 1,
            "agents": {"original":{"display_name":"Original","provider":"codex"}},
            "provider_auth_profiles": {"work":{"provider":"codex","home":home}},
            "provider_auth_fallbacks": {"codex":{"enabled":false}},
            "channels":{"by_id":{
                "8210":{"agent":"original","auth_profile":if parent_named {"work"} else {"default"}},
                "8212":{"agent":"original","auth_profile":"work"}
            }}
        });
        let org_path =
            crate::runtime_layout::org_schema_path(&crate::config::runtime_root().unwrap());
        std::fs::create_dir_all(org_path.parent().unwrap()).unwrap();
        std::fs::write(org_path, serde_yaml::to_string(&org).unwrap()).unwrap();
        let store = RoutineStore::new_with_timezone_and_checkpoint_limit(
            Arc::new(pool.clone()),
            "UTC",
            4096,
        );
        let router = axum::Router::new()
            .route(
                "/api/routines/{id}",
                axum::routing::patch(crate::server::routes::routines::patch_routine),
            )
            .with_state(test_state(pool.clone()));
        Self {
            db,
            pool,
            store,
            router,
        }
    }

    async fn seed_conversation(
        &self,
        name: &str,
        channel: &str,
        dm: bool,
        tmux: &TmuxEnv,
    ) -> (String, String, String) {
        use crate::domain::provider::ProviderKind;
        use crate::services::routines::provider_choice::ProviderChoice;
        let routine = self
            .store
            .attach_routine_with_choice(
                NewRoutine {
                    agent_id: Some("original".into()),
                    fallback_agent_id: None,
                    max_retries: Some(0),
                    script_ref: "provider.js".into(),
                    name: name.into(),
                    status: None,
                    execution_strategy: "persistent".into(),
                    schedule: None,
                    next_due_at: None,
                    checkpoint: None,
                    discord_thread_id: (!dm).then(|| channel.to_owned()),
                    timeout_secs: None,
                },
                None,
                3,
                &ProviderChoice {
                    provider: Some("codex".into()),
                    model: Some("model-a".into()),
                },
            )
            .await
            .unwrap();
        let run = self
            .store
            .claim_run_now(&routine.id)
            .await
            .unwrap()
            .unwrap();
        let label = if dm {
            format!("dm-{channel}")
        } else {
            crate::services::routines::agent_executor::routine_agent_session_name(name, "original")
        };
        let pane = ProviderKind::Codex.build_tmux_session_name(&label);
        tmux.start(&pane);
        let locator = if dm {
            pane.clone()
        } else {
            format!("channel-{channel}")
        };
        let key = format!("{}:{locator}", crate::services::platform::hostname_short());
        let native = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO sessions(session_key,agent_id,provider,status,channel_id,thread_channel_id,raw_provider_session_id) VALUES($1,'original','codex','idle',$2,$3,$4)")
            .bind(&key).bind(channel).bind((!dm).then_some(channel)).bind(&native)
            .execute(&self.pool).await.unwrap();
        let turn = &run.run_id;
        sqlx::query("INSERT INTO turns(turn_id,session_key,channel_id,agent_id,provider,session_id,started_at,finished_at) VALUES($1,$2,$3,'original','codex',$4,NOW(),NOW())")
            .bind(turn).bind(&key).bind(channel).bind(&native).execute(&self.pool).await.unwrap();
        let result = json!({"status":"started","execution_mode":"tmux","turn_id":turn,
            "agent_id":"original","provider":"codex","channel_id":channel,"model":"model-a",
            "is_dm":dm,"dm_user_id":dm.then_some(channel)});
        assert!(
            self.store
                .mark_agent_turn_started(&run.run_id, turn, Some(result), "original", "primary")
                .await
                .unwrap()
        );
        assert!(
            self.store
                .finish_run(
                    &run.run_id,
                    Some(json!({"completed":true})),
                    None,
                    None,
                    None
                )
                .await
                .unwrap()
        );
        (routine.id, pane, native)
    }

    async fn close(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

fn own_model_patch_test(name: &str) -> bool {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let name = name.strip_prefix("agentdesk::").unwrap_or(name);
    if !crate::services::discord::admin_host_guard::tests::api_child(name) {
        return false;
    }
    let prefix = format!("AdkTest-{}", uuid::Uuid::new_v4());
    crate::domain::provider::naming::install_test_prefix(&prefix).unwrap();
    true
}

#[tokio::test(flavor = "current_thread")]
async fn routine_fix8_return_to_named_codex_checks_existing_model_pg() {
    if !own_model_patch_test(concat!(
        module_path!(),
        "::routine_fix8_return_to_named_codex_checks_existing_model_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let user = tempfile::tempdir().unwrap();
    let _home =
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("HOME", user.path());
    let tmux = TmuxEnv::install(LiveServer);
    let fx = ModelPatchFixture::new(false).await;
    let mut results = Vec::new();
    for (name, channel, leave, back) in [
        (
            "set",
            "8212",
            json!({"execution_strategy":"fresh","model":"model-b"}),
            json!({"execution_strategy":"persistent"}),
        ),
        (
            "clear",
            "8213",
            json!({"execution_strategy":"fresh","model":null}),
            json!({"execution_strategy":"persistent"}),
        ),
        (
            "provider",
            "8214",
            json!({"provider":"claude","model":"model-b"}),
            json!({"provider":"codex"}),
        ),
        (
            "agent",
            "8215",
            json!({"agent_id":"other","model":"model-b"}),
            json!({"agent_id":"original"}),
        ),
    ] {
        let (id, pane, _) = fx.seed_conversation(name, channel, false, &tmux).await;
        crate::services::tmux_common::write_tmux_session_auth_profile(&pane, "work").unwrap();
        let (status, body) = patch_model_request(&fx.router, &id, leave).await;
        assert_eq!(status, StatusCode::OK, "{name}: {body}");
        let before = snapshot(&fx.pool, "routines", "id", &[&id]).await;
        let pane_pid = crate::services::platform::tmux::pane_pid(&pane).unwrap();
        let (status, body) = patch_model_request(&fx.router, &id, back).await;
        let unchanged = snapshot(&fx.pool, "routines", "id", &[&id]).await == before;
        let same_pane = crate::services::platform::tmux::pane_pid(&pane) == Some(pane_pid);
        results.push((name, status, body, unchanged, same_pane));
    }
    let (id, pane, old_native) = fx
        .seed_conversation("fresh-replaced", "8216", false, &tmux)
        .await;
    crate::services::tmux_common::write_tmux_session_auth_profile(&pane, "work").unwrap();
    let (status, body) = patch_model_request(
        &fx.router,
        &id,
        json!({
            "execution_strategy":"fresh", "model":"model-b"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let fresh = fx.store.claim_run_now(&id).await.unwrap().unwrap();
    let spec: Value = sqlx::query_scalar("SELECT run_spec FROM routine_runs WHERE id=$1")
        .bind(&fresh.run_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(spec["execution_strategy"], "fresh");
    assert_eq!(spec["model"], "model-b");
    let scratch = tempfile::tempdir().unwrap();
    let selection = crate::services::codex_tui::session::resolve_codex_tui_session(
        Some(&old_native),
        scratch.path(),
        Some(scratch.path()),
        true,
    );
    assert_eq!(selection.resume_session_id(), None);
    sqlx::query("INSERT INTO turns_archive(turn_id,session_key,channel_id,agent_id,provider,session_id,started_at,finished_at)
                 SELECT turn_id,session_key,channel_id,agent_id,provider,session_id,started_at,finished_at
                 FROM turns WHERE session_id=$1")
        .bind(&old_native).execute(&fx.pool).await.unwrap();
    sqlx::query("DELETE FROM turns WHERE session_id=$1")
        .bind(&old_native)
        .execute(&fx.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET raw_provider_session_id=$1 WHERE channel_id='8216'")
        .bind(uuid::Uuid::new_v4().to_string())
        .execute(&fx.pool)
        .await
        .unwrap();
    assert!(
        fx.store
            .finish_run(
                &fresh.run_id,
                Some(json!({"fresh_context_guaranteed":true})),
                None,
                None,
                None
            )
            .await
            .unwrap()
    );
    let (status, body) =
        patch_model_request(&fx.router, &id, json!({"execution_strategy":"persistent"})).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "after fresh selector replacement: {body}"
    );
    fx.close().await;
    assert!(
        results
            .iter()
            .all(
                |(_, status, body, unchanged, alive)| *status == StatusCode::UNPROCESSABLE_ENTITY
                    && body["error"]
                        .as_str()
                        .is_some_and(|reason| reason.contains("named account"))
                    && *unchanged
                    && *alive
            ),
        "re-entry must refuse without changing the row or pane: {results:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn routine_fix8_uses_thread_and_dm_accounts_for_patch_pg() {
    if !own_model_patch_test(concat!(
        module_path!(),
        "::routine_fix8_uses_thread_and_dm_accounts_for_patch_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let user = tempfile::tempdir().unwrap();
    let _home =
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("HOME", user.path());
    let tmux = TmuxEnv::install(LiveServer);
    let fx = ModelPatchFixture::new(true).await;
    let (thread, thread_pane, thread_native) =
        fx.seed_conversation("default", "8211", false, &tmux).await;
    let (thread_status, body) =
        patch_model_request(&fx.router, &thread, json!({"model":"model-b"})).await;
    let thread_selector: String =
        sqlx::query_scalar("SELECT raw_provider_session_id FROM sessions WHERE channel_id='8211'")
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let rollouts = tempfile::tempdir().unwrap();
    std::fs::write(
        rollouts.path().join("rollout-thread.jsonl"),
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":thread_native,"cwd":cwd.path()}})
        ),
    )
    .unwrap();
    let selected = crate::services::codex_tui::session::resolve_codex_tui_session(
        Some(&thread_selector),
        cwd.path(),
        Some(rollouts.path()),
        false,
    );
    let thread_ok = thread_status == StatusCode::OK
        && body["routine"]["model"] == "model-b"
        && selected.resume_session_id() == Some(thread_native.as_str())
        && tmux.alive(&thread_pane);
    fx.close().await;

    let fx = ModelPatchFixture::new(false).await;
    let (dm, dm_pane, _) = fx.seed_conversation("named-dm", "8212", true, &tmux).await;
    let before = snapshot(&fx.pool, "routines", "id", &[&dm]).await;
    let (dm_status, body) = patch_model_request(&fx.router, &dm, json!({"model":null})).await;
    let dm_ok = dm_status == StatusCode::UNPROCESSABLE_ENTITY
        && body["error"]
            .as_str()
            .is_some_and(|reason| reason.contains("named account"))
        && snapshot(&fx.pool, "routines", "id", &[&dm]).await == before
        && tmux.alive(&dm_pane);
    fx.close().await;
    assert!(
        thread_ok && dm_ok,
        "actual channel policy: default thread={thread_status}, named DM={dm_status}"
    );
}
