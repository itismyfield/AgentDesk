use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct MockRequests {
    total: AtomicUsize,
    channels: std::sync::Mutex<Vec<String>>,
}

impl MockRequests {
    fn for_channel(&self, channel: u64) -> usize {
        let id = channel.to_string();
        self.channels
            .lock()
            .unwrap()
            .iter()
            .filter(|value| **value == id)
            .count()
    }
}

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
    Arc<MockRequests>,
    tokio::task::JoinHandle<()>,
) {
    mock_discord_with_failures(0).await
}

async fn mock_discord_with_failures(
    failed_requests: usize,
) -> (
    Arc<serenity::Http>,
    Arc<MockRequests>,
    tokio::task::JoinHandle<()>,
) {
    use axum::response::IntoResponse;

    let count = Arc::new(MockRequests::default());
    let requests = count.clone();
    let app = axum::Router::new().fallback(move |method: axum::http::Method, uri: axum::http::Uri| {
        let attempt = requests.total.fetch_add(1, Ordering::SeqCst);
        let id = uri.path().rsplit('/').next().unwrap().to_owned();
        if method == axum::http::Method::GET && uri.path().contains("/channels/") {
            requests.channels.lock().unwrap().push(id.clone());
        }
        async move {
            if attempt < failed_requests {
                return (axum::http::StatusCode::FORBIDDEN, axum::Json(serde_json::json!({
                    "code": 50001, "message": "Missing Access"
                }))).into_response();
            }
            let (kind, name, parent) = match id.as_str() {
                "42001" => (11, "child-codex", Some("42002")),
                "42003" => (1, "", None),
                "42004" => (0, "queue-claude", Some("42005")),
                "42005" => (4, "folder-codex", None),
                "42006" => (15, "forum-claude", Some("42005")),
                "42007" => (12, "child-codex", Some("42002")),
                "42008" => (10, "child-codex", Some("42002")),
                _ => (0, "queue-claude", None),
            };
            axum::Json(serde_json::json!({
                "id": id, "type": kind, "name": name, "guild_id": "42000",
                "position": 0, "permission_overwrites": [], "nsfw": false,
                "parent_id": parent,
                "recipients": [{"id": "42010", "username": "queue-test", "discriminator": "0", "avatar": null}],
                "thread_metadata": {"archived": false, "auto_archive_duration": 60,
                    "archive_timestamp": "2026-09-22T00:00:00Z", "locked": false}
            })).into_response()
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
    assert!(count.total.load(Ordering::SeqCst) > 0);
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
        count.total.load(Ordering::SeqCst) > 0,
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

#[tokio::test]
async fn rest_queue_routing_reads_each_channel_once_without_category_fetches() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    let settings = shared.settings.read().await.clone();
    let (http, count, server) = mock_discord().await;
    for channel in [42002, 42003, 42004, 42006] {
        let before = count.total.load(Ordering::SeqCst);
        assert_eq!(
            session_runtime::validate_rest_channel_routing(
                &http,
                None,
                &ProviderKind::Claude,
                &settings,
                ChannelId::new(channel),
                None,
            )
            .await,
            Ok(()),
            "routing channel {channel}"
        );
        assert_eq!(
            count.for_channel(channel),
            1,
            "channel {channel} must be read once"
        );
        assert_eq!(
            count.total.load(Ordering::SeqCst) - before,
            1,
            "no category lookup for {channel}"
        );
    }
    assert_eq!(
        count.for_channel(42005),
        0,
        "discarded category names need no REST lookup"
    );
    server.abort();
}

#[tokio::test]
async fn rest_queue_routing_reads_thread_and_parent_once_for_every_thread_kind() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    let mut settings = shared.settings.read().await.clone();
    settings.allowed_channel_ids = vec![42002];
    let (http, count, server) = mock_discord().await;
    for channel in [42001, 42007, 42008] {
        let before = count.total.load(Ordering::SeqCst);
        assert_eq!(
            session_runtime::validate_rest_channel_routing(
                &http,
                None,
                &ProviderKind::Claude,
                &settings,
                ChannelId::new(channel),
                None,
            )
            .await,
            Ok(()),
            "the parent provider and allowlist apply to thread {channel}"
        );
        assert_eq!(
            count.for_channel(channel),
            1,
            "thread {channel} must be read once"
        );
        assert_eq!(
            count.total.load(Ordering::SeqCst) - before,
            2,
            "one thread GET and one parent GET"
        );
    }
    assert_eq!(count.for_channel(42002), 3);
    server.abort();
}

#[tokio::test]
async fn rest_queue_routing_failed_lookup_uses_one_consistent_failure_snapshot() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    let settings = shared.settings.read().await.clone();
    for hint in [None, Some(true), Some(false)] {
        let (partial_http, partial_count, partial_server) = mock_discord_with_failures(1).await;
        let (failed_http, failed_count, failed_server) =
            mock_discord_with_failures(usize::MAX).await;
        let partial = session_runtime::validate_rest_channel_routing(
            &partial_http,
            None,
            &ProviderKind::Claude,
            &settings,
            ChannelId::new(42002),
            hint,
        )
        .await;
        let failed = session_runtime::validate_rest_channel_routing(
            &failed_http,
            None,
            &ProviderKind::Claude,
            &settings,
            ChannelId::new(42002),
            hint,
        )
        .await;
        assert_eq!(
            partial, failed,
            "a first failure must not be mixed with later successful reads (hint={hint:?})"
        );
        assert_eq!(
            failed,
            if hint == Some(true) {
                Ok(())
            } else {
                Err(settings::BotChannelRoutingGuardFailure::ProviderMismatch)
            }
        );
        assert_eq!(partial_count.for_channel(42002), 1);
        assert_eq!(failed_count.for_channel(42002), 1);
        partial_server.abort();
        failed_server.abort();
    }
}

#[tokio::test]
async fn rest_queue_routing_uses_fresh_policy_with_empty_or_stale_gateway_cache() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    let settings = shared.settings.read().await.clone();
    let empty = Arc::new(serenity::cache::Cache::new());
    let populated = Arc::new(serenity::cache::Cache::new());
    let mut guild = serde_json::to_value(serenity::model::guild::Guild::default()).unwrap();
    guild["id"] = serde_json::json!("42000");
    guild["channels"] = serde_json::json!([
        {"id": "42001", "guild_id": "42000", "type": 11, "name": "stale-claude", "parent_id": "42002", "position": 0, "permission_overwrites": []},
        {"id": "42002", "guild_id": "42000", "type": 0, "name": "stale-codex", "parent_id": null, "position": 0, "permission_overwrites": []},
        {"id": "42004", "guild_id": "42000", "type": 0, "name": "stale-codex", "parent_id": "42005", "position": 0, "permission_overwrites": []}
    ]);
    let mut event: serenity::model::event::GuildCreateEvent =
        serde_json::from_value(guild).unwrap();
    populated.update(&mut event);
    assert_eq!(
        populated.guild(42000_u64).unwrap().channels[&ChannelId::new(42002)].name,
        "stale-codex"
    );
    let (http, count, server) = mock_discord().await;
    for cache in [None, Some(&empty), Some(&populated)] {
        for (channel, expected_gets) in [(42001, 2), (42004, 1)] {
            let before = count.total.load(Ordering::SeqCst);
            assert_eq!(
                session_runtime::validate_rest_channel_routing(
                    &http,
                    cache,
                    &ProviderKind::Claude,
                    &settings,
                    ChannelId::new(channel),
                    None,
                )
                .await,
                Ok(()),
                "fresh REST names must determine routing after startup and despite stale cache"
            );
            assert_eq!(count.total.load(Ordering::SeqCst) - before, expected_gets);
        }
        assert_eq!(
            session_runtime::validate_rest_channel_routing(
                &http,
                cache,
                &ProviderKind::Codex,
                &settings,
                ChannelId::new(42001),
                None,
            )
            .await,
            Err(settings::BotChannelRoutingGuardFailure::ProviderMismatch),
            "stale cached parent names cannot admit a different provider"
        );
    }
    assert_eq!(count.for_channel(42005), 0);
    server.abort();
}
