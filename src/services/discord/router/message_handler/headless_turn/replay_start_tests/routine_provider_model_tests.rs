use super::*;

#[test]
fn persistent_antigravity_routine_applies_model_edits_only_to_a_new_conversation_pg() {
    use crate::services::discord::inflight::{self, GuardedClearOutcome, InflightTurnIdentity};
    use crate::services::routines::provider_choice::ProviderChoice;
    use crate::services::routines::store::{
        NewRoutine, NextDueAtUpdate, RoutinePatch, RoutineStore,
    };

    let name = concat!(
        module_path!(),
        "::persistent_antigravity_routine_applies_model_edits_only_to_a_new_conversation_pg"
    );
    if !child(name) {
        return;
    }
    intake_replay_rt().block_on(async {
        let channel = ChannelId::new(8_219_111);
        let provider = ProviderKind::Antigravity;
        let host = Host::with_provider(&[channel], true, provider.clone()).await;
        host.shared.overrides.model_overrides.insert(channel, "channel-model".into());
        sqlx::query("INSERT INTO agents(id,name,provider) VALUES('agy-fixture','Agy fixture','antigravity')")
            .execute(&host.pool).await.unwrap();
        let cli = host._workspace.path().join("agy");
        let args = host.calls.with_extension("args");
        std::fs::write(&cli, format!(r#"#!/usr/bin/env python3
import json, pathlib, sys
argv = sys.argv[1:]
if argv == ['--version']:
    print('agy 1.0.0')
    sys.exit(0)
calls = pathlib.Path({calls:?})
count = len(calls.read_text().splitlines()) if calls.exists() else 0
with calls.open('a') as out:
    out.write('start\n')
conversation = argv[argv.index('--conversation') + 1] if '--conversation' in argv else '82190000-0000-4000-8000-' + str(count + 1).zfill(12)
pathlib.Path({args:?}).write_text(json.dumps(dict(argv=argv, conversation=conversation)))
print(json.dumps(dict(event='init', conversation_id=conversation)))
print(json.dumps(dict(event='result', status='SUCCESS', conversation_id=conversation, response='fixture answer')))
"#, calls = host.calls, args = args)).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _cli = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_AGY_PATH",
            &cli,
        );
        let store = RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(host.pool.clone()), "UTC", 4096);
        let routine = store.attach_routine_with_choice(
            NewRoutine {
                name: "persistent antigravity".into(),
                script_ref: "provider.js".into(),
                execution_strategy: "persistent".into(),
                agent_id: Some("agy-fixture".into()),
                fallback_agent_id: None,
                max_retries: Some(0),
                schedule: None,
                next_due_at: None,
                checkpoint: None,
                status: None,
                discord_thread_id: None,
                timeout_secs: None,
            },
            None,
            3,
            &ProviderChoice { provider: Some("antigravity".into()), model: Some("model-a".into()) },
        ).await.unwrap();
        let key = headless_session_key(&host.shared, channel, &provider, None, None).await.unwrap();
        for (index, (patch, replace, expected_model, expected_session)) in [
            (None, false, "model-a", None),
            (Some(Some("model-b")), false, "model-a", Some("82190000-0000-4000-8000-000000000001")),
            (None, true, "model-b", None),
            (Some(None), false, "model-b", Some("82190000-0000-4000-8000-000000000003")),
            (None, true, "channel-model", None),
        ].into_iter().enumerate() {
            if let Some(model) = patch {
                store.patch_routine(&routine.id, RoutinePatch {
                    provider_choice: Some(ProviderChoice { provider: Some("antigravity".into()), model: model.map(str::to_owned) }),
                    ..Default::default()
                }).await.unwrap().unwrap();
            }
            if replace {
                crate::services::discord::adk_session::clear_provider_session_id_pg(&host.pool, &key).await.unwrap();
                let mut core = host.shared.core.lock().await;
                let session = core.sessions.get_mut(&channel).unwrap();
                session.clear_provider_session();
                session.cleared = true;
            }
            let run = store.claim_run_now(&routine.id).await.unwrap().unwrap();
            let choice = store.run_provider_choice(&run.run_id).await.unwrap();
            let reservation = health::reserve_headless_agent_turn(channel);
            let metadata = serde_json::json!({
                "routine_id": routine.id, "turn_id": reservation.turn_id(),
                "execution_strategy": run.execution_strategy, "model": choice.model,
            });
            let started = host.start(channel, "routine", Some(metadata), reservation).await.unwrap();
            host.native_started(index + 1).await;
            host.settled(channel).await;
            let invocation: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&args).unwrap()).unwrap();
            let argv: Vec<String> = serde_json::from_value(invocation["argv"].clone()).unwrap();
            let flag = |name| argv.windows(2).find(|pair| pair[0] == name).map(|pair| pair[1].as_str());
            assert_eq!(flag("--conversation"), expected_session, "turn {}", index + 1);
            assert_eq!(flag("--model"), Some(expected_model), "turn {}", index + 1);
            // Simulate re-entry after the fixture finishes, using the selector returned by the CLI.
            if let Some(row) = inflight::load_inflight_state_read_only(&provider, channel.get()) {
                assert_eq!(started.turn_id, format!("discord:{channel}:{}", row.user_msg_id));
                assert!(row.full_response.contains("fixture answer"));
                let cleared = inflight::clear_inflight_state_if_matches_identity(
                    &provider, channel.get(), &InflightTurnIdentity::from_state(&row),
                );
                assert!(matches!(cleared, GuardedClearOutcome::Cleared | GuardedClearOutcome::Missing), "{cleared:?}");
            }
            let native = invocation["conversation"].as_str().unwrap();
            crate::services::discord::adk_session::save_provider_session_id(
                &key, native, Some(native), &provider, channel, host.shared.api_port,
            ).await;
            host.shared.core.lock().await.sessions.get_mut(&channel).unwrap().clear_provider_session();
            assert!(store.complete_agent_run(&run.run_id, None, None, None, NextDueAtUpdate::Preserve).await.unwrap());
        }
        host.close().await;
    });
}

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
