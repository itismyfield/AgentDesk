use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn rest_worker_queue_transport_requires_credentials_but_not_gateway() {
    let shared = make_shared_data_for_tests();
    assert!(QueueTransport::from_runtime(&shared).is_none());
    shared
        .http
        .cached_bot_token
        .set("worker-queue-test-token".into())
        .unwrap();
    let transport = QueueTransport::from_runtime(&shared).expect("REST worker transport");
    let deps = transport.intake_deps(&shared);
    assert!(deps.cache.is_none());
    assert!(deps.ctx_for_chained_dispatch.is_none());
    assert_eq!(deps.token, "worker-queue-test-token");
    assert!(Arc::ptr_eq(deps.shared, &shared));
}

async fn mock_discord() -> (
    Arc<serenity::Http>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        requests.fetch_add(1, Ordering::SeqCst);
        async move {
            let id = uri.path().rsplit('/').next().unwrap();
            let (kind, name, parent) = match id {
                "42001" => (11, "child-codex", Some("42002")),
                "42003" => (1, "", None),
                _ => (0, "queue-claude", None),
            };
            axum::Json(serde_json::json!({
                "id": id, "type": kind, "name": name, "guild_id": "42000",
                "position": 0, "permission_overwrites": [], "nsfw": false,
                "parent_id": parent,
                "recipients": [{"id": "42010", "username": "queue-test", "discriminator": "0", "avatar": null}],
                "thread_metadata": {"archived": false, "auto_archive_duration": 60,
                    "archive_timestamp": "2026-09-22T00:00:00Z", "locked": false}
            }))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("worker-queue-test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (http, count, server)
}

#[tokio::test]
async fn rest_queue_routing_preserves_thread_parent_and_dm_policy() {
    async fn validate(
        http: &Arc<serenity::Http>,
        channel: u64,
        provider: &ProviderKind,
        settings: &DiscordBotSettings,
    ) -> Result<(), settings::BotChannelRoutingGuardFailure> {
        session_runtime::validate_rest_channel_routing(
            http,
            None,
            provider,
            settings,
            ChannelId::new(channel),
            None,
        )
        .await
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let (http, count, server) = mock_discord().await;
    let mut settings = shared.settings.read().await.clone();
    settings.allowed_channel_ids = vec![42002];
    assert_eq!(
        validate(&http, 42001, &ProviderKind::Claude, &settings).await,
        Ok(())
    );
    assert_eq!(
        validate(&http, 42001, &ProviderKind::Codex, &settings).await,
        Err(settings::BotChannelRoutingGuardFailure::ProviderMismatch),
        "thread name must not override its parent provider"
    );
    settings.allowed_channel_ids = vec![42001];
    assert_eq!(
        validate(&http, 42001, &ProviderKind::Claude, &settings).await,
        Err(settings::BotChannelRoutingGuardFailure::ChannelNotAllowed),
        "allowlist must still check the thread parent"
    );
    assert_eq!(
        validate(&http, 42003, &ProviderKind::Codex, &settings).await,
        Ok(())
    );
    assert!(count.load(Ordering::SeqCst) > 0);
    server.abort();
}

#[tokio::test]
async fn rest_queue_rejected_by_changed_policy_keeps_durable_head_and_lease_free() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    shared.settings.write().await.allowed_channel_ids = vec![42009];
    shared
        .http
        .cached_bot_token
        .set("worker-queue-test-token".into())
        .unwrap();
    let (http, count, server) = mock_discord().await;
    let mut transport = QueueTransport::from_runtime(&shared).unwrap();
    transport.http = http;
    let channel = ChannelId::new(42002);
    let provider = ProviderKind::Claude;
    let head = Intervention {
        author_id: UserId::new(42010),
        author_is_bot: false,
        message_id: MessageId::new(42011),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(42011)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "preserve this queued request".into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    };
    shared
        .mailbox(channel)
        .replace_queue(
            vec![head],
            queue_persistence_context(&shared, &provider, channel),
        )
        .await;
    let outcome = crate::services::discord::kickoff_idle_queue_channel(
        &transport.intake_deps(&shared),
        &provider,
        channel,
    )
    .await;
    assert!(!outcome.started);
    assert!(
        count.load(Ordering::SeqCst) > 0,
        "REST routing guard actually ran"
    );
    let snapshot = mailbox_snapshot(&shared, channel).await;
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(snapshot.intervention_queue[0].message_id.get(), 42011);
    assert!(snapshot.pending_user_dispatch.is_none());
    let (restored, _) = load_pending_queues(&provider, &shared.token_hash);
    assert_eq!(restored[&channel].len(), 1);
    assert_eq!(restored[&channel][0].message_id.get(), 42011);
    server.abort();
}
