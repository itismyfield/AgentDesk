//! A stored request is posted once with disabled buttons under an already guarded forum, only by a
//! connected bot; a lost answer finds that same post, a refusal is kept, neither is posted again.
use std::sync::atomic::Ordering;
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
use crate::db::decisions::AttemptState;
use crate::db::decisions::selection::{Chosen, Click};
use crate::services::discord::SharedData;
use crate::services::discord::health::owner_runtime_for_tests;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::manual_steer::action_handle::CustomIdError;

const GUILD: u64 = 96_326_000_998;
const CATEGORY: u64 = 940_821_200_000_001;
const CHANNEL: u64 = 940_821_200_000_002;
const BARE: u64 = 940_821_200_000_003;
const OWNER: u64 = 940_821_200_000_009;
const STRANGER: u64 = 940_821_200_000_008;
const BOT: u64 = discord_mock::BOT_ID;

/// How the mock forum answers a post: keeps it and loses the answer, keeps it and answers, or
/// refuses it.
#[derive(Clone, Copy)]
enum Answer {
    Lost,
    Created,
    Forbidden,
}

/// A thread in the forum: who started it, who wrote its starter, and what the starter says.
struct Thread {
    id: u64,
    owner: u64,
    author: u64,
    content: String,
}

struct Mock {
    ctx: Option<serenity::Context>,
    forum: u64,
    answer: Answer,
    posts: Vec<Value>,
    guarded_at_post: Vec<bool>,
    threads: Vec<Thread>,
    /// The starter edits taken, once a scenario lets the mock accept them.
    edits: Option<Vec<Value>>,
    /// The thread edits the announce bot made.
    seals: Vec<Value>,
}

type Shared = Arc<Mutex<Mock>>;

fn channel(id: u64, kind: u8, parent: Option<u64>) -> Value {
    json!({"id": id.to_string(), "type": kind, "guild_id": GUILD.to_string(), "name": "decision",
        "parent_id": parent.map(|id| id.to_string()), "position": 0, "permission_overwrites": []})
}

fn thread_json(thread: &Thread, forum: u64) -> Value {
    let mut json = channel(thread.id, 11, Some(forum));
    json["owner_id"] = json!(thread.owner.to_string());
    json
}

