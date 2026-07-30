use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::future::BoxFuture;
use poise::serenity_prelude as serenity;
use serde_json::{Value, json};
use serenity::cache::{Cache, CacheUpdate};
use serenity::{ChannelId, MessageId, UserId};
use tokio::sync::Notify;

use super::super::{Data, ProviderKind, mailbox_snapshot};

const CHANNEL_ID: u64 = 940_487_400_000_001;
const USER_ID: u64 = 940_487_400_000_002;
const BOT_ID: u64 = 940_487_400_000_003;
const A_MESSAGE_ID: u64 = 940_487_400_000_011;
const B_MESSAGE_ID: u64 = 940_487_400_000_012;
const FIRST_RESPONSE_MESSAGE_ID: u64 = 940_487_400_000_021;

#[derive(Clone)]
struct DiscordMockState {
    placeholder_posts: Arc<AtomicUsize>,
    local_note_posts: Arc<AtomicUsize>,
    first_placeholder_arrived: Arc<Notify>,
    release_first_placeholder: Arc<Notify>,
    second_placeholder_arrived: Arc<Notify>,
    next_response_id: Arc<AtomicU64>,
}

impl DiscordMockState {
    fn new() -> Self {
        Self {
            placeholder_posts: Arc::new(AtomicUsize::new(0)),
            local_note_posts: Arc::new(AtomicUsize::new(0)),
            first_placeholder_arrived: Arc::new(Notify::new()),
            release_first_placeholder: Arc::new(Notify::new()),
            second_placeholder_arrived: Arc::new(Notify::new()),
            next_response_id: Arc::new(AtomicU64::new(FIRST_RESPONSE_MESSAGE_ID)),
        }
    }
}

fn discord_user_json(id: u64, name: &str, bot: bool) -> Value {
    json!({
        "id": id.to_string(),
        "username": name,
        "discriminator": "0",
        "global_name": null,
        "avatar": null,
        "bot": bot,
        "system": false,
        "mfa_enabled": false,
        "banner": null,
        "accent_color": null,
        "locale": null,
        "verified": null,
        "email": null,
        "flags": 0,
        "premium_type": 0,
        "public_flags": 0,
        "member": null,
        "primary_guild": null,
        "avatar_decoration_data": null,
        "collectibles": null
    })
}

fn private_channel_json() -> Value {
    json!({
        "id": CHANNEL_ID.to_string(),
        "last_message_id": null,
        "last_pin_timestamp": null,
        "type": 1,
        "recipients": [discord_user_json(USER_ID, "queue-user", false)]
    })
}

fn discord_message_json(id: u64, content: &str) -> Value {
    json!({
        "id": id.to_string(),
        "channel_id": CHANNEL_ID.to_string(),
        "author": discord_user_json(BOT_ID, "queue-bot", true),
        "content": content,
        "timestamp": "2026-07-26T00:00:00.000000+00:00",
        "edited_timestamp": null,
        "tts": false,
        "mention_everyone": false,
        "mentions": [],
        "mention_roles": [],
        "mention_channels": [],
        "attachments": [],
        "embeds": [],
        "reactions": [],
        "nonce": null,
        "pinned": false,
        "webhook_id": null,
        "type": 0,
        "activity": null,
        "application": null,
        "application_id": null,
        "message_reference": null,
        "flags": 0,
        "referenced_message": null,
        "message_snapshots": [],
        "interaction": null,
        "interaction_metadata": null,
        "thread": null,
        "components": [],
        "sticker_items": [],
        "position": null,
        "role_subscription_data": null,
        "guild_id": null,
        "member": null,
        "poll": null
    })
}

async fn get_channel(Path(_channel_id): Path<u64>) -> Json<Value> {
    Json(private_channel_json())
}

