use super::*;
use serde_json::json;

#[tokio::test(flavor = "current_thread")]
async fn i8117_loggerless_actions_keep_run_api_diagnostics_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let store = store(&pool);
    let root = tempfile::tempdir().unwrap();
    let loader = RoutineScriptLoader::new().unwrap();
    for (name, body, expected, action_section) in [
        (
            "complete",
            "return {action:'complete',result:{ok:true}}",
            "succeeded",
            true,
        ),
        (
            "skip",
            "return {action:'skip',reason:'fixture'}",
            "skipped",
            true,
        ),
        (
            "pause",
            "return {action:'pause',reason:'fixture'}",
            "paused",
            true,
        ),
        (
            "eval-error",
            "throw new Error('raw-secret-fixture')",
            "failed",
            false,
        ),
    ] {
        let path = root.path().join(format!("{name}.js"));
        std::fs::write(
            &path,
            format!("agentdesk.routines.register({{name:'fixture',tick(ctx){{{body};}}}});"),
        )
        .unwrap();
        let mut routine = new_routine(name, None, None);
        routine.script_ref = loader.load_script(root.path(), &path).unwrap();
        let routine = store.attach_routine(routine).await.unwrap();
        let outcomes = run_due_tick(&store, &loader, &[root.path().into()], None, None, 1, false)
            .await
            .unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, expected);
        let runs = store.list_runs(&routine.id, 10).await.unwrap();
        assert_eq!(runs.len(), 1);
        let sections = &runs[0].discord_log_sections;
        for key in ["started", "js_inputs", "outcome"] {
            assert!(
                sections[key].is_string(),
                "{name}: missing {key}: {sections}"
            );
        }
        assert_eq!(
            sections["js_action"].is_string(),
            action_section,
            "{sections}"
        );
        assert!(
            !serde_json::to_string(&runs)
                .unwrap()
                .contains("raw-secret-fixture")
        );
        assert!(runs[0].discord_message_id.is_none());
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn i8117_failed_paused_notices_are_safe_and_nonturn_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let store = store(&pool);
    let logger = RoutineDiscordLogger::new_with_health_registry(Arc::new(pool.clone()), None);
    for status in ["failed", "paused"] {
        let routine = store
            .attach_routine(new_routine(status, None, Some("6551999")))
            .await
            .unwrap();
        let claimed = store.claim_run_now(&routine.id).await.unwrap().unwrap();
        if status == "failed" {
            store
                .fail_run(&claimed.run_id, "raw-error-fixture", None, None)
                .await
                .unwrap();
        } else {
            store
                .pause_after_run(&claimed.run_id, None, None, Some("raw-error-fixture"))
                .await
                .unwrap();
        }
        let outcome = RoutineRunOutcome {
            run_id: claimed.run_id,
            routine_id: routine.id.clone(),
            script_ref: routine.script_ref.clone(),
            action: "error".into(),
            status: status.into(),
            result_json: Some(json!({"assistant_message_preview":"raw-preview-fixture"})),
            error: Some("raw-error-fixture".into()),
            fresh_context_guaranteed: false,
        };
        logger.log_run_outcome(&store, &outcome).await;
        logger.log_run_outcome(&store, &outcome).await;
        let notices: Vec<(String, Option<String>)> = sqlx::query_as("SELECT content, reason_code FROM message_outbox WHERE source = 'routine-runtime' ORDER BY id")
            .fetch_all(&pool).await.unwrap();
        assert_eq!(
            notices.len(),
            if status == "failed" { 1 } else { 2 },
            "one notice per run"
        );
        let (text, reason) = notices.last().unwrap();
        assert!(text.contains("/routines"), "{text}");
        for raw in [
            "raw-error-fixture",
            "raw-preview-fixture",
            "checkpoint-fixture",
            &routine.script_ref,
        ] {
            assert!(!text.contains(raw), "{text}");
        }
        assert!(
            crate::services::message_outbox::is_non_turn_operational_alert(
                "routine-runtime",
                reason.as_deref()
            )
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn i8117_due_manual_and_poll_store_without_diagnostic_wire_pg() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let app = axum::Router::new().fallback(move || {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            axum::Json(json!({"id":"8117001"}))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let _api = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_DISCORD_API_BASE_URL",
        std::ffi::OsStr::new(&base),
    );
    let token = crate::runtime_layout::credential_token_path(root.path(), "notify");
    std::fs::create_dir_all(token.parent().unwrap()).unwrap();
    std::fs::write(token, "fixture-token").unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let store = store(&pool);
    sqlx::raw_sql("CREATE TABLE diagnostic_updates(run_id text); CREATE FUNCTION capture_diagnostics() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO diagnostic_updates VALUES(NEW.id); RETURN NEW; END $$; CREATE TRIGGER capture_diagnostics AFTER UPDATE OF discord_log_sections ON routine_runs FOR EACH ROW EXECUTE FUNCTION capture_diagnostics();")
        .execute(&pool).await.unwrap();
    let loader = RoutineScriptLoader::new().unwrap();
    let path = root.path().join("diagnostics.js");
    std::fs::write(&path, "agentdesk.routines.register({name:'fixture',tick(ctx){return {action:'complete',result:{ok:true}};}});").unwrap();
    let script = loader.load_script(root.path(), &path).unwrap();
    let logger = RoutineDiscordLogger::new_with_health_registry(Arc::new(pool.clone()), None);
    for manual in [false, true] {
        let mut definition =
            new_routine(if manual { "manual" } else { "due" }, None, Some("8117999"));
        definition.script_ref = script.clone();
        let routine = store.attach_routine(definition).await.unwrap();
        if manual {
            let mut config = crate::config::Config::default();
            config.routines.enabled = true;
            config.routines.dir = root.path().into();
            config.routines.additional_dirs.clear();
            config.routines.max_consecutive_failures = 0;
            config.policies.dir = root.path().join("empty-policies");
            std::fs::create_dir_all(&config.policies.dir).unwrap();
            let broadcast_tx = crate::eventbus::new_broadcast();
            let state = crate::app_state::AppState {
                pg_pool: Some(pool.clone()),
                engine: crate::engine::PolicyEngine::new(&config).unwrap(),
                config: Arc::new(config),
                batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
                broadcast_tx,
                health_registry: None,
                cluster_instance_id: None,
            };
            let axum::Json(response) = crate::server::routes::routines::run_routine_now(
                axum::extract::State(state),
                axum::extract::Path(routine.id.clone()),
            )
            .await
            .unwrap();
            assert_eq!(response["outcome"]["status"], "succeeded");
        } else {
            assert_eq!(
                run_due_tick(
                    &store,
                    &loader,
                    &[root.path().into()],
                    None,
                    Some(&logger),
                    1,
                    false
                )
                .await
                .unwrap()
                .len(),
                1
            );
        }
        let runs = store.list_runs(&routine.id, 10).await.unwrap();
        let updates: i64 =
            sqlx::query_scalar("SELECT count(*) FROM diagnostic_updates WHERE run_id=$1")
                .bind(&runs[0].id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(updates, 4, "each diagnostic section is stored once");
        let sections = runs[0].discord_log_sections.as_object().unwrap();
        assert_eq!(sections.len(), 4, "{sections:?}");
        assert!(
            runs[0].discord_message_id.is_none(),
            "diagnostics have no Discord message"
        );
        let outbox: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(outbox, 0, "successful diagnostics must enqueue no notice");
    }
    insert_boot_agent(&pool).await;
    let claimed = claim_boot_agent_run(&store).await;
    super::super::run_diagnostics::started(&store, &claimed).await;
    let run_id = claimed.run_id.clone();
    let routine_id = claimed.routine_id.clone();
    let executor = RoutineAgentExecutor::new(
        Arc::new(pool.clone()),
        Some(Arc::new(HealthRegistry::new())),
        60,
    );
    let deferred = executor
        .start_agent_run(
            &store,
            claimed,
            "fixture".into(),
            None,
            None,
            None,
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(deferred.status, "deferred");
    retry_deferred_start(&pool, &run_id, Utc::now() - Duration::minutes(11)).await;
    let outcomes = super::super::poll_agent_turns(&store, &executor, 10, false)
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].status, "failed");
    let runs = store.list_runs(&routine_id, 10).await.unwrap();
    assert!(
        runs[0].discord_log_sections["outcome"]
            .as_str()
            .is_some_and(|s| s.contains("failed")),
        "poll must persist its outcome"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "no diagnostic send/edit/resolve HTTP"
    );
    server.abort();
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn i8117_diagnostic_storage_failure_is_sticky_in_run_api_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let store = store(&pool);
    let root = tempfile::tempdir().unwrap();
    let loader = RoutineScriptLoader::new().unwrap();
    let path = root.path().join("storage-failure.js");
    std::fs::write(&path, "agentdesk.routines.register({name:'fixture',tick(ctx){return {action:'complete',result:{ok:true}};}});").unwrap();
    let mut definition = new_routine("storage-failure", None, None);
    definition.script_ref = loader.load_script(root.path(), &path).unwrap();
    let routine = store.attach_routine(definition).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION reject_diagnostics() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.discord_log_sections IS DISTINCT FROM OLD.discord_log_sections THEN RAISE EXCEPTION 'fixture diagnostics unavailable'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_diagnostics BEFORE UPDATE ON routine_runs FOR EACH ROW EXECUTE FUNCTION reject_diagnostics();")
        .execute(&pool).await.unwrap();
    let outcomes = run_due_tick(&store, &loader, &[root.path().into()], None, None, 1, false)
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 1);
    let runs = store.list_runs(&routine.id, 10).await.unwrap();
    assert_eq!(runs[0].discord_log_status.as_deref(), Some("failed"));
    assert!(runs[0].discord_log_error.is_some());
    store
        .record_discord_log_result(&runs[0].id, "stored", None)
        .await
        .unwrap();
    assert_eq!(
        store.list_runs(&routine.id, 10).await.unwrap()[0]
            .discord_log_status
            .as_deref(),
        Some("failed")
    );
    pool.close().await;
    db.drop().await;
}