fn message_json(thread: &Thread) -> Value {
    json!({"id": thread.id.to_string(), "channel_id": thread.id.to_string(),
        "author": discord_mock::discord_user_json(thread.author, "decision-bot", true),
        "content": thread.content, "timestamp": "2026-11-25T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false,
        "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
        "pinned": false, "type": 0})
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
            let thread = Thread {
                id: forum + 100 + mock.posts.len() as u64,
                owner: BOT,
                author: BOT,
                content,
            };
            mock.posts.push(post);
            match mock.answer {
                Answer::Forbidden => {
                    let refused = json!({"message": "Missing Permissions", "code": 50013});
                    (StatusCode::FORBIDDEN, axum::Json(refused)).into_response()
                }
                Answer::Lost => {
                    mock.threads.push(thread);
                    let lost = json!({"message": "answer lost", "code": 0});
                    (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(lost)).into_response()
                }
                Answer::Created => {
                    let created = thread_json(&thread, forum);
                    mock.threads.push(thread);
                    (StatusCode::CREATED, axum::Json(created)).into_response()
                }
            }
        }
        (Method::GET, ["guilds", _, "threads", "active"]) => {
            let mock = mock.lock().unwrap();
            let threads: Vec<Value> = (mock.threads.iter())
                .map(|thread| thread_json(thread, forum))
                .collect();
            axum::Json(json!({"threads": threads, "members": []})).into_response()
        }
        (Method::GET, ["channels", c, "messages", m]) if c == m => {
            let mock = mock.lock().unwrap();
            let Some(thread) = mock.threads.iter().find(|thread| thread.id == id(c)) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            axum::Json(message_json(thread)).into_response()
        }
        (Method::PATCH, ["channels", c, "messages", m]) if c == m => {
            let edit: Value = serde_json::from_slice(&body).unwrap();
            let mut mock = mock.lock().unwrap();
            let mock = &mut *mock;
            let thread = mock.threads.iter_mut().find(|thread| thread.id == id(c));
            let (Some(edits), Some(thread)) = (mock.edits.as_mut(), thread) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            thread.content = edit["content"].as_str().unwrap().to_string();
            edits.push(edit);
            axum::Json(message_json(thread)).into_response()
        }
        (Method::PATCH, ["channels", c]) => {
            let seal: Value = serde_json::from_slice(&body).unwrap();
            let mut mock = mock.lock().unwrap();
            mock.seals.push(seal);
            let Some(thread) = mock.threads.iter().find(|thread| thread.id == id(c)) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            axum::Json(thread_json(thread, forum)).into_response()
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

/// One provider runtime whose gateway Context and REST client point at a mock forum, its bot
/// connected or not as the scenario sets it.
struct Rig {
    db: TestPostgresDb,
    pool: sqlx::PgPool,
    registry: Arc<HealthRegistry>,
    shared: Arc<SharedData>,
    config: DiscordConfig,
    mock: Shared,
    tasks: [tokio::task::JoinHandle<()>; 2],
}

impl Rig {
    async fn new(answer: Answer, forum: u64) -> Self {
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
            edits: None,
            seals: Vec::new(),
        }));
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
        let rig = Self {
            db,
            pool,
            registry,
            shared,
            config,
            mock,
            tasks: [http, gateway],
        };
        rig.connected(true);
        rig
    }

    fn connected(&self, up: bool) {
        self.shared.bot_connected.store(up, Ordering::SeqCst);
    }

    async fn store(&self, request_ref: &str, channel: u64) -> Uuid {
        let ask = ask(request_ref);
        let stored = super::super::store(&self.pool, &ask, (channel, channel), "e").await;
        stored.unwrap().0
    }

    async fn publish(&self, request_id: Uuid, request_ref: &str, channel: u64) -> Outcome {
        let ask = ask(request_ref);
        let asked = Asked {
            request_id,
            ask: &ask,
            logical_channel: channel,
            provider: &ProviderKind::Claude,
        };
        publish(&self.pool, Some(&*self.registry), &self.config, asked).await
    }

    async fn state(&self, request_id: Uuid) -> Publication {
        store::load(&self.pool, request_id).await.unwrap()
    }

    fn posts(&self) -> usize {
        self.mock.lock().unwrap().posts.len()
    }

    /// The starter edits the mock took; calling it lets the mock accept them from now on.
    fn edits(&self) -> Vec<Value> {
        let mut mock = self.mock.lock().unwrap();
        mock.edits.get_or_insert_with(Vec::new).clone()
    }

    async fn handle(&self, thread: u64, option: i16) -> String {
        let handle = sqlx::query_scalar(
            "SELECT o.handle FROM decision_options o JOIN decision_requests r USING (request_id)
             WHERE r.thread_id = $1 AND o.ordinal = $2",
        );
        let handle = handle.bind(thread.to_string()).bind(option);
        handle.fetch_one(&self.pool).await.unwrap()
    }

    /// The owner's click on `option` of the starter of `thread`.
    async fn click(&self, thread: u64, option: i16) -> Option<Chosen> {
        let handle = self.handle(thread, option).await;
        let click = Click {
            handle: &handle,
            thread_id: thread,
            message_id: thread,
            bot_user_id: BOT,
            actor: OWNER,
        };
        selection::choose_once(&self.pool, &click).await.unwrap()
    }

    async fn count(&self, table: &str) -> i64 {
        let count = format!("SELECT COUNT(*) FROM {table}");
        let count = sqlx::query_scalar(&count).fetch_one(&self.pool);
        count.await.unwrap()
    }

    async fn close(self) {
        self.tasks.iter().for_each(tokio::task::JoinHandle::abort);
        self.pool.close().await;
        self.db.drop().await;
    }
}

