//! Mock Discord transport for the relay e2e harness: a loopback REST + gateway
//! server, and a real `serenity::Context` pointed at it.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use poise::serenity_prelude as serenity;
use serde_json::{Value, json};
use serenity::cache::Cache;
use serenity::{ChannelId, MessageId, UserId};
use tokio::sync::Notify;

pub(in crate::services::discord::tui_prompt_relay) const CHANNEL_ID: u64 = 940_487_400_000_001;
pub(super) const USER_ID: u64 = 940_487_400_000_002;
pub(super) const BOT_ID: u64 = 940_487_400_000_003;
const FIRST_RESPONSE_MESSAGE_ID: u64 = 940_487_400_000_021;

/// Counters and gates over the mock's message endpoint. A `"..."` body is the
/// relay's placeholder post, which the harness uses as its dispatch witness;
/// the first one parks until released so a second turn can queue behind an
/// occupied mailbox.
///
/// `unhandled` records every request that reached the 404 fallback, so a
/// production call the mock cannot answer fails an assertion instead of
/// silently degrading a scenario into a weaker one.
#[derive(Clone)]
pub(super) struct DiscordMockState {
    pub(super) placeholder_posts: Arc<AtomicUsize>,
    pub(super) local_note_posts: Arc<AtomicUsize>,
    pub(super) first_placeholder_arrived: Arc<Notify>,
    pub(super) release_first_placeholder: Arc<Notify>,
    pub(super) unhandled: Arc<Mutex<Vec<String>>>,
    /// Channel history `GET /messages` pages over. Empty until a scenario
    /// seeds it, which is the "nothing to catch up" answer.
    pub(super) history: Arc<Mutex<Vec<Value>>>,
    pub(super) history_queries: Arc<Mutex<Vec<HistoryQuery>>>,
    next_response_id: Arc<AtomicU64>,
}

impl DiscordMockState {
    pub(super) fn new() -> Self {
        Self {
            placeholder_posts: Arc::new(AtomicUsize::new(0)),
            local_note_posts: Arc::new(AtomicUsize::new(0)),
            first_placeholder_arrived: Arc::new(Notify::new()),
            release_first_placeholder: Arc::new(Notify::new()),
            unhandled: Arc::new(Mutex::new(Vec::new())),
            history: Arc::new(Mutex::new(Vec::new())),
            history_queries: Arc::new(Mutex::new(Vec::new())),
            next_response_id: Arc::new(AtomicU64::new(FIRST_RESPONSE_MESSAGE_ID)),
        }
    }
}

/// A `GET /messages` query. Serenity sends at most one cursor; `around` does
/// not deserialize, so a query the mock cannot page reaches the 404 fallback.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::services::discord::tui_prompt_relay) struct HistoryQuery {
    pub(in crate::services::discord::tui_prompt_relay) limit: Option<usize>,
    pub(in crate::services::discord::tui_prompt_relay) before: Option<u64>,
    pub(in crate::services::discord::tui_prompt_relay) after: Option<u64>,
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

