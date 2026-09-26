//! Custody notice contracts: episodes come from the real boot custody pass, and every notice
//! crosses serenity's HTTP client to a loopback Discord.

use super::custody_notice::notify_custody_episodes;
use super::nondestructive_loader_tests::{CLAUDE, Env, row};
use super::*;
use crate::services::discord::SharedData;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BOT: u64 = 5_998_900;

/// How the loopback Discord treats the next message it has not seen the nonce of.
#[derive(Clone, Copy)]
enum Post {
    Answer,
    LoseResponse,
    LoseRequest,
    Hang,
}

/// Created messages as (id, channel, nonce, content), each request's method and nonce, and
/// the plan for the next new messages.
#[derive(Clone, Default)]
struct Wire {
    messages: Arc<Mutex<Vec<(u64, u64, String, String)>>>,
    requests: Arc<Mutex<Vec<(Method, Option<String>)>>>,
    plan: Arc<Mutex<VecDeque<Post>>>,
}

fn author() -> Value {
    json!({ "id": BOT.to_string(), "username": "custody-bot", "discriminator": "0001",
        "avatar": null, "bot": true, "public_flags": 0 })
}

fn message_json(id: u64, channel: u64, content: &str) -> Value {
    json!({
        "id": id.to_string(), "channel_id": channel.to_string(), "author": author(),
        "content": content, "timestamp": "2026-09-26T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false,
        "mentions": [], "mention_roles": [], "mention_channels": [], "attachments": [],
        "embeds": [], "reactions": [], "nonce": null, "pinned": false, "type": 0,
        "flags": 0, "components": [], "sticker_items": [], "message_snapshots": []
    })
}