async fn discord_rest(State(state): State<DiscordMockState>, request: Request<Body>) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    if method == Method::GET && path == format!("/api/v10/channels/{CHANNEL_ID}") {
        return Json(private_channel_json()).into_response();
    }
    if method == Method::POST && path == format!("/api/v10/channels/{CHANNEL_ID}/messages") {
        let body = match axum::body::to_bytes(request.into_body(), 1024 * 1024).await {
            Ok(body) => body,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"message": error.to_string(), "code": 0})),
                )
                    .into_response();
            }
        };
        let payload: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
        let content = payload
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if content == "..." {
            let index = state.placeholder_posts.fetch_add(1, Ordering::SeqCst);
            if index == 0 {
                state.first_placeholder_arrived.notify_waiters();
                state.release_first_placeholder.notified().await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"message": "release A", "code": 0})),
                )
                    .into_response();
            }
            state.second_placeholder_arrived.notify_waiters();
        } else {
            state.local_note_posts.fetch_add(1, Ordering::SeqCst);
        }
        let id = state.next_response_id.fetch_add(1, Ordering::SeqCst);
        return (StatusCode::OK, Json(discord_message_json(id, &content))).into_response();
    }

    if (method == Method::PUT || method == Method::DELETE)
        && path.starts_with(&format!("/api/v10/channels/{CHANNEL_ID}/messages/"))
        && path.contains("/reactions/")
    {
        return StatusCode::NO_CONTENT.into_response();
    }
    if method == Method::DELETE
        && path.starts_with(&format!("/api/v10/channels/{CHANNEL_ID}/messages/"))
    {
        return StatusCode::NO_CONTENT.into_response();
    }
    if method == Method::GET && path.starts_with("/api/v10/users/") {
        return Json(discord_user_json(USER_ID, "queue-user", false)).into_response();
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({"message": format!("unhandled {method} {path}"), "code": 0})),
    )
        .into_response()
}

async fn gateway_socket(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(|mut socket| async move { while socket.recv().await.is_some() {} })
}

#[derive(Debug, PartialEq, Eq)]
enum PromotedBCompletionProgress {
    AwaitingMailboxRelease,
    AwaitingQueueEligible,
    Complete,
}

fn validate_promoted_b_completion_lifecycle(
    events: &[super::super::turn_completion_events::TurnCompletionEvent],
    channel_id: ChannelId,
) -> Result<PromotedBCompletionProgress, String> {
    let expected_mailbox_release =
        super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
            channel_id,
            Some(B_MESSAGE_ID),
        );
    let expected_queue_eligible =
        super::super::turn_completion_events::TurnCompletionEvent::queue_eligible(
            channel_id,
            Some(B_MESSAGE_ID),
        );
    match events {
        [] => Ok(PromotedBCompletionProgress::AwaitingMailboxRelease),
        [event] if *event == expected_mailbox_release => {
            Ok(PromotedBCompletionProgress::AwaitingQueueEligible)
        }
        [event] => Err(format!(
            "promoted B must publish MailboxReleased first; received phase={:?}, turn_id={:?}, channel_id={}",
            event.phase, event.turn_id, event.channel_id
        )),
        [mailbox_release, queue_eligible]
            if *mailbox_release == expected_mailbox_release
                && *queue_eligible == expected_queue_eligible =>
        {
            Ok(PromotedBCompletionProgress::Complete)
        }
        [_, event, ..] => Err(format!(
            "promoted B must publish exactly one MailboxReleased followed by exactly one QueueEligible; received phase={:?}, turn_id={:?}, channel_id={}",
            event.phase, event.turn_id, event.channel_id
        )),
    }
}

async fn drain_promoted_b_completion_lifecycle(
    rx: &mut tokio::sync::broadcast::Receiver<
        super::super::turn_completion_events::TurnCompletionEvent,
    >,
    channel_id: ChannelId,
    timeout: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut events = Vec::with_capacity(2);

    loop {
        let event = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => panic!(
                "promoted B completion lifecycle did not reach QueueEligible before the deadline; observed_events={events:?}"
            ),
            Ok(Ok(event)) => event,
            Ok(Err(error)) => panic!(
                "completion receiver must remain open while draining promoted B; recv error={error:?}"
            ),
        };
        events.push(event);
        match validate_promoted_b_completion_lifecycle(&events, channel_id) {
            Ok(PromotedBCompletionProgress::Complete) => return,
            Ok(PromotedBCompletionProgress::AwaitingMailboxRelease)
            | Ok(PromotedBCompletionProgress::AwaitingQueueEligible) => {}
            Err(error) => panic!("{error}"),
        }
    }
}

async fn assert_no_local_only_completion_event(
    rx: &mut tokio::sync::broadcast::Receiver<
        super::super::turn_completion_events::TurnCompletionEvent,
    >,
    window: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => return,
            Ok(Ok(event)) => panic!(
                "local-only halves must not publish a completion event; received phase={:?}, turn_id={:?}, channel_id={}",
                event.phase, event.turn_id, event.channel_id
            ),
            Ok(Err(error)) => {
                panic!("local-only completion receiver must remain open; recv error={error:?}")
            }
        }
    }
}

