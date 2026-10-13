#[cfg(unix)]
#[tokio::test]
async fn t5_real_gateway_busy_never_arms_finalize_error_kick() {
    use super::*;
    use crate::services::discord::gateway::{DiscordGateway, LiveDiscordTurnContext};
    use crate::services::discord::{self as discord, DiscordSession};
    use crate::services::turn_orchestrator::{Intervention, InterventionMode};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    };
    use std::time::{Duration, Instant};
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let config = crate::runtime_layout::config_file_path(root.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "server: {}\ndata: {dir: data}\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    let _routing = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_INTAKE_ROUTING_MODE",
        "disabled".as_ref(),
    );
    let _ready = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_TEST_CODEX_PROMOTE_READY",
        "1".as_ref(),
    );
    let channel = ChannelId::new(8_016_000);
    let shared = discord::make_shared_data_for_tests();
    shared.settings.write().await.provider = ProviderKind::Claude;
    shared.core.lock().await.sessions.insert(
        channel,
        DiscordSession {
            session_id: Some("r1-finalize".into()),
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: Some(root.path().display().to_string()),
            history: Vec::new(),
            pending_uploads: vec!["U".into()],
            cleared: false,
            remote_profile_name: None,
            channel_id: None,
            channel_name: Some("r1-test".into()),
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
        },
    );
    let held = Arc::new(std::sync::Mutex::new(None));
    let observed = held.clone();
    let server_shared = shared.clone();
    let app = axum::Router::new().route("/gateway",axum::routing::get(|ws:axum::extract::ws::WebSocketUpgrade|async {
        ws.on_upgrade(|mut socket|async move {while socket.recv().await.is_some(){}})
    })).fallback(move |uri:axum::http::Uri| {
        let held = observed.clone();
        let shared = server_shared.clone();
        async move {
            if uri.path() == format!("/api/v10/channels/{}",channel.get())
                && shared.mailbox(channel).snapshot().await.pending_user_dispatch.is_some()
                && held.lock().unwrap().is_none()
            {
                let guard = shared.session_transition_lock(channel).try_lock_owned().expect("finalize releases its dequeue guard before Gateway intake");
                *held.lock().unwrap() = Some(guard);
            }
            axum::Json(serde_json::json!({"id":channel.get().to_string(),"type":0,"name":"r1-test","guild_id":"8000000",
                "position":0,"permission_overwrites":[],"nsfw":false,"parent_id":null}))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let proxy = format!("http://127.0.0.1:{port}");
    let http = Arc::new(
        serenity::HttpBuilder::new("r1-finalize-token")
            .proxy(proxy.clone())
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let ctx = discord::tui_prompt_relay::relay_e2e::discord_mock::serenity_context(
        proxy,
        format!("ws://127.0.0.1:{port}/gateway"),
    )
    .await;
    let gateway = Arc::new(DiscordGateway::new(
        http,
        shared.clone(),
        ProviderKind::Claude,
        Some(LiveDiscordTurnContext {
            ctx,
            token: "r1-finalize-token".into(),
            request_owner: UserId::new(1),
        }),
    ));
    for id in [8_016_002, 8_016_003] {
        let intervention = Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(id),
            queued_generation: 3,
            source_message_ids: vec![MessageId::new(id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: format!("original {id}"),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: if id == 8_016_002 {
                vec!["Q".into()]
            } else {
                Vec::new()
            },
            voice_announcement: None,
        };
        assert!(
            discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
                discord::mailbox_enqueue_intervention(
                    &shared,
                    &ProviderKind::Claude,
                    channel,
                    intervention
                )
            )
            .await
            .enqueued
        );
    }
    let kicks = Arc::new(AtomicUsize::new(0));
    let counted = kicks.clone();
    let _hook =
        discord::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(move |_, _, ch, _| {
            let counted = counted.clone();
            Box::pin(async move {
                if ch != channel {
                    return None;
                }
                counted.fetch_add(1, SeqCst);
                Some(discord::IdleQueueKickoffChannelOutcome { started: false })
            })
        }));
    shared.restart.finalizing_turns.store(1, SeqCst);
    shared.restart.global_finalizing.store(1, SeqCst);
    tokio::time::timeout(
        Duration::from_secs(10),
        Box::pin(finalize_and_drain_queued_turns(
            shared.clone(),
            true,
            false,
            gateway,
            channel,
            ProviderKind::Claude,
            "owner".into(),
            None,
            None,
            true,
        )),
    )
    .await
    .expect("actual finalize/Gateway busy must finish");
    assert!(
        held.lock().unwrap().is_some(),
        "transport must force the real post-dequeue busy branch"
    );
    let snapshot = shared.mailbox(channel).snapshot().await;
    assert_eq!(
        snapshot
            .intervention_queue
            .iter()
            .map(|i| i.message_id.get())
            .collect::<Vec<_>>(),
        [8_016_002, 8_016_003]
    );
    assert_eq!(snapshot.intervention_queue[0].pending_uploads, ["U", "Q"]);
    assert!(snapshot.pending_user_dispatch.is_none());
    assert!(
        shared.core.lock().await.sessions[&channel]
            .pending_uploads
            .is_empty()
    );
    tokio::time::pause();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        kicks.load(SeqCst),
        0,
        "busy cannot reach the finalize Err fast-kick branch"
    );
    shared.mailboxes.remove_fixture_for_test(channel);
    server.abort();
}