pub(super) fn discord_message_json(id: u64, content: &str) -> Value {
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

/// A history entry as `catch_up` phase 2 sees it: bot posts anchor the "last
/// answered" boundary, user posts are the unanswered candidates ahead of it.
pub(super) fn history_message_json(id: u64, content: &str, bot: bool) -> Value {
    let mut message = discord_message_json(id, content);
    if !bot {
        message["author"] = discord_user_json(USER_ID, "queue-user", false);
    }
    message
}

fn history_message_id(message: &Value) -> u64 {
    message["id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .expect("history message id")
}

/// Discord's page shape: the `limit` messages nearest the cursor (the newest
/// without one), always newest first. `None` for both cursors or a `limit`
/// outside Discord's documented 1..=100, whose real answer is unmeasured.
fn history_page(history: &[Value], query: &HistoryQuery) -> Option<Vec<Value>> {
    let limit = query.limit.unwrap_or(50);
    if (query.before.is_some() && query.after.is_some()) || !(1..=100).contains(&limit) {
        return None;
    }
    let mut page: Vec<Value> = history
        .iter()
        .filter(|message| {
            let id = history_message_id(message);
            query.before.is_none_or(|before| id < before)
                && query.after.is_none_or(|after| id > after)
        })
        .cloned()
        .collect();
    page.sort_by_key(|message| std::cmp::Reverse(history_message_id(message)));
    let keep = page.len().min(limit);
    // `after` pages forward from the cursor, so it keeps the oldest end.
    if query.after.is_some() {
        page.drain(..page.len() - keep);
    } else {
        page.truncate(keep);
    }
    Some(page)
}

/// Pins the page contract `catch_up` and `recovery_text` read: which ids, and
/// newest first, since both index into the page assuming that order.
#[test]
fn history_page_returns_the_ids_nearest_the_cursor_newest_first() {
    // Seeded out of order so the page order comes from the mock, not the seed.
    let history: Vec<Value> = [3, 1, 4, 10, 5, 9, 2, 6, 8, 7]
        .into_iter()
        .map(|id| history_message_json(id, "m", true))
        .collect();
    let page = |limit, before, after| {
        history_page(
            &history,
            &HistoryQuery {
                limit,
                before,
                after,
            },
        )
        .map(|page| page.iter().map(history_message_id).collect::<Vec<u64>>())
    };
    let cases: [(Option<usize>, Option<u64>, Option<u64>, Option<Vec<u64>>); 12] = [
        (Some(3), None, None, Some(vec![10, 9, 8])),
        (None, None, None, Some(vec![10, 9, 8, 7, 6, 5, 4, 3, 2, 1])),
        (Some(2), Some(5), None, Some(vec![4, 3])),
        (Some(5), Some(3), None, Some(vec![2, 1])),
        (Some(5), Some(1), None, Some(vec![])),
        (Some(2), None, Some(5), Some(vec![7, 6])),
        (Some(5), None, Some(8), Some(vec![10, 9])),
        (Some(5), None, Some(10), Some(vec![])),
        (
            Some(100),
            None,
            None,
            Some(vec![10, 9, 8, 7, 6, 5, 4, 3, 2, 1]),
        ),
        (Some(2), Some(9), Some(3), None),
        (Some(0), None, None, None),
        (Some(101), None, None, None),
    ];
    for (limit, before, after, expected) in cases {
        assert_eq!(
            page(limit, before, after),
            expected,
            "limit={limit:?} before={before:?} after={after:?}"
        );
    }
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
        } else {
            state.local_note_posts.fetch_add(1, Ordering::SeqCst);
        }
        let id = state.next_response_id.fetch_add(1, Ordering::SeqCst);
        return (StatusCode::OK, Json(discord_message_json(id, &content))).into_response();
    }

    // `catch_up` reads this before it can reach its dedup branch; an unseeded
    // channel answers "nothing to catch up" rather than an error.
    if method == Method::GET && path == format!("/api/v10/channels/{CHANNEL_ID}/messages") {
        if let Ok(Query(query)) = Query::<HistoryQuery>::try_from_uri(request.uri()) {
            state
                .history_queries
                .lock()
                .expect("history queries")
                .push(query.clone());
            let history = state.history.lock().expect("mock history");
            if let Some(page) = history_page(&history, &query) {
                return Json(Value::Array(page)).into_response();
            }
        }
    }
    if method == Method::PATCH
        && path.starts_with(&format!("/api/v10/channels/{CHANNEL_ID}/messages/"))
    {
        let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        let payload: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
        let content = payload
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = path.rsplit('/').next().and_then(|tail| tail.parse().ok());
        let id = id.unwrap_or(FIRST_RESPONSE_MESSAGE_ID);
        return Json(discord_message_json(id, content)).into_response();
    }
    if method == Method::POST && path == format!("/api/v10/channels/{CHANNEL_ID}/typing") {
        return StatusCode::NO_CONTENT.into_response();
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
    // `catch_up` resolves the bot identity here and skips every candidate whose
    // author matches it, so `/users/@me` must not answer with the human.
    if method == Method::GET && path == "/api/v10/users/@me" {
        return Json(discord_user_json(BOT_ID, "queue-bot", true)).into_response();
    }
    if method == Method::GET && path.starts_with("/api/v10/users/") {
        return Json(discord_user_json(USER_ID, "queue-user", false)).into_response();
    }

    state
        .unhandled
        .lock()
        .expect("unhandled log")
        .push(format!("{method} {path}"));
    (
        StatusCode::NOT_FOUND,
        Json(json!({"message": format!("unhandled {method} {path}"), "code": 0})),
    )
        .into_response()
}

async fn gateway_socket(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(|mut socket| async move { while socket.recv().await.is_some() {} })
}

pub(super) async fn start(
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

pub(super) async fn serenity_context(proxy: String, gateway_url: String) -> serenity::Context {
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

/// An inbound user message on the fixture's private channel.
pub(in crate::services::discord) fn user_message(id: u64, text: &str) -> serenity::Message {
    let mut message = serenity::Message::default();
    message.id = MessageId::new(id);
    message.channel_id = ChannelId::new(CHANNEL_ID);
    message.author.id = UserId::new(USER_ID);
    message.author.name = "queue-user".to_string();
    message.content = text.to_string();
    message.timestamp = message.id.created_at();
    message
}