async fn assert_local_only_completion_lifecycle(
    rx: &mut tokio::sync::broadcast::Receiver<
        super::super::turn_completion_events::TurnCompletionEvent,
    >,
    channel_id: ChannelId,
    lifecycle_timeout: std::time::Duration,
    strict_window: std::time::Duration,
) {
    drain_promoted_b_completion_lifecycle(rx, channel_id, lifecycle_timeout).await;
    assert_no_local_only_completion_event(rx, strict_window).await;
}

async fn completion_guard_must_panic(
    events: Vec<super::super::turn_completion_events::TurnCompletionEvent>,
) {
    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    for event in events {
        tx.send(event).expect("completion receiver registered");
    }
    let task = tokio::spawn(async move {
        assert_local_only_completion_lifecycle(
            &mut rx,
            ChannelId::new(CHANNEL_ID),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
        )
        .await;
    });
    let error = task
        .await
        .expect_err("the completion lifecycle guard must reject the mutation");
    assert!(
        error.is_panic(),
        "completion lifecycle rejection must terminate through its own assertion"
    );
}

#[test]
fn promoted_b_completion_lifecycle_validates_order_and_cardinality() {
    let channel_id = ChannelId::new(CHANNEL_ID);
    let mailbox_release =
        super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
            channel_id,
            Some(B_MESSAGE_ID),
        );
    let queue_eligible = super::super::turn_completion_events::TurnCompletionEvent::queue_eligible(
        channel_id,
        Some(B_MESSAGE_ID),
    );

    assert_eq!(
        validate_promoted_b_completion_lifecycle(
            &[mailbox_release.clone(), queue_eligible.clone()],
            channel_id,
        ),
        Ok(PromotedBCompletionProgress::Complete)
    );
    assert!(
        validate_promoted_b_completion_lifecycle(
            &[queue_eligible.clone(), mailbox_release.clone()],
            channel_id,
        )
        .is_err(),
        "QueueEligible before MailboxReleased must be rejected"
    );
    assert!(
        validate_promoted_b_completion_lifecycle(
            &[mailbox_release.clone(), mailbox_release],
            channel_id,
        )
        .is_err(),
        "duplicate MailboxReleased must be rejected"
    );
    assert!(
        validate_promoted_b_completion_lifecycle(&[queue_eligible], channel_id).is_err(),
        "QueueEligible without MailboxReleased must be rejected"
    );
}

