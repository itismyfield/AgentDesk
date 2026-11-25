//! A stored request is posted once with disabled buttons under an already guarded forum; a lost
//! answer is settled by finding that post, a refusal is kept, and neither is posted again.
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use poise::serenity_prelude as serenity;
use serde_json::{Value, json};

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::decision_forums::{ForumState, store as store_forum};
use crate::services::discord::health::owner_runtime_for_tests;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;

const GUILD: u64 = 96_326_000_998;
const CATEGORY: u64 = 940_821_200_000_001;
const CHANNEL: u64 = 940_821_200_000_002;
const BARE: u64 = 940_821_200_000_003;
const OWNER: u64 = 940_821_200_000_009;

/// How the mock forum answers a post: it keeps the post and loses the answer, or refuses it.
#[derive(Clone, Copy)]
enum Answer {
    Lost,
    Forbidden,
}

struct Mock {
    ctx: Option<serenity::Context>,
    forum: u64,
    answer: Answer,
    posts: Vec<Value>,
    guarded_at_post: Vec<bool>,
    /// Threads in the forum: (id, starter author, starter content).
    threads: Vec<(u64, u64, String)>,
}

type Shared = Arc<Mutex<Mock>>;

fn channel(id: u64, kind: u8, parent: Option<u64>) -> Value {
    json!({"id": id.to_string(), "type": kind, "guild_id": GUILD.to_string(), "name": "decision",
        "parent_id": parent.map(|id| id.to_string()), "position": 0, "permission_overwrites": []})
}

async fn discord(State(mock): State<Shared>, method: Method, uri: Uri, body: Bytes) -> Response {
    let path = uri.path().trim_start_matches("/api/v10/").to_string();
    let parts: Vec<&str> = path.split('/').collect();
    fn id(text: &str) -> u64 {
        text.parse().unwrap_or(0)
    }
    let forum = mock.lock().unwrap().forum;
    match (method, parts.as_slice()) {
        (Method::GET, ["channels", c]) if id(c) == CHANNEL => {
            axum::Json(channel(CHANNEL, 0, Some(CATEGORY))).into_response()
        }
        (Method::GET, ["channels", c]) if id(c) == BARE => {
            axum::Json(channel(BARE, 0, None)).into_response()
        }
        (Method::GET, ["channels", c]) if id(c) == forum => {
            axum::Json(channel(forum, 15, Some(CATEGORY))).into_response()
        }
        (Method::POST, ["channels", c, "threads"]) if id(c) == forum => {
            let ctx = mock.lock().unwrap().ctx.clone().unwrap();
            let mut probe = discord_mock::user_message(forum + 50, "first reply");
            probe.guild_id = Some(serenity::GuildId::new(GUILD));
            let parent = Some(serenity::ChannelId::new(forum));
            let guarded = ingress::consumes(&ctx, None, &probe, parent).await;
            let post: Value = serde_json::from_slice(&body).unwrap();
            let mut mock = mock.lock().unwrap();
            mock.guarded_at_post.push(guarded);
            let content = post["message"]["content"].as_str().unwrap().to_string();
            mock.posts.push(post);
            match mock.answer {
                Answer::Lost => {
                    let thread = forum + 100 + mock.threads.len() as u64;
                    mock.threads.push((thread, discord_mock::BOT_ID, content));
                    let lost = json!({"message": "answer lost", "code": 0});
                    (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(lost)).into_response()
                }
                Answer::Forbidden => {
                    let refused = json!({"message": "Missing Permissions", "code": 50013});
                    (StatusCode::FORBIDDEN, axum::Json(refused)).into_response()
                }
            }
        }
        (Method::GET, ["guilds", _, "threads", "active"]) => {
            let threads: Vec<Value> = (mock.lock().unwrap().threads.iter())
                .map(|(thread, author, _)| {
                    let mut thread_json = channel(*thread, 11, Some(forum));
                    thread_json["owner_id"] = json!(author.to_string());
                    thread_json
                })
                .collect();
            axum::Json(json!({"threads": threads, "members": []})).into_response()
        }
        (Method::GET, ["channels", c, "messages", m]) if c == m => {
            let mock = mock.lock().unwrap();
            let Some((thread, author, content)) = mock.threads.iter().find(|t| t.0 == id(c)) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            axum::Json(
                json!({"id": thread.to_string(), "channel_id": thread.to_string(),
                "author": discord_mock::discord_user_json(*author, "decision-bot", true),
                "content": content, "timestamp": "2026-11-25T00:00:00.000000+00:00",
                "edited_timestamp": null, "tts": false, "mention_everyone": false,
                "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
                "pinned": false, "type": 0}),
            )
            .into_response()
        }
        _ => {
            let unknown = json!({"message": "unexpected fixture endpoint", "code": 0});
            (StatusCode::NOT_FOUND, axum::Json(unknown)).into_response()
        }
    }
}