/// A handed-back gateway (Context kept) posts nothing; connected, the bot posts once, loses the
/// answer, finds that post by owner, author and final marker, and never posts again.
#[tokio::test]
async fn decision_post_whose_answer_is_lost_is_found_not_sent_again_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let forum = 940_821_200_100_000;
    let rig = Rig::new(Answer::Lost, forum).await;
    let request_id = rig.store("r1", CHANNEL).await;
    rig.connected(false);
    let unready = Outcome::Unready("decision_bot_unavailable");
    assert_eq!(rig.publish(request_id, "r1", CHANNEL).await, unready);
    assert_eq!(
        (rig.state(request_id).await, rig.posts()),
        (Publication::Stored, 0)
    );
    rig.connected(true);
    // Listed before the real post: posts that each fail one check against this request.
    let (mine, other) = (card::marker(request_id), card::marker(Uuid::new_v4()));
    let decoys = [
        (STRANGER, BOT, format!("x\n{mine}")),
        (BOT, STRANGER, format!("x\n{mine}")),
        (BOT, BOT, format!("> {mine}\n{other}")),
    ];
    for (index, (owner, author, content)) in decoys.into_iter().enumerate() {
        let id = forum + 1 + index as u64;
        let decoy = Thread {
            id,
            owner,
            author,
            content,
        };
        rig.mock.lock().unwrap().threads.push(decoy);
    }
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        outcomes.push(rig.publish(request_id, "r1", CHANNEL).await);
    }
    let bare = rig.store("bare", BARE).await;
    outcomes.push(rig.publish(bare, "bare", BARE).await);
    let thread = forum + 100;
    let posted = [
        Outcome::Pending,
        Outcome::Posted(thread),
        Outcome::Posted(thread),
        Outcome::NoCategory,
    ];
    assert_eq!(outcomes, posted);
    let mock = rig.mock.lock().unwrap();
    assert_eq!(mock.posts.len(), 1, "one post for one request");
    assert_eq!(
        mock.guarded_at_post,
        [true],
        "the forum is guarded before the POST"
    );
    // The disabled-button stage: every button disabled, carrying a Decision handle manual steer
    // refuses, and the notice that no selection is taken yet.
    let message = &mock.posts[0]["message"];
    let content = message["content"].as_str().unwrap();
    assert!(content.contains(card::NOT_YET) && content.contains(&format!("<@{OWNER}>")));
    assert_eq!(content.lines().last(), Some(mine.as_str()));
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
        assert_eq!(parsed, Err(CustomIdError::NotManualSteer));
    }
    drop(mock);
    rig.close().await;
}

/// Discord refuses the starter: the refusal is the answer every time and nothing is posted again.
#[tokio::test]
async fn decision_post_refused_is_not_sent_again_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = Rig::new(Answer::Forbidden, 940_821_200_200_000).await;
    let request_id = rig.store("r1", CHANNEL).await;
    let refused = Outcome::Refused("decision_post_forbidden".into());
    for _ in 0..3 {
        assert_eq!(rig.publish(request_id, "r1", CHANNEL).await, refused);
    }
    assert_eq!(rig.posts(), 1);
    let failed = Publication::PostFailed("decision_post_forbidden".into());
    assert_eq!(rig.state(request_id).await, failed);
    rig.close().await;
}

/// A forum refused before any POST leaves the request stored: once the configuration is fixed the
/// same request posts its starter, once.
#[tokio::test]
async fn decision_forum_refusal_leaves_the_request_to_post_once_fixed_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let forum = 940_821_200_300_000;
    let mut rig = Rig::new(Answer::Created, forum).await;
    let request_id = rig.store("r1", CHANNEL).await;
    rig.config.decisions.forum_name = Some("elsewhere".into());
    let mismatch = Outcome::Refused("forum_name_mismatch".into());
    assert_eq!(rig.publish(request_id, "r1", CHANNEL).await, mismatch);
    assert_eq!(
        (rig.state(request_id).await, rig.posts()),
        (Publication::Stored, 0)
    );
    rig.config.decisions.forum_name = None;
    let posted = Outcome::Posted(forum + 100);
    assert_eq!(rig.publish(request_id, "r1", CHANNEL).await, posted);
    assert_eq!(rig.posts(), 1);
    rig.close().await;
}