#[tokio::test]
async fn completion_lifecycle_rejects_duplicate_mailbox_release() {
    let channel_id = ChannelId::new(CHANNEL_ID);
    completion_guard_must_panic(vec![
        super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
        super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
        super::super::turn_completion_events::TurnCompletionEvent::queue_eligible(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
    ])
    .await;
}

#[tokio::test]
async fn completion_lifecycle_rejects_event_after_queue_eligible() {
    let channel_id = ChannelId::new(CHANNEL_ID);
    completion_guard_must_panic(vec![
        super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
        super::super::turn_completion_events::TurnCompletionEvent::queue_eligible(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
        super::super::turn_completion_events::TurnCompletionEvent::queue_eligible(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
    ])
    .await;
}

#[tokio::test]
async fn completion_lifecycle_rejects_queue_eligible_before_mailbox_release() {
    let channel_id = ChannelId::new(CHANNEL_ID);
    completion_guard_must_panic(vec![
        super::super::turn_completion_events::TurnCompletionEvent::queue_eligible(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
        super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
            channel_id,
            Some(B_MESSAGE_ID),
        ),
    ])
    .await;
}

async fn start_mock_discord(
    state: DiscordMockState,
) -> (String, String, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/gateway", get(gateway_socket))
        .route("/api/v10/channels/{channel_id}", get(get_channel))
        .fallback(discord_rest)
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock Discord");
    let address = listener.local_addr().expect("mock Discord address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock Discord");
    });
    (
        format!("http://{address}"),
        format!("ws://{address}/gateway"),
        server,
    )
}

struct NoopFramework;

#[async_trait::async_trait]
impl serenity::framework::Framework for NoopFramework {
    async fn dispatch(&self, _ctx: serenity::Context, _event: serenity::FullEvent) {}
}

async fn serenity_context(proxy: String, gateway_url: String) -> serenity::Context {
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(proxy)
            .ratelimiter_disabled(true)
            .build(),
    );
    let cache = Arc::new(Cache::new());
    let mut ready: serenity::ReadyEvent = serde_json::from_value(json!({
        "v": 10,
        "user": discord_user_json(BOT_ID, "queue-bot", true),
        "guilds": [],
        "session_id": "queue-wake-e2e",
        "resume_gateway_url": gateway_url,
        "shard": [0, 1],
        "application": {"id": BOT_ID.to_string(), "flags": 0}
    }))
    .expect("ready fixture");
    cache.update(&mut ready);

    let data = Arc::new(tokio::sync::RwLock::new(serenity::prelude::TypeMap::new()));
    let ws_url = Arc::new(tokio::sync::Mutex::new(gateway_url));
    let framework: Arc<dyn serenity::framework::Framework> = Arc::new(NoopFramework);
    let (manager, _manager_result) =
        serenity::gateway::ShardManager::new(serenity::gateway::ShardManagerOptions {
            data: data.clone(),
            event_handlers: vec![],
            raw_event_handlers: vec![],
            framework: Arc::new(std::sync::OnceLock::from(framework)),
            shard_index: 0,
            shard_init: 0,
            shard_total: 1,
            voice_manager: None,
            ws_url: ws_url.clone(),
            cache: cache.clone(),
            http: http.clone(),
            intents: serenity::GatewayIntents::DIRECT_MESSAGES
                | serenity::GatewayIntents::MESSAGE_CONTENT,
            presence: None,
        });
    let shard = serenity::gateway::Shard::new(
        ws_url,
        "test-token",
        serenity::model::gateway::ShardInfo {
            id: serenity::ShardId(0),
            total: 1,
        },
        serenity::GatewayIntents::DIRECT_MESSAGES | serenity::GatewayIntents::MESSAGE_CONTENT,
        None,
    )
    .await
    .expect("test shard");
    let runner = serenity::gateway::ShardRunner::new(serenity::gateway::ShardRunnerOptions {
        data: data.clone(),
        event_handlers: vec![],
        raw_event_handlers: vec![],
        framework: Some(Arc::new(NoopFramework)),
        manager,
        shard,
        voice_manager: None,
        cache: cache.clone(),
        http: http.clone(),
    });

    serenity::Context {
        data,
        shard: serenity::ShardMessenger::new(&runner),
        shard_id: serenity::ShardId(0),
        http,
        cache,
    }
}

fn test_message(id: u64, text: &str) -> serenity::Message {
    let mut message = serenity::Message::default();
    message.id = MessageId::new(id);
    message.channel_id = ChannelId::new(CHANNEL_ID);
    message.author.id = UserId::new(USER_ID);
    message.author.name = "queue-user".to_string();
    message.content = text.to_string();
    message.timestamp = message.id.created_at();
    message
}

fn test_watcher_handle(
    tmux_session_name: &str,
    output_path: &std::path::Path,
) -> super::super::TmuxWatcherHandle {
    super::super::TmuxWatcherHandle {
        tmux_session_name: tmux_session_name.to_string(),
        output_path: output_path.display().to_string(),
        paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
            super::super::tmux_watcher_now_ms(),
        )),
    }
}