fn ask(request_ref: &str) -> Ask {
    serde_json::from_value(json!({"agent_id": "decision-asker",
    "target": {"channel_id": CHANNEL.to_string()}, "ref": request_ref, "title": "배포 순서",
    "context": "둘 중 하나를 고른다", "options": [
        {"label": "지금 배포", "detail": "오늘 낸다", "recommendation_percent": 60},
        {"label": "내일 배포", "detail": "하루 미룬다", "recommendation_percent": 40},
    ]}))
    .unwrap()
}

/// Posts one request three times into a forum that answers `answer`, then another request of a
/// channel in no category: what each call answered, the mock afterwards and the first request.
async fn publish_thrice(answer: Answer, forum: u64) -> (Vec<Outcome>, Shared, Uuid) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let ready = ForumState::Ready(forum.to_string());
    store_forum(&pool, GUILD, CATEGORY, "decision", &ready)
        .await
        .unwrap();
    let mock: Shared = Arc::new(Mutex::new(Mock {
        ctx: None,
        forum,
        answer,
        posts: Vec::new(),
        guarded_at_post: Vec::new(),
        threads: Vec::new(),
    }));
    // Another post of the same bot in the forum, which is not this request's.
    let other = format!("{}\n", card::marker(Uuid::new_v4()));
    (mock.lock().unwrap().threads).push((forum + 99, discord_mock::BOT_ID, other));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .fallback(discord)
        .with_state(mock.clone());
    let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (_, gateway_url, gateway) =
        discord_mock::start(discord_mock::DiscordMockState::new()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway_url).await;
    mock.lock().unwrap().ctx = Some(ctx.clone());
    let (registry, shared) = owner_runtime_for_tests::registered("claude").await;
    assert!(shared.http.cached_serenity_ctx.set(ctx.clone()).is_ok());
    let announce = UtilityBotRole::Announce;
    (registry.set_utility_bot_http_for_tests(announce, ctx.http.clone())).await;
    let config = DiscordConfig {
        owner_id: Some(OWNER),
        ..DiscordConfig::default()
    };
    let mut outcomes = Vec::new();
    let mut first = None;
    for (request_ref, channel, calls) in [("r1", CHANNEL, 3), ("bare", BARE, 1)] {
        let ask = ask(request_ref);
        let stored = super::super::store(&pool, &ask, (channel, channel), &"e".repeat(64));
        let (request_id, _) = stored.await.unwrap();
        first.get_or_insert(request_id);
        for _ in 0..calls {
            let asked = Asked {
                request_id,
                ask: &ask,
                logical_channel: channel,
                provider: &ProviderKind::Claude,
            };
            outcomes.push(publish(&pool, Some(&*registry), &config, asked).await);
        }
    }
    http.abort();
    gateway.abort();
    pool.close().await;
    db.drop().await;
    (outcomes, mock, first.unwrap())
}

/// The answer to the only POST is lost: the post is found again by its marker and author, the
/// forum was guarded before the POST, and nothing is posted a second time.
#[tokio::test]
async fn decision_post_whose_answer_is_lost_is_found_not_sent_again_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let forum = 940_821_200_100_000;
    let (outcomes, mock, request_id) = publish_thrice(Answer::Lost, forum).await;
    let thread = forum + 101;
    let posted = [
        Outcome::Pending,
        Outcome::Posted(thread),
        Outcome::Posted(thread),
        Outcome::NoCategory,
    ];
    assert_eq!(outcomes, posted);
    let mock = mock.lock().unwrap();
    assert_eq!(mock.posts.len(), 1, "one post for one request");
    assert_eq!(
        mock.guarded_at_post,
        [true],
        "the forum is guarded before the POST"
    );
    // The post is the disabled-button stage: every button disabled, carrying a Decision handle
    // manual steer refuses, and the notice that no selection is taken yet.
    let message = &mock.posts[0]["message"];
    let content = message["content"].as_str().unwrap();
    assert!(content.contains(card::NOT_YET) && content.contains(&format!("<@{OWNER}>")));
    assert!(content.lines().any(|line| line == card::marker(request_id)));
    assert_eq!(
        message["allowed_mentions"]["users"],
        json!([OWNER.to_string()])
    );
    let buttons = message["components"][0]["components"].as_array().unwrap();
    assert_eq!(buttons.len(), 2);
    for button in buttons {
        assert_eq!(button["disabled"], true);
        let custom_id = button["custom_id"].as_str().unwrap();
        assert!(custom_id.starts_with("decision:v1:"), "{custom_id}");
        let parsed = ActionHandle::parse_custom_id(custom_id);
        assert_eq!(
            parsed,
            Err(crate::services::manual_steer::action_handle::CustomIdError::NotManualSteer)
        );
    }
}

/// Discord refuses the post: the refusal is the answer every time and nothing is posted again;
/// a channel in no category is answered as such without a post.
#[tokio::test]
async fn decision_post_refused_is_not_sent_again_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let forum = 940_821_200_200_000;
    let (outcomes, mock, _) = publish_thrice(Answer::Forbidden, forum).await;
    let refused = Outcome::Refused("decision_post_forbidden".into());
    let expected = [
        refused.clone(),
        refused.clone(),
        refused,
        Outcome::NoCategory,
    ];
    assert_eq!(outcomes, expected);
    assert_eq!(mock.lock().unwrap().posts.len(), 1);
}