/// A thread target is not wired for selection: its post keeps disabled buttons, says why and
/// takes no click. The agent's own channel gets enabled buttons, once; the owner's first click is
/// the request's one choice, stored with its event before any delivery, and a restart starts
/// that stored selection exactly once.
#[tokio::test]
async fn decision_buttons_open_only_where_wired_and_take_one_choice_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let forum = 940_821_200_300_000;
    let rig = Rig::new(Answer::Created, forum).await;
    rig.edits();
    let scope = (CHANNEL, CHANNEL + 77);
    let stored = super::super::store(&rig.pool, &ask("thread"), scope, "e").await;
    let kept = rig.publish(stored.unwrap().0, "thread", CHANNEL).await;
    assert_eq!(kept, Outcome::Posted(forum + 100));
    let edit = rig.edits().pop().unwrap();
    let why = format!("{} — thread_target_not_wired", card::NOT_YET);
    assert!(edit["content"].as_str().unwrap().contains(&why), "{edit}");
    assert_eq!(edit["components"][0]["components"][0]["disabled"], true);
    assert_eq!(rig.click(forum + 100, 1).await, Some(Chosen::NotOpen));

    let (request_id, thread) = (rig.store("main", CHANNEL).await, forum + 101);
    for _ in 0..2 {
        let open = rig.publish(request_id, "main", CHANNEL).await;
        assert_eq!(open, Outcome::Open(thread));
    }
    assert_eq!(rig.posts(), 2, "an open request was posted again");
    let edit = rig.edits().pop().unwrap();
    assert!(
        edit["content"].as_str().unwrap().contains(card::OPEN),
        "{edit}"
    );
    assert_eq!(edit["components"][0]["components"][1]["disabled"], false);
    assert_eq!(edit["allowed_mentions"]["parse"], json!([]));
    assert_eq!(
        rig.count("decision_input_events").await,
        0,
        "before a click"
    );

    let handle = rig.handle(thread, 1).await;
    let stray = Click {
        handle: &handle,
        thread_id: thread,
        message_id: thread + 1,
        bot_user_id: BOT,
        actor: OWNER,
    };
    let stray = selection::choose_once(&rig.pool, &stray).await.unwrap();
    assert_eq!(stray, None, "a click on another message was taken");
    let Some(Chosen::First(first)) = rig.click(thread, 2).await else {
        panic!("the first click is the choice");
    };
    let already = |same| Chosen::Already {
        label: "내일 배포".into(),
        same,
    };
    assert_eq!(rig.click(thread, 2).await, Some(already(true)));
    assert_eq!(rig.click(thread, 1).await, Some(already(false)));
    for table in [
        "decision_choices",
        "decision_input_events",
        "decision_delivery_attempts",
    ] {
        assert_eq!(rig.count(table).await, 1, "{table}");
    }
    let event = crate::db::decisions::load_event(&rig.pool, first.event_id).await;
    let event = event.unwrap().expect("the selection's event");
    assert!(event.approves(&first.text), "{}", first.text);
    assert_eq!((event.actor_id(), event.channel_id()), (OWNER, CHANNEL));
    let unstarted = selection::unstarted(&rig.pool).await.unwrap();
    assert_eq!(unstarted, [first.clone()], "what a restart starts");
    let reserve = crate::db::decisions::reserve_first_attempt;
    let once = reserve(&rig.pool, first.event_id, "t").await.unwrap();
    let twice = reserve(&rig.pool, first.event_id, "t").await.unwrap();
    assert_eq!((once, twice), (true, false));
    assert!(selection::unstarted(&rig.pool).await.unwrap().is_empty());
    rig.close().await;
}