struct AbortOnDrop<T>(Option<tokio::task::JoinHandle<T>>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

async fn wait_until(
    timeout: std::time::Duration,
    mut predicate: impl FnMut() -> BoxFuture<'static, bool>,
) -> bool {
    tokio::time::timeout(timeout, async {
        loop {
            if predicate().await {
                return;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_model_observation_wakes_idle_durable_queue_through_production_workers() {
    let env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("isolated AgentDesk root");
    let _root_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let _intake_mode_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "ADK_INTAKE_ROUTING_MODE",
        std::path::Path::new("disabled"),
    );
    let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let state = DiscordMockState::new();
    let (proxy, gateway_url, server) = start_mock_discord(state.clone()).await;
    let _server_guard = AbortOnDrop(Some(server));
    let ctx = serenity_context(proxy, gateway_url).await;
    let shared = super::super::make_shared_data_for_tests();
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(USER_ID);
        settings.allow_all_users = true;
    }
    let voice_config = crate::voice::VoiceConfig::default();
    let data = Data {
        shared: shared.clone(),
        token: "test-token".to_string(),
        provider: ProviderKind::Claude,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
    };
    let channel_id = ChannelId::new(CHANNEL_ID);
    let cwd = root.path().to_str().expect("utf8 test root");
    super::super::rebind_channel_session(
        &shared,
        &ProviderKind::Claude,
        channel_id,
        cwd,
        "48740000-0000-0000-0000-000000000001",
    )
    .await;

    let a_event = serenity::FullEvent::Message {
        new_message: test_message(A_MESSAGE_ID, "A holds the active mailbox"),
    };
    let mut a_task = tokio::spawn({
        let ctx = ctx.clone();
        let data = Data {
            shared: data.shared.clone(),
            token: data.token.clone(),
            provider: data.provider.clone(),
            voice_config: data.voice_config.clone(),
            voice_receiver: data.voice_receiver.clone(),
        };
        async move { super::super::router::handle_event(&ctx, &a_event, &data).await }
    });
    tokio::select! {
        _ = state.first_placeholder_arrived.notified() => {}
        result = &mut a_task => {
            let snapshot = mailbox_snapshot(&shared, channel_id).await;
            let checkpoint = shared.last_message_ids.get(&channel_id).map(|entry| *entry);
            panic!(
                "A exited before the placeholder POST: {result:?}; current_bot={}; active={:?}; queue_len={}; checkpoint={checkpoint:?}",
                ctx.cache.current_user().id,
                snapshot.active_user_message_id,
                snapshot.intervention_queue.len()
            )
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
            panic!("A did not reach the real placeholder POST")
        }
    }
    let _a_guard = AbortOnDrop(Some(a_task));

    let active_a = mailbox_snapshot(&shared, channel_id).await;
    assert_eq!(
        active_a.active_user_message_id,
        Some(MessageId::new(A_MESSAGE_ID))
    );

    let b_event = serenity::FullEvent::Message {
        new_message: test_message(B_MESSAGE_ID, "B must survive as durable pending intake"),
    };
    super::super::router::handle_event(&ctx, &b_event, &data)
        .await
        .expect("B traverses FullEvent intake");
    let queued_b = mailbox_snapshot(&shared, channel_id).await;
    assert_eq!(
        queued_b.active_user_message_id,
        Some(MessageId::new(A_MESSAGE_ID))
    );
    assert_eq!(queued_b.intervention_queue.len(), 1);
    assert_eq!(
        queued_b.intervention_queue[0].message_id,
        MessageId::new(B_MESSAGE_ID)
    );
    let (durable_b, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &ProviderKind::Claude,
        &shared.token_hash,
        channel_id,
    );
    assert_eq!(
        durable_b.len(),
        1,
        "B must be durably persisted by BusyActiveTurn"
    );
    assert_eq!(durable_b[0].message_id, MessageId::new(B_MESSAGE_ID));

    // Subscribe before the release that makes A's placeholder-failure recovery publish its
    // completion event. `broadcast` only buffers sends that happen after a receiver exists, so
    // subscribing later left the negative assertion below racing A's normal `QueueEligible`
    // publish instead of observing the local-only halves.
    let mut a_release_rx =
        super::super::turn_completion_events::subscribe_turn_completion_events(&shared);

    state.release_first_placeholder.notify_waiters();
    assert!(
        wait_until(std::time::Duration::from_secs(1), {
            let shared = shared.clone();
            move || {
                let shared = shared.clone();
                Box::pin(async move {
                    let snapshot = mailbox_snapshot(&shared, ChannelId::new(CHANNEL_ID)).await;
                    snapshot.active_user_message_id.is_none()
                        && snapshot.intervention_queue.len() == 1
                        && shared
                            .restart
                            .deferred_hook_channels
                            .contains_key(&ChannelId::new(CHANNEL_ID))
                })
            }
        })
        .await,
        "production placeholder-failure recovery must leave idle durable B behind an armed normal worker"
    );

    // Drain A's release event explicitly so the local-only assertion below observes only the
    // `/model` halves rather than whatever A left buffered.
    let a_release = tokio::time::timeout(std::time::Duration::from_secs(2), a_release_rx.recv())
        .await
        .expect("A placeholder-failure release must publish one completion event")
        .expect("completion bus open");
    assert_eq!(a_release.channel_id, channel_id);

    let before_model = mailbox_snapshot(&shared, channel_id).await;
    assert!(before_model.active_user_message_id.is_none());
    assert_eq!(
        before_model.intervention_queue[0].message_id,
        MessageId::new(B_MESSAGE_ID)
    );

    shared
        .http
        .cached_serenity_ctx
        .set(ctx.clone())
        .expect("cache test Serenity context");
    shared
        .http
        .cached_bot_token
        .set("test-token".to_string())
        .expect("cache test bot token");
    let transcript_path = root.path().join("claude-queue-wake.jsonl");
    std::fs::write(&transcript_path, "").expect("write watcher transcript");
    let tmux = "AgentDesk-claude-4874-local-model-wake";
    shared
        .tmux_watchers
        .insert(channel_id, test_watcher_handle(tmux, &transcript_path));
    super::spawn_tui_prompt_relay(shared.clone(), ProviderKind::Claude);

    // Subscribe after draining A's release event so every edge after the local-only observations is
    // inspected, including promoted B's known two-phase completion lifecycle.
    let mut local_only_completion_rx =
        super::super::turn_completion_events::subscribe_turn_completion_events(&shared);
    let command_half = "<command-message>x</command-message>\n<command-name>/model</command-name>";
    let stdout_half = "<local-command-stdout>Set model to Fable 5</local-command-stdout>";
    assert_eq!(
        crate::services::tui_prompt_dedupe::observe_prompt_by_provider_session(
            "claude",
            tmux,
            command_half,
        ),
        crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
    );
    assert_eq!(
        crate::services::tui_prompt_dedupe::observe_prompt_by_provider_session(
            "claude",
            tmux,
            stdout_half,
        ),
        crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
    );

    tokio::time::timeout(
        std::time::Duration::from_millis(1500),
        state.second_placeholder_arrived.notified(),
    )
    .await
    .expect("local /model must wake the occupied two-second deferred worker");

    let promoted_b = mailbox_snapshot(&shared, channel_id).await;
    assert_eq!(
        promoted_b.active_user_message_id,
        Some(MessageId::new(B_MESSAGE_ID))
    );
    assert!(promoted_b.intervention_queue.is_empty());
    let (durable_after, _) =
        crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
            &ProviderKind::Claude,
            &shared.token_hash,
            channel_id,
        );
    assert!(
        durable_after.is_empty(),
        "production kickoff must durably dequeue B"
    );
    assert_eq!(state.placeholder_posts.load(Ordering::SeqCst), 2);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        state.placeholder_posts.load(Ordering::SeqCst),
        2,
        "coalesced two-half wake must not dispatch B twice"
    );
    assert_eq!(state.local_note_posts.load(Ordering::SeqCst), 2);

    assert!(
        !crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
            "claude", tmux, CHANNEL_ID,
        )
    );
    assert!(
        !crate::services::tui_prompt_dedupe::is_ssh_direct_observation_pending("claude", tmux,)
    );
    assert_eq!(
        crate::services::tui_prompt_dedupe::prompt_anchor_for_response("claude", tmux, CHANNEL_ID,),
        None,
    );
    let inflight =
        super::super::inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL_ID);
    assert!(
        !super::tui_direct_watcher_synthetic_inflight_matches(inflight.as_ref(), tmux, 1),
        "local-only halves must not create synthetic inflight ownership"
    );

    // Source tracing reproduced MailboxReleased(B) 3/3 as promoted B's normal Discord bridge
    // lifecycle: TerminalEvent::Complete with FinalizeContext::bridge(),
    // request_owner_name="queue-user", is_external_input_tui_direct=false, and no TUI runtime.
    // CompletionAdmission::claim_queue_eligible then emits QueueEligible(B) exactly once after its
    // mailbox-released and terminal barriers settle. Inspect that exact ordered pair before opening
    // the strict window. The idle-queue consumer continues without dispatch on MailboxReleased; the
    // other production consumer can only stop B's typing indicator. #5018 tracks causal origin to
    // close the remaining value-only gap where a faulty local-only publisher replaces, rather than
    // duplicates, one of B's own lifecycle edges.
    assert_local_only_completion_lifecycle(
        &mut local_only_completion_rx,
        channel_id,
        std::time::Duration::from_secs(10),
        std::time::Duration::from_millis(100),
    )
    .await;

    drop(_dedupe_guard);
    drop(_intake_mode_guard);
    drop(_root_guard);
    drop(env_lock);
}