async fn discord(State(wire): State<Wire>, request: Request<Body>) -> Response {
    let (method, uri) = (request.method().clone(), request.uri().clone());
    let body = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap();
    let payload: Option<Value> = serde_json::from_slice(&body).ok();
    let nonce = payload
        .as_ref()
        .and_then(|payload| payload["nonce"].as_str().map(str::to_string));
    wire.requests
        .lock()
        .unwrap()
        .push((method.clone(), nonce.clone()));
    if uri.path() == "/api/v10/users/@me" {
        return Json(author()).into_response();
    }
    let channel = uri.path().strip_prefix("/api/v10/channels/");
    let channel = channel.and_then(|rest| rest.strip_suffix("/messages")?.parse::<u64>().ok());
    let Some(channel) = channel else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let messages = wire.messages.lock().unwrap().clone();
    let ours = messages.iter().filter(|message| message.1 == channel);
    if method == Method::GET {
        let query = uri.query().unwrap_or_default().split('&');
        let before = query.filter_map(|pair| pair.strip_prefix("before=")?.parse::<u64>().ok());
        let before = before.last();
        let page = ours
            .rev()
            .filter(|message| before.is_none_or(|id| message.0 < id));
        let page: Vec<Value> = page
            .take(100)
            .map(|m| message_json(m.0, m.1, &m.3))
            .collect();
        return Json(page).into_response();
    }
    // enforce_nonce: a nonce this channel already saw returns the message it created.
    let nonce = nonce.unwrap_or_default();
    if let Some(known) = ours.clone().find(|message| message.2 == nonce) {
        return Json(message_json(known.0, known.1, &known.3)).into_response();
    }
    let plan = wire
        .plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(Post::Answer);
    if matches!(plan, Post::LoseRequest) {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let content = payload
        .as_ref()
        .and_then(|payload| payload["content"].as_str());
    let content = content.unwrap_or_default().to_string();
    let created = {
        let mut messages = wire.messages.lock().unwrap();
        let id = 6_000_000 + messages.len() as u64;
        messages.push((id, channel, nonce, content.clone()));
        message_json(id, channel, &content)
    };
    match plan {
        Post::Hang => std::future::pending().await,
        Post::LoseResponse => StatusCode::BAD_GATEWAY.into_response(),
        _ => Json(created).into_response(),
    }
}

struct Harness {
    env: Env,
    wire: Wire,
    http: serenity::Http,
    shared: Arc<SharedData>,
}

async fn harness(plan: &[Post]) -> Harness {
    let env = Env::new();
    let wire = Wire::default();
    wire.plan.lock().unwrap().extend(plan);
    let app = Router::new().fallback(discord).with_state(wire.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = serenity::HttpBuilder::new("test-token").proxy(proxy);
    let http = http.ratelimiter_disabled(true).build();
    let shared = crate::services::discord::make_shared_data_for_tests();
    Harness {
        env,
        wire,
        http,
        shared,
    }
}

impl Harness {
    /// One boot: the real custody pass, then one notice pass at `now`.
    async fn boot(&self, now: DateTime<Utc>) {
        reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE).await;
        notify_custody_episodes(&self.http, &self.shared, &CLAUDE, now).await;
    }

    fn requests(&self) -> usize {
        self.wire.requests.lock().unwrap().len()
    }

    fn post_nonces(&self) -> Vec<String> {
        let requests = self.wire.requests.lock().unwrap();
        let posts = requests.iter().filter(|(method, _)| method == Method::POST);
        posts
            .map(|(_, nonce)| nonce.clone().unwrap_or_default())
            .collect()
    }

    fn contents(&self) -> Vec<String> {
        let messages = self.wire.messages.lock().unwrap();
        messages.iter().map(|message| message.3.clone()).collect()
    }

    fn episode_dirs(&self) -> Vec<PathBuf> {
        let root = self
            .env
            .dir()
            .with_file_name("discord_custody")
            .join("claude");
        let dirs = fs::read_dir(root).into_iter().flatten().flatten();
        dirs.map(|dir| dir.path()).collect()
    }
}

/// Seeds an owner-1 ExternalInput row whose transcript turn exists or is missing.
fn tui_direct(env: &Env, channel_id: u64, transcript: bool) -> InflightTurnState {
    let mut state = row(channel_id, Some("AgentDesk-claude-notice"));
    (state.request_owner_user_id, state.turn_source) = (1, TurnSource::ExternalInput);
    let out = env.dir().with_file_name(format!("{channel_id}.jsonl"));
    fs::create_dir_all(env.dir()).unwrap();
    if transcript {
        fs::write(&out, "turn\n").unwrap();
    }
    (state.output_path, state.turn_start_offset) = (Some(out.display().to_string()), Some(0));
    env.seed(&state, 0);
    state
}

fn after(now: DateTime<Utc>, seconds: i64) -> DateTime<Utc> {
    now + chrono::Duration::seconds(seconds)
}

// Contract: a TUI-direct custody episode is announced once in its channel, naming the
// episode and its custody path, and a later boot sends nothing more.
#[tokio::test]
async fn a_tui_direct_episode_is_announced_once_across_boots() {
    let h = harness(&[]).await;
    tui_direct(&h.env, 5_998_001, true);
    let now = Utc::now();
    h.boot(now).await;
    let requests = h.requests();
    h.boot(after(now, 300)).await;

    let [dir] = h.episode_dirs().try_into().unwrap();
    let [text] = h.contents().try_into().unwrap();
    let episode = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        text.contains("재시작으로 이 턴 출력 일부가 전달되지 않았을 수 있음"),
        "{text}"
    );
    assert!(
        text.contains(&episode) && text.contains(&dir.display().to_string()),
        "{text}"
    );
    assert!(!text.contains("보존 실패"), "{text}");
    assert_eq!(h.requests(), requests, "a settled episode makes no request");
}

// Contract: a POST whose response was lost is retried inside the nonce window with the same
// nonce, and Discord's nonce check leaves one message.
#[tokio::test]
async fn a_lost_response_inside_the_nonce_window_is_retried_with_the_same_nonce() {
    let h = harness(&[Post::LoseResponse]).await;
    tui_direct(&h.env, 5_998_011, true);
    let now = Utc::now();
    h.boot(now).await;
    h.boot(after(now, 30)).await;

    let nonces = h.post_nonces();
    assert_eq!(nonces.len(), 2);
    assert_eq!(nonces[0], nonces[1]);
    assert_eq!(h.contents().len(), 1);
}