/// A write journaled by a process that is gone is settled without another write: its event
/// closes unknown, the card shows that with every button disabled, the announce bot archives and
/// locks the thread once, and a later click stores nothing.
#[tokio::test]
async fn decision_unknown_selection_is_settled_without_a_resend_then_sealed_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let forum = 940_821_200_400_000;
    let rig = Rig::new(Answer::Created, forum).await;
    rig.edits();
    let (request_id, thread) = (rig.store("r1", CHANNEL).await, forum + 100);
    let open = rig.publish(request_id, "r1", CHANNEL).await;
    assert_eq!(open, Outcome::Open(thread));
    let Some(Chosen::First(chosen)) = rig.click(thread, 1).await else {
        panic!("the first click is the choice");
    };
    let gone = "UPDATE decision_delivery_attempts
                SET state = 'may_have_sent', holder_instance_id = 'decision-node-gone'";
    sqlx::query(gone).execute(&rig.pool).await.unwrap();
    (crate::services::cluster::node_registry::SELF_INSTANCE_ID)
        .get_or_init(|| "decision-test-node".to_string());
    let before = rig.edits().len();
    for _ in 0..2 {
        lifecycle::converge(&rig.pool, &rig.registry).await;
    }
    let state: (String, String, bool) = sqlx::query_as(
        "SELECT a.state, r.lifecycle, r.sealed_at IS NOT NULL
         FROM decision_delivery_attempts a, decision_requests r WHERE r.request_id = $1",
    )
    .bind(request_id)
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    assert_eq!(state, ("unknown_closed".into(), "closed".into(), true));
    assert!(selection::unstarted(&rig.pool).await.unwrap().is_empty());
    let shown = rig.edits();
    assert_eq!(shown.len(), before + 1, "a sealed card is not edited again");
    let card = shown.last().unwrap();
    let unknown = card::ended(&chosen.label, AttemptState::UnknownClosed, None);
    assert!(
        card["content"].as_str().unwrap().contains(&unknown),
        "{card}"
    );
    let buttons = card["components"][0]["components"].as_array().unwrap();
    assert!(buttons.iter().all(|button| button["disabled"] == true));
    let seals = rig.mock.lock().unwrap().seals.clone();
    assert_eq!(seals.len(), 1, "{seals:?}");
    assert_eq!(
        (&seals[0]["archived"], &seals[0]["locked"]),
        (&json!(true), &json!(true))
    );
    let late = Chosen::Already {
        label: chosen.label.clone(),
        same: false,
    };
    assert_eq!(rig.click(thread, 2).await, Some(late));
    assert_eq!(rig.count("decision_choices").await, 1);
    rig.close().await;
}

/// Only the configured owner's click is a selection: anyone else's stores nothing.
#[tokio::test]
async fn decision_click_by_anyone_but_the_owner_stores_nothing_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let forum = 940_821_200_500_000;
    let rig = Rig::new(Answer::Created, forum).await;
    rig.edits();
    let (request_id, thread) = (rig.store("r1", CHANNEL).await, forum + 100);
    let open = rig.publish(request_id, "r1", CHANNEL).await;
    assert_eq!(open, Outcome::Open(thread));
    let handle = rig.handle(thread, 1).await;
    let ctx = rig.mock.lock().unwrap().ctx.clone().unwrap();
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(rig.pool.clone()));
    shared.settings.write().await.owner_user_id = Some(OWNER);
    let data = crate::services::discord::Data {
        shared,
        token: "test-token".into(),
        provider: ProviderKind::Claude,
    };
    let mut message = discord_mock::user_message(thread, "card");
    message.channel_id = serenity::ChannelId::new(thread);
    let click_by = |user: u64| -> serenity::ComponentInteraction {
        serde_json::from_value(json!({"id": "940487400000042",
            "application_id": BOT.to_string(), "token": "test-token", "version": 1,
            "data": {"custom_id": handle, "component_type": 2},
            "channel_id": thread.to_string(), "message": message, "locale": "ko",
            "entitlements": [], "attachment_size_limit": 10485760,
            "user": discord_mock::discord_user_json(user, "clicker", false)}))
        .expect("complete component fixture")
    };
    let click = super::super::component::click;
    let refused = click(&ctx, &click_by(STRANGER), &data).await;
    assert!(refused.contains("운영자만"), "{refused}");
    assert_eq!(rig.count("decision_choices").await, 0);
    let taken = click(&ctx, &click_by(OWNER), &data).await;
    assert!(taken.contains("선택 접수"), "{taken}");
    assert_eq!(rig.count("decision_choices").await, 1);
    rig.close().await;
}
