use super::*;

#[test]
fn routine_model_reaches_native_cli_ahead_of_channel_model_and_unset_inherits_pg() {
    let name = concat!(
        module_path!(),
        "::routine_model_reaches_native_cli_ahead_of_channel_model_and_unset_inherits_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channels = [ChannelId::new(8_219_101), ChannelId::new(8_219_102)];
        let host = Host::new(&channels).await;
        let args = host.calls.with_extension("args");
        let cli = host._workspace.path().join("claude");
        std::fs::write(&cli, format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf 'start\\n' >> '{}'\ncat > '{}.prompt'\nprintf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"fixture answer\",\"session_id\":\"82190000-0000-4000-8000-000000000001\"}}'\n",
            args.display(), host.calls.display(), host.calls.display()
        )).unwrap();
        for (index, channel) in channels.into_iter().enumerate() {
            host.shared.overrides.model_overrides.insert(channel, "channel-model".into());
            let reservation = health::reserve_headless_agent_turn(channel);
            let mut metadata = Host::direct_metadata(&reservation);
            let expected = if index == 0 {
                metadata["model"] = serde_json::json!("routine-model");
                "routine-model"
            } else {
                "channel-model"
            };
            host.start(channel, "routine", Some(metadata), reservation).await.unwrap();
            host.native_started(index + 1).await;
            host.settled(channel).await;
            let recorded = std::fs::read_to_string(&args).unwrap();
            let argv: Vec<_> = recorded.lines().collect();
            assert!(argv.windows(2).any(|pair| pair == ["--model", expected]), "{expected}: {recorded}");
        }
        host.close().await;
    });
}
#[tokio::test(flavor = "current_thread")]
async fn routine_lifecycle_thread_uses_the_selected_provider_parent_pg() {
    use crate::services::routines::provider_choice::ProviderChoice;
    use crate::services::routines::store::{NewRoutine, RoutineStore};
    use crate::services::routines::{RoutineDiscordLogger, RoutineLifecycleEvent};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_cc,discord_channel_cdx) VALUES('duo','Duo','claude','8219101','8219102')")
        .execute(&pool).await.unwrap();
    let (send, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let app = axum::Router::new().route("/api/v10/channels/{parent}/threads", axum::routing::post(
        move |axum::extract::Path(parent): axum::extract::Path<u64>| {
            let send = send.clone();
            async move {
                send.send(parent).unwrap();
                axum::Json(serde_json::json!({"id":"8219199", "type":11, "guild_id":"8219000", "parent_id":parent.to_string(), "name":"routine"}))
            }
        }
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{addr}"))
            .ratelimiter_disabled(true)
            .build(),
    );
    let registry = Arc::new(health::HealthRegistry::new());
    for provider in ["claude", "codex"] {
        registry.register_http(provider.into(), http.clone()).await;
    }
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 4096);
    let routine = store
        .attach_routine_with_choice(
            NewRoutine {
                agent_id: Some("duo".into()),
                fallback_agent_id: None,
                max_retries: Some(0),
                script_ref: "provider.js".into(),
                name: "provider".into(),
                status: None,
                execution_strategy: "persistent".into(),
                schedule: None,
                next_due_at: None,
                checkpoint: None,
                discord_thread_id: None,
                timeout_secs: None,
            },
            None,
            3,
            &ProviderChoice {
                provider: Some("codex".into()),
                model: None,
            },
        )
        .await
        .unwrap();
    let logger =
        RoutineDiscordLogger::new_with_health_registry(Arc::new(pool.clone()), Some(registry));
    let status = logger
        .log_routine_event_with_store(&store, &routine, RoutineLifecycleEvent::Attached)
        .await;
    assert_eq!(status.status, "ok", "{status:?}");
    assert_eq!(seen.try_recv().unwrap(), 8_219_102);
    assert_eq!(
        store
            .get_routine(&routine.id)
            .await
            .unwrap()
            .unwrap()
            .discord_thread_id
            .as_deref(),
        Some("8219199")
    );
    server.abort();
    pool.close().await;
    db.drop().await;
}