// Contract: outside the nonce window, a notice whose token is already in the channel
// history is settled as sent with no second POST.
#[tokio::test]
async fn outside_the_window_a_notice_found_in_history_is_settled_without_a_post() {
    let h = harness(&[Post::LoseResponse]).await;
    tui_direct(&h.env, 5_998_021, true);
    let now = Utc::now();
    h.boot(now).await;
    h.boot(after(now, 121)).await;
    let requests = h.requests();
    h.boot(after(now, 400)).await;

    assert_eq!((h.post_nonces().len(), h.contents().len()), (1, 1));
    assert_eq!(
        h.requests(),
        requests,
        "the history match settled the episode"
    );
}

// Contract: outside the nonce window, a notice missing from the channel history is posted
// again with a new nonce.
#[tokio::test]
async fn outside_the_window_a_notice_missing_from_history_is_posted_again() {
    let h = harness(&[Post::LoseRequest]).await;
    tui_direct(&h.env, 5_998_031, true);
    let now = Utc::now();
    h.boot(now).await;
    h.boot(after(now, 121)).await;

    let nonces = h.post_nonces();
    assert_eq!(nonces.len(), 2);
    assert_ne!(nonces[0], nonces[1]);
    assert_eq!(h.contents().len(), 1);
}

// Contract: the posting record is durable before the POST, so a boot that dies mid-POST
// leaves the next boot the same nonce and Discord keeps one message.
#[tokio::test]
async fn a_crash_after_the_posting_record_is_settled_by_the_next_boot() {
    let h = harness(&[Post::Hang]).await;
    tui_direct(&h.env, 5_998_041, true);
    let now = Utc::now();
    let crashed = tokio::time::timeout(Duration::from_secs(5), h.boot(now)).await;
    assert!(crashed.is_err(), "the first POST must still be in flight");
    h.boot(after(now, 30)).await;

    let nonces = h.post_nonces();
    assert_eq!(nonces.len(), 2);
    assert_eq!(nonces[0], nonces[1]);
    assert_eq!(h.contents().len(), 1);
}

// Contract: an episode that holds no TUI-direct turn is preserved but never announced.
#[tokio::test]
async fn an_episode_without_a_tui_direct_turn_is_not_announced() {
    let h = harness(&[]).await;
    h.env.seed(&row(5_998_051, None), 0);
    h.boot(Utc::now()).await;

    assert_eq!(h.episode_dirs().len(), 1);
    assert_eq!(h.requests(), 0);
}

// Contract: an episode whose custody copy failed is announced as a preservation failure.
#[tokio::test]
async fn an_episode_whose_copy_failed_is_announced_as_a_preservation_failure() {
    let h = harness(&[]).await;
    tui_direct(&h.env, 5_998_061, false);
    h.boot(Utc::now()).await;

    let [text] = h.contents().try_into().unwrap();
    assert!(text.contains("보존 실패"), "{text}");
}

// Contract: an anchorless turn whose start offset is rewritten between boots is announced once,
// and another anchorless turn started in the same second is announced on its own.
#[tokio::test]
async fn an_anchorless_turn_is_announced_once_and_apart_from_a_same_second_turn() {
    let h = harness(&[]).await;
    let mut turn = tui_direct(&h.env, 5_998_071, true);
    turn.user_msg_id = 0;
    for offset in [0, 1] {
        turn.turn_start_offset = Some(offset);
        h.env.seed(&turn, 0);
        h.boot(Utc::now()).await;
    }
    assert_eq!(h.contents().len(), 1);
    turn.finalizer_turn_id += 1;
    h.env.seed(&turn, 0);
    h.boot(Utc::now()).await;
    assert_eq!(h.contents().len(), 2);
}
